//! OpenAI-compatible HTTP proxy.
//!
//! Intercepts chat-completion requests, redacts every message through the
//! gateway, asserts the outbound payload is clean, forwards to an upstream
//! provider, and rehydrates the assistant reply before returning it.

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::stream::{Stream, TryStreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::{Gateway, Vault};

/// Attempts made when the *connection* fails. A connect error means the request
/// never reached the provider, so retrying cannot double-charge or double-generate.
const MAX_CONNECT_ATTEMPTS: u32 = 2;

/// Default request-body cap (2 MiB). Detection cost grows with input length, so
/// an unbounded body is an easy way to stall the gateway.
const DEFAULT_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Shared state for the proxy handler.
#[derive(Clone)]
pub struct ProxyState {
    pub gateway: Arc<Mutex<Gateway>>,
    pub client: Client,
    pub upstream_url: String,
    pub upstream_key: String,
    /// Persist path for the learned store. If `None`, endpoints fall back to
    /// `PORTCULLIS_STORE` and then to `store.json`.
    pub store_path: Option<String>,
    /// Admin bearer token. If `None`, endpoints fall back to
    /// `PORTCULLIS_ADMIN_TOKEN` and reject requests when it is unset.
    pub admin_token: Option<String>,
    /// Anthropic Messages API upstream URL.
    pub anthropic_upstream_url: String,
    /// Anthropic API key (sent as `x-api-key`).
    pub anthropic_upstream_key: String,
    /// Default `anthropic-version` header when the client does not supply one.
    pub anthropic_version: String,
    /// Total timeout for non-streaming upstream requests. Streaming requests are
    /// exempt — a long generation is not a hang — and rely on the HTTP client's
    /// per-read timeout instead.
    pub request_timeout: Duration,
    /// Process start time, for `/healthz`.
    pub started_at: Instant,
    /// Client header names to pass through to the upstream, lowercased. Opt-in:
    /// nothing is forwarded unless listed here.
    pub forward_headers: Vec<String>,
    /// Static headers added to every upstream request, as `(name, value)`.
    pub extra_upstream_headers: Vec<(String, String)>,
    /// `(scope, token)` pairs. Empty means single-tenant mode: every taught term
    /// applies to every request. Non-empty means every request must present one of
    /// these tokens and only sees `global` terms plus its own scope's.
    pub scope_tokens: Vec<(String, String)>,
    /// Counters behind `/metrics`. Shared with the gateway, so a scrape never waits
    /// on the gateway lock.
    pub metrics: Arc<crate::Metrics>,
    /// Bearer token required by `/metrics`. `None`: open, like `/healthz`.
    pub metrics_token: Option<String>,
    /// Serializes teach/unteach so their read-modify-write of the store file cannot
    /// interleave. Held across disk I/O, which is why it is not the gateway lock.
    pub store_write_lock: Arc<Mutex<()>>,
    /// Transparent mode (`PORTCULLIS_PASSTHROUGH`): a request may name its own
    /// upstream in [`UPSTREAM_HEADER`] and is forwarded there with the client's own
    /// credentials and headers. See [`passthrough_target`].
    pub passthrough: bool,
    /// Optional host allowlist for passthrough targets (`PORTCULLIS_PASSTHROUGH_HOSTS`),
    /// lowercased. Empty: any host. An entry matches the host or any subdomain of it.
    pub passthrough_hosts: Vec<String>,
}

impl ProxyState {
    pub fn new(
        gateway: Gateway,
        client: Client,
        upstream_url: String,
        upstream_key: String,
    ) -> Self {
        let metrics = gateway.metrics();
        Self {
            gateway: Arc::new(Mutex::new(gateway)),
            client,
            upstream_url,
            upstream_key,
            store_path: None,
            admin_token: None,
            anthropic_upstream_url: "https://api.anthropic.com/v1/messages".into(),
            anthropic_upstream_key: String::new(),
            anthropic_version: "2023-06-01".into(),
            request_timeout: Duration::from_secs(600),
            started_at: Instant::now(),
            forward_headers: Vec::new(),
            extra_upstream_headers: Vec::new(),
            scope_tokens: Vec::new(),
            metrics,
            metrics_token: None,
            store_write_lock: Arc::new(Mutex::new(())),
            passthrough: false,
            passthrough_hosts: Vec::new(),
        }
    }
}

/// Accept either a full endpoint (`.../chat/completions`) or the base URL that
/// provider docs usually show (`https://openrouter.ai/api/v1`), and resolve it to
/// a full endpoint.
///
/// Only appended when the path clearly does not already name one, so an Azure
/// deployment URL or an existing query string is left alone.
fn normalize_chat_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.contains('?') || trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

/// The Anthropic equivalent of [`normalize_chat_url`].
fn normalize_messages_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.contains('?') || trimmed.ends_with("/messages") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/messages")
    }
}

