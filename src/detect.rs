use regex::Regex;

use crate::store::Store;

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
pub struct DictionaryDetector<'a> {
    store: &'a Store,
}

impl<'a> DictionaryDetector<'a> {
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }
}

impl Detector for DictionaryDetector<'_> {
    fn detect(&self, text: &str) -> Vec<Span> {
        let mut spans = Vec::new();
        let lower = text.to_lowercase();
        for (form, label) in self.store.hidden_forms() {
            let needle = form.to_lowercase();
            if needle.is_empty() {
                continue;
            }
            let mut from = 0usize;
            while let Some(idx) = lower[from..].find(&needle) {
                let start = from + idx;
                let end = start + needle.len();
                spans.push(Span { start, end, label: label.clone(), source: "dictionary", score: 0.0 });
                from = end.max(start + 1);
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
            patterns: defs.iter().map(|(p, l)| (Regex::new(p).unwrap(), *l)).collect(),
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
