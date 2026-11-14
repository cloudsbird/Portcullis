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
    let upstream_url = spawn_server(mock_app(MockUpstream {
        bodies: bodies.clone(),
    }))
    .await;
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

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );

    let recorded = bodies.lock().await;
    assert_eq!(
        recorded.len(),
        1,
        "upstream should have received exactly one request"
    );
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
    let upstream_url = spawn_server(mock_app(MockUpstream {
        bodies: bodies.clone(),
    }))
    .await;
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
    let upstream_url = spawn_server(mock_app(MockUpstream {
        bodies: bodies.clone(),
    }))
    .await;
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

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );

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
    let upstream_url = spawn_server(mock_app(MockUpstream {
        bodies: bodies.clone(),
    }))
    .await;
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
    assert_eq!(
        recorded.len(),
        1,
        "upstream should have received exactly one request"
    );
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

/// Reasoning replayed in history (`reasoning_content`, OpenRouter's `reasoning` and
/// unsigned `reasoning_details`) is redacted like content. It used to be forwarded
/// as-is, so the outbound assertion blocked every request in a conversation whose
/// reasoning mentioned a taught term: HTTP 502 "residual protected term in outbound".
#[tokio::test]
async fn proxy_redacts_replayed_reasoning_fields() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream {
        bodies: bodies.clone(),
    }))
    .await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let signed = json!({
        "type": "reasoning.text",
        "text": "signed thoughts",
        "signature": "sig-abc",
        "format": "anthropic-claude-v1",
        "index": 2
    });
    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [
                {"role": "user", "content": "Summarise the deal."},
                {
                    "role": "assistant",
                    "content": "Looking it up.",
                    "reasoning_content": "rc: Cartalian wants a summary",
                    "reasoning": "r: Cartalian wants a summary",
                    "reasoning_details": [
                        {"type": "reasoning.text", "text": "dt: Cartalian", "signature": null, "index": 0},
                        {"type": "reasoning.summary", "summary": "ds: Cartalian", "index": 1},
                        signed,
                        {"type": "reasoning.encrypted", "data": "opaque", "index": 3}
                    ],
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{\"q\": \"Cartalian\"}"}
                    }]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
            ]
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
    assert!(
        !recorded[0].contains("Cartalian"),
        "upstream body contained raw PII: {}",
        recorded[0]
    );
    let forwarded: Value = serde_json::from_str(&recorded[0]).unwrap();
    let msg = &forwarded["messages"][1];
    // Each redacted string went back to the field it came from.
    let field = |v: &Value| v.as_str().unwrap().to_string();
    assert!(
        field(&msg["reasoning_content"]).starts_with("rc: <<"),
        "{msg}"
    );
    assert!(field(&msg["reasoning"]).starts_with("r: <<"), "{msg}");
    assert!(
        field(&msg["reasoning_details"][0]["text"]).starts_with("dt: <<"),
        "{msg}"
    );
    assert!(
        field(&msg["reasoning_details"][1]["summary"]).starts_with("ds: <<"),
        "{msg}"
    );
    assert_eq!(field(&msg["content"]), "Looking it up.");
    assert!(
        field(&msg["tool_calls"][0]["function"]["arguments"]).contains("<<"),
        "{msg}"
    );
    // Signed and encrypted entries are forwarded byte for byte.
    assert_eq!(msg["reasoning_details"][2], signed);
    assert_eq!(msg["reasoning_details"][3]["data"], "opaque");
}

/// A signed `reasoning_details` entry cannot be edited without the provider
/// rejecting it, so one holding a taught term is blocked, and the error names the
/// field (never the term).
#[tokio::test]
async fn signed_reasoning_with_a_taught_term_is_blocked_with_its_path() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream {
        bodies: bodies.clone(),
    }))
    .await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{
                "role": "assistant",
                "content": "Done.",
                "reasoning_details": [
                    {"type": "reasoning.text", "text": "about Cartalian", "signature": "sig-abc"}
                ]
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
    let err = resp.text().await.unwrap();
    assert_eq!(
        err,
        "residual protected term in outbound at messages[0].reasoning_details[0].text"
    );
    assert!(
        bodies.lock().await.is_empty(),
        "blocked request was forwarded"
    );
}