/// Parse `PORTCULLIS_UPSTREAM_HEADERS`, a JSON object of header name → value.
/// Example: `'{"x-opencode-session":"portcullis"}'` (note the single quotes — a
/// shell `VAR={"k":"v"}` assignment strips the inner double quotes).
///
/// Malformed input is a **startup error**, not a warning: a typo here would
/// otherwise surface later as a confusing provider-side failure on every request.
fn parse_extra_headers(raw: &str) -> Result<Vec<(String, String)>, String> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    match serde_json::from_str::<serde_json::Map<String, Value>>(raw) {
        Ok(map) => Ok(map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect()),
        Err(e) => Err(format!(
            "PORTCULLIS_UPSTREAM_HEADERS must be a JSON object with string values, \
             e.g. '{{\"x-opencode-session\":\"abc\"}}' — got: {raw} ({e})"
        )),
    }
}

/// Header naming the real upstream endpoint in passthrough mode. Never forwarded.
pub const UPSTREAM_HEADER: &str = "x-portcullis-upstream";

/// Client headers never copied upstream in passthrough mode: hop-by-hop headers,
/// headers reqwest computes itself, and our own control header. `accept-encoding`
/// is dropped because the body is parsed here, so it must arrive uncompressed.
const PASSTHROUGH_SKIP_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "accept-encoding",
    UPSTREAM_HEADER,
];

/// Resolve the per-request upstream in passthrough mode.
///
/// * No [`UPSTREAM_HEADER`]: `Ok(None)` — use the configured upstream.
/// * Header present but passthrough disabled: rejected, rather than silently sending
///   the request to a different provider than the client asked for.
/// * Otherwise the URL must be http(s) and, if an allowlist is configured, on it.
pub(super) fn passthrough_target(
    state: &ProxyState,
    headers: &HeaderMap,
) -> Result<Option<String>, (StatusCode, String)> {
    let Some(raw) = headers.get(UPSTREAM_HEADER) else {
        return Ok(None);
    };
    if !state.passthrough {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{UPSTREAM_HEADER} was sent but PORTCULLIS_PASSTHROUGH is not enabled"),
        ));
    }
    let raw = raw.to_str().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("{UPSTREAM_HEADER} is not valid text"),
        )
    })?;
    let url = reqwest::Url::parse(raw.trim()).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("{UPSTREAM_HEADER} is not a URL: {e}"),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{UPSTREAM_HEADER} must be an http(s) URL"),
        ));
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() || !host_allowed(&host, &state.passthrough_hosts) {
        state.metrics.blocked("passthrough_host");
        return Err((
            StatusCode::FORBIDDEN,
            format!("upstream host '{host}' is not in PORTCULLIS_PASSTHROUGH_HOSTS"),
        ));
    }
    Ok(Some(url.to_string()))
}

/// `true` when `allow` is empty, or `host` equals or is a subdomain of an entry.
fn host_allowed(host: &str, allow: &[String]) -> bool {
    allow.is_empty()
        || allow
            .iter()
            .any(|a| host == a || host.ends_with(&format!(".{a}")))
}

/// `1`, `true`, `yes` or `on` (any case) → true; anything else, or unset → false.
fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Parse a positive integer number of seconds from the environment.
fn env_secs(key: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default),
    )
}

