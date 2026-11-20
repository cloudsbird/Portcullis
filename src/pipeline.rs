use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::detect::{detect_windowed, merge_by, Detector, DictionaryDetector, RegexDetector, Span};
use crate::jsonview::{self, View};
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

/// A placeholder-shaped token (`<<EMAIL_1`) that is never closed with `>>`.
///
/// Only that shape counts. A bare `<<` is ordinary text: shell heredocs (`cat <<EOF`),
/// C++ streams and bit shifts all have one with no `>>` anywhere in the request.
fn has_unclosed_placeholder(seg: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"<<\s*[A-Z][A-Z0-9_]*_[0-9]+\s*(>>)?")
            .expect("placeholder shape regex is valid")
    });
    re.captures_iter(seg).any(|c| c.get(1).is_none())
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
    /// Layers of JSON escaping to decode from the raw slice to get the real value: 0
    /// when the span was found in the raw text, otherwise the depth of the
    /// [`View`] it was found in. The vault keys on the decoded value, so `José` in a
    /// message and its escaped form in a JSON tool result get the same placeholder.
    decode: u8,
}

impl CachedSpan {
    /// The real value this span covers in `seg`.
    fn value<'a>(&self, seg: &'a str) -> std::borrow::Cow<'a, str> {
        jsonview::unescape_n(&seg[self.start..self.end], self.decode)
    }
}

/// Why a scan refused to produce an outbound payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanError {
    /// The ML detector failed on a segment and the gateway is configured to fail
    /// closed (see [`Gateway::with_fail_closed`]).
    DetectorFailed,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::DetectorFailed => {
                f.write_str("detector failed; request blocked (fail-closed)")
            }
        }
    }
}

