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

/// Walk a message and collect every redactable text string into `out`,
/// replacing each collected string with an empty placeholder in `msg`.
/// All other fields are left untouched so they survive forwarding.
fn collect_message_texts(msg: &mut Value, out: &mut Vec<String>) {
    // `content` may be a plain string or an array of multimodal parts.
    if let Some(content) = msg.get_mut("content") {
        collect_content_texts(content, out);
    }

    // `tool_calls[*].function.arguments` is text-bearing JSON.
    if let Some(tool_calls) = msg.get_mut("tool_calls") {
        collect_tool_calls_texts(tool_calls, out);
    }

    // `name` and `tool_call_id` are identifiers. They are intentionally
    // preserved as-is rather than redacted.
    // TODO: consider whether identifier-like fields should be redacted.
}

fn collect_content_texts(content: &mut Value, out: &mut Vec<String>) {
    match content {
        Value::String(s) => {
            out.push(std::mem::take(s));
        }
        Value::Array(parts) => {
            for part in parts {
                if let Value::Object(map) = part {
                    for key in &["text", "input_text", "output_text"] {
                        if let Some(Value::String(s)) = map.get_mut(*key) {
                            out.push(std::mem::take(s));
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn collect_tool_calls_texts(tool_calls: &mut Value, out: &mut Vec<String>) {
    let Value::Array(calls) = tool_calls else {
        return;
    };
    for call in calls {
        let Value::Object(call_map) = call else {
            continue;
        };
        let Some(Value::Object(func_map)) = call_map.get_mut("function") else {
            continue;
        };
        if let Some(Value::String(args)) = func_map.get_mut("arguments") {
            out.push(std::mem::take(args));
        }
    }
}

/// Place redacted strings back into `msg`, following the same walk order as
/// `collect_message_texts`.
fn replace_message_texts(msg: &mut Value, redacted: &[String], idx: &mut usize) {
    if let Some(content) = msg.get_mut("content") {
        replace_content_texts(content, redacted, idx);
    }
    if let Some(tool_calls) = msg.get_mut("tool_calls") {
        replace_tool_calls_texts(tool_calls, redacted, idx);
    }
}

fn replace_content_texts(content: &mut Value, redacted: &[String], idx: &mut usize) {
    match content {
        Value::String(s) => {
            *s = redacted[*idx].clone();
            *idx += 1;
        }
        Value::Array(parts) => {
            for part in parts {
                if let Value::Object(map) = part {
                    for key in &["text", "input_text", "output_text"] {
                        if let Some(Value::String(_)) = map.get_mut(*key) {
                            map[*key] = Value::String(redacted[*idx].clone());
                            *idx += 1;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn replace_tool_calls_texts(tool_calls: &mut Value, redacted: &[String], idx: &mut usize) {
    let Value::Array(calls) = tool_calls else {
        return;
    };
    for call in calls {
        let Value::Object(call_map) = call else {
            continue;
        };
        let Some(Value::Object(func_map)) = call_map.get_mut("function") else {
            continue;
        };
        if let Some(Value::String(_)) = func_map.get_mut("arguments") {
            func_map["arguments"] = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages API redaction helpers
// ---------------------------------------------------------------------------

/// Collect every redactable string from an Anthropic request body.
fn collect_anthropic_texts(body: &mut Value, out: &mut Vec<String>) {
    if let Some(system) = body.get_mut("system") {
        collect_anthropic_system(system, out);
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            collect_anthropic_message(msg, out);
        }
    }
}

fn collect_anthropic_system(system: &mut Value, out: &mut Vec<String>) {
    match system {
        Value::String(s) => {
            out.push(std::mem::take(s));
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                let Value::Object(map) = block else { continue };
                if map.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(Value::String(s)) = map.get_mut("text") {
                        out.push(std::mem::take(s));
                    }
                }
            }
        }
        _ => {}
    }
}

fn collect_anthropic_message(msg: &mut Value, out: &mut Vec<String>) {
    let Value::Object(map) = msg else { return };
    if let Some(content) = map.get_mut("content") {
        collect_anthropic_content(content, out);
    }
}

fn collect_anthropic_content(content: &mut Value, out: &mut Vec<String>) {
    match content {
        Value::String(s) => {
            out.push(std::mem::take(s));
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                let Value::Object(map) = block else { continue };
                match map.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(Value::String(s)) = map.get_mut("text") {
                            out.push(std::mem::take(s));
                        }
                    }
                    Some("tool_result") => {
                        if let Some(content) = map.get_mut("content") {
                            collect_anthropic_content(content, out);
                        }
                    }
                    Some("tool_use") => {
                        if let Some(input) = map.get_mut("input") {
                            collect_string_leaves(input, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn collect_string_leaves(value: &mut Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            out.push(std::mem::take(s));
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                collect_string_leaves(item, out);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                collect_string_leaves(v, out);
            }
        }
        _ => {}
    }
}

/// Place redacted strings back into an Anthropic request body.
fn replace_anthropic_texts(body: &mut Value, redacted: &[String], idx: &mut usize) {
    if let Some(system) = body.get_mut("system") {
        replace_anthropic_system(system, redacted, idx);
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            replace_anthropic_message(msg, redacted, idx);
        }
    }
}

fn replace_anthropic_system(system: &mut Value, redacted: &[String], idx: &mut usize) {
    match system {
        Value::String(_) => {
            *system = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                let Value::Object(map) = block else { continue };
                if map.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(Value::String(_)) = map.get_mut("text") {
                        map["text"] = Value::String(redacted[*idx].clone());
                        *idx += 1;
                    }
                }
            }
        }
        _ => {}
    }
}

fn replace_anthropic_message(msg: &mut Value, redacted: &[String], idx: &mut usize) {
    let Value::Object(map) = msg else { return };
    if let Some(content) = map.get_mut("content") {
        replace_anthropic_content(content, redacted, idx);
    }
}

fn replace_anthropic_content(content: &mut Value, redacted: &[String], idx: &mut usize) {
    match content {
        Value::String(_) => {
            *content = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                let Value::Object(map) = block else { continue };
                match map.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(Value::String(_)) = map.get_mut("text") {
                            map["text"] = Value::String(redacted[*idx].clone());
                            *idx += 1;
                        }
                    }
                    Some("tool_result") => {
                        if let Some(content) = map.get_mut("content") {
                            replace_anthropic_content(content, redacted, idx);
                        }
                    }
                    Some("tool_use") => {
                        if let Some(input) = map.get_mut("input") {
                            replace_string_leaves(input, redacted, idx);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn replace_string_leaves(value: &mut Value, redacted: &[String], idx: &mut usize) {
    match value {
        Value::String(_) => {
            *value = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                replace_string_leaves(item, redacted, idx);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                replace_string_leaves(v, redacted, idx);
            }
        }
        _ => {}
    }
}

pub async fn chat_completions(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match handle(state, headers, body).await {
        Ok(resp) => resp,
        Err((status, msg)) => (status, msg).into_response(),
    }
}

async fn handle(
    state: ProxyState,
    headers: HeaderMap,
    body: Value,
) -> Result<Response, (StatusCode, String)> {
    let messages = body.get("messages").and_then(|m| m.as_array()).ok_or((
        StatusCode::BAD_REQUEST,
        "missing or invalid messages".to_string(),
    ))?;

    // Collect every redactable string from the original messages while
    // preserving all other fields.
    let mut redacted_messages: Vec<Value> = Vec::with_capacity(messages.len());
    let mut texts_to_redact: Vec<String> = Vec::new();
    for msg in messages.iter() {
        let mut msg = msg.clone();
        collect_message_texts(&mut msg, &mut texts_to_redact);
        redacted_messages.push(msg);
    }

    // Redact all text-bearing strings in one batch (better cache hit rate).
    // Detection is CPU-bound and synchronous, so it runs on the blocking pool
    // rather than stalling an async worker thread.
    let (redacted_texts, vault) = {
        let gateway = state.gateway.clone();
        let texts = texts_to_redact.clone();
        tokio::task::spawn_blocking(move || {
            let mut gw = gateway.blocking_lock();
            let mut vault = Vault::new();
            let out = gw.process_with(&mut vault, &texts);
            (out, vault)
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("redaction task failed: {e}"),
            )
        })?
    };

    // Place redacted strings back into the message copies.
    {
        let mut idx = 0;
        for msg in &mut redacted_messages {
            replace_message_texts(msg, &redacted_texts, &mut idx);
        }
    }

    let mut upstream_body = body.clone();
    upstream_body["messages"] = json!(redacted_messages);

    // Invariant 4: fail-closed outbound assertion — run over the ENTIRE assembled
    // payload, not only the strings we knew to redact. If the walker failed to
    // collect a field that carries a protected term, this is what stops it leaving.
    {
        let gw = state.gateway.lock().await;
        let assembled = serde_json::to_string(&upstream_body)
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        gw.assert_clean(&[assembled])
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    }

    let streaming = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let upstream_resp = send_upstream(&state, streaming, &headers, || {
        state
            .client
            .post(&state.upstream_url)
            .bearer_auth(&state.upstream_key)
            .json(&upstream_body)
    })
    .await?;

    if streaming {
        return stream_response(Arc::new(vault), upstream_resp).await;
    }

    let status = upstream_resp.status();
    let mut upstream_json: Value = upstream_resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    // Rehydrate assistant text locally before returning to the caller. Covers
    // content, reasoning content, and tool-call arguments.
    {
        if let Some(choices) = upstream_json
            .get_mut("choices")
            .and_then(|c| c.as_array_mut())
        {
            for choice in choices.iter_mut() {
                if let Some(msg) = choice.get_mut("message") {
                    rehydrate_response_message(msg, &vault);
                }
            }
        }
    }

    Ok((status, Json(upstream_json)).into_response())
}

// ---------------------------------------------------------------------------
// Anthropic Messages API handler
// ---------------------------------------------------------------------------

pub async fn anthropic_messages(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match handle_anthropic(state, headers, body).await {
        Ok(resp) => resp,
        Err((status, msg)) => (status, msg).into_response(),
    }
}

async fn handle_anthropic(
    state: ProxyState,
    headers: HeaderMap,
    body: Value,
) -> Result<Response, (StatusCode, String)> {
    // Collect every redactable string while preserving all other fields.
    let mut redacted_body = body.clone();
    let mut texts_to_redact: Vec<String> = Vec::new();
    collect_anthropic_texts(&mut redacted_body, &mut texts_to_redact);

    // Redact all text-bearing strings in one batch (better cache hit rate).
    // Detection is CPU-bound and synchronous, so it runs on the blocking pool
    // rather than stalling an async worker thread.
    let (redacted_texts, vault) = {
        let gateway = state.gateway.clone();
        let texts = texts_to_redact.clone();
        tokio::task::spawn_blocking(move || {
            let mut gw = gateway.blocking_lock();
            let mut vault = Vault::new();
            let out = gw.process_with(&mut vault, &texts);
            (out, vault)
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("redaction task failed: {e}"),
            )
        })?
    };

    // Place redacted strings back into the request copy.
    {
        let mut idx = 0;
        replace_anthropic_texts(&mut redacted_body, &redacted_texts, &mut idx);
    }

    // Invariant 4: fail-closed outbound assertion over the ENTIRE assembled body.
    {
        let gw = state.gateway.lock().await;
        let assembled = serde_json::to_string(&redacted_body)
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        gw.assert_clean(&[assembled])
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    }

    let streaming = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Anthropic uses `x-api-key` and `anthropic-version`, not Bearer auth.
    let version = headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.anthropic_version.clone());

    let upstream_resp = send_upstream(&state, streaming, &headers, || {
        state
            .client
            .post(&state.anthropic_upstream_url)
            .header("x-api-key", &state.anthropic_upstream_key)
            .header("anthropic-version", &version)
            .header("content-type", "application/json")
            .json(&redacted_body)
    })
    .await?;

    if streaming {
        return anthropic_stream_response(Arc::new(vault), upstream_resp).await;
    }

    let status = upstream_resp.status();
    let mut upstream_json: Value = upstream_resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    rehydrate_anthropic_response(&mut upstream_json, &vault);

    Ok((status, Json(upstream_json)).into_response())
}

fn rehydrate_anthropic_response(body: &mut Value, vault: &Vault) {
    let Some(content) = body.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for block in content.iter_mut() {
        let Value::Object(map) = block else { continue };
        match map.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(Value::String(s)) = map.get_mut("text") {
                    *s = vault.restore(s);
                }
            }
            Some("tool_use") => {
                if let Some(input) = map.get_mut("input") {
                    rehydrate_string_leaves(input, vault);
                }
            }
            _ => {}
        }
    }
}

fn rehydrate_string_leaves(value: &mut Value, vault: &Vault) {
    match value {
        Value::String(s) => *s = vault.restore(s),
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                rehydrate_string_leaves(item, vault);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                rehydrate_string_leaves(v, vault);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Server-Sent Events streaming path
// ---------------------------------------------------------------------------

/// Forward a streaming upstream response to the caller, rehydrating every
/// `data:` line chunk by chunk while buffering incomplete SSE events so that
/// placeholders split across TCP chunks are still restored correctly.
async fn stream_response(
    vault: Arc<Vault>,
    upstream_resp: reqwest::Response,
) -> Result<Response, (StatusCode, String)> {
    let status = upstream_resp.status();
    let content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();

    let bytes_stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
        Box::pin(upstream_resp.bytes_stream());

    let stream = futures::stream::try_unfold(
        (String::new(), StreamCarry::default(), bytes_stream, vault),
        |(mut buf, mut carry, mut stream, vault)| async move {
            loop {
                // Emit a complete SSE event as soon as we have the blank-line
                // delimiter. `carry` additionally holds back a partial
                // placeholder that spans two events.
                if let Some(event_end) = buf.find("\n\n") {
                    let after = buf.split_off(event_end + 2);
                    let event = std::mem::replace(&mut buf, after);
                    let processed = process_sse_event(&event, &mut carry, &vault);
                    return Ok::<_, std::io::Error>(Some((
                        Bytes::from(processed),
                        (buf, carry, stream, vault),
                    )));
                }

                match stream.try_next().await {
                    Ok(Some(chunk)) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    Ok(None) => {
                        let mut tail = if buf.is_empty() {
                            String::new()
                        } else {
                            let processed = process_sse_event(&buf, &mut carry, &vault);
                            buf.clear();
                            processed
                        };
                        tail.push_str(&flush_carry(&mut carry, &vault));
                        if tail.is_empty() {
                            return Ok(None);
                        }
                        return Ok(Some((Bytes::from(tail), (buf, carry, stream, vault))));
                    }
                    Err(e) => {
                        return Err(std::io::Error::other(e));
                    }
                }
            }
        },
    );

    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(axum::http::header::CONTENT_TYPE, ct.as_bytes());
    }
    builder
        .body(Body::from_stream(stream))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

// ---------------------------------------------------------------------------
// Placeholder-level carry buffer for the SSE path
// ---------------------------------------------------------------------------

/// Upper bound on how much of a suspected partial placeholder we hold back.
/// Guarantees a malformed stream can never grow the buffer without bound.
const MAX_PENDING: usize = 256;

/// Per-field carry buffers, so a partial placeholder in `content` and one in
/// tool-call `arguments` cannot corrupt each other.
#[derive(Default)]
struct StreamCarry {
    content: String,
    args: String,
    /// Reasoning/thinking keeps its own buffer. Two text fields interleave in one
    /// stream, so stitching a split placeholder across them would corrupt both.
    reasoning: String,
}

/// Split `combined` into (safe to emit, holds back a partial placeholder).
///
/// A placeholder looks like `<<LABEL_N>>`. If the text ends with a `<<` that
/// has no closing `>>` after it, everything from that `<<` onward is withheld
/// until a later event completes it.
fn split_partial_placeholder(combined: &str) -> (&str, &str) {
    if let Some(open) = combined.rfind("<<") {
        if !combined[open + 2..].contains(">>") {
            return (&combined[..open], &combined[open..]);
        }
    }
    (combined, "")
}

/// Rehydrate `text`, prepending and updating the carry buffer so a placeholder
/// split across events is restored as a single unit.
fn rehydrate_with_carry(text: &str, pending: &mut String, vault: &Vault) -> String {
    let combined = format!("{pending}{text}");
    let (emit, held) = split_partial_placeholder(&combined);
    let (emit, held) = if held.len() > MAX_PENDING {
        // Give up holding rather than buffer unboundedly.
        (combined.as_str(), "")
    } else {
        (emit, held)
    };
    let emit_owned = emit.to_string();
    *pending = held.to_string();
    vault.restore(&emit_owned)
}

/// Emit any held-back text as a final synthetic delta event.
fn flush_carry(carry: &mut StreamCarry, vault: &Vault) -> String {
    if carry.content.is_empty() && carry.args.is_empty() && carry.reasoning.is_empty() {
        return String::new();
    }
    let mut deltas = Vec::new();
    if !carry.content.is_empty() {
        let held = std::mem::take(&mut carry.content);
        deltas.push(json!({
            "index": 0,
            "delta": { "content": vault.restore(&held) },
            "finish_reason": null
        }));
    }
    if !carry.reasoning.is_empty() {
        let held = std::mem::take(&mut carry.reasoning);
        deltas.push(json!({
            "index": 0,
            "delta": { "reasoning_content": vault.restore(&held) },
            "finish_reason": null
        }));
    }
    if !carry.args.is_empty() {
        let held = std::mem::take(&mut carry.args);
        deltas.push(json!({
            "index": 0,
            "delta": { "tool_calls": [ {
                "index": 0,
                "function": { "arguments": vault.restore(&held) }
            } ] },
            "finish_reason": null
        }));
    }
    let payload = json!({ "object": "chat.completion.chunk", "choices": deltas });
    format!("data: {payload}\n\n")
}

/// Rehydrate the text-bearing fields inside one SSE event.
///
/// `carry` holds back a partial placeholder that spans two events; it is
/// flushed before `[DONE]` and again at end of stream.
fn process_sse_event(event: &str, carry: &mut StreamCarry, vault: &Vault) -> String {
    let mut out = String::new();
    for line in event.lines() {
        if let Some(payload) = line.strip_prefix("data: ") {
            if payload == "[DONE]" {
                // Flush anything still held back before terminating.
                out.push_str(&flush_carry(carry, vault));
                out.push_str(line);
            } else {
                match serde_json::from_str::<Value>(payload) {
                    Ok(mut value) => {
                        rehydrate_sse_delta(&mut value, carry, vault);
                        out.push_str("data: ");
                        match serde_json::to_string(&value) {
                            Ok(serialized) => out.push_str(&serialized),
                            Err(_) => out.push_str(payload),
                        }
                    }
                    Err(_) => out.push_str(line),
                }
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    // Each SSE event is terminated by a blank line.
    out.push('\n');
    out
}

/// Apply `Gateway::rehydrate` to every text-bearing delta field in a streaming
/// chunk: `choices[].delta.content`, `choices[].delta.reasoning_content`, and
/// `choices[].delta.tool_calls[].function.arguments`.
fn rehydrate_sse_delta(value: &mut Value, carry: &mut StreamCarry, vault: &Vault) {
    let Some(choices) = value.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for choice in choices.iter_mut() {
        let Some(delta) = choice.get_mut("delta") else {
            continue;
        };
        if let Some(Value::String(content)) = delta.get_mut("content") {
            let rehydrated = rehydrate_with_carry(content, &mut carry.content, vault);
            *content = rehydrated;
        }
        // Reasoning models stream a second text field. Without this, a
        // placeholder the model *reasoned* about comes back to the client as a
        // literal `<<ORG_1>>`.
        if let Some(Value::String(reasoning)) = delta.get_mut("reasoning_content") {
            let rehydrated = rehydrate_with_carry(reasoning, &mut carry.reasoning, vault);
            *reasoning = rehydrated;
        }
        if let Some(tool_calls) = delta.get_mut("tool_calls") {
            rehydrate_tool_calls(tool_calls, &mut carry.args, vault);
        }
    }
}

/// Rehydrate the text-bearing fields of a complete (non-streaming) assistant
/// message: content, reasoning content, and tool-call arguments.
fn rehydrate_response_message(msg: &mut Value, vault: &Vault) {
    for field in ["content", "reasoning_content"] {
        if let Some(Value::String(text)) = msg.get_mut(field) {
            *text = vault.restore(text);
        }
    }
    if let Some(calls) = msg.get_mut("tool_calls").and_then(|c| c.as_array_mut()) {
        for call in calls.iter_mut() {
            if let Some(Value::String(args)) = call
                .get_mut("function")
                .and_then(|f| f.get_mut("arguments"))
            {
                *args = vault.restore(args);
            }
        }
    }
}

fn rehydrate_tool_calls(tool_calls: &mut Value, pending: &mut String, vault: &Vault) {
    let Some(calls) = tool_calls.as_array_mut() else {
        return;
    };
    for call in calls.iter_mut() {
        let Some(func) = call.get_mut("function") else {
            continue;
        };
        if let Some(Value::String(args)) = func.get_mut("arguments") {
            let rehydrated = rehydrate_with_carry(args, pending, vault);
            *args = rehydrated;
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages API streaming SSE path
// ---------------------------------------------------------------------------

/// Forward an Anthropic streaming upstream response, rehydrating text and
/// input_json deltas while reusing the placeholder-level carry buffer.
async fn anthropic_stream_response(
    vault: Arc<Vault>,
    upstream_resp: reqwest::Response,
) -> Result<Response, (StatusCode, String)> {
    let status = upstream_resp.status();
    let content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();

    let bytes_stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
        Box::pin(upstream_resp.bytes_stream());

    let stream = futures::stream::try_unfold(
        (String::new(), StreamCarry::default(), bytes_stream, vault),
        |(mut buf, mut carry, mut stream, vault)| async move {
            loop {
                if let Some(event_end) = buf.find("\n\n") {
                    let after = buf.split_off(event_end + 2);
                    let event = std::mem::replace(&mut buf, after);
                    let processed = process_anthropic_sse_event(&event, &mut carry, &vault);
                    return Ok::<_, std::io::Error>(Some((
                        Bytes::from(processed),
                        (buf, carry, stream, vault),
                    )));
                }

                match stream.try_next().await {
                    Ok(Some(chunk)) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    Ok(None) => {
                        let mut tail = if buf.is_empty() {
                            String::new()
                        } else {
                            let processed = process_anthropic_sse_event(&buf, &mut carry, &vault);
                            buf.clear();
                            processed
                        };
                        tail.push_str(&flush_anthropic_carry(&mut carry, &vault));
                        if tail.is_empty() {
                            return Ok(None);
                        }
                        return Ok(Some((Bytes::from(tail), (buf, carry, stream, vault))));
                    }
                    Err(e) => {
                        return Err(std::io::Error::other(e));
                    }
                }
            }
        },
    );

    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(axum::http::header::CONTENT_TYPE, ct.as_bytes());
    }
    builder
        .body(Body::from_stream(stream))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Rehydrate the text-bearing fields inside one Anthropic SSE event.
///
/// `carry` holds back a partial placeholder that spans two events; it is
/// flushed before `message_stop` and again at end of stream.
fn process_anthropic_sse_event(event: &str, carry: &mut StreamCarry, vault: &Vault) -> String {
    let mut out = String::new();

    // If this event carries `message_stop`, flush any held-back placeholder
    // text before emitting the stop event.
    let is_message_stop = event.lines().any(|line| {
        line.strip_prefix("data: ")
            .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
            .and_then(|v| {
                v.get("type")
                    .and_then(|t| t.as_str().map(|s| s == "message_stop"))
            })
            .unwrap_or(false)
    });
    if is_message_stop {
        out.push_str(&flush_anthropic_carry(carry, vault));
    }

    for line in event.lines() {
        if let Some(payload) = line.strip_prefix("data: ") {
            match serde_json::from_str::<Value>(payload) {
                Ok(mut value) => {
                    rehydrate_anthropic_sse_delta(&mut value, carry, vault);
                    out.push_str("data: ");
                    match serde_json::to_string(&value) {
                        Ok(serialized) => out.push_str(&serialized),
                        Err(_) => out.push_str(payload),
                    }
                }
                Err(_) => out.push_str(line),
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    // Each SSE event is terminated by a blank line.
    out.push('\n');
    out
}

/// Apply `Gateway::rehydrate` to text-bearing Anthropic streaming deltas.
fn rehydrate_anthropic_sse_delta(value: &mut Value, carry: &mut StreamCarry, vault: &Vault) {
    if value.get("type").and_then(|t| t.as_str()) != Some("content_block_delta") {
        return;
    }
    let Some(delta) = value.get_mut("delta") else {
        return;
    };
    let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match delta_type {
        "text_delta" => {
            if let Some(Value::String(content)) = delta.get_mut("text") {
                let rehydrated = rehydrate_with_carry(content, &mut carry.content, vault);
                *content = rehydrated;
            }
        }
        "input_json_delta" => {
            if let Some(Value::String(partial_json)) = delta.get_mut("partial_json") {
                let rehydrated = rehydrate_with_carry(partial_json, &mut carry.args, vault);
                *partial_json = rehydrated;
            }
        }
        _ => {}
    }
}

/// Emit held-back Anthropic placeholder text as final synthetic deltas.
fn flush_anthropic_carry(carry: &mut StreamCarry, vault: &Vault) -> String {
    if carry.content.is_empty() && carry.args.is_empty() {
        return String::new();
    }
    let mut events = Vec::new();
    if !carry.content.is_empty() {
        let held = std::mem::take(&mut carry.content);
        let payload = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": vault.restore(&held) }
        });
        events.push(format!("event: content_block_delta\ndata: {payload}\n\n"));
    }
    if !carry.args.is_empty() {
        let held = std::mem::take(&mut carry.args);
        let payload = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": vault.restore(&held) }
        });
        events.push(format!("event: content_block_delta\ndata: {payload}\n\n"));
    }
    events.join("")
}

// ---------------------------------------------------------------------------
// In-chat learning surface (M2)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TeachRequest {
    term: String,
    label: String,
    #[serde(default = "default_scope")]
    scope: String,
    /// Match only at word boundaries.
    #[serde(default)]
    whole_word: bool,
}

#[derive(Deserialize)]
struct UnteachRequest {
    term: String,
}

fn default_scope() -> String {
    "global".into()
}

/// Resolve the effective store path for persistence.
fn store_path(state: &ProxyState) -> String {
    state.store_path.clone().unwrap_or_else(|| {
        std::env::var("PORTCULLIS_STORE").unwrap_or_else(|_| "store.json".into())
    })
}

/// Constant-time equality check for bearer tokens.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = 0u8;
    for i in 0..max {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0 && a.len() == b.len()
}

/// Enforce `Authorization: Bearer <PORTCULLIS_ADMIN_TOKEN>`.
///
/// Returns 503 if no token is configured, 401 on mismatch or missing header.
fn require_admin(state: &ProxyState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let token: String = match state.admin_token.as_deref() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => match std::env::var("PORTCULLIS_ADMIN_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "admin token not configured".into(),
                ))
            }
        },
    };

    let header = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
    let provided = match header {
        Some(h) if h.starts_with("Bearer ") => &h["Bearer ".len()..],
        _ => {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing or malformed bearer token".into(),
            ))
        }
    };

    if !constant_time_eq(provided, &token) {
        return Err((StatusCode::UNAUTHORIZED, "invalid bearer token".into()));
    }
    Ok(())
}

async fn teach_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(req): Json<TeachRequest>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let path = store_path(&state);
    let mut gw = state.gateway.lock().await;
    gw.teach_with(&req.term, &req.label, &req.scope, req.whole_word);
    if let Err(e) = gw.store.save(&path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let total = gw.store.deny.len();
    Json(json!({ "ok": true, "taught": req.term, "total": total })).into_response()
}

async fn unteach_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(req): Json<UnteachRequest>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let path = store_path(&state);
    let mut gw = state.gateway.lock().await;
    let removed = gw.unteach(&req.term);
    if let Err(e) = gw.store.save(&path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    Json(json!({ "ok": true, "removed": removed })).into_response()
}

async fn terms_handler(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let gw = state.gateway.lock().await;
    let terms: Vec<Value> = gw
        .store
        .deny
        .iter()
        .map(|e| json!({"term": e.term, "label": e.label, "scope": e.scope}))
        .collect();
    Json(json!({ "terms": terms })).into_response()
}

async fn suggestions_handler(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let gw = state.gateway.lock().await;
    let suggestions = gw.suggestions();
    Json(json!({ "suggestions": suggestions })).into_response()
}

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
        .route("/teach", post(teach_handler))
        .route("/unteach", post(unteach_handler))
        .route("/terms", get(terms_handler))
        .route("/suggestions", get(suggestions_handler))
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
        "portcullis listening"
    );

    axum::serve(listener, app(state)).await?;
    Ok(())
}