/// Forward to the upstream provider, applying the configured timeout and
/// retrying once only when the connection itself failed.
///
/// Deliberately **not** retried: HTTP 5xx responses and timeouts after the request
/// was sent. Against an LLM provider those can mean the request was already
/// processed, so a silent retry would double-charge and generate twice.
async fn send_upstream(
    state: &ProxyState,
    streaming: bool,
    client_headers: &HeaderMap,
    passthrough: bool,
    build: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, (StatusCode, String)> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut request = build();

        // Headers configured for this upstream (e.g. providers that require a
        // project or session header).
        for (name, value) in &state.extra_upstream_headers {
            request = request.header(name.as_str(), value.as_str());
        }

        if passthrough {
            // Transparent mode: the client is talking to its own provider with its
            // own credentials, so its headers go along as sent (minus hop-by-hop
            // and our control header).
            for (name, value) in client_headers {
                if !PASSTHROUGH_SKIP_HEADERS.contains(&name.as_str()) {
                    request = request.header(name, value);
                }
            }
        } else {
            // Opt-in pass-through of client headers. Nothing is forwarded unless it
            // was explicitly listed, so an unrelated credential cannot ride along.
            for name in &state.forward_headers {
                if let Some(value) = client_headers.get(name.as_str()) {
                    if let Ok(value) = value.to_str() {
                        request = request.header(name.as_str(), value);
                    }
                }
            }
        }

        if !streaming {
            request = request.timeout(state.request_timeout);
        }
        let sent = Instant::now();
        match request.send().await {
            Ok(response) => {
                state
                    .metrics
                    .upstream(response.status().as_str(), sent.elapsed().as_secs_f64());
                return Ok(response);
            }
            Err(e) if attempt < MAX_CONNECT_ATTEMPTS && e.is_connect() => {
                tracing::warn!(error = %e, attempt, "upstream connect failed; retrying");
            }
            Err(e) => {
                let kind = if e.is_timeout() { "timeout" } else { "error" };
                state.metrics.upstream(kind, sent.elapsed().as_secs_f64());
                return Err((StatusCode::BAD_GATEWAY, format!("upstream {kind}: {e}")));
            }
        }
    }
}

/// Which API a request came in on, so an error goes back in that API's own JSON shape
/// and the client's SDK shows its message instead of a parse failure.
#[derive(Clone, Copy)]
pub(super) enum Dialect {
    OpenAi,
    Anthropic,
}

/// Turn an internal `(status, message)` into a provider-shaped JSON error.
///
/// Status codes are kept as they were, so client retry behaviour does not change. The
/// message says plainly whether anything reached the provider and what to do next; the
/// original text is kept in it (in parentheses) for logs and searches. No message ever
/// contains a protected term or value: they only name fields.
pub(super) fn error_response(dialect: Dialect, status: StatusCode, raw: String) -> Response {
    let (code, message) = explain_error(status, &raw);
    let body = match dialect {
        Dialect::OpenAi => json!({
            "error": { "message": message, "type": "portcullis_error", "code": code }
        }),
        Dialect::Anthropic => json!({
            "type": "error",
            "error": { "type": anthropic_error_type(status), "message": message }
        }),
    };
    (status, Json(body)).into_response()
}

fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        _ => "api_error",
    }
}

