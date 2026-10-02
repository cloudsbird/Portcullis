use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use crate::detect::{merge, Detector, DictionaryDetector, RegexDetector};
use crate::metrics::Metrics;
use crate::store::Store;
use crate::vault::Vault;

/// `scope -> (deny-list fingerprint, compiled dictionary)`.
type DictCache = HashMap<Option<String>, (u64, Arc<DictionaryDetector>)>;

/// Distinct scopes whose compiled dictionaries are kept (the scope set is configured, so
/// this is only a backstop).
const DICT_CACHE_LIMIT: usize = 64;

/// A hash of everything in the deny-list that affects matching.
fn deny_fingerprint(store: &Store) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for e in &store.deny {
        (&e.term, &e.label, &e.scope, &e.aliases, e.whole_word).hash(&mut h);
    }
    h.finish()
}

/// Widen `start..end` outward to character boundaries and clamp to the segment.
/// `None` only for an empty or out-of-range span. Widening, never dropping, is the
/// safe direction: it can only hide more.
fn snap_to_boundaries(seg: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let mut start = start.min(seg.len());
    let mut end = end.min(seg.len());
    while !seg.is_char_boundary(start) {
        start -= 1;
    }
    while !seg.is_char_boundary(end) {
        end += 1;
    }
    (start < end).then_some((start, end))
}

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
    suggestions: VecDeque<(Option<String>, String)>,
    metrics: Arc<Metrics>,
    /// Compiled dictionaries by scope, each tagged with the fingerprint of the deny-list
    /// it was built from.
    dicts: Mutex<DictCache>,
}

