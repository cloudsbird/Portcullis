//! M5: per-client scope isolation, end to end through the proxy.

use std::sync::Arc;

use axum::{extract::State, routing::post, Json, Router};
use portcullis::proxy::{parse_scope_tokens, ProxyState};
use portcullis::{Gateway, Store};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[derive(Clone)]
struct MockUpstream {
    bodies: Arc<Mutex<Vec<String>>>,
}

async fn mock_handler(State(state): State<MockUpstream>, Json(body): Json<Value>) -> Json<Value> {
    state.bodies.lock().await.push(body.to_string());
    Json(json!({
        "id": "x", "object": "chat.completion", "created": 0, "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]
    }))
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    format!("http://{addr}")
}

struct Harness {
    proxy: String,
    bodies: Arc<Mutex<Vec<String>>>,
    client: Client,
}

/// Acme belongs to tenant `alpha`, Borealis to `beta`, Cartalian to everyone.
async fn harness() -> Harness {
    let mut store = Store::default();
    store.teach("Acme", "ORG", "alpha");
    store.teach("Borealis", "ORG", "beta");
    store.teach("Cartalian", "ORG", "global");

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let upstream = spawn(
        Router::new()
            .route("/v1/chat/completions", post(mock_handler))
            .with_state(MockUpstream {
                bodies: bodies.clone(),
            }),
    )
    .await;

    let client = Client::new();
    let mut state = ProxyState::new(
        Gateway::new(store),
        client.clone(),
        format!("{upstream}/v1/chat/completions"),
        "key".into(),
    );
    state.scope_tokens = parse_scope_tokens("alpha=tok-alpha,beta=tok-beta").unwrap();
    state.admin_token = Some("admin-secret".into());
    state.store_path = Some(
        std::env::temp_dir()
            .join(format!("portcullis-scopes-{}.json", std::process::id()))
            .to_string_lossy()
            .into_owned(),
    );
    let proxy = spawn(portcullis::proxy::app(state)).await;
    Harness {
        proxy,
        bodies,
        client,
    }
}

impl Harness {
    async fn ask(&self, token: Option<&str>, text: &str) -> StatusCode {
        let mut req = self
            .client
            .post(format!("{}/v1/chat/completions", self.proxy))
            .json(&json!({"model": "m", "messages": [{"role": "user", "content": text}]}));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.unwrap().status()
    }

    async fn last_upstream_body(&self) -> String {
        self.bodies.lock().await.last().cloned().unwrap_or_default()
    }
}

const TEXT: &str = "Acme, Borealis and Cartalian walk into a bar.";

#[tokio::test]
async fn requests_without_a_valid_token_are_rejected_before_anything_is_forwarded() {
    let h = harness().await;
    assert_eq!(h.ask(None, TEXT).await, StatusCode::UNAUTHORIZED);
    assert_eq!(h.ask(Some("nope"), TEXT).await, StatusCode::UNAUTHORIZED);
    assert!(
        h.bodies.lock().await.is_empty(),
        "nothing may reach upstream"
    );
}

#[tokio::test]
async fn each_tenant_sees_only_global_terms_and_its_own() {
    let h = harness().await;

    assert_eq!(h.ask(Some("tok-alpha"), TEXT).await, StatusCode::OK);
    let a = h.last_upstream_body().await;
    assert!(!a.contains("Acme"), "alpha's own term leaked: {a}");
    assert!(!a.contains("Cartalian"), "global term leaked: {a}");
    assert!(
        a.contains("Borealis"),
        "beta's term must not apply to alpha: {a}"
    );

    assert_eq!(h.ask(Some("tok-beta"), TEXT).await, StatusCode::OK);
    let b = h.last_upstream_body().await;
    assert!(!b.contains("Borealis"), "beta's own term leaked: {b}");
    assert!(!b.contains("Cartalian"), "global term leaked: {b}");
    assert!(
        b.contains("Acme"),
        "alpha's term must not apply to beta: {b}"
    );
}

/// The delta cache is keyed by scope: identical text must not reuse another
/// tenant's redaction.
#[tokio::test]
async fn the_delta_cache_does_not_cross_tenants() {
    let h = harness().await;
    assert_eq!(h.ask(Some("tok-alpha"), TEXT).await, StatusCode::OK);
    assert_eq!(h.ask(Some("tok-beta"), TEXT).await, StatusCode::OK);
    let b = h.last_upstream_body().await;
    assert!(
        b.contains("Acme"),
        "beta was served alpha's cached redaction: {b}"
    );
    // And back again: alpha still gets its own.
    assert_eq!(h.ask(Some("tok-alpha"), TEXT).await, StatusCode::OK);
    assert!(!h.last_upstream_body().await.contains("Acme"));
}

#[tokio::test]
async fn x_api_key_is_accepted_as_the_scope_token() {
    let h = harness().await;
    let status = h
        .client
        .post(format!("{}/v1/chat/completions", h.proxy))
        .header("x-api-key", "tok-alpha")
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": TEXT}]}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::OK);
    assert!(!h.last_upstream_body().await.contains("Acme"));
}

#[tokio::test]
async fn teaching_an_unknown_scope_is_refused_and_a_known_one_takes_effect() {
    let h = harness().await;
    let teach = |scope: &'static str| {
        let req = h
            .client
            .post(format!("{}/teach", h.proxy))
            .bearer_auth("admin-secret")
            .json(&json!({"term": "Zephyr", "label": "ORG", "scope": scope}));
        async move { req.send().await.unwrap().status() }
    };
    assert_eq!(teach("gamma").await, StatusCode::BAD_REQUEST);
    assert_eq!(teach("alpha").await, StatusCode::OK);

    assert_eq!(
        h.ask(Some("tok-alpha"), "Zephyr here").await,
        StatusCode::OK
    );
    assert!(!h.last_upstream_body().await.contains("Zephyr"));
    assert_eq!(h.ask(Some("tok-beta"), "Zephyr here").await, StatusCode::OK);
    assert!(h.last_upstream_body().await.contains("Zephyr"));
}

#[test]
fn scope_token_config_is_strict() {
    assert_eq!(
        parse_scope_tokens("a=1, b=2").unwrap(),
        vec![("a".into(), "1".into()), ("b".into(), "2".into())]
    );
    assert!(parse_scope_tokens("").unwrap().is_empty());
    assert!(parse_scope_tokens("a=1,a=2").is_err(), "duplicate scope");
    assert!(parse_scope_tokens("a=1,b=1").is_err(), "shared token");
    assert!(parse_scope_tokens("global=1").is_err(), "reserved name");
    assert!(parse_scope_tokens("a=").is_err(), "empty token");
    let err = parse_scope_tokens("a:supersecret").unwrap_err();
    assert!(
        !err.contains("supersecret"),
        "errors must not echo tokens: {err}"
    );
}