/// `(code, human message)` for an internal error message.
fn explain_error(status: StatusCode, raw: &str) -> (&'static str, String) {
    const BLOCKED: &str = "Portcullis blocked this request, so nothing was sent to the provider";
    if let Some(rest) = raw.strip_prefix("residual protected term in outbound") {
        let field = rest.trim_start_matches(" at ").trim();
        let place = if field.is_empty() {
            String::new()
        } else {
            format!(" (in {field})")
        };
        let hint = if ["reasoning", "thinking", "signature"]
            .iter()
            .any(|k| field.contains(k))
        {
            "It is in signed reasoning from an earlier turn, which cannot be edited without the provider rejecting it. Start a new conversation."
        } else if field.ends_with(".name")
            || field.contains("tool_call_id")
            || field.contains(" key")
            || field.ends_with(".id")
        {
            "It is in a name or identifier the provider matches on, which cannot be replaced. Rename it, or unteach the term (or allow it) if it does not need hiding."
        } else {
            "Unteach the term (or allow it) if it does not need hiding; otherwise report this field as a Portcullis bug."
        };
        return (
            "residual_term",
            format!("{BLOCKED}: a term you taught is still present{place}. {hint} ({raw})"),
        );
    }
    if raw.starts_with("malformed placeholder") {
        return (
            "malformed_placeholder",
            format!("{BLOCKED}: it contains an unfinished placeholder such as `<<EMAIL_1` with no closing `>>`. Remove it from the conversation. ({raw})"),
        );
    }
    if raw.starts_with("detector failed") {
        return (
            "detector_failed",
            format!("{BLOCKED}: the ML detector failed and PORTCULLIS_DETECTOR_FAIL_CLOSED is on. Retry, or check the Portcullis log. ({raw})"),
        );
    }
    if raw.starts_with("upstream ") {
        return (
            "upstream_error",
            format!("Portcullis could not get a response from the provider ({raw})."),
        );
    }
    let code = match status.as_u16() {
        400 | 422 => "invalid_request",
        401 => "unauthorized",
        403 => "forbidden",
        _ => "portcullis_error",
    };
    (code, format!("Portcullis: {raw}"))
}

/// One log line per request. Bodies and headers are never logged, so no prompt
/// content, term or credential can end up in the logs.
async fn log_requests(State(state): State<ProxyState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(req).await;
    state.metrics.request(
        crate::metrics::route_label(&path),
        response.status().as_u16(),
        started.elapsed().as_secs_f64(),
    );
    tracing::info!(
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

/// Prometheus scrape endpoint. Counts and timings only — never a term, a value or a
/// key. Open by default like `/healthz`; set `PORTCULLIS_METRICS_TOKEN` to require a
/// bearer token (Prometheus: `authorization: { credentials: … }`).
async fn metrics_handler(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    if let Some(token) = state.metrics_token.as_deref() {
        let provided = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        if !admin::constant_time_eq(provided, token) {
            return (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response();
        }
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

/// Liveness and readiness probe. Unauthenticated, and deliberately reports only
/// counts — never a term, a value or a key.
async fn healthz(State(state): State<ProxyState>) -> Json<Value> {
    // Lock-free on purpose: scans serialize and can take seconds, and a liveness probe
    // that queues behind one gets a healthy process restarted.
    let store_terms = state.metrics.store_terms();
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "store_terms": store_terms,
    }))
}

mod admin;
mod anthropic;
mod openai;
mod stream;

pub use anthropic::anthropic_messages;
pub use openai::chat_completions;

pub fn app(state: ProxyState) -> Router {
    let max_body = std::env::var("PORTCULLIS_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_BODY_BYTES);

    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_handler))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/teach", post(admin::teach_handler))
        .route("/unteach", post(admin::unteach_handler))
        .route("/terms", get(admin::terms_handler))
        .route("/suggestions", get(admin::suggestions_handler))
        .layer(middleware::from_fn_with_state(state.clone(), log_requests))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state)
}

