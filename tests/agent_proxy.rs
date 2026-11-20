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

/// Hermes returns tool results as JSON text (`json.dumps({"output": ...})`), so every
/// newline in command output arrives as the two characters `\n`. The escape letter must
/// not be read as part of the next word: that leaked whole-word terms at the start of a
/// line, and turned `\njane@acme.io` into the "email" `njane@acme.io`.
#[tokio::test]
async fn a_json_tool_result_neither_leaks_nor_corrupts_values() {
    let mut store = Store::default();
    store.teach_with("Ann", "PERSON", "global", true);
    let h = Harness::new(store).await;
    h.reply_with(chat_reply(json!({
        "role": "assistant", "content": "<<PERSON_1>> uses <<EMAIL_1>>."
    })))
    .await;
    let tool_output = r#"{"output": "owner:\nAnn\ncontact:\njane@acme.io\tAnn", "exit_code": 0}"#;
    let resp = h
        .chat(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "who owns it?"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": {"name": "terminal", "arguments": "{\"cmd\":\"cat OWNERS\"}"}
                }]},
                {"role": "tool", "tool_call_id": "c1", "content": tool_output}
            ]
        }))
        .await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    let reply: Value = resp.json().await.unwrap();
    assert_eq!(
        reply["choices"][0]["message"]["content"], "Ann uses jane@acme.io.",
        "restored values must be exactly what was sent"
    );

    let sent = h.forwarded().await;
    let content = sent[0]["messages"][2]["content"].as_str().unwrap();
    assert!(
        !content.contains("Ann"),
        "whole-word term leaked: {content}"
    );
    assert!(!content.contains("jane@acme.io"), "email leaked: {content}");
    assert_eq!(
        content,
        r#"{"output": "owner:\n<<PERSON_1>>\ncontact:\n<<EMAIL_1>>\t<<PERSON_1>>", "exit_code": 0}"#
    );
    serde_json::from_str::<Value>(content).expect("the redacted tool result is still valid JSON");
}

/// A non-ASCII taught term inside JSON text written with `ensure_ascii=True`.
#[tokio::test]
async fn a_unicode_escaped_taught_term_is_redacted() {
    let h = Harness::new(taught(&[("José Ortega", "PERSON")])).await;
    let resp = h
        .chat(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "José Ortega asked"},
                {"role": "tool", "tool_call_id": "c1",
                 // `\u00e9`, built at runtime so no tool or editor can "helpfully" decode it.
                 "content": format!(r#"{{"author": "Jos{}u00e9 Ortega"}}"#, '\\')}
            ]
        }))
        .await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    let sent = h.forwarded().await;
    assert_eq!(sent[0]["messages"][0]["content"], "<<PERSON_1>> asked");
    assert_eq!(
        sent[0]["messages"][1]["content"], r#"{"author": "<<PERSON_1>>"}"#,
        "same person, same placeholder"
    );
}

