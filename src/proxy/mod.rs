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
}

impl ProxyState {
    pub fn new(
        gateway: Gateway,
        client: Client,
        upstream_url: String,
        upstream_key: String,
    ) -> Self {
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

        // Opt-in pass-through of client headers. Nothing is forwarded unless it
        // was explicitly listed, so an unrelated credential cannot ride along.
        for name in &state.forward_headers {
            if let Some(value) = client_headers.get(name.as_str()) {
                if let Ok(value) = value.to_str() {
                    request = request.header(name.as_str(), value);
                }
            }
        }

        if !streaming {
            request = request.timeout(state.request_timeout);
        }
        match request.send().await {
            Ok(response) => return Ok(response),
            Err(e) if attempt < MAX_CONNECT_ATTEMPTS && e.is_connect() => {
                tracing::warn!(error = %e, attempt, "upstream connect failed; retrying");
            }
            Err(e) => {
                let kind = if e.is_timeout() { "timeout" } else { "error" };
                return Err((StatusCode::BAD_GATEWAY, format!("upstream {kind}: {e}")));
            }
        }
    }
}

/// One log line per request. Bodies and headers are never logged, so no prompt
/// content, term or credential can end up in the logs.
async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(req).await;
    tracing::info!(
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

/// Liveness and readiness probe. Unauthenticated, and deliberately reports only
/// counts — never a term, a value or a key.
async fn healthz(State(state): State<ProxyState>) -> Json<Value> {
    let store_terms = {
        let gw = state.gateway.lock().await;
        gw.store.deny.len()
    };
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
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(anthropic_messages))
        .route("/teach", post(admin::teach_handler))
        .route("/unteach", post(admin::unteach_handler))
        .route("/terms", get(admin::terms_handler))
        .route("/suggestions", get(admin::suggestions_handler))
        .layer(middleware::from_fn(log_requests))
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
    let upstream_url = std::env::var("PORTCULLIS_UPSTREAM_URL")
        .map_err(|_| anyhow::anyhow!("PORTCULLIS_UPSTREAM_URL is not set"))?;
    // Accept a base URL or a full endpoint — see normalize_chat_url.
    let upstream_url = normalize_chat_url(&upstream_url);
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
    state.anthropic_upstream_url = anthropic_upstream_url;
    state.anthropic_upstream_key = anthropic_upstream_key;
    state.anthropic_version = anthropic_version;
    let addr: SocketAddr = bind.parse()?;
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
        Some(scope) => Ok(Some(scope.to_string())),
        None => Err((
            StatusCode::UNAUTHORIZED,
            "missing or invalid scope token".into(),
        )),
    }
}
