use regex::Regex;

use crate::store::Store;

/// A detected sensitive span.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub label: String,
    pub source: &'static str,
}

/// Anything that can propose spans. The ONNX/GLiNER2 detector implements this at M1.
pub trait Detector {
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
                spans.push(Span { start, end, label: label.clone(), source: "dictionary" });
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
                });
            }
        }
        spans
    }
}

/// Merge spans, dropping overlaps (leftmost-longest wins).
pub fn merge(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
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
