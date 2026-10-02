use std::collections::{HashMap, VecDeque};

use sha2::{Digest, Sha256};

use crate::detect::{merge, Detector, DictionaryDetector, RegexDetector};
use crate::store::Store;
use crate::vault::Vault;

/// Maximum number of recent auto-suggest candidates to retain.
const SUGGESTION_LIMIT: usize = 200;

/// The gateway: the learned store, the vault, and the per-session delta cache.
pub struct Gateway {
    pub store: Store,
    vault: Vault,
    /// `sha256(original segment) -> redacted segment`. Never stores raw originals.
    cache: HashMap<String, String>,
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
                Err(e) => eprintln!("portcullis: ONNX detector disabled ({e})"),
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
        if self.suggestions.len() >= SUGGESTION_LIMIT {
            self.suggestions.pop_front();
        }
        self.suggestions.push_back(term.to_string());
    }

    /// Teach a term. **Invalidates the delta cache** (invariant 3): a cached redaction
    /// predates this term and would otherwise leak it in old segments forever.
    pub fn teach(&mut self, term: &str, label: &str, scope: &str) {
        self.store.teach(term, label, scope);
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

    /// Redact one segment: deterministic dictionary first, then builtin patterns.
    fn redact_segment(&mut self, seg: &str) -> String {
        let mut spans = {
            let dict = DictionaryDetector::new(&self.store);
            dict.detect(seg)
        };
        spans.extend(self.regexes.detect(seg));
        if let Some(extra) = &self.extra {
            spans.extend(extra.detect(seg));
        }
        let spans = merge(spans);
        if spans.is_empty() {
            return seg.to_string();
        }
        let mut out = String::with_capacity(seg.len());
        let mut cursor = 0usize;
        for s in &spans {
            if s.start < cursor {
                continue;
            }
            out.push_str(&seg[cursor..s.start]);
            let real = &seg[s.start..s.end];
            if self.store.is_allowed(real) {
                out.push_str(real);
            } else {
                let ph = self.vault.placeholder_for(&s.label, real);
                out.push_str(&ph);
                if s.source != "dictionary" && !self.is_known(real) {
                    self.push_suggestion(real);
                }
            }
            cursor = s.end;
        }
        out.push_str(&seg[cursor..]);
        out
    }

    /// Redact a whole outbound payload using delta-scan.
    ///
    /// Invariant 1: a cache hit re-emits the **redacted** form, never the raw input.
    /// Invariant 2: every segment passes through here — there is no bypass path.
    pub fn process(&mut self, segments: &[String]) -> Vec<String> {
        segments
            .iter()
            .map(|seg| {
                let h = Self::hash(seg);
                match self.cache.get(&h).cloned() {
                    Some(redacted) => redacted,
                    None => {
                        let redacted = self.redact_segment(seg);
                        self.cache.insert(h, redacted.clone());
                        redacted
                    }
                }
            })
            .collect()
    }

    /// Invariant 4: fail-closed outbound assertion (cheap; no model).
    pub fn assert_clean(&self, outbound: &[String]) -> Result<(), String> {
        for seg in outbound {
            for (form, _) in self.store.hidden_forms() {
                if self.store.is_allowed(&form) {
                    continue;
                }
                if seg.to_lowercase().contains(&form.to_lowercase()) {
                    return Err(format!("residual protected term in outbound: {form}"));
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
