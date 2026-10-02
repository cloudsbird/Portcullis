//! OpenAI-compatible HTTP proxy.
//!
//! Intercepts chat-completion requests, redacts every message through the
//! gateway, asserts the outbound payload is clean, forwards to an upstream
//! provider, and rehydrates the assistant reply before returning it.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::Gateway;

/// Shared state for the proxy handler.
#[derive(Clone)]
pub struct ProxyState {
    pub gateway: Arc<Mutex<Gateway>>,
    pub client: Client,
    pub upstream_url: String,
    pub upstream_key: String,
}

impl ProxyState {
    pub fn new(gateway: Gateway, client: Client, upstream_url: String, upstream_key: String) -> Self {
        Self {
            gateway: Arc::new(Mutex::new(gateway)),
            client,
            upstream_url,
            upstream_key,
        }
    }
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
    let Value::Array(calls) = tool_calls else { return };
    for call in calls {
        let Value::Object(call_map) = call else { continue };
        let Some(Value::Object(func_map)) = call_map.get_mut("function") else { continue };
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
    let Value::Array(calls) = tool_calls else { return };
    for call in calls {
        let Value::Object(call_map) = call else { continue };
        let Some(Value::Object(func_map)) = call_map.get_mut("function") else { continue };
        if let Some(Value::String(_)) = func_map.get_mut("arguments") {
            func_map["arguments"] = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
    }
}

pub async fn chat_completions(State(state): State<ProxyState>, Json(body): Json<Value>) -> Response {
    match handle(state, body).await {
        Ok(resp) => resp,
        Err((status, msg)) => (status, msg).into_response(),
    }
}

async fn handle(state: ProxyState, body: Value) -> Result<Response, (StatusCode, String)> {
    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or((StatusCode::BAD_REQUEST, "missing or invalid messages".to_string()))?;

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
    let redacted_texts = {
        let mut gw = state.gateway.lock().await;
        gw.process(&texts_to_redact)
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

    let upstream_resp = state
        .client
        .post(&state.upstream_url)
        .bearer_auth(&state.upstream_key)
        .json(&upstream_body)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let status = upstream_resp.status();
    let mut upstream_json: Value = upstream_resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    // Rehydrate assistant content locally before returning to the caller.
    if let Some(choices) = upstream_json.get_mut("choices").and_then(|c| c.as_array_mut()) {
        for choice in choices.iter_mut() {
            if let Some(msg) = choice.get_mut("message") {
                if let Some(content) = msg.get_mut("content").and_then(|c| c.as_str()) {
                    let restored = {
                        let gw = state.gateway.lock().await;
                        gw.rehydrate(content)
                    };
                    msg["content"] = json!(restored);
                }
            }
        }
    }

    Ok((status, Json(upstream_json)).into_response())
}

pub fn app(state: ProxyState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

/// Run the proxy server bound to `bind`.
///
/// Reads `PORTCULLIS_UPSTREAM_URL` and `PORTCULLIS_UPSTREAM_KEY` from the
/// environment.
pub async fn serve(gateway: Gateway, bind: &str) -> anyhow::Result<()> {
    let upstream_url = std::env::var("PORTCULLIS_UPSTREAM_URL")
        .map_err(|_| anyhow::anyhow!("PORTCULLIS_UPSTREAM_URL is not set"))?;
    let upstream_key = std::env::var("PORTCULLIS_UPSTREAM_KEY").unwrap_or_default();

    let client = Client::builder()
        .use_rustls_tls()
        .build()?;

    let state = ProxyState::new(gateway, client, upstream_url, upstream_key);
    let addr: SocketAddr = bind.parse()?;
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app(state)).await?;
    Ok(())
}