/// Run the proxy server bound to `bind`.
///
/// Reads `PORTCULLIS_UPSTREAM_URL` and `PORTCULLIS_UPSTREAM_KEY` from the
/// environment. Also reads `PORTCULLIS_ANTHROPIC_UPSTREAM_URL` (default
/// https://api.anthropic.com/v1/messages), `PORTCULLIS_ANTHROPIC_KEY`, and
/// `PORTCULLIS_ANTHROPIC_VERSION` (default 2023-06-01).
pub async fn serve(gateway: Gateway, bind: &str) -> anyhow::Result<()> {
    let gateway = gateway.with_fail_closed(env_flag("PORTCULLIS_DETECTOR_FAIL_CLOSED"));
    if gateway.fail_closed() {
        tracing::info!("detector fail-closed: a request is blocked if the ML detector fails");
    }
    let passthrough = env_flag("PORTCULLIS_PASSTHROUGH");
    let passthrough_hosts: Vec<String> = std::env::var("PORTCULLIS_PASSTHROUGH_HOSTS")
        .map(|raw| {
            raw.split(',')
                .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // In passthrough mode every request can name its own upstream, so a default is
    // optional; requests without the header then get a clear 400 instead.
    let upstream_url = match std::env::var("PORTCULLIS_UPSTREAM_URL") {
        Ok(u) if !u.trim().is_empty() => normalize_chat_url(&u),
        _ if passthrough => String::new(),
        _ => anyhow::bail!("PORTCULLIS_UPSTREAM_URL is not set"),
    };
    let upstream_key = std::env::var("PORTCULLIS_UPSTREAM_KEY").unwrap_or_default();
    let store_path = std::env::var("PORTCULLIS_STORE").unwrap_or_else(|_| "store.json".into());
    let admin_token = std::env::var("PORTCULLIS_ADMIN_TOKEN").unwrap_or_default();
    let admin_token = if admin_token.is_empty() {
        None
    } else {
        Some(admin_token)
    };

    let anthropic_upstream_url = std::env::var("PORTCULLIS_ANTHROPIC_UPSTREAM_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/v1/messages".into());
    let anthropic_upstream_url = normalize_messages_url(&anthropic_upstream_url);
    let anthropic_upstream_key = std::env::var("PORTCULLIS_ANTHROPIC_KEY").unwrap_or_default();
    let anthropic_version =
        std::env::var("PORTCULLIS_ANTHROPIC_VERSION").unwrap_or_else(|_| "2023-06-01".into());

    // Timeouts — a hung provider must not hang the client forever.
    //  * connect_timeout caps connection establishment.
    //  * read_timeout caps the gap *between* bytes, which is what catches a stalled
    //    stream while still allowing a genuinely long generation.
    //  * request_timeout caps a whole non-streaming call. It is applied per request
    //    in `send_upstream`, and never to streaming requests (it would kill them).
    let connect_timeout = env_secs("PORTCULLIS_CONNECT_TIMEOUT_SECS", 10);
    let read_timeout = env_secs("PORTCULLIS_READ_TIMEOUT_SECS", 180);
    let request_timeout = env_secs("PORTCULLIS_REQUEST_TIMEOUT_SECS", 600);

    // Opt-in client-header pass-through, e.g. `x-opencode-session`. Nothing is
    // forwarded unless named here.
    let forward_headers: Vec<String> = std::env::var("PORTCULLIS_FORWARD_HEADERS")
        .map(|raw| {
            raw.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    // Static upstream headers, for providers that require one on every call.
    // A malformed value fails startup rather than silently dropping the header.
    let extra_upstream_headers =
        parse_extra_headers(&std::env::var("PORTCULLIS_UPSTREAM_HEADERS").unwrap_or_default())
            .map_err(|e| anyhow::anyhow!(e))?;

    // Multi-tenant isolation (M5). Validated now, so a misconfiguration stops startup
    // instead of quietly weakening isolation.
    let scope_tokens = load_scope_tokens()?;
    if !scope_tokens.is_empty() {
        if let Some(admin) = admin_token.as_deref() {
            if scope_tokens.iter().any(|(_, t)| t == admin) {
                anyhow::bail!("a scope token must not equal PORTCULLIS_ADMIN_TOKEN");
            }
        }
        if let Some(bad) = forward_headers
            .iter()
            .find(|h| matches!(h.as_str(), "authorization" | "x-api-key"))
        {
            anyhow::bail!(
                "PORTCULLIS_FORWARD_HEADERS must not include {bad} when scopes are enabled: it carries the client's scope token"
            );
        }
    }

    let addr: SocketAddr = bind.parse()?;
    if passthrough {
        if !scope_tokens.is_empty() {
            anyhow::bail!(
                "PORTCULLIS_PASSTHROUGH cannot be combined with scope tokens: passthrough forwards the client's Authorization header, which carries the scope token"
            );
        }
        // Reachable from other machines + any upstream = an open proxy. Require an
        // explicit allowlist before allowing that combination.
        if !addr.ip().is_loopback() && passthrough_hosts.is_empty() {
            anyhow::bail!(
                "PORTCULLIS_PASSTHROUGH on a non-loopback bind ({addr}) requires PORTCULLIS_PASSTHROUGH_HOSTS"
            );
        }
    }

    let client = Client::builder()
        .use_rustls_tls()
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .pool_idle_timeout(Duration::from_secs(90))
        .build()?;

    let mut state = ProxyState::new(gateway, client, upstream_url, upstream_key);
    state.request_timeout = request_timeout;
    state.forward_headers = forward_headers;
    state.extra_upstream_headers = extra_upstream_headers;
    state.store_path = Some(store_path);
    state.admin_token = admin_token;
    state.scope_tokens = scope_tokens;
    state.metrics_token = std::env::var("PORTCULLIS_METRICS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    state.anthropic_upstream_url = anthropic_upstream_url;
    state.anthropic_upstream_key = anthropic_upstream_key;
    state.anthropic_version = anthropic_version;
    state.passthrough = passthrough;
    state.passthrough_hosts = passthrough_hosts;
    let listener = TcpListener::bind(addr).await?;

    // Say plainly where traffic is going. The single most common deployment
    // mistake is pointing the upstream somewhere unexpected.
    tracing::info!(
        listen = %addr,
        openai_upstream = %state.upstream_url,
        anthropic_upstream = %state.anthropic_upstream_url,
        store = %state.store_path.clone().unwrap_or_default(),
        forward_headers = ?state.forward_headers,
        extra_upstream_headers = state.extra_upstream_headers.len(),
        admin_api = state.admin_token.is_some(),
        store_encrypted = crate::Store::is_encrypted(state.store_path.as_deref().unwrap_or("store.json")),
        scopes = state.scope_tokens.len(),
        metrics_auth = state.metrics_token.is_some(),
        passthrough = state.passthrough,
        passthrough_hosts = ?state.passthrough_hosts,
        "portcullis listening"
    );

    axum::serve(listener, app(state)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scopes (M5): per-client isolation
// ---------------------------------------------------------------------------

/// Parse `scope=token,scope=token` into `(scope, token)` pairs.
///
/// Strict on purpose: a typo here silently weakens isolation, so anything odd fails
/// startup instead of being skipped.
pub fn parse_scope_tokens(raw: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (scope, token) = pair
            .split_once('=')
            .ok_or_else(|| format!("scope entry '{}' is not scope=token", redact_pair(pair)))?;
        let (scope, token) = (scope.trim(), token.trim());
        if scope.is_empty() || token.is_empty() {
            return Err("a scope and its token must both be non-empty".into());
        }
        if scope.eq_ignore_ascii_case(crate::GLOBAL_SCOPE) {
            return Err(format!(
                "'{}' is reserved for terms shared by every client",
                crate::GLOBAL_SCOPE
            ));
        }
        if out.iter().any(|(s, _)| s.eq_ignore_ascii_case(scope)) {
            return Err(format!("scope '{scope}' is listed twice"));
        }
        if out.iter().any(|(_, t)| t == token) {
            // Two scopes sharing a token could not be told apart.
            return Err(format!("scope '{scope}' reuses another scope's token"));
        }
        out.push((scope.to_string(), token.to_string()));
    }
    Ok(out)
}

/// Never echo a token (the part after `=`) into an error.
fn redact_pair(pair: &str) -> &str {
    pair.split_once('=').map_or("<malformed>", |(s, _)| s)
}

/// Load scope tokens from `PORTCULLIS_SCOPE_TOKENS` and, optionally, a JSON file
/// (`{"scope": "token"}`) named by `PORTCULLIS_SCOPE_TOKENS_FILE`.
fn load_scope_tokens() -> anyhow::Result<Vec<(String, String)>> {
    let mut raw = std::env::var("PORTCULLIS_SCOPE_TOKENS").unwrap_or_default();
    if let Ok(path) = std::env::var("PORTCULLIS_SCOPE_TOKENS_FILE") {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read PORTCULLIS_SCOPE_TOKENS_FILE: {e}"))?;
        let map: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&text).map_err(|e| {
                anyhow::anyhow!("PORTCULLIS_SCOPE_TOKENS_FILE is not a JSON object of strings: {e}")
            })?;
        for (scope, token) in map {
            if !raw.is_empty() {
                raw.push(',');
            }
            raw.push_str(&format!("{scope}={token}"));
        }
    }
    parse_scope_tokens(&raw).map_err(|e| anyhow::anyhow!("invalid scope tokens: {e}"))
}

/// Decide which scope a request belongs to.
///
/// * No scope tokens configured: single-tenant mode, `Ok(None)`, every term applies.
/// * Otherwise the request must present a configured token, as
///   `Authorization: Bearer <token>` (OpenAI-style clients) or `x-api-key: <token>`
///   (Anthropic-style). Anything else is rejected **before** a byte is processed or
///   forwarded. The token proves the scope; a client cannot name another's.
pub(super) fn resolve_scope(
    state: &ProxyState,
    headers: &HeaderMap,
) -> Result<Option<String>, (StatusCode, String)> {
    if state.scope_tokens.is_empty() {
        return Ok(None);
    }

    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());

    // Compare against every configured token with no early exit, so response time
    // does not reveal how close a guess was.
    let mut found: Option<&str> = None;
    for (scope, token) in &state.scope_tokens {
        let mut hit = false;
        for candidate in [bearer, api_key].into_iter().flatten() {
            hit |= admin::constant_time_eq(candidate, token);
        }
        if hit {
            found = Some(scope);
        }
    }
    match found {
        Some(scope) => {
            state.metrics.scope_request(scope);
            Ok(Some(scope.to_string()))
        }
        None => {
            state.metrics.blocked("unauthorized_scope");
            Err((
                StatusCode::UNAUTHORIZED,
                "missing or invalid scope token".into(),
            ))
        }
    }
}

/// Every decoded string in `body` — values **and** object keys.
///
/// The fail-closed assertion also runs over the serialized body, but JSON escapes some
/// characters (`"`, `\`, control characters, and optionally non-ASCII), so a taught
/// term containing one would not appear verbatim there. Checking the decoded strings
/// closes that gap.
///
/// Each string comes with its JSON path (`messages[2].reasoning_content`), for the
/// block message. A key is listed before anything under it, so the first match is
/// never reported through a path whose own keys hold a protected term.
pub(super) fn outbound_strings(body: &Value) -> (Vec<String>, Vec<String>) {
    fn walk(v: &Value, path: &str, strings: &mut Vec<String>, paths: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                strings.push(s.clone());
                paths.push(path.to_string());
            }
            Value::Array(a) => {
                for (i, x) in a.iter().enumerate() {
                    walk(x, &format!("{path}[{i}]"), strings, paths);
                }
            }
            Value::Object(m) => {
                for (k, x) in m {
                    strings.push(k.clone());
                    paths.push(match path {
                        "" => "a top-level key".to_string(),
                        p => format!("a key in {p}"),
                    });
                    let child = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    walk(x, &child, strings, paths);
                }
            }
            _ => {}
        }
    }
    let (mut strings, mut paths) = (Vec::new(), Vec::new());
    walk(body, "", &mut strings, &mut paths);
    (strings, paths)
}

/// Shortest string treated as a base64 blob when it is not a `data:` URL. Real image,
/// audio and file payloads are far larger; a short value under a `data` key is more
/// likely text, and text keeps being checked.
const MIN_BASE64_BLOB: usize = 1024;

fn looks_base64(s: &str) -> bool {
    s.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_' | b'\r' | b'\n')
    })
}

