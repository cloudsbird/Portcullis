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

    /// Inference failures since the last call, for detectors that can fail (the ML
    /// one). The gateway turns this into a metric, so a detector that has quietly
    /// stopped working is visible.
    fn take_failures(&self) -> u64 {
        0
    }
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

/// Longest text, in bytes, handed to an ML detector in one call.
///
/// A transformer encoder's memory grows with the square of its input length, so
/// one call over a long segment (a big tool output, a pasted file) can need gigabytes:
/// measured peaks were 0.8 GB at 2 KB, 2.5 GB at 10 KB and an OOM kill at 30 KB.
/// Windowing keeps every call near the model's idle footprint, whatever the segment
/// length. This counts bytes, not tokens; byte-dense text (CJK, punctuation-heavy
/// JSON) yields more tokens per byte, which is why it sits below the measured 2 KB.
pub const ML_WINDOW_BYTES: usize = 1536;

/// Bytes shared by neighbouring windows, so an entity cut by one window's edge is
/// seen whole by the next. PII spans are short; this is far longer than any of them.
pub const ML_WINDOW_OVERLAP: usize = 256;

/// Largest char boundary at or below `i`.
fn floor_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Split `text` into byte ranges of at most `window` bytes, each starting where the
/// previous one's last `overlap` bytes begin. Cuts land on whitespace where there is
/// any in reach, so words are not split; otherwise on a char boundary. Every range is
/// a valid slice of `text`, and together they cover all of it.
pub fn windows(text: &str, window: usize, overlap: usize) -> Vec<(usize, usize)> {
    // A window of a few bytes cannot hold even one 4-byte character, and an overlap
    // of half the window or more would stop the loop from making progress.
    let window = window.max(16);
    let overlap = overlap.min(window / 2 - 1);
    let len = text.len();
    let mut out = Vec::new();
    let mut start = 0;
    while len - start > window {
        let hard_end = floor_boundary(text, start + window);
        // Prefer to end just after whitespace in the window's back half.
        let back = floor_boundary(text, start + window / 2);
        let end = match text[back..hard_end].rfind(char::is_whitespace) {
            Some(i) => {
                let ws = back + i;
                ws + text[ws..].chars().next().map_or(1, char::len_utf8)
            }
            None => hard_end,
        };
        out.push((start, end));
        // Step back by `overlap`, then forward to the next word start inside it.
        let lap = floor_boundary(text, end.saturating_sub(overlap));
        let next = match text[lap..end].find(char::is_whitespace) {
            Some(i) => {
                let ws = lap + i;
                ws + text[ws..].chars().next().map_or(1, char::len_utf8)
            }
            None => lap,
        };
        // `end` lies past the window's midpoint and `overlap < window/2`, so `next`
        // is ahead of `start` at any sane size; falling back to `end` guarantees
        // progress even for a degenerate one.
        start = if next > start { next } else { end };
    }
    out.push((start, len));
    out
}

/// Run `detector` over `text` window by window (see [`windows`]) and return spans in
/// `text`'s coordinates. Windows run one after another, never batched, so peak memory
/// is that of a single window. Overlapping windows can report the same entity twice;
/// [`merge`] keeps one. Offsets are clamped to their window but not snapped to char
/// boundaries — callers that slice must do that.
pub fn detect_windowed(detector: &dyn Detector, text: &str) -> Vec<Span> {
    detect_windowed_with(detector, text, ML_WINDOW_BYTES, ML_WINDOW_OVERLAP)
}

