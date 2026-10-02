use std::collections::{HashMap, VecDeque};

use sha2::{Digest, Sha256};

use crate::detect::{merge, Detector, DictionaryDetector, RegexDetector};
use crate::store::Store;
use crate::vault::Vault;

/// Delta-cache size at which it is flushed, so a long-lived proxy cannot grow it
/// without bound.
const CACHE_LIMIT: usize = 20_000;

/// A detected span, by offset only — never the text it covers.
#[derive(Clone)]
struct CachedSpan {
    start: usize,
    end: usize,
    label: String,
}

/// Maximum number of recent auto-suggest candidates to retain.
const SUGGESTION_LIMIT: usize = 200;

/// The gateway: the learned store, the vault, and the per-session delta cache.
pub struct Gateway {
    pub store: Store,
    vault: Vault,
    /// `sha256(original segment) -> spans`. Stores offsets, never raw originals.
    cache: HashMap<String, Vec<CachedSpan>>,
    regexes: RegexDetector,
    /// Optional additional detector (e.g. the ONNX GLiNER2 model). Runs *after* the
    /// deterministic layers, so it can only add recall — never override the guarantee.
    extra: Option<Box<dyn Detector>>,
    /// Recent spans redacted by a non-dictionary detector (regex, ONNX, …).
    suggestions: VecDeque<String>,
}

