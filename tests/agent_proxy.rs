//! The proxy against the traffic a real coding agent sends: JSON tool results, tool
//! definitions, images, heredocs, parallel tool calls. Each test here started from a
//! failure seen (or provoked) with Hermes in front of a real provider.

use std::sync::Arc;

use axum::{
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use portcullis::proxy::ProxyState;
use portcullis::{Gateway, Store};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// What the fake provider answers with.
#[allow(dead_code)] // `Sse` is used by the streaming tests added with the per-index carry.
#[derive(Clone)]
enum Reply {
    Json(Value),
    Sse(String),
}

#[derive(Clone)]
struct Upstream {
    bodies: Arc<Mutex<Vec<Value>>>,
    reply: Arc<Mutex<Reply>>,
}

async fn upstream_handler(State(up): State<Upstream>, Json(body): Json<Value>) -> Response {
    up.bodies.lock().await.push(body);
    match up.reply.lock().await.clone() {
        Reply::Json(v) => Json(v).into_response(),
        Reply::Sse(s) => ([(header::CONTENT_TYPE, "text/event-stream")], s).into_response(),
    }
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("http://{addr}")
}

fn chat_reply(message: Value) -> Reply {
    Reply::Json(json!({
        "id": "chatcmpl-t", "object": "chat.completion", "created": 0, "model": "m",
        "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }]
    }))
}

struct Harness {
    client: Client,
    proxy: String,
    bodies: Arc<Mutex<Vec<Value>>>,
    reply: Arc<Mutex<Reply>>,
}

impl Harness {
    async fn new(store: Store) -> Self {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new(chat_reply(
            json!({"role": "assistant", "content": "ok"}),
        )));
        let up = spawn(
            Router::new()
                .route("/v1/chat/completions", post(upstream_handler))
                .route("/v1/messages", post(upstream_handler))
                .with_state(Upstream {
                    bodies: bodies.clone(),
                    reply: reply.clone(),
                }),
        )
        .await;
        let client = Client::new();
        let mut state = ProxyState::new(
            Gateway::new(store),
            client.clone(),
            format!("{up}/v1/chat/completions"),
            "key".into(),
        );
        state.anthropic_upstream_url = format!("{up}/v1/messages");
        let proxy = spawn(portcullis::proxy::app(state)).await;
        Self {
            client,
            proxy,
            bodies,
            reply,
        }
    }

    async fn reply_with(&self, reply: Reply) {
        *self.reply.lock().await = reply;
    }

    async fn chat(&self, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}/v1/chat/completions", self.proxy))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn messages(&self, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}/v1/messages", self.proxy))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn forwarded(&self) -> Vec<Value> {
        self.bodies.lock().await.clone()
    }
}

fn taught(terms: &[(&str, &str)]) -> Store {
    let mut store = Store::default();
    for (term, label) in terms {
        store.teach(term, label, "global");
    }
    store
}

/// Megabytes of base64 contain any short word, ignoring case. The image is forwarded
/// untouched (Portcullis never redacted pixels), so it must not trip the taught-term
/// check either.
#[tokio::test]
async fn an_image_does_not_block_a_short_taught_term() {
    let h = Harness::new(taught(&[("Ann", "PERSON")])).await;
    let pixels = format!("{}xANNx{}", "QUJD".repeat(600), "REVG".repeat(600));
    let data_url = format!("data:image/png;base64,{pixels}");
    let resp = h
        .chat(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "Ann sent this"},
                {"type": "image_url", "image_url": {"url": data_url}}
            ]}]
        }))
        .await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    let sent = h.forwarded().await;
    let parts = &sent[0]["messages"][0]["content"];
    assert_eq!(parts[0]["text"], "<<PERSON_1>> sent this");
    assert_eq!(
        parts[1]["image_url"]["url"], data_url,
        "image must pass unchanged"
    );
}

