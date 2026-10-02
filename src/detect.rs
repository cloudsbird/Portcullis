use std::collections::HashMap;

use regex::{Regex, RegexBuilder};

use crate::store::{HiddenForm, Store};

/// A detected sensitive span.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub label: String,
    pub source: &'static str,
    /// Confidence score, when available ( ONNX path ). Dictionary/regex spans set this to 0.0.
    pub score: f32,
}

/// Anything that can propose spans. The ONNX/GLiNER2 detector implements this at M1.
///
/// `Send + Sync` is required because the gateway is shared across axum handler
/// tasks behind an `Arc<Mutex<..>>`.
pub trait Detector: Send + Sync {
    fn detect(&self, text: &str) -> Vec<Span>;
}

/// Deterministic dictionary layer — the **guarantee**. Runs first, always.
///
/// All forms are compiled into one case-insensitive alternation, so a scan is a
/// single pass over the text. Matching runs on the *original* text, so span
/// offsets are always valid char boundaries — lowercasing a copy and reusing its
/// offsets is wrong for characters whose case mapping changes byte length.
pub struct DictionaryDetector {
    regex: Option<Regex>,
    /// `lowercased form -> label`, used to name a match without a group scan.
    labels: HashMap<String, String>,
    /// Labels in alternation order, the fallback when the map misses.
    ordered: Vec<String>,
}

impl DictionaryDetector {
    pub fn new(store: &Store) -> Self {
        Self::for_scope(store, None)
    }

    /// The dictionary as seen by a request in `scope` (`None`: every entry).
    pub fn for_scope(store: &Store, scope: Option<&str>) -> Self {
        let mut forms: Vec<HiddenForm> = store
            .hidden_entries_for(scope)
            .into_iter()
            .filter(|h| !h.form.is_empty())
            .collect();
        // Longest first, so at one start position the longest form wins.
        forms.sort_by_key(|h| std::cmp::Reverse(h.form.len()));

        // De-duplicate by lowercased form. A non-whole-word entry is the stricter
        // protection, so it wins over a whole-word duplicate.
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut kept: Vec<HiddenForm> = Vec::new();
        for h in forms {
            let key = h.form.to_lowercase();
            match seen.get(&key) {
                Some(&i) => {
                    if kept[i].whole_word && !h.whole_word {
                        kept[i] = h;
                    }
                }
                None => {
                    seen.insert(key, kept.len());
                    kept.push(h);
                }
            }
        }

        if kept.is_empty() {
            return Self {
                regex: None,
                labels: HashMap::new(),
                ordered: Vec::new(),
            };
        }

        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let alternatives: Vec<String> = kept
            .iter()
            .map(|h| {
                // `\b` only means something next to a word character.
                let edge = |c: Option<char>| {
                    if h.whole_word && c.is_some_and(is_word) {
                        r"\b"
                    } else {
                        ""
                    }
                };
                let lead = edge(h.form.chars().next());
                let trail = edge(h.form.chars().next_back());
                format!("({lead}{}{trail})", regex::escape(&h.form))
            })
            .collect();

        let regex = RegexBuilder::new(&alternatives.join("|"))
            .case_insensitive(true)
            // Escaped literals cannot be invalid; only an absurd number of terms
            // could exceed this, and that must fail loudly rather than silently
            // disable the guarantee.
            .size_limit(256 << 20)
            .build()
            .expect("escaped dictionary terms form a valid regex");

        let labels = kept
            .iter()
            .map(|h| (h.form.to_lowercase(), h.label.clone()))
            .collect();
        let ordered = kept.into_iter().map(|h| h.label).collect();
        Self {
            regex: Some(regex),
            labels,
            ordered,
        }
    }
}

impl Detector for DictionaryDetector {
    fn detect(&self, text: &str) -> Vec<Span> {
        let Some(regex) = &self.regex else {
            return Vec::new();
        };
        let mut spans = Vec::new();
        for caps in regex.captures_iter(text) {
            let m = caps.get(0).expect("group 0 always participates");
            let label = self
                .labels
                .get(&m.as_str().to_lowercase())
                .cloned()
                .or_else(|| {
                    // Case folding can match text whose lowercase differs from the form.
                    (1..caps.len())
                        .find(|&i| caps.get(i).is_some())
                        .map(|i| self.ordered[i - 1].clone())
                });
            if let Some(label) = label {
                spans.push(Span {
                    start: m.start(),
                    end: m.end(),
                    label,
                    source: "dictionary",
                    score: 0.0,
                });
            }
        }
        spans
    }
}

