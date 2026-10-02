//! Prometheus metrics (M7).
//!
//! Hand-rolled and dependency-free: a handful of counters and histograms behind one
//! mutex, rendered in the Prometheus text exposition format by `GET /metrics`.
//!
//! **Values never appear here.** Labels are drawn from small fixed sets — route,
//! status, redaction label, block reason, configured scope names — and every
//! open-ended set is capped, so neither a prompt nor a scanner probing random paths can
//! put text or unbounded cardinality into a metric.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Upper bounds, in seconds, of the latency buckets.
const BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
];

/// Most distinct values an open-ended label may take before the rest become `other`.
const LABEL_CAP: usize = 64;

#[derive(Default, Clone)]
struct Histogram {
    counts: [u64; BUCKETS.len()],
    sum: f64,
    count: u64,
}

impl Histogram {
    fn observe(&mut self, seconds: f64) {
        for (i, bound) in BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                self.counts[i] += 1;
            }
        }
        self.sum += seconds;
        self.count += 1;
    }

    /// Cumulative buckets, as Prometheus expects.
    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        for (i, bound) in BUCKETS.iter().enumerate() {
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels}{sep}le=\"{bound}\"}} {}",
                self.counts[i]
            );
        }
        let _ = writeln!(
            out,
            "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {}",
            self.count
        );
        let braces = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{labels}}}")
        };
        let _ = writeln!(out, "{name}_sum{braces} {}", self.sum);
        let _ = writeln!(out, "{name}_count{braces} {}", self.count);
    }
}

#[derive(Default)]
struct Inner {
    requests: BTreeMap<(String, u16), u64>,
    request_seconds: BTreeMap<String, Histogram>,
    scope_requests: BTreeMap<String, u64>,
    redactions: BTreeMap<String, u64>,
    blocked: BTreeMap<String, u64>,
    upstream_responses: BTreeMap<String, u64>,
    scan_seconds: Histogram,
    lock_wait_seconds: Histogram,
    upstream_seconds: Histogram,
}