#[tokio::test]
async fn an_anthropic_image_does_not_block_a_short_taught_term() {
    let h = Harness::new(taught(&[("Ann", "PERSON")])).await;
    h.reply_with(Reply::Json(json!({
        "id": "m", "type": "message", "role": "assistant", "model": "c",
        "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1},
        "content": [{"type": "text", "text": "ok"}]
    })))
    .await;
    let pixels = format!("{}xANNx{}", "QUJD".repeat(600), "REVG".repeat(600));
    let resp = h
        .messages(json!({
            "model": "c", "max_tokens": 5,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": pixels}},
                {"type": "text", "text": "what is this"}
            ]}]
        }))
        .await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
}

/// Tool definitions are text the provider reads, so a taught term in one is redacted
/// like any other text instead of blocking every request that carries the tool.
#[tokio::test]
async fn a_taught_term_in_a_tool_description_is_redacted_not_blocked() {
    let h = Harness::new(taught(&[("Cartalian", "ORG")])).await;
    let resp = h
        .chat(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {
                "name": "search_docs",
                "description": "Search the Cartalian wiki",
                "parameters": {"type": "object", "properties": {
                    "q": {"type": "string", "description": "Query about Cartalian"},
                    "description": {"type": "string", "description": "free text"}
                }}
            }}]
        }))
        .await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    let sent = h.forwarded().await[0].to_string();
    assert!(!sent.contains("Cartalian"), "{sent}");
    assert!(sent.contains("Search the <<ORG_1>> wiki"), "{sent}");
    assert!(
        sent.contains("\"name\":\"search_docs\""),
        "names are kept: {sent}"
    );
}

#[tokio::test]
async fn a_taught_term_in_an_anthropic_tool_description_is_redacted() {
    let h = Harness::new(taught(&[("Cartalian", "ORG")])).await;
    h.reply_with(Reply::Json(json!({
        "id": "m", "type": "message", "role": "assistant", "model": "c",
        "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1},
        "content": [{"type": "text", "text": "ok"}]
    })))
    .await;
    let resp = h
        .messages(json!({
            "model": "c", "max_tokens": 5,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "search", "description": "Cartalian wiki search",
                       "input_schema": {"type": "object", "properties": {
                           "q": {"type": "string", "description": "about Cartalian"}}}}]
        }))
        .await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    let sent = h.forwarded().await[0].to_string();
    assert!(!sent.contains("Cartalian"), "{sent}");
}

/// A shell heredoc is `<<` with no `>>`. That is not a broken placeholder.
#[tokio::test]
async fn a_heredoc_is_not_a_malformed_placeholder() {
    let h = Harness::new(Store::default()).await;
    let resp = h
        .chat(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "run: cat <<'EOF'\nhello\nEOF"}]
        }))
        .await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
}

/// When a request is blocked, the client gets an error in its provider's own JSON
/// shape that says nothing was sent and what to do, not a bare string.
#[tokio::test]
async fn a_blocked_request_explains_itself_in_openai_json() {
    let h = Harness::new(taught(&[("Cartalian", "ORG")])).await;
    let resp = h
        .chat(json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": null, "tool_calls": [{
                "id": "c1", "type": "function",
                "function": {"name": "cartalian_lookup", "arguments": "{}"}
            }]}]
        }))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
    let err: Value = resp.json().await.expect("error body is JSON");
    assert_eq!(err["error"]["code"], "residual_term", "{err}");
    let msg = err["error"]["message"].as_str().unwrap();
    assert!(msg.contains("nothing was sent"), "{msg}");
    assert!(msg.contains("function.name"), "names the field: {msg}");
    assert!(
        !msg.to_lowercase().contains("cartalian"),
        "never echoes the term: {msg}"
    );
    assert!(h.forwarded().await.is_empty());
}

#[tokio::test]
async fn a_blocked_request_explains_itself_in_anthropic_json() {
    let h = Harness::new(taught(&[("Cartalian", "ORG")])).await;
    let resp = h
        .messages(json!({
            "model": "c", "max_tokens": 5,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "about Cartalian", "signature": "sig"},
                {"type": "text", "text": "done"}
            ]}]
        }))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
    let err: Value = resp.json().await.expect("error body is JSON");
    assert_eq!(err["type"], "error", "{err}");
    assert_eq!(err["error"]["type"], "api_error", "{err}");
    let msg = err["error"]["message"].as_str().unwrap();
    assert!(msg.contains("new conversation"), "{msg}");
    assert!(h.forwarded().await.is_empty());
}