pub fn detect_windowed_with(
    detector: &dyn Detector,
    text: &str,
    window: usize,
    overlap: usize,
) -> Vec<Span> {
    let mut spans = Vec::new();
    for (a, b) in windows(text, window, overlap) {
        let w = b - a;
        spans.extend(
            detector
                .detect(&text[a..b])
                .into_iter()
                .filter_map(|mut s| {
                    s.start = s.start.min(w) + a;
                    s.end = s.end.min(w) + a;
                    (s.start < s.end).then_some(s)
                }),
        );
    }
    spans
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
pub fn merge(spans: Vec<Span>) -> Vec<Span> {
    merge_by(spans, |s| s)
}

/// [`merge`] for items that carry a span plus something else. The sort is stable, so
/// among exact ties the item pushed first wins.
pub fn merge_by<T>(mut items: Vec<T>, span: impl Fn(&T) -> &Span) -> Vec<T> {
    items.sort_by(|a, b| {
        let (a, b) = (span(a), span(b));
        a.start
            .cmp(&b.start)
            .then_with(|| source_priority(a.source).cmp(&source_priority(b.source)))
            .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
    });
    let mut out: Vec<T> = Vec::new();
    for item in items {
        if let Some(last) = out.last() {
            if span(&item).start < span(last).end {
                continue;
            }
        }
        out.push(item);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every range is a valid slice, within the window, and the ranges cover `text`
    /// end to end with each one overlapping or touching the previous.
    fn check_windows(text: &str, window: usize, overlap: usize) -> Vec<(usize, usize)> {
        let ws = windows(text, window, overlap);
        assert_eq!(ws.first().unwrap().0, 0);
        assert_eq!(ws.last().unwrap().1, text.len());
        for (i, &(a, b)) in ws.iter().enumerate() {
            assert!(text.is_char_boundary(a) && text.is_char_boundary(b));
            assert!(a < b || text.is_empty(), "empty window {a}..{b}");
            assert!(b - a <= window.max(16), "window {a}..{b} over {window}");
            if i > 0 {
                let (pa, pb) = ws[i - 1];
                assert!(a > pa, "no progress: {pa}..{pb} then {a}..{b}");
                assert!(a <= pb, "gap between {pa}..{pb} and {a}..{b}");
            }
        }
        ws
    }

    #[test]
    fn short_text_is_one_window() {
        assert_eq!(windows("", 64, 8), vec![(0, 0)]);
        let exact = "a".repeat(64);
        assert_eq!(windows(&exact, 64, 8), vec![(0, 64)]);
        let over = "a".repeat(65);
        assert_eq!(check_windows(&over, 64, 8).len(), 2);
    }

    #[test]
    fn windows_cut_on_whitespace_when_they_can() {
        let text = "word ".repeat(200);
        for (a, b) in check_windows(&text, 64, 16) {
            assert!(a == 0 || text[..a].ends_with(' '), "starts mid-word at {a}");
            assert!(
                b == text.len() || text[..b].ends_with(' '),
                "ends mid-word at {b}"
            );
        }
    }

    #[test]
    fn windows_without_whitespace_or_with_multibyte_text_stay_valid() {
        check_windows(&"x".repeat(5000), 64, 16);
        check_windows(&"{\"k\":\"v\"},".repeat(500), 64, 16);
        check_windows(&"é€😀字".repeat(400), 64, 16);
        check_windows(&"😀".repeat(400), 17, 8);
        // Degenerate settings are clamped rather than looping or panicking.
        check_windows(&"é€😀 字".repeat(100), 0, 1000);
        check_windows(&"é€😀 字".repeat(100), 16, 16);
    }

    /// Finds "Alice Smith" only when it sees the whole name, and records the longest
    /// text it was given.
    #[derive(Default)]
    struct FullName {
        longest: std::sync::atomic::AtomicUsize,
    }
    impl Detector for FullName {
        fn detect(&self, text: &str) -> Vec<Span> {
            self.longest
                .fetch_max(text.len(), std::sync::atomic::Ordering::Relaxed);
            text.match_indices("Alice Smith")
                .map(|(i, m)| Span {
                    start: i,
                    end: i + m.len(),
                    label: "PERSON".into(),
                    source: "onnx",
                    score: 0.9,
                })
                .collect()
        }
    }

    #[test]
    fn windowed_detection_is_bounded_and_finds_entities_on_every_cut() {
        let det = FullName::default();
        // Shift the name across every offset relative to the window edges.
        for pad in 0..80 {
            let text = format!(
                "{}Alice Smith {}",
                "filler ".repeat(pad),
                "tail ".repeat(40)
            );
            let spans = merge(detect_windowed_with(&det, &text, 64, 24));
            let found: Vec<&str> = spans.iter().map(|s| &text[s.start..s.end]).collect();
            assert_eq!(found, vec!["Alice Smith"], "pad {pad}");
        }
        assert!(det.longest.load(std::sync::atomic::Ordering::Relaxed) <= 64);
    }

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
