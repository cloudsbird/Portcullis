//! Transparent passthrough mode (`PORTCULLIS_PASSTHROUGH`).
//!
//! A request names its real upstream in `x-portcullis-upstream`; Portcullis still
//! redacts and rehydrates, but forwards with the *client's* credentials and
//! headers instead of its own configured upstream and key.

use std::sync::Arc;

use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
use portcullis::proxy::{ProxyState, UPSTREAM_HEADER};
use portcullis::{Gateway, Store};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[derive(Clone, Default)]
struct Recorder {
    /// (headers, body) of every request the mock upstream received.
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

async fn openai_echo(
    State(rec): State<Recorder>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Echo the (redacted) user text back so rehydration can be checked.
    let said = body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    rec.seen.lock().await.push((headers, body));
    Json(json!({
        "id": "x", "object": "chat.completion", "model": "m",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": format!("You said: {said}")}}]
    }))
}

async fn anthropic_echo(
    State(rec): State<Recorder>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let said = body["messages"][0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    rec.seen.lock().await.push((headers, body));
    Json(json!({
        "id": "msg", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": format!("You said: {said}")}],
        "stop_reason": "end_turn"
    }))
}

/// Spawn the mock upstream; returns its base URL (`http://127.0.0.1:port`).
async fn spawn_upstream(rec: Recorder) -> String {
    let app = Router::new()
        .route("/v1/chat/completions", post(openai_echo))
        .route("/v1/messages", post(anthropic_echo))
        .with_state(rec);
    spawn(app).await
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("http://{addr}")
}

/// A proxy whose *configured* upstream is `default_upstream`, with passthrough on/off.
async fn spawn_proxy(default_upstream: &str, passthrough: bool, hosts: Vec<String>) -> String {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let mut state = ProxyState::new(
        Gateway::new(store),
        Client::new(),
        format!("{default_upstream}/v1/chat/completions"),
        "portcullis-own-key".into(),
    );
    state.passthrough = passthrough;
    state.passthrough_hosts = hosts;
    spawn(portcullis::proxy::app(state)).await
}

fn chat_body() -> Value {
    json!({"model": "m", "messages": [{"role": "user",
        "content": "Email jane.doe@acme.io about Cartalian"}]})
}

#[tokio::test]
async fn passthrough_forwards_to_named_upstream_with_client_credentials() {
    let default_rec = Recorder::default();
    let default_up = spawn_upstream(default_rec.clone()).await;
    let real_rec = Recorder::default();
    let real_up = spawn_upstream(real_rec.clone()).await;
    let proxy = spawn_proxy(&default_up, true, vec![]).await;

    let resp = Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .header(UPSTREAM_HEADER, format!("{real_up}/v1/chat/completions"))
        .header("authorization", "Bearer client-own-key")
        .header("x-opencode-session", "sess-42")
        .json(&chat_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let reply: Value = resp.json().await.unwrap();
    assert_eq!(
        reply["choices"][0]["message"]["content"],
        "You said: Email jane.doe@acme.io about Cartalian",
        "reply must be rehydrated"
    );

    assert!(
        default_rec.seen.lock().await.is_empty(),
        "configured upstream must not be used"
    );
    let seen = real_rec.seen.lock().await;
    assert_eq!(seen.len(), 1);
    let (headers, body) = &seen[0];
    let sent = body.to_string();
    assert!(!sent.contains("jane.doe@acme.io"), "email leaked: {sent}");
    assert!(!sent.contains("Cartalian"), "taught term leaked: {sent}");
    assert_eq!(headers["authorization"], "Bearer client-own-key");
    assert_eq!(headers["x-opencode-session"], "sess-42");
    assert!(
        headers.get(UPSTREAM_HEADER).is_none(),
        "control header must not be forwarded"
    );
}

#[tokio::test]
async fn upstream_header_rejected_when_passthrough_disabled() {
    let rec = Recorder::default();
    let up = spawn_upstream(rec.clone()).await;
    let proxy = spawn_proxy(&up, false, vec![]).await;

    let resp = Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .header(UPSTREAM_HEADER, format!("{up}/v1/chat/completions"))
        .json(&chat_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    assert!(rec.seen.lock().await.is_empty(), "nothing may be forwarded");
}

#[tokio::test]
async fn passthrough_allowlist_blocks_other_hosts() {
    let rec = Recorder::default();
    let up = spawn_upstream(rec.clone()).await;
    let proxy = spawn_proxy(&up, true, vec!["api.example.com".into()]).await;

    let resp = Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .header(UPSTREAM_HEADER, format!("{up}/v1/chat/completions"))
        .json(&chat_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(rec.seen.lock().await.is_empty());
}

#[tokio::test]
async fn passthrough_without_header_uses_configured_upstream() {
    let rec = Recorder::default();
    let up = spawn_upstream(rec.clone()).await;
    let proxy = spawn_proxy(&up, true, vec![]).await;

    let resp = Client::new()
        .post(format!("{proxy}/v1/chat/completions"))
        .header("authorization", "Bearer client-own-key")
        .json(&chat_body())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = rec.seen.lock().await;
    assert_eq!(
        seen[0].0["authorization"], "Bearer portcullis-own-key",
        "configured route keeps using Portcullis' own key"
    );
}

#[tokio::test]
async fn passthrough_anthropic_forwards_client_api_key() {
    let rec = Recorder::default();
    let up = spawn_upstream(rec.clone()).await;
    let proxy = spawn_proxy("http://127.0.0.1:9", true, vec![]).await;

    let resp = Client::new()
        .post(format!("{proxy}/v1/messages"))
        .header(UPSTREAM_HEADER, format!("{up}/v1/messages"))
        .header("x-api-key", "sk-ant-client")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": "m", "max_tokens": 10,
            "messages": [{"role": "user", "content": "I am jane.doe@acme.io"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let reply: Value = resp.json().await.unwrap();
    assert_eq!(
        reply["content"][0]["text"],
        "You said: I am jane.doe@acme.io"
    );

    let seen = rec.seen.lock().await;
    let (headers, body) = &seen[0];
    assert!(!body.to_string().contains("jane.doe@acme.io"));
    assert_eq!(headers["x-api-key"], "sk-ant-client");
    assert_eq!(
        headers.get_all("anthropic-version").iter().count(),
        1,
        "anthropic-version must not be duplicated"
    );
}