impl Gateway {
    pub fn new(store: Store) -> Self {
        let metrics = Arc::new(Metrics::new());
        metrics.set_store_terms(store.deny.len());
        Self {
            store,
            vault: Vault::new(),
            cache: HashMap::new(),
            regexes: RegexDetector::new(),
            extra: None,
            suggestions: VecDeque::new(),
            metrics,
            dicts: Mutex::new(HashMap::new()),
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

    /// The counters this gateway feeds. Shared, so a scrape never waits on the
    /// gateway lock.
    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    /// The compiled dictionary for `scope`, reused across requests.
    ///
    /// Compiling it is the expensive part of a dictionary scan (one regex over every
    /// term), so it is cached. The cache is validated against a fingerprint of the
    /// deny-list on every call, not invalidated by hooks: `store` is a public field and
    /// can be edited directly, and a stale dictionary would silently miss a new term.
    fn dictionary(&self, scope: Option<&str>) -> Arc<DictionaryDetector> {
        let fingerprint = deny_fingerprint(&self.store);
        let key = scope.map(str::to_string);
        let mut cache = self.dicts.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((fp, dict)) = cache.get(&key) {
            if *fp == fingerprint {
                return dict.clone();
            }
        }
        if cache.len() >= DICT_CACHE_LIMIT {
            cache.clear();
        }
        let dict = Arc::new(DictionaryDetector::for_scope(&self.store, scope));
        cache.insert(key, (fingerprint, dict.clone()));
        dict
    }

    /// Replace the whole store (e.g. with a fresh read from disk). A policy change, so
    /// the delta cache is cleared (invariant 3).
    pub fn replace_store(&mut self, store: Store) {
        self.store = store;
        self.cache.clear();
        self.metrics.set_store_terms(self.store.deny.len());
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Return the recent auto-suggest candidates, across every scope.
    pub fn suggestions(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.suggestions
            .iter()
            .filter(|(_, t)| seen.insert(t.as_str()))
            .map(|(_, t)| t.clone())
            .collect()
    }

    /// Candidates seen in traffic of one scope only (`None`: single-tenant traffic).
    pub fn suggestions_for(&self, scope: Option<&str>) -> Vec<String> {
        self.suggestions
            .iter()
            .filter(|(s, _)| s.as_deref() == scope)
            .map(|(_, t)| t.clone())
            .collect()
    }

    fn is_known(&self, scope: Option<&str>, term: &str) -> bool {
        if self.store.is_allowed_for(scope, term) {
            return true;
        }
        let lower = term.to_lowercase();
        self.store
            .hidden_entries_for(scope)
            .iter()
            .any(|h| h.form.to_lowercase() == lower)
    }

    fn push_suggestion(&mut self, scope: Option<&str>, term: &str) {
        let entry = (scope.map(str::to_string), term.to_string());
        // Deduplicate: move an existing entry to the back so repeats do not
        // flood the bounded buffer, and the most recent sightings are kept.
        if let Some(pos) = self.suggestions.iter().position(|e| *e == entry) {
            self.suggestions.remove(pos);
        }
        if self.suggestions.len() >= SUGGESTION_LIMIT {
            self.suggestions.pop_front();
        }
        self.suggestions.push_back(entry);
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
        self.metrics.set_store_terms(self.store.deny.len());
    }

    pub fn unteach(&mut self, term: &str) -> bool {
        self.unteach_in(term, None)
    }

    /// Remove a term from one scope, or (`None`) from every scope.
    pub fn unteach_in(&mut self, term: &str, scope: Option<&str>) -> bool {
        let changed = self.store.unteach_in(term, scope);
        self.metrics.set_store_terms(self.store.deny.len());
        if changed {
            self.cache.clear();
        }
        changed
    }

    /// Cache key. The scope is part of it: the same text redacts differently for
    /// different scopes, so a hit must never cross one.
    fn hash(scope: Option<&str>, seg: &str) -> String {
        let mut h = Sha256::new();
        h.update(scope.unwrap_or("\u{0}*").as_bytes());
        h.update([0u8]);
        h.update(seg.as_bytes());
        hex::encode(h.finalize())
    }

    /// Find what to redact in one segment: deterministic dictionary first, then
    /// builtin patterns, then the optional ML detector. Returns offsets only —
    /// never the matched text — so the result is safe to cache and can be applied
    /// against any session's vault.
    fn detect_spans(
        &mut self,
        scope: Option<&str>,
        dict: &DictionaryDetector,
        seg: &str,
    ) -> Vec<CachedSpan> {
        let mut spans = dict.detect(seg);
        spans.extend(self.regexes.detect(seg));
        if let Some(extra) = &self.extra {
            // A third-party detector's offsets are not trusted to land on character
            // boundaries: slicing mid-character would panic, and a misaligned span
            // would redact the wrong bytes. Widen to the enclosing boundaries.
            spans.extend(extra.detect(seg).into_iter().filter_map(|mut s| {
                (s.start, s.end) = snap_to_boundaries(seg, s.start, s.end)?;
                Some(s)
            }));
            let failed = extra.take_failures();
            if failed > 0 {
                self.metrics.detector_errors(failed);
            }
        }
        let mut kept = Vec::new();
        for s in merge(spans) {
            let real = &seg[s.start..s.end];
            if self.store.is_allowed_for(scope, real) {
                continue;
            }
            if s.source != "dictionary" && !self.is_known(scope, real) {
                self.push_suggestion(scope, real);
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
        self.process_scoped(None, vault, segments)
    }

    /// [`Gateway::process_with`] for a request in `scope`: only terms taught for that
    /// scope or for `global` apply (see [`crate::applies`]). `None` is single-tenant
    /// mode, where every term applies.
    pub fn process_scoped(
        &mut self,
        scope: Option<&str>,
        vault: &mut Vault,
        segments: &[String],
    ) -> Vec<String> {
        if self.cache.len() >= CACHE_LIMIT {
            self.cache.clear();
        }
        // Compiled once per call, and only if some segment misses the cache.
        let mut dict: Option<Arc<DictionaryDetector>> = None;
        let (mut hits, mut misses) = (0u64, 0u64);
        let mut out = Vec::with_capacity(segments.len());
        for seg in segments {
            let h = Self::hash(scope, seg);
            let spans = match self.cache.get(&h) {
                Some(spans) => {
                    hits += 1;
                    spans.clone()
                }
                None => {
                    misses += 1;
                    let dict = dict.get_or_insert_with(|| self.dictionary(scope)).clone();
                    let spans = self.detect_spans(scope, &dict, seg);
                    self.cache.insert(h, spans.clone());
                    spans
                }
            };
            for s in &spans {
                self.metrics.redaction(&s.label);
            }
            out.push(Self::apply_spans(vault, seg, &spans));
        }
        self.metrics.cache(hits, misses);
        self.metrics.set_store_terms(self.store.deny.len());
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
        self.assert_clean_scoped(None, outbound)
    }

    /// [`Gateway::assert_clean`] for a request in `scope`.
    /// Only the taught-term half of [`Gateway::assert_clean_scoped`]. For checking the
    /// individual strings of a JSON body, where "<<" without ">>" is just prose.
    pub fn assert_no_terms_scoped(
        &self,
        scope: Option<&str>,
        segments: &[String],
    ) -> Result<(), String> {
        let dict = self.dictionary(scope);
        for seg in segments {
            for span in dict.detect(seg) {
                if !self.store.is_allowed_for(scope, &seg[span.start..span.end]) {
                    return Err("residual protected term in outbound".into());
                }
            }
        }
        Ok(())
    }

    pub fn assert_clean_scoped(
        &self,
        scope: Option<&str>,
        outbound: &[String],
    ) -> Result<(), String> {
        self.assert_no_terms_scoped(scope, outbound)?;
        for seg in outbound {
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

    struct Misaligned;
    impl Detector for Misaligned {
        // Byte 1 is inside the 2-byte "é"; byte 4 is inside the 3-byte "€".
        fn detect(&self, _text: &str) -> Vec<crate::detect::Span> {
            vec![crate::detect::Span {
                start: 1,
                end: 4,
                label: "PERSON".into(),
                source: "onnx",
                score: 0.9,
            }]
        }
    }

    #[test]
    fn a_misaligned_detector_span_is_widened_not_panicked_on_or_dropped() {
        let mut gw = Gateway::new(Store::default()).with_detector(Box::new(Misaligned));
        let out = gw.process(&["é€x".to_string()]);
        assert_eq!(out[0], "<<PERSON_1>>x", "{:?}", out[0]);
    }

    #[test]
    fn suggestions_are_deduplicated_and_bounded() {
        let mut gw = Gateway::new(Store::default());

        // Repeats must not flood the buffer.
        for _ in 0..10 {
            gw.push_suggestion(None, "repeat@example.com");
        }
        assert_eq!(gw.suggestions().len(), 1);

        // The buffer stays bounded at SUGGESTION_LIMIT.
        for i in 0..(SUGGESTION_LIMIT + 50) {
            gw.push_suggestion(None, &format!("user{i}@example.com"));
        }
        assert_eq!(gw.suggestions().len(), SUGGESTION_LIMIT);
    }
}