/// `Some(false)`-style gate run on a regex hit before it becomes a span.
type Validator = fn(&str) -> bool;

pub struct RegexDetector {
    patterns: Vec<(Regex, &'static str, Option<Validator>)>,
}

fn digits(s: &str) -> impl Iterator<Item = u32> + '_ {
    s.chars().filter_map(|c| c.to_digit(10))
}

/// Luhn checksum over the digits of `s`, with the usual 13–19 digit card length.
fn luhn_valid(s: &str) -> bool {
    let ds: Vec<u32> = digits(s).collect();
    if !(13..=19).contains(&ds.len()) {
        return false;
    }
    let sum: u32 = ds
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 {
                    x - 9
                } else {
                    x
                }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// A phone-shaped run needs enough digits to be a number at all, and an ISO date
/// (`2026-10-02`) is not one.
fn plausible_phone(s: &str) -> bool {
    let n = digits(s).count();
    if n < 7 {
        return false;
    }
    let b = s.as_bytes();
    let iso_date = b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        });
    !iso_date
}

impl RegexDetector {
    pub fn new() -> Self {
        // Order matters for ties: at the same start and length, the earlier pattern
        // wins in `merge`, so the more specific label goes first. A card that fails
        // Luhn is still caught as a PHONE-shaped run, so validation never lowers recall.
        let defs: &[(&str, &str, Option<Validator>)] = &[
            (r"[\w.+-]+@[\w-]+\.[\w.-]+", "EMAIL", None),
            (r"\b(?:\d[ -]?){12,18}\d\b", "CARD", Some(luhn_valid)),
            (r"\b(?:\d{1,3}\.){3}\d{1,3}\b", "IP", None),
            (r"\+?\d[\d\s().-]{6,}\d", "PHONE", Some(plausible_phone)),
        ];
        Self {
            patterns: defs
                .iter()
                .map(|(p, l, v)| (Regex::new(p).expect("builtin pattern is valid"), *l, *v))
                .collect(),
        }
    }
}

impl Default for RegexDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector for RegexDetector {
    fn detect(&self, text: &str) -> Vec<Span> {
        let mut spans = Vec::new();
        for (re, label, validator) in &self.patterns {
            for m in re.find_iter(text) {
                if validator.is_some_and(|ok| !ok(m.as_str())) {
                    continue;
                }
                spans.push(Span {
                    start: m.start(),
                    end: m.end(),
                    label: (*label).to_string(),
                    source: "regex",
                    score: 0.0,
                });
            }
        }
        spans
    }
}

/// Source priority: the deterministic layers win over the ML detector on overlap.
fn source_priority(source: &str) -> u8 {
    match source {
        "dictionary" => 0,
        "regex" => 1,
        _ => 2,
    }
}

/// Merge spans, dropping overlaps: earliest start wins, then highest-priority
/// source, then longest. This keeps the deterministic dictionary/regex layers
/// ahead of the ONNX detector — it can add recall, never override the guarantee.
pub fn merge(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| source_priority(a.source).cmp(&source_priority(b.source)))
            .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
    });
    let mut out: Vec<Span> = Vec::new();
    for s in spans {
        if let Some(last) = out.last() {
            if s.start < last.end {
                continue;
            }
        }
        out.push(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(text: &str) -> Vec<(String, String)> {
        merge(RegexDetector::new().detect(text))
            .into_iter()
            .map(|s| (s.label, text[s.start..s.end].to_string()))
            .collect()
    }

    #[test]
    fn valid_card_is_labelled_card() {
        let got = labels("pay 4111 1111 1111 1111 today");
        assert!(
            got.contains(&("CARD".into(), "4111 1111 1111 1111".into())),
            "{got:?}"
        );
    }

    #[test]
    fn card_that_fails_luhn_is_still_redacted_as_a_number() {
        let got = labels("pay 4111 1111 1111 1112 today");
        assert!(got.iter().any(|(l, _)| l == "PHONE"), "{got:?}");
    }

    #[test]
    fn dates_and_short_digit_runs_are_not_phones() {
        assert!(labels("released 2026-10-02.").is_empty());
        assert!(labels("see 1.........9").is_empty());
    }

    #[test]
    fn real_phones_and_ips_are_kept() {
        assert!(labels("call +1 (415) 555-0132")
            .iter()
            .any(|(l, _)| l == "PHONE"));
        assert_eq!(
            labels("host 192.168.10.4"),
            vec![("IP".into(), "192.168.10.4".into())]
        );
    }
}