/// A `data:…;base64,…` URL whose payload really is base64.
fn is_base64_data_url(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("data:") else {
        return false;
    };
    match rest.split_once(";base64,") {
        Some((media_type, payload)) => !media_type.contains(',') && looks_base64(payload),
        None => false,
    }
}

/// A binary payload carried in a request: a base64 image, audio clip or file.
///
/// Portcullis forwards these unchanged; it never redacted pixels or audio. Checking them
/// for taught terms only finds noise: a megabyte of base64 contains almost any short
/// word, ignoring case, so one image would block every turn of a conversation. Text
/// inside an image is a documented limitation (docs/threat-model.md).
fn is_binary_payload(key: &str, value: &str, parent: &serde_json::Map<String, Value>) -> bool {
    if is_base64_data_url(value) {
        return true;
    }
    let blob = value.len() >= MIN_BASE64_BLOB && looks_base64(value);
    match key {
        // Anthropic `source: {type: "base64", data}`; OpenAI `input_audio: {data, format}`.
        "data" => {
            blob && (parent.get("type").and_then(Value::as_str) == Some("base64")
                || parent.contains_key("format"))
        }
        // OpenAI file parts: `file: {file_data, filename}`.
        "file_data" => blob,
        _ => false,
    }
}

/// `body` with every binary payload (see [`is_binary_payload`]) replaced by `""`. Only
/// for the outbound check; the request is forwarded as it was.
fn without_binary_payloads(body: &Value) -> Value {
    fn strip(v: &mut Value) {
        match v {
            Value::Object(map) => {
                let binary: Vec<String> = map
                    .iter()
                    .filter(|(k, x)| matches!(x, Value::String(s) if is_binary_payload(k, s, map)))
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in binary {
                    map.insert(k, Value::String(String::new()));
                }
                for (_, x) in map.iter_mut() {
                    strip(x);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            Value::String(s) if is_base64_data_url(s) => s.clear(),
            _ => {}
        }
    }
    let mut checked = body.clone();
    strip(&mut checked);
    checked
}

/// Every free-text `description` under a tool definition: the tool's own and its
/// parameters'. The provider reads them, so a taught term there is redacted like any
/// other text. Names, types and enums are protocol and stay as sent.
pub(super) fn collect_descriptions(v: &mut Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, x) in map.iter_mut() {
                match x {
                    Value::String(s) if k == "description" => out.push(std::mem::take(s)),
                    _ => collect_descriptions(x, out),
                }
            }
        }
        Value::Array(items) => {
            for x in items {
                collect_descriptions(x, out);
            }
        }
        _ => {}
    }
}

