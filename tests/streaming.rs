//! End-to-end streaming (SSE) proxy tests.

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::State,
    response::Response,
    routing::post,
    Json, Router,
};
use futures::stream;
use portcullis::proxy::ProxyState;
use portcullis::{Gateway, Store};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[derive(Clone)]
struct MockUpstream {
    bodies: Arc<Mutex<Vec<String>>>,
    chunks: Vec<Bytes>,
}

async fn mock_json_handler(
    State(state): State<MockUpstream>,
    Json(body): Json<Value>,
) -> Json<Value> {
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

async fn mock_sse_handler(State(state): State<MockUpstream>, Json(body): Json<Value>) -> Response {
    state.bodies.lock().await.push(body.to_string());
    let items: Vec<Result<Bytes, std::io::Error>> = state.chunks.into_iter().map(Ok).collect();
    let stream = stream::iter(items);
    Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn mock_sse_app(state: MockUpstream) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(mock_sse_handler))
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

/// (a) A non-streaming request still follows the existing buffered path.
#[tokio::test]
async fn non_streaming_request_behaves_as_before() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: Vec::new(),
    };
    let upstream_url = spawn_server(
        Router::new()
            .route("/v1/chat/completions", post(mock_json_handler))
            .with_state(upstream),
    )
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
    let json: Value = resp.json().await.unwrap();
    assert_eq!(json["choices"][0]["message"]["content"], "Understood.");

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

/// (b) A streaming request rehydrates delta content and never forwards raw PII.
#[tokio::test]
async fn streaming_request_rehydrates_and_does_not_forward_pii() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![
            Bytes::from("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"He works at <<ORG_1>>\"},\"finish_reason\":null}]}\n\n"),
            Bytes::from("data: [DONE]\n\n"),
        ],
    };
    let upstream_url = spawn_server(mock_sse_app(upstream)).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Cartalian is our client."}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );
    let ct = resp
        .headers()
        .get("content-type")
        .expect("missing content-type")
        .to_str()
        .unwrap();
    assert!(
        ct.contains("text/event-stream"),
        "expected text/event-stream, got {}",
        ct
    );

    let text = resp.text().await.unwrap();

    let recorded = bodies.lock().await;
    assert_eq!(
        recorded.len(),
        1,
        "upstream should have received exactly one request"
    );
    assert!(
        !recorded[0].contains("Cartalian"),
        "upstream body leaked raw PII: {}",
        recorded[0]
    );

    assert!(
        text.contains("Cartalian"),
        "client did not receive rehydrated text: {}",
        text
    );
    assert!(
        text.contains("data: [DONE]"),
        "[DONE] marker missing from response: {}",
        text
    );
}

/// (c) A placeholder split across two SSE chunks is still restored cleanly.
#[tokio::test]
async fn streaming_request_handles_split_placeholder() {
    let mut store = Store::default();
    store.teach("daniel@example.com", "EMAIL", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![
            Bytes::from(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"He works at <<EMAI",
            ),
            Bytes::from("L_1>>\"},\"finish_reason\":null}]}\n\n"),
            Bytes::from("data: [DONE]\n\n"),
        ],
    };
    let upstream_url = spawn_server(mock_sse_app(upstream)).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "What is daniel@example.com working on?"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );
    let text = resp.text().await.unwrap();

    assert!(
        !text.contains("<<EMAI"),
        "split placeholder prefix leaked to client: {}",
        text
    );
    assert!(
        !text.contains("L_1>>"),
        "split placeholder suffix leaked to client: {}",
        text
    );
    assert!(
        text.contains("daniel@example.com"),
        "email was not restored: {}",
        text
    );
    assert!(
        text.contains("data: [DONE]"),
        "[DONE] marker missing from response: {}",
        text
    );
}

/// A streaming request still runs the fail-closed assertion before forwarding.
#[tokio::test]
async fn streaming_request_fails_closed_and_does_not_forward() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    // Teaching the label name as protected makes the redacted placeholder
    // "<<ORG_1>>" fail assert_clean, so the streaming path must also fail closed.
    store.teach("ORG", "SENSITIVE", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![Bytes::from("data: [DONE]\n\n")],
    };
    let upstream_url = spawn_server(mock_sse_app(upstream)).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Cartalian is our client."}],
            "stream": true
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

/// Streaming tool_call deltas have their `function.arguments` placeholder restored.
#[tokio::test]
async fn streaming_request_rehydrates_tool_call_arguments() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![
            Bytes::from("data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"get_client\",\"arguments\":\"{\\\"client\\\": \\\"<<ORG_1>>\\\"}\"}}]},\"finish_reason\":null}]}\n\n"),
            Bytes::from("data: [DONE]\n\n"),
        ],
    };
    let upstream_url = spawn_server(mock_sse_app(upstream)).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Cartalian is our client."}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );
    let text = resp.text().await.unwrap();

    let recorded = bodies.lock().await;
    assert_eq!(recorded.len(), 1);
    assert!(
        !recorded[0].contains("Cartalian"),
        "upstream body leaked raw PII: {}",
        recorded[0]
    );

    assert!(
        text.contains("Cartalian"),
        "tool_call arguments were not rehydrated: {}",
        text
    );
    assert!(
        text.contains("data: [DONE]"),
        "[DONE] marker missing from response: {}",
        text
    );
}

/// (d) The upstream `data: [DONE]` marker is passed through unchanged.
#[tokio::test]
async fn streaming_request_preserves_done() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![
            Bytes::from("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\n"),
            Bytes::from("data: [DONE]\n\n"),
        ],
    };
    let upstream_url = spawn_server(mock_sse_app(upstream)).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "Cartalian is our client."}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("data: [DONE]"),
        "[DONE] marker not preserved: {}",
        text
    );
}

/// A placeholder split across two COMPLETE SSE events must still be restored.
///
/// This is the case that actually happens in practice: LLM tokens map roughly
/// one-to-one onto SSE deltas, so `<<EMAIL_1>>` frequently spans several
/// events. Buffering whole SSE events is NOT sufficient — the carry buffer must
/// work at the placeholder level.
#[tokio::test]
async fn streaming_request_handles_placeholder_split_across_events() {
    let mut store = Store::default();
    store.teach("daniel@example.com", "EMAIL", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![
            Bytes::from(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"He works at <<EMAI\"},\"finish_reason\":null}]}\n\n",
            ),
            Bytes::from(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"L_1>>\"},\"finish_reason\":null}]}\n\n",
            ),
            Bytes::from("data: [DONE]\n\n"),
        ],
    };
    let upstream_url = spawn_server(mock_sse_app(upstream)).await;
    let client = Client::new();
    let state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "What is daniel@example.com working on?"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "proxy returned error: {:?}",
        resp.text().await
    );
    let text = resp.text().await.unwrap();

    assert!(
        !text.contains("<<EMAI"),
        "half-placeholder prefix leaked to client: {}",
        text
    );
    assert!(
        !text.contains("L_1>>"),
        "half-placeholder suffix leaked to client: {}",
        text
    );
    assert!(
        text.contains("daniel@example.com"),
        "email was not restored across events: {}",
        text
    );
}
