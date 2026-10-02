//! End-to-end Anthropic Messages API proxy tests.

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

async fn mock_anthropic_json_handler(
    State(state): State<MockUpstream>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.bodies.lock().await.push(body.to_string());
    Json(json!({
        "id": "msg_01Test",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [
            { "type": "text", "text": "Understood, <<ORG_1>> is the client." }
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": { "input_tokens": 10, "output_tokens": 10 }
    }))
}

async fn mock_anthropic_sse_handler(
    State(state): State<MockUpstream>,
    Json(body): Json<Value>,
) -> Response {
    state.bodies.lock().await.push(body.to_string());
    let items: Vec<Result<Bytes, std::io::Error>> = state.chunks.into_iter().map(Ok).collect();
    let stream = stream::iter(items);
    Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn mock_anthropic_app(state: MockUpstream) -> Router {
    Router::new()
        .route("/v1/messages", post(mock_anthropic_sse_handler))
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
    format!("http://{}/v1/messages", addr)
}

fn anthropic_proxy_state(gw: Gateway, upstream_url: String) -> ProxyState {
    let mut state = ProxyState::new(gw, Client::new(), "unused".into(), "key".into());
    state.anthropic_upstream_url = upstream_url;
    state.anthropic_upstream_key = "test-anthropic-key".into();
    state
}

/// (a) Non-streaming: terms in `system` and `messages[].content` are redacted
/// before forwarding, and the response text is rehydrated.
#[tokio::test]
async fn anthropic_non_streaming_redacts_system_and_content() {
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
            .route("/v1/messages", post(mock_anthropic_json_handler))
            .with_state(upstream),
    )
    .await;
    let state = anthropic_proxy_state(gw, upstream_url);
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = Client::new()
        .post(&proxy_url)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "claude-test",
            "system": [{"type": "text", "text": "Cartalian is the client."}],
            "messages": [{"role": "user", "content": "Tell me about Cartalian."}],
            "max_tokens": 1024
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
    let content = json["content"][0]["text"].as_str().expect("text content");
    assert!(content.contains("Cartalian"), "response was not rehydrated: {}", content);

    let recorded = bodies.lock().await;
    assert_eq!(recorded.len(), 1, "upstream should have received exactly one request");
    assert!(
        !recorded[0].contains("Cartalian"),
        "upstream body contained raw PII: {}",
        recorded[0]
    );
}

/// (b) tool_use input string leaves are redacted, and tool_result text is redacted.
#[tokio::test]
async fn anthropic_redacts_tool_use_and_tool_result() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    store.teach("daniel@example.com", "EMAIL", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: Vec::new(),
    };
    let upstream_url = spawn_server(
        Router::new()
            .route("/v1/messages", post(mock_anthropic_json_handler))
            .with_state(upstream),
    )
    .await;
    let state = anthropic_proxy_state(gw, upstream_url);
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = Client::new()
        .post(&proxy_url)
        .json(&json!({
            "model": "claude-test",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tu_1",
                            "content": "Contact daniel@example.com at Cartalian."
                        },
                        {
                            "type": "tool_use",
                            "id": "tu_2",
                            "name": "get_client",
                            "input": {
                                "client": "Cartalian Industries",
                                "contact": {
                                    "email": "daniel@example.com"
                                }
                            }
                        }
                    ]
                }
            ],
            "max_tokens": 1024
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
    let content = forwarded["messages"][0]["content"].as_array().expect("content array");
    assert_eq!(content.len(), 2);

    let tool_result_text = content[0]["content"].as_str().unwrap();
    assert!(
        !tool_result_text.contains("Cartalian") && !tool_result_text.contains("daniel@example.com"),
        "tool_result leaked raw PII: {}",
        tool_result_text
    );

    let input = &content[1]["input"];
    assert!(
        !input["client"].as_str().unwrap().contains("Cartalian"),
        "tool_use input client leaked raw PII"
    );
    assert!(
        !input["contact"]["email"].as_str().unwrap().contains("daniel@example.com"),
        "tool_use input email leaked raw PII"
    );
}

/// (c) Streaming: a text placeholder split across two complete
/// content_block_delta events is rehydrated correctly and message_stop is preserved.
#[tokio::test]
async fn anthropic_streaming_text_delta_split_placeholder() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![
            Bytes::from("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"He works at <<ORG\"}}\n\n"),
            Bytes::from("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"_1>>\"}}\n\n"),
            Bytes::from("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        ],
    };
    let upstream_url = spawn_server(mock_anthropic_app(upstream)).await;
    let state = anthropic_proxy_state(gw, upstream_url);
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = Client::new()
        .post(&proxy_url)
        .json(&json!({
            "model": "claude-test",
            "messages": [{"role": "user", "content": "Where does Cartalian work?"}],
            "stream": true,
            "max_tokens": 1024
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
        !text.contains("<<ORG"),
        "split placeholder prefix leaked to client: {}",
        text
    );
    assert!(
        !text.contains("_1>>"),
        "split placeholder suffix leaked to client: {}",
        text
    );
    assert!(
        text.contains("Cartalian"),
        "client did not receive rehydrated text: {}",
        text
    );
    assert!(
        text.contains("message_stop"),
        "message_stop event missing from response: {}",
        text
    );
}

/// (d) Streaming: a placeholder split across two input_json_delta partial_json
/// events is rehydrated correctly.
#[tokio::test]
async fn anthropic_streaming_input_json_delta_split_placeholder() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![
            Bytes::from("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"client\\\": \\\"<<ORG\"}}\n\n"),
            Bytes::from("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"_1>>\\\"}\"}}\n\n"),
            Bytes::from("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        ],
    };
    let upstream_url = spawn_server(mock_anthropic_app(upstream)).await;
    let state = anthropic_proxy_state(gw, upstream_url);
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = Client::new()
        .post(&proxy_url)
        .json(&json!({
            "model": "claude-test",
            "messages": [{"role": "user", "content": "Which client? Cartalian."}],
            "stream": true,
            "max_tokens": 1024
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
        !text.contains("<<ORG"),
        "split placeholder prefix leaked to client: {}",
        text
    );
    assert!(
        !text.contains("_1>>"),
        "split placeholder suffix leaked to client: {}",
        text
    );
    assert!(
        text.contains("Cartalian"),
        "input_json partial was not rehydrated: {}",
        text
    );
}

/// (e) Fail-closed: when assert_clean fails, respond 502 and do not forward.
#[tokio::test]
async fn anthropic_fails_closed_and_does_not_forward() {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    // Teaching the label name as protected makes the redacted placeholder
    // "<<ORG_1>>" fail assert_clean, so the Anthropic path must also fail closed.
    store.teach("ORG", "SENSITIVE", "global");
    let gw = Gateway::new(store);

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = MockUpstream {
        bodies: bodies.clone(),
        chunks: vec![Bytes::from("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")],
    };
    let upstream_url = spawn_server(mock_anthropic_app(upstream)).await;
    let state = anthropic_proxy_state(gw, upstream_url);
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;

    let resp = Client::new()
        .post(&proxy_url)
        .json(&json!({
            "model": "claude-test",
            "messages": [{"role": "user", "content": "Cartalian is our client."}],
            "stream": true,
            "max_tokens": 1024
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