/// Mirror of [`collect_descriptions`]: same walk, same order.
pub(super) fn replace_descriptions(v: &mut Value, redacted: &[String], idx: &mut usize) {
    match v {
        Value::Object(map) => {
            for (k, x) in map.iter_mut() {
                match x {
                    Value::String(s) if k == "description" => {
                        *s = redacted[*idx].clone();
                        *idx += 1;
                    }
                    _ => replace_descriptions(x, redacted, idx),
                }
            }
        }
        Value::Array(items) => {
            for x in items {
                replace_descriptions(x, redacted, idx);
            }
        }
        _ => {}
    }
}

/// The fail-closed outbound check (invariant 4), shared by both providers' handlers.
/// Counts a block in the metrics by reason.
///
/// Binary payloads are left out of **both** passes (see [`is_binary_payload`]); they are
/// still forwarded unchanged.
pub(super) fn assert_outbound_clean(
    gw: &Gateway,
    metrics: &crate::Metrics,
    scope: Option<&str>,
    body: &Value,
) -> Result<(), (StatusCode, String)> {
    let body = &without_binary_payloads(body);
    let assembled =
        serde_json::to_string(body).map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    // Decoded strings first, so a block names the field it came from.
    let (strings, paths) = outbound_strings(body);
    let decoded = match gw.first_residual_term(scope, &strings) {
        Some(i) => Err(format!(
            "residual protected term in outbound at {}",
            paths[i]
        )),
        None => Ok(()),
    };
    decoded
        .and_then(|()| gw.assert_clean_scoped(scope, &[assembled]))
        .map_err(|e| {
            metrics.blocked(if e.contains("residual") {
                "residual_term"
            } else {
                "malformed_placeholder"
            });
            (StatusCode::BAD_GATEWAY, e)
        })
}