/// Tool-call arguments are JSON text, so a restored value is inserted JSON-escaped.
#[tokio::test]
async fn restored_tool_call_arguments_are_valid_json() {
    let h = Harness::new(taught(&[(r#"Dana "DJ" Smith"#, "PERSON")])).await;
    h.reply_with(chat_reply(json!({
        "role": "assistant", "content": null, "tool_calls": [{
            "id": "c1", "type": "function",
            "function": {"name": "write", "arguments": "{\"text\":\"Hi <<PERSON_1>>\"}"}
        }]
    })))
    .await;
    let resp = h
        .chat(json!({
            "model": "m",
            "messages": [{"role": "user", "content": r#"Write to Dana "DJ" Smith"#}]
        }))
        .await;
    assert!(resp.status().is_success(), "{}", resp.text().await.unwrap());
    let reply: Value = resp.json().await.unwrap();
    let args = reply["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    let parsed: Value = serde_json::from_str(args).expect("arguments must stay valid JSON");
    assert_eq!(parsed["text"], r#"Hi Dana "DJ" Smith"#);
}

fn sse(events: &[Value]) -> String {
    let mut out: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    out
}

/// Accumulate `tool_calls[].function.arguments` by index from an OpenAI SSE body.
fn tool_args_by_index(body: &str) -> std::collections::BTreeMap<u64, String> {
    let mut args = std::collections::BTreeMap::new();
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        for choice in v["choices"].as_array().into_iter().flatten() {
            for call in choice["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let idx = call["index"].as_u64().unwrap();
                if let Some(a) = call["function"]["arguments"].as_str() {
                    args.entry(idx).or_insert_with(String::new).push_str(a);
                }
            }
        }
    }
    args
}

/// Parallel tool calls stream interleaved. A placeholder split across two events of
/// one call must not be stitched into the other call.
#[tokio::test]
async fn parallel_streamed_tool_calls_keep_their_own_placeholders() {
    let h = Harness::new(Store::default()).await;
    let call = |index: u64, args: &str| {
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": index, "function": {"arguments": args}}
        ]}}]})
    };
    h.reply_with(Reply::Sse(sse(&[
        call(0, "{\"to\":\"<<EMA"),
        call(1, "{\"to\":\"<<EMAIL_1>>\"}"),
        call(0, "IL_1>>\"}"),
        call(2, "{\"note\":\"a <<b"),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ])))
    .await;
    let resp = h
        .chat(json!({
            "model": "m", "stream": true,
            "messages": [{"role": "user", "content": "mail jane@acme.io twice"}]
        }))
        .await;
    assert!(resp.status().is_success());
    let args = tool_args_by_index(&resp.text().await.unwrap());
    assert_eq!(args[&0], r#"{"to":"jane@acme.io"}"#);
    assert_eq!(args[&1], r#"{"to":"jane@acme.io"}"#);
    assert_eq!(
        args[&2], r#"{"note":"a <<b"#,
        "held text goes back to its own call"
    );
}

/// Anthropic: held text is flushed into its own block, before that block's
/// `content_block_stop`, and `input_json_delta` is restored JSON-escaped.
#[tokio::test]
async fn anthropic_stream_flushes_each_block_before_it_stops() {
    let h = Harness::new(taught(&[(r#"Dana "DJ" Smith"#, "PERSON")])).await;
    let ev = |v: Value| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap());
    let body: String = [
        ev(json!({"type": "message_start", "message": {"id": "m", "type": "message",
            "role": "assistant", "content": [], "model": "c", "usage": {"input_tokens": 1, "output_tokens": 0}}})),
        ev(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})),
        ev(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "see <<x"}})),
        ev(json!({"type": "content_block_stop", "index": 0})),
        ev(json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t", "name": "w", "input": {}}})),
        ev(json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"to\":\"<<PERS"}})),
        ev(json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "ON_1>>\"}"}})),
        ev(json!({"type": "content_block_stop", "index": 1})),
        ev(json!({"type": "message_stop"})),
    ]
    .concat();
    h.reply_with(Reply::Sse(body)).await;
    let resp = h
        .messages(json!({
            "model": "c", "max_tokens": 5, "stream": true,
            "messages": [{"role": "user", "content": r#"to Dana "DJ" Smith"#}]
        }))
        .await;
    assert!(resp.status().is_success());
    let text = resp.text().await.unwrap();

    let events: Vec<Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|p| serde_json::from_str(p).ok())
        .collect();
    let (mut block0, mut json1, mut stopped0) = (String::new(), String::new(), false);
    for e in &events {
        match (e["type"].as_str().unwrap(), e["index"].as_u64()) {
            ("content_block_delta", Some(0)) => {
                assert!(!stopped0, "delta for block 0 after its stop: {text}");
                block0.push_str(e["delta"]["text"].as_str().unwrap());
            }
            ("content_block_delta", Some(1)) => {
                json1.push_str(e["delta"]["partial_json"].as_str().unwrap())
            }
            ("content_block_stop", Some(0)) => stopped0 = true,
            _ => {}
        }
    }
    assert_eq!(block0, "see <<x");
    let input: Value = serde_json::from_str(&json1).expect("tool input stays valid JSON");
    assert_eq!(input["to"], r#"Dana "DJ" Smith"#);
}
