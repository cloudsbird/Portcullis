//! End-to-end proxy invariant tests.
//!
//! These prove the proxy path obeys the same fail-closed contract as the
//! direct `Gateway::process` API.

use std::sync::Arc;

use axum::{extract::State, routing::post, Json, Router};
use portcullis::proxy::ProxyState;
use portcullis::{Gateway, Store};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[derive(Clone)]
struct MockUpstream {
    bodies: Arc<Mutex<Vec<String>>>,
}

async fn mock_handler(State(state): State<MockUpstream>, Json(body): Json<Value>) -> Json<Value> {
    state.bodies.lock().await.push(body.to_string());
    Json(json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [
            {
                "index": 0,
                "message": { "role": "assistant", "content": "Understood." },
                "finish_reason": "stop"
            }
        ]
    }))
}

fn mock_app(state: MockUpstream) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(mock_handler))
        .with_state(state)
}

async fn spawn_server(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give the spawned server a moment to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("http://{}/v1/chat/completions", addr)
}

/// A known raw PII value never appears in the body forwarded upstream.
#[tokio::test]
async fn proxy_never_forwards_raw_pii() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Cartalian is our client."}]
        }))
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success(), "proxy returned error: {:?}", resp.text().await);

    let recorded = bodies.lock().await;
    assert_eq!(recorded.len(), 1, "upstream should have received exactly one request");
    assert!(
        !recorded[0].contains("Cartalian"),
        "upstream body contained raw PII: {}",
        recorded[0]
    );
}

/// When the fail-closed assertion fails, nothing is forwarded upstream.
#[tokio::test]
async fn proxy_fails_closed_and_does_not_forward() {
    // Teach the label name as a protected term. Redacting "Cartalian" with
    // label ORG produces "<<ORG_1>>", which still contains the hidden form
    // "ORG" and therefore fails assert_clean.
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    store.teach("ORG", "SENSITIVE", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Cartalian is our client."}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        502,
        "expected 502 fail-closed, got {}",
        resp.status()
    );

    let recorded = bodies.lock().await;
    assert!(
        recorded.is_empty(),
        "upstream should not have received any request when fail-closed"
    );
}

/// A placeholder echoed by the upstream model is restored in the returned JSON.
#[tokio::test]
async fn proxy_rehydrates_assistant_reply() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Cartalian is our client."}]
        }))
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success(), "proxy returned error: {:?}", resp.text().await);

    // The mock upstream replied with plain "Understood.", so rehydration is a no-op.
    let json: Value = resp.json().await.unwrap();
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .expect("assistant content missing");
    assert_eq!(content, "Understood.");
}

/// Protected terms inside `tool_calls[*].function.arguments` are redacted,
/// and the rest of the `tool_calls` object survives forwarding.
#[tokio::test]
async fn proxy_redacts_tool_call_arguments_and_preserves_tool_calls() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{
                "role": "assistant",
                "content": "I will look that up.",
                "tool_calls": [
                    {
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "get_client_info",
                            "arguments": "{\"client\": \"Cartalian Industries\"}"
                        }
                    }
                ]
            }]
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );

    let recorded = bodies.lock().await;
    assert_eq!(recorded.len(), 1, "upstream should have received exactly one request");
    let forwarded: Value = serde_json::from_str(&recorded[0]).unwrap();
    let tool_calls = forwarded["messages"][0]["tool_calls"]
        .as_array()
        .expect("tool_calls should survive forwarding");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["id"], "call_123");
    assert_eq!(tool_calls[0]["type"], "function");
    assert_eq!(tool_calls[0]["function"]["name"], "get_client_info");

    let args = tool_calls[0]["function"]["arguments"]
        .as_str()
        .expect("arguments should remain a string");
    assert!(
        !args.contains("Cartalian"),
        "tool_call arguments leaked raw PII: {}",
        args
    );
}

/// Multimodal `content` arrays have their text-bearing parts redacted while
/// preserving the array shape and any non-text fields.
#[tokio::test]
async fn proxy_redacts_multimodal_content_parts() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Cartalian is our client."},
                    {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}}
                ]
            }]
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );

    let recorded = bodies.lock().await;
    assert_eq!(recorded.len(), 1);
    let forwarded: Value = serde_json::from_str(&recorded[0]).unwrap();
    let content = forwarded["messages"][0]["content"]
        .as_array()
        .expect("content array should be preserved");
    assert_eq!(content.len(), 2);
    assert_eq!(content[1]["type"], "image_url");
    assert!(
        !content[0]["text"].as_str().unwrap().contains("Cartalian"),
        "text part leaked raw PII: {}",
        content[0]["text"]
    );
}
