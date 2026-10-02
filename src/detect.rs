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
        let mut forms: Vec<HiddenForm> = store
            .hidden_entries()
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

/// Builtin structured patterns (emails, phones, cards, IPs). Cheap, deterministic.
pub struct RegexDetector {
    patterns: Vec<(Regex, &'static str)>,
}

impl RegexDetector {
    pub fn new() -> Self {
        let defs: &[(&str, &str)] = &[
            (r"[\w.+-]+@[\w-]+\.[\w.-]+", "EMAIL"),
            (r"\+?\d[\d\s().-]{6,}\d", "PHONE"),
            (r"\b(?:\d[ -]*?){13,16}\b", "CARD"),
            (r"\b(?:\d{1,3}\.){3}\d{1,3}\b", "IP"),
        ];
        Self {
            patterns: defs
                .iter()
                .map(|(p, l)| (Regex::new(p).unwrap(), *l))
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
        for (re, label) in &self.patterns {
            for m in re.find_iter(text) {
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