impl Gateway {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            vault: Vault::new(),
            cache: HashMap::new(),
            regexes: RegexDetector::new(),
            extra: None,
            suggestions: VecDeque::new(),
        }
    }

    /// Attach an extra detector (e.g. `OnnxDetector`). Dictionary + regex still run
    /// first; this only adds recall.
    pub fn with_detector(mut self, detector: Box<dyn Detector>) -> Self {
        self.extra = Some(detector);
        self
    }

    /// Build a gateway with the default detector stack: the deterministic
    /// dictionary + regex layers, plus the native ONNX detector when the `onnx`
    /// feature is enabled and a model is present at `PORTCULLIS_MODEL_DIR`.
    pub fn with_default_detectors(store: Store) -> Self {
        // `mut` is only needed when the `onnx` feature is enabled (see below).
        #[allow(unused_mut)]
        let mut gw = Gateway::new(store);
        #[cfg(feature = "onnx")]
        {
            match crate::detect_onnx::OnnxDetector::from_env() {
                Ok(detector) => gw = gw.with_detector(Box::new(detector)),
                Err(e) => tracing::warn!("ONNX detector disabled ({e})"),
            }
        }
        gw
    }

    /// Build a gateway from resolved [`DetectorSettings`] — the path the CLI and
    /// the proxy use, so a named model preset supplies the directory, its label
    /// set and its threshold together.
    #[cfg_attr(not(feature = "onnx"), allow(unused_variables))]
    pub fn with_settings(store: Store, settings: &crate::model::DetectorSettings) -> Self {
        // `mut` is only needed when the `onnx` feature is enabled (see below).
        #[allow(unused_mut)]
        let mut gw = Gateway::new(store);
        #[cfg(feature = "onnx")]
        {
            match crate::detect_onnx::OnnxDetector::from_dir(&settings.dir) {
                Ok(mut detector) => {
                    if let Some(labels) = &settings.labels {
                        detector = detector.with_labels(labels.clone());
                    }
                    if let Some(threshold) = settings.threshold {
                        detector = detector.with_threshold(threshold);
                    }
                    gw = gw.with_detector(Box::new(detector));
                }
                Err(e) => {
                    let which = settings
                        .model_name
                        .as_deref()
                        .map(|n| format!(" [model '{n}']"))
                        .unwrap_or_default();
                    tracing::warn!("detector disabled ({e}){which}");
                }
            }
        }
        gw
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Return the recent auto-suggest candidates.
    pub fn suggestions(&self) -> Vec<String> {
        self.suggestions.iter().cloned().collect()
    }

    fn is_known(&self, term: &str) -> bool {
        if self.store.is_allowed(term) {
            return true;
        }
        let lower = term.to_lowercase();
        self.store
            .hidden_forms()
            .iter()
            .any(|(form, _)| form.to_lowercase() == lower)
    }

    fn push_suggestion(&mut self, term: &str) {
        // Deduplicate: move an existing entry to the back so repeats do not
        // flood the bounded buffer, and the most recent sightings are kept.
        if let Some(pos) = self.suggestions.iter().position(|t| t == term) {
            self.suggestions.remove(pos);
        }
        if self.suggestions.len() >= SUGGESTION_LIMIT {
            self.suggestions.pop_front();
        }
        self.suggestions.push_back(term.to_string());
    }

    /// Teach a term. **Invalidates the delta cache** (invariant 3): a cached redaction
    /// predates this term and would otherwise leak it in old segments forever.
    pub fn teach(&mut self, term: &str, label: &str, scope: &str) {
        self.teach_with(term, label, scope, false);
    }

    /// Like [`Gateway::teach`], choosing whether the term only matches as a whole word.
    pub fn teach_with(&mut self, term: &str, label: &str, scope: &str, whole_word: bool) {
        self.store.teach_with(term, label, scope, whole_word);
        self.cache.clear();
    }

    pub fn unteach(&mut self, term: &str) -> bool {
        let changed = self.store.unteach(term);
        if changed {
            self.cache.clear();
        }
        changed
    }

    fn hash(seg: &str) -> String {
        let mut h = Sha256::new();
        h.update(seg.as_bytes());
        hex::encode(h.finalize())
    }

    /// Find what to redact in one segment: deterministic dictionary first, then
    /// builtin patterns, then the optional ML detector. Returns offsets only —
    /// never the matched text — so the result is safe to cache and can be applied
    /// against any session's vault.
    fn detect_spans(&mut self, dict: &DictionaryDetector, seg: &str) -> Vec<CachedSpan> {
        let mut spans = dict.detect(seg);
        spans.extend(self.regexes.detect(seg));
        if let Some(extra) = &self.extra {
            spans.extend(extra.detect(seg));
        }
        let mut kept = Vec::new();
        for s in merge(spans) {
            let real = &seg[s.start..s.end];
            if self.store.is_allowed(real) {
                continue;
            }
            if s.source != "dictionary" && !self.is_known(real) {
                self.push_suggestion(real);
            }
            kept.push(CachedSpan {
                start: s.start,
                end: s.end,
                label: s.label,
            });
        }
        kept
    }

    /// Replace each span with its placeholder from `vault`.
    fn apply_spans(vault: &mut Vault, seg: &str, spans: &[CachedSpan]) -> String {
        let mut out = String::with_capacity(seg.len());
        let mut cursor = 0usize;
        for s in spans {
            out.push_str(&seg[cursor..s.start]);
            out.push_str(&vault.placeholder_for(&s.label, &seg[s.start..s.end]));
            cursor = s.end;
        }
        out.push_str(&seg[cursor..]);
        out
    }

    /// Redact a whole outbound payload using delta-scan, minting placeholders in
    /// `vault`.
    ///
    /// Give each request its **own** vault (see [`Vault`]): placeholders are only
    /// meaningful within one conversation, and a vault shared between clients would
    /// restore one client's values into another client's response.
    ///
    /// Invariant 1: the cache holds span offsets, never raw text, and a hit
    /// re-emits the **redacted** form.
    /// Invariant 2: every segment passes through here — there is no bypass path.
    pub fn process_with(&mut self, vault: &mut Vault, segments: &[String]) -> Vec<String> {
        if self.cache.len() >= CACHE_LIMIT {
            self.cache.clear();
        }
        // Compiled once per call, and only if some segment misses the cache.
        let mut dict: Option<DictionaryDetector> = None;
        let mut out = Vec::with_capacity(segments.len());
        for seg in segments {
            let h = Self::hash(seg);
            let spans = match self.cache.get(&h) {
                Some(spans) => spans.clone(),
                None => {
                    let dict = dict.get_or_insert_with(|| DictionaryDetector::new(&self.store));
                    let spans = self.detect_spans(dict, seg);
                    self.cache.insert(h, spans.clone());
                    spans
                }
            };
            out.push(Self::apply_spans(vault, seg, &spans));
        }
        out
    }

    /// [`Gateway::process_with`] against the gateway's own vault. Meant for the
    /// single-user CLI and for tests; the proxy uses one vault per request.
    pub fn process(&mut self, segments: &[String]) -> Vec<String> {
        let mut vault = std::mem::take(&mut self.vault);
        let out = self.process_with(&mut vault, segments);
        self.vault = vault;
        out
    }

    /// Invariant 4: fail-closed outbound assertion (cheap; no model).
    pub fn assert_clean(&self, outbound: &[String]) -> Result<(), String> {
        let dict = DictionaryDetector::new(&self.store);
        for seg in outbound {
            for span in dict.detect(seg) {
                if !self.store.is_allowed(&seg[span.start..span.end]) {
                    return Err("residual protected term in outbound".into());
                }
            }
            if seg.contains("<<") && !seg.contains(">>") {
                return Err("malformed placeholder in outbound".into());
            }
        }
        Ok(())
    }

    /// Rehydrate a model response locally (tolerant of placeholder reformatting).
    pub fn rehydrate(&self, text: &str) -> String {
        self.vault.restore(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_are_deduplicated_and_bounded() {
        let mut gw = Gateway::new(Store::default());

        // Repeats must not flood the buffer.
        for _ in 0..10 {
            gw.push_suggestion("repeat@example.com");
        }
        assert_eq!(gw.suggestions().len(), 1);

        // The buffer stays bounded at SUGGESTION_LIMIT.
        for i in 0..(SUGGESTION_LIMIT + 50) {
            gw.push_suggestion(&format!("user{i}@example.com"));
        }
        assert_eq!(gw.suggestions().len(), SUGGESTION_LIMIT);
    }
}