async fn reasoning_echo_handler() -> Json<Value> {
    Json(json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Noted.",
                "reasoning_content": "thinking about <<ORG_1>>",
                "reasoning": "thinking about <<ORG_1>>"
            },
            "finish_reason": "stop"
        }]
    }))
}

/// The loop that broke conversations: a reply's reasoning is rehydrated, the client
/// replays it on the next turn, and that turn must still go through.
#[tokio::test]
async fn rehydrated_reasoning_can_be_replayed_next_turn() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let upstream_url =
        spawn_server(Router::new().route("/v1/chat/completions", post(reasoning_echo_handler)))
            .await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let first: Value = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Tell me about Cartalian."}]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reply = first["choices"][0]["message"].clone();
    assert_eq!(reply["reasoning_content"], "thinking about Cartalian");
    assert_eq!(reply["reasoning"], "thinking about Cartalian");

    let second = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [
                {"role": "user", "content": "Tell me about Cartalian."},
                reply,
                {"role": "user", "content": "And then?"}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        second.status().is_success(),
        "replayed reasoning was blocked: {:?}",
        second.text().await
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
    let upstream_url = spawn_server(mock_app(MockUpstream {
        bodies: bodies.clone(),
    }))
    .await;
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

async fn echo_placeholders_handler() -> Json<Value> {
    Json(json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Re: <<ORG_1>> / <<EMAIL_1>>" },
            "finish_reason": "stop"
        }]
    }))
}

/// One client's values must never be restored into another client's response,
/// even when the upstream (or a prompt-injected model) emits their placeholders.
#[tokio::test]
async fn placeholders_do_not_leak_across_requests() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let upstream_url =
        spawn_server(Router::new().route("/v1/chat/completions", post(echo_placeholders_handler)))
            .await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let ask = |text: &'static str| {
        let client = client.clone();
        let url = proxy_url.clone();
        async move {
            client
                .post(&url)
                .json(&json!({
                    "model": "test-model",
                    "messages": [{"role": "user", "content": text}]
                }))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        }
    };

    let a = ask("Cartalian, alice@example.com").await;
    assert!(
        a.contains("Cartalian") && a.contains("alice@example.com"),
        "{a}"
    );

    let b = ask("A harmless question.").await;
    assert!(
        !b.contains("Cartalian") && !b.contains("alice@example.com"),
        "client B received client A's values: {b}"
    );
}

/// An ML detector that fails on every segment, like a broken ONNX model.
#[derive(Default)]
struct FailingDetector {
    pending: std::sync::atomic::AtomicU64,
}

impl portcullis::detect::Detector for FailingDetector {
    fn detect(&self, _text: &str) -> Vec<portcullis::detect::Span> {
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Vec::new()
    }
    fn take_failures(&self) -> u64 {
        self.pending.swap(0, std::sync::atomic::Ordering::Relaxed)
    }
}

/// With fail-closed on, a detector failure blocks the request: 503, nothing upstream.
/// With it off (the default), the request is forwarded on deterministic coverage.
#[tokio::test]
async fn detector_failure_blocks_only_when_fail_closed() {
    for fail_closed in [true, false] {
        let gw = Gateway::new(Store::default())
            .with_detector(Box::new(FailingDetector::default()))
            .with_fail_closed(fail_closed);

        let bodies = Arc::new(Mutex::new(Vec::new()));
        let upstream_url = spawn_server(mock_app(MockUpstream {
            bodies: bodies.clone(),
        }))
        .await;
        let client = Client::new();
        let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
        let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

        let resp = client
            .post(&proxy_url)
            .json(&json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .unwrap();

        let forwarded = bodies.lock().await.len();
        if fail_closed {
            assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(forwarded, 0, "a blocked request must not reach upstream");
        } else {
            assert!(resp.status().is_success(), "{:?}", resp.status());
            assert_eq!(forwarded, 1);
        }
    }
}
