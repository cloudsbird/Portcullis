//! M2 — in-chat teach/unteach learning surface and auto-suggest.
//!
//! These tests prove the HTTP learning endpoints invalidate the delta cache
//! (invariant 3) and are properly authenticated.

use std::sync::atomic::{AtomicU64, Ordering};
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

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_store_path() -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let path = format!("/tmp/portcullis-m2-test-{}-{}.json", pid, n);
    let _ = std::fs::remove_file(&path);
    path
}

fn base_url(proxy_url: &str) -> &str {
    proxy_url.strip_suffix("/v1/chat/completions").unwrap()
}

/// Teaching over HTTP must go through `Gateway::teach`, which invalidates the
/// delta cache. A message that passed through unredacted before teaching must
/// be redacted afterwards.
#[tokio::test]
async fn teach_over_http_invalidates_delta_cache() {
    let term = "Aerolith";
    let label = "ORG";
    let store_path = temp_store_path();

    let gw = Gateway::new(Store::default());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();

    let mut state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    state.store_path = Some(store_path.clone());
    state.admin_token = Some("test-token".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;
    let base = base_url(&proxy_url);

    let chat_body = json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": format!("{} is the client.", term)}]
    });

    // First request: term is not taught, so it should pass through unchanged.
    let resp1 = client
        .post(&proxy_url)
        .json(&chat_body)
        .send()
        .await
        .unwrap();
    assert!(resp1.status().is_success(), "first chat failed: {:?}", resp1.text().await);

    {
        let recorded = bodies.lock().await;
        assert_eq!(recorded.len(), 1, "upstream should have received the first request");
        assert!(
            recorded[0].contains(term),
            "first request should contain the raw, untaught term: {}",
            recorded[0]
        );
    }

    // Teach the term via the HTTP surface.
    let teach_resp = client
        .post(format!("{}/teach", base))
        .bearer_auth("test-token")
        .json(&json!({"term": term, "label": label, "scope": "global"}))
        .send()
        .await
        .unwrap();
    assert!(
        teach_resp.status().is_success(),
        "teach failed: {:?}",
        teach_resp.text().await
    );

    // Resend the exact same message. The cache must have been invalidated, so
    // the now-taught term is redacted.
    let resp2 = client
        .post(&proxy_url)
        .json(&chat_body)
        .send()
        .await
        .unwrap();
    assert!(resp2.status().is_success(), "second chat failed: {:?}", resp2.text().await);

    {
        let recorded = bodies.lock().await;
        assert_eq!(recorded.len(), 2, "upstream should have received the second request");
        assert!(
            !recorded[1].contains(term),
            "second request leaked the now-taught term: {}",
            recorded[1]
        );
    }

    let _ = std::fs::remove_file(&store_path);
}

/// The learning endpoints require a bearer token and reject missing/wrong tokens.
#[tokio::test]
async fn teach_requires_bearer_token() {
    let gw = Gateway::new(Store::default());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();

    let mut state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    state.admin_token = Some("secret-token".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;
    let base = base_url(&proxy_url);

    let body = json!({"term": "Aerolith", "label": "ORG"});

    let no_token = client
        .post(format!("{}/teach", base))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(no_token.status(), 401, "missing token should be rejected");

    let wrong_token = client
        .post(format!("{}/teach", base))
        .bearer_auth("wrong-token")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_token.status(), 401, "wrong token should be rejected");
}

/// Auto-suggest surfaces terms that the non-dictionary detectors found but the
/// store does not yet know.
#[tokio::test]
async fn suggestions_returns_untaught_detected_term() {
    let email = "suggestions-test@example.com";
    let gw = Gateway::new(Store::default());
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();

    let mut state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    state.admin_token = Some("secret-token".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;
    let base = base_url(&proxy_url);

    // The regex email detector will redact this even though it is not taught,
    // so it becomes a suggestion candidate. Keep the sentence open after the
    // email so the regex does not greedily include trailing punctuation.
    let chat_resp = client
        .post(&proxy_url)
        .json(&json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": format!("My email is {}", email)}]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        chat_resp.status().is_success(),
        "chat failed: {:?}",
        chat_resp.text().await
    );

    let sug_resp = client
        .get(format!("{}/suggestions", base))
        .bearer_auth("secret-token")
        .send()
        .await
        .unwrap();
    assert!(
        sug_resp.status().is_success(),
        "suggestions failed: {:?}",
        sug_resp.text().await
    );

    let json: Value = sug_resp.json().await.unwrap();
    let suggestions = json["suggestions"]
        .as_array()
        .expect("suggestions should be an array");
    assert!(
        suggestions.iter().any(|v| v.as_str() == Some(email)),
        "expected suggestion {} in {:?}",
        email,
        suggestions
    );
}

/// POST /unteach removes a taught term.
#[tokio::test]
async fn unteach_removes_term() {
    let store_path = temp_store_path();
    let mut store = Store::default();
    store.teach("Aerolith", "ORG", "global");
    let gw = Gateway::new(store);
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream_url = spawn_server(mock_app(MockUpstream { bodies: bodies.clone() })).await;
    let client = Client::new();

    let mut state = ProxyState::new(gw, client.clone(), upstream_url, "key".into());
    state.store_path = Some(store_path.clone());
    state.admin_token = Some("secret-token".into());
    let proxy_url = spawn_server(portcullis::proxy::app(state)).await;
    let base = base_url(&proxy_url);

    let terms_resp = client
        .get(format!("{}/terms", base))
        .bearer_auth("secret-token")
        .send()
        .await
        .unwrap();
    assert!(terms_resp.status().is_success());
    let terms_json: Value = terms_resp.json().await.unwrap();
    let terms = terms_json["terms"].as_array().unwrap();
    assert!(terms.iter().any(|e| e["term"] == "Aerolith"));

    let unteach_resp = client
        .post(format!("{}/unteach", base))
        .bearer_auth("secret-token")
        .json(&json!({"term": "Aerolith"}))
        .send()
        .await
        .unwrap();
    assert!(unteach_resp.status().is_success());
    let unteach_json: Value = unteach_resp.json().await.unwrap();
    assert_eq!(unteach_json["ok"], true);
    assert_eq!(unteach_json["removed"], true);

    let terms_resp2 = client
        .get(format!("{}/terms", base))
        .bearer_auth("secret-token")
        .send()
        .await
        .unwrap();
    let terms_json2: Value = terms_resp2.json().await.unwrap();
    let terms2 = terms_json2["terms"].as_array().unwrap();
    assert!(terms2.iter().all(|e| e["term"] != "Aerolith"));

    let _ = std::fs::remove_file(&store_path);
}