impl std::error::Error for ScanError {}

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
    /// Block a scan when the ML detector fails, instead of forwarding with dictionary
    /// and regex coverage only.
    fail_closed: bool,
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
            fail_closed: false,
        }
    }

    /// Choose what happens when the ML detector fails on a segment.
    ///
    /// Either way the failed scan is **not cached**, so the segment is scanned again
    /// on the next turn rather than losing ML coverage for the rest of the
    /// conversation. With `fail_closed`, [`Gateway::try_process_scoped`] also returns
    /// [`ScanError::DetectorFailed`] and nothing is sent.
    pub fn with_fail_closed(mut self, fail_closed: bool) -> Self {
        self.fail_closed = fail_closed;
        self
    }

    pub fn fail_closed(&self) -> bool {
        self.fail_closed
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

    /// Allow-list `term` in `scope`. A policy change, so the delta cache is cleared
    /// (invariant 3): cached spans would otherwise keep hiding it.
    pub fn allow(&mut self, term: &str, scope: &str) {
        self.store.allow(term, scope);
        self.cache.clear();
    }

    /// Remove `term` from the allow-list. Clears the delta cache: a segment cached
    /// while the term was allowed would otherwise keep sending it unredacted.
    pub fn unallow_in(&mut self, term: &str, scope: Option<&str>) -> bool {
        let changed = self.store.unallow_in(term, scope);
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
    ///
    /// When the segment holds JSON escapes (see [`crate::jsonview`]) every layer runs
    /// over the decoded view, and the dictionary over the raw text as well, so a taught
    /// term that itself contains a backslash is still caught. View spans are mapped back
    /// to raw ranges covering whole escape sequences.
    fn detect_spans(
        &mut self,
        scope: Option<&str>,
        dict: &DictionaryDetector,
        seg: &str,
    ) -> (Vec<CachedSpan>, bool) {
        let mut failed = false;
        let view = View::of(seg);
        let (text, depth) = match &view {
            Some(v) => (v.text.as_str(), v.depth),
            None => (seg, 0),
        };

        // (span in `text` coordinates, found in the view?)
        let mut found: Vec<(Span, bool)> = Vec::new();
        if view.is_some() {
            found.extend(dict.detect(seg).into_iter().map(|s| (s, false)));
        }
        found.extend(dict.detect(text).into_iter().map(|s| (s, true)));
        found.extend(self.regexes.detect(text).into_iter().map(|s| (s, true)));
        if let Some(extra) = &self.extra {
            // A third-party detector's offsets are not trusted to land on character
            // boundaries: slicing mid-character would panic, and a misaligned span
            // would redact the wrong bytes. Widen to the enclosing boundaries.
            //
            // The text goes to the detector in bounded windows, never whole: encoder
            // memory is quadratic in input length, so one long tool output would
            // otherwise take the process down.
            found.extend(
                detect_windowed(extra.as_ref(), text)
                    .into_iter()
                    .filter_map(|mut s| {
                        (s.start, s.end) = snap_to_boundaries(text, s.start, s.end)?;
                        Some((s, true))
                    }),
            );
            // Any failed window fails the segment. The metric counts segments, not
            // windows, so its meaning does not depend on how long the text was.
            if extra.take_failures() > 0 {
                self.metrics.detector_errors(1);
                failed = true;
            }
        }

        // Everything in raw coordinates, so raw and view spans merge correctly.
        let raw: Vec<(Span, u8)> = found
            .into_iter()
            .map(|(mut s, in_view)| match (&view, in_view) {
                (Some(v), true) => {
                    (s.start, s.end) = v.to_raw(s.start, s.end);
                    (s, depth)
                }
                _ => (s, 0),
            })
            .filter(|(s, _)| s.start < s.end)
            .collect();

        let mut kept = Vec::new();
        for (s, decode) in merge_by(raw, |(s, _)| s) {
            let span = CachedSpan {
                start: s.start,
                end: s.end,
                label: s.label,
                decode,
            };
            let real = span.value(seg);
            if self.store.is_allowed_for(scope, &real) {
                continue;
            }
            // Secrets are not candidates for teaching, and must not sit in a list an
            // admin endpoint returns.
            if s.source != "dictionary" && span.label != "SECRET" && !self.is_known(scope, &real) {
                self.push_suggestion(scope, &real);
            }
            kept.push(span);
        }
        (kept, failed)
    }

    /// Replace each span with its placeholder from `vault`.
    fn apply_spans(vault: &mut Vault, seg: &str, spans: &[CachedSpan]) -> String {
        let mut out = String::with_capacity(seg.len());
        let mut cursor = 0usize;
        for s in spans {
            out.push_str(&seg[cursor..s.start]);
            out.push_str(&vault.placeholder_for(&s.label, &s.value(seg)));
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
        self.scan(scope, vault, segments, false)
            .expect("a fail-open scan cannot fail")
    }

    /// [`Gateway::process_scoped`], honouring [`Gateway::with_fail_closed`]: if the ML
    /// detector fails on any segment and the gateway fails closed, nothing is
    /// returned. This is what the proxy uses.
    pub fn try_process_scoped(
        &mut self,
        scope: Option<&str>,
        vault: &mut Vault,
        segments: &[String],
    ) -> Result<Vec<String>, ScanError> {
        let fail_closed = self.fail_closed;
        self.scan(scope, vault, segments, fail_closed)
    }

    fn scan(
        &mut self,
        scope: Option<&str>,
        vault: &mut Vault,
        segments: &[String],
        fail_closed: bool,
    ) -> Result<Vec<String>, ScanError> {
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
                    let (spans, failed) = self.detect_spans(scope, &dict, seg);
                    if failed {
                        // Not cached: a cached partial scan would deny this segment
                        // ML coverage on every later turn.
                        if fail_closed {
                            self.metrics.cache(hits, misses);
                            return Err(ScanError::DetectorFailed);
                        }
                    } else {
                        self.cache.insert(h, spans.clone());
                    }
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
        Ok(out)
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
        match self.first_residual_term(scope, segments) {
            Some(_) => Err("residual protected term in outbound".into()),
            None => Ok(()),
        }
    }

    /// Index of the first segment that still contains a protected term, if any.
    /// Lets a caller say *where* a block happened without echoing the term.
    pub fn first_residual_term(&self, scope: Option<&str>, segments: &[String]) -> Option<usize> {
        self.residual(scope, segments, true)
    }

    /// The taught-term check over one already-serialized body, on the raw text only.
    ///
    /// The proxy checks every decoded string of the body (and its JSON view) one by one
    /// first; this second pass over the whole serialized body must not also build a
    /// decoded view of it, which would hold several copies of a large request in
    /// memory under the gateway lock.
    pub fn assert_clean_serialized(&self, scope: Option<&str>, body: &str) -> Result<(), String> {
        let seg = [body.to_string()];
        if self.residual(scope, &seg, false).is_some() {
            return Err("residual protected term in outbound".into());
        }
        if has_unclosed_placeholder(body) {
            return Err("malformed placeholder in outbound".into());
        }
        Ok(())
    }

    fn residual(&self, scope: Option<&str>, segments: &[String], decode: bool) -> Option<usize> {
        let dict = self.dictionary(scope);
        let found_in = |text: &str| {
            dict.detect(text).iter().any(|span| {
                !self
                    .store
                    .is_allowed_for(scope, &text[span.start..span.end])
            })
        };
        // The decoded view too: a term after a JSON `\n`, or written with `\u` escapes,
        // is invisible to a match on the raw text.
        segments.iter().position(|seg| {
            found_in(seg) || (decode && View::of(seg).is_some_and(|v| found_in(&v.text)))
        })
    }

    pub fn assert_clean_scoped(
        &self,
        scope: Option<&str>,
        outbound: &[String],
    ) -> Result<(), String> {
        self.assert_no_terms_scoped(scope, outbound)?;
        if outbound.iter().any(|seg| has_unclosed_placeholder(seg)) {
            return Err("malformed placeholder in outbound".into());
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

    /// Fails on every call, counting each failure like the ONNX detector does.
    #[derive(Default)]
    struct Failing {
        calls: std::sync::atomic::AtomicU64,
        pending: std::sync::atomic::AtomicU64,
    }
    impl Detector for Failing {
        fn detect(&self, _text: &str) -> Vec<crate::detect::Span> {
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.pending.fetch_add(1, Ordering::Relaxed);
            Vec::new()
        }
        fn take_failures(&self) -> u64 {
            self.pending.swap(0, std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// Shares the call counter with the test after the detector is boxed.
    struct Shared(Arc<Failing>);
    impl Detector for Shared {
        fn detect(&self, text: &str) -> Vec<crate::detect::Span> {
            self.0.detect(text)
        }
        fn take_failures(&self) -> u64 {
            self.0.take_failures()
        }
    }

    #[test]
    fn a_failed_scan_is_not_cached_so_the_next_turn_rescans() {
        let failing = Arc::new(Failing::default());
        let mut gw =
            Gateway::new(Store::default()).with_detector(Box::new(Shared(failing.clone())));
        let seg = vec!["mail a@b.co".to_string()];
        let first = gw.process(&seg);
        let second = gw.process(&seg);
        assert_eq!(first, second, "fail-open still redacts deterministically");
        assert!(first[0].contains("<<EMAIL_1>>"), "{first:?}");
        assert_eq!(
            failing.calls.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "a failed scan must be retried, not served from the cache"
        );
    }

    #[test]
    fn fail_open_forwards_and_fail_closed_blocks() {
        let seg = vec!["hello".to_string()];

        let mut open = Gateway::new(Store::default()).with_detector(Box::new(Failing::default()));
        let mut vault = Vault::new();
        assert!(open.try_process_scoped(None, &mut vault, &seg).is_ok());

        let mut closed = Gateway::new(Store::default())
            .with_detector(Box::new(Failing::default()))
            .with_fail_closed(true);
        let mut vault = Vault::new();
        assert_eq!(
            closed.try_process_scoped(None, &mut vault, &seg),
            Err(ScanError::DetectorFailed)
        );
    }

    /// Records the longest text it was asked to scan.
    struct Longest(Arc<std::sync::atomic::AtomicUsize>);
    impl Detector for Longest {
        fn detect(&self, text: &str) -> Vec<crate::detect::Span> {
            self.0
                .fetch_max(text.len(), std::sync::atomic::Ordering::Relaxed);
            Vec::new()
        }
    }

    #[test]
    fn a_long_segment_reaches_the_detector_in_bounded_windows() {
        let longest = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut gw =
            Gateway::new(Store::default()).with_detector(Box::new(Longest(longest.clone())));
        let seg = "lorem ipsum dolor sit amet ".repeat(8000); // ~216 KB
        gw.process(&[seg]);
        let n = longest.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            n > 0 && n <= crate::detect::ML_WINDOW_BYTES,
            "longest call {n} bytes"
        );
    }

    #[test]
    fn a_failing_long_segment_counts_one_detector_error() {
        let mut gw = Gateway::new(Store::default()).with_detector(Box::new(Failing::default()));
        gw.process(&["word ".repeat(10_000)]);
        assert!(
            gw.metrics
                .render()
                .contains("portcullis_detector_errors_total 1"),
            "{}",
            gw.metrics.render()
        );
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