/// Process-wide counters. Cheap to clone behind an `Arc`; never holds the gateway lock,
/// so a scrape is not queued behind a slow scan.
pub struct Metrics {
    started: Instant,
    inner: Mutex<Inner>,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    store_terms: AtomicU64,
    detector_errors: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Count into an open-ended label set, folding overflow into `other`.
fn bump(map: &mut BTreeMap<String, u64>, key: &str) {
    let key = if map.contains_key(key) || map.len() < LABEL_CAP {
        key
    } else {
        "other"
    };
    *map.entry(key.to_string()).or_insert(0) += 1;
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            inner: Mutex::new(Inner::default()),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            store_terms: AtomicU64::new(0),
            detector_errors: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding this lock must not take metrics (and every later
        // request) down with it.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// One finished HTTP request. `route` must come from [`route_label`].
    pub fn request(&self, route: &str, status: u16, seconds: f64) {
        let mut g = self.lock();
        *g.requests.entry((route.to_string(), status)).or_insert(0) += 1;
        g.request_seconds
            .entry(route.to_string())
            .or_default()
            .observe(seconds);
    }

    /// A request attributed to a configured scope (only when scopes are enabled).
    pub fn scope_request(&self, scope: &str) {
        bump(&mut self.lock().scope_requests, scope);
    }

    pub fn cache(&self, hits: u64, misses: u64) {
        self.cache_hits.fetch_add(hits, Ordering::Relaxed);
        self.cache_misses.fetch_add(misses, Ordering::Relaxed);
    }

    /// A span was replaced by a placeholder. `label` is the redaction label, never text.
    pub fn redaction(&self, label: &str) {
        bump(&mut self.lock().redactions, &label.to_ascii_uppercase());
    }

    /// A request stopped before leaving: `residual_term`, `malformed_placeholder`,
    /// `unauthorized_scope`, `unauthorized_admin`.
    pub fn blocked(&self, reason: &str) {
        bump(&mut self.lock().blocked, reason);
    }

    /// An upstream outcome: an HTTP status, or `timeout` / `error`.
    pub fn upstream(&self, outcome: &str, seconds: f64) {
        let mut g = self.lock();
        bump(&mut g.upstream_responses, outcome);
        g.upstream_seconds.observe(seconds);
    }

    /// Time spent scanning, and waiting for the gateway lock beforehand.
    pub fn scan(&self, lock_wait_seconds: f64, scan_seconds: f64) {
        let mut g = self.lock();
        g.lock_wait_seconds.observe(lock_wait_seconds);
        g.scan_seconds.observe(scan_seconds);
    }

    /// The ML detector failed on `n` segments (they kept dictionary and regex coverage).
    pub fn detector_errors(&self, n: u64) {
        self.detector_errors.fetch_add(n, Ordering::Relaxed);
    }

    /// Taught terms, for `/healthz` (read lock-free, so a probe never queues behind a scan).
    pub fn store_terms(&self) -> u64 {
        self.store_terms.load(Ordering::Relaxed)
    }

    pub fn set_store_terms(&self, n: usize) {
        self.store_terms.store(n as u64, Ordering::Relaxed);
    }

    /// Render the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let g = self.lock();
        let mut o = String::new();

        let _ = writeln!(o, "# HELP portcullis_build_info Build information.");
        let _ = writeln!(o, "# TYPE portcullis_build_info gauge");
        let _ = writeln!(
            o,
            "portcullis_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );

        let _ = writeln!(o, "# HELP portcullis_uptime_seconds Seconds since start.");
        let _ = writeln!(o, "# TYPE portcullis_uptime_seconds gauge");
        let _ = writeln!(
            o,
            "portcullis_uptime_seconds {}",
            self.started.elapsed().as_secs_f64()
        );

        let _ = writeln!(
            o,
            "# HELP portcullis_store_terms Terms in the learned store."
        );
        let _ = writeln!(o, "# TYPE portcullis_store_terms gauge");
        let _ = writeln!(
            o,
            "portcullis_store_terms {}",
            self.store_terms.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            o,
            "# HELP portcullis_http_requests_total HTTP requests by route and status."
        );
        let _ = writeln!(o, "# TYPE portcullis_http_requests_total counter");
        for ((route, status), n) in &g.requests {
            let _ = writeln!(
                o,
                "portcullis_http_requests_total{{route=\"{}\",status=\"{status}\"}} {n}",
                escape(route)
            );
        }

        let _ = writeln!(
            o,
            "# HELP portcullis_http_request_duration_seconds Time to response headers; a stream's \
             body time is not included."
        );
        let _ = writeln!(
            o,
            "# TYPE portcullis_http_request_duration_seconds histogram"
        );
        for (route, h) in &g.request_seconds {
            h.render(
                &mut o,
                "portcullis_http_request_duration_seconds",
                &format!("route=\"{}\"", escape(route)),
            );
        }

        counter(
            &mut o,
            "portcullis_scope_requests_total",
            "Requests by configured scope.",
            "scope",
            &g.scope_requests,
        );
        counter(
            &mut o,
            "portcullis_redactions_total",
            "Values replaced by a placeholder, by label. Counts occurrences, never text.",
            "label",
            &g.redactions,
        );
        counter(
            &mut o,
            "portcullis_blocked_total",
            "Requests stopped before anything was forwarded, by reason.",
            "reason",
            &g.blocked,
        );
        counter(
            &mut o,
            "portcullis_upstream_responses_total",
            "Provider outcomes: an HTTP status, or timeout/error.",
            "outcome",
            &g.upstream_responses,
        );

        let _ = writeln!(
            o,
            "# HELP portcullis_delta_cache_total Segment lookups in the delta cache."
        );
        let _ = writeln!(o, "# TYPE portcullis_delta_cache_total counter");
        let _ = writeln!(
            o,
            "portcullis_delta_cache_total{{result=\"hit\"}} {}",
            self.cache_hits.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            o,
            "portcullis_delta_cache_total{{result=\"miss\"}} {}",
            self.cache_misses.load(Ordering::Relaxed)
        );

        let _ = writeln!(
            o,
            "# HELP portcullis_detector_errors_total Segments where the ML detector failed and only dictionary and regex coverage applied."
        );
        let _ = writeln!(o, "# TYPE portcullis_detector_errors_total counter");
        let _ = writeln!(
            o,
            "portcullis_detector_errors_total {}",
            self.detector_errors.load(Ordering::Relaxed)
        );

        for (name, help, h) in [
            (
                "portcullis_scan_duration_seconds",
                "Time spent redacting a request (detection).",
                &g.scan_seconds,
            ),
            (
                "portcullis_gateway_lock_wait_seconds",
                "Time a request waited for the single gateway lock. Rising values mean \
                 detection is the bottleneck.",
                &g.lock_wait_seconds,
            ),
            (
                "portcullis_upstream_duration_seconds",
                "Time to the provider's response headers.",
                &g.upstream_seconds,
            ),
        ] {
            let _ = writeln!(o, "# HELP {name} {help}");
            let _ = writeln!(o, "# TYPE {name} histogram");
            h.render(&mut o, name, "");
        }
        o
    }
}

fn counter(o: &mut String, name: &str, help: &str, label: &str, map: &BTreeMap<String, u64>) {
    let _ = writeln!(o, "# HELP {name} {help}");
    let _ = writeln!(o, "# TYPE {name} counter");
    for (value, n) in map {
        let _ = writeln!(o, "{name}{{{label}=\"{}\"}} {n}", escape(value));
    }
}

/// Map a request path onto a fixed route name. Anything unrecognised is `other`, so a
/// scanner probing random URLs cannot create new series.
pub fn route_label(path: &str) -> &'static str {
    match path {
        "/v1/chat/completions" => "openai",
        "/v1/messages" => "anthropic",
        "/teach" | "/unteach" | "/terms" | "/suggestions" => "admin",
        "/healthz" => "health",
        "/metrics" => "metrics",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_prometheus_text() {
        let m = Metrics::new();
        m.request("openai", 200, 0.03);
        m.request("openai", 502, 0.2);
        m.redaction("org");
        m.blocked("residual_term");
        m.cache(3, 1);
        m.scan(0.001, 0.4);
        m.set_store_terms(7);
        let text = m.render();

        assert!(text.contains("portcullis_http_requests_total{route=\"openai\",status=\"200\"} 1"));
        assert!(text.contains("portcullis_redactions_total{label=\"ORG\"} 1"));
        assert!(text.contains("portcullis_blocked_total{reason=\"residual_term\"} 1"));
        assert!(text.contains("portcullis_delta_cache_total{result=\"hit\"} 3"));
        assert!(text.contains("portcullis_store_terms 7"));
        // Histogram buckets are cumulative and end in +Inf == count.
        assert!(text.contains(
            "portcullis_http_request_duration_seconds_bucket{route=\"openai\",le=\"0.05\"} 1"
        ));
        assert!(text.contains(
            "portcullis_http_request_duration_seconds_bucket{route=\"openai\",le=\"+Inf\"} 2"
        ));
        assert!(text.contains("portcullis_scan_duration_seconds_count 1"));
    }

    #[test]
    fn open_ended_labels_are_capped() {
        let m = Metrics::new();
        for i in 0..(LABEL_CAP * 3) {
            m.redaction(&format!("label{i}"));
        }
        let text = m.render();
        let series = text
            .lines()
            .filter(|l| l.starts_with("portcullis_redactions_total{"))
            .count();
        assert!(series <= LABEL_CAP + 1, "{series} series");
        assert!(text.contains("label=\"OTHER\"") || text.contains("label=\"other\""));
    }

    #[test]
    fn label_values_are_escaped_and_paths_are_folded() {
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(route_label("/v1/chat/completions"), "openai");
        assert_eq!(route_label("/wp-login.php"), "other");
    }
}
