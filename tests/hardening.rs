//! Production-hardening tests: health, body limits, upstream timeouts, and the
//! permissions on the file that holds everything you consider private.

use std::time::Duration;

use axum::{extract::State, routing::post, Json, Router};
use portcullis::proxy::{app, ProxyState};
use portcullis::{Gateway, Store};
use reqwest::Client;
use serde_json::{json, Value};

/// Start `app` on an ephemeral port and return its base URL.
async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("http://{addr}")
}

fn proxy_state(request_timeout: Duration, upstream_url: String) -> ProxyState {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let gw = Gateway::new(store);
    let mut state = ProxyState::new(gw, Client::new(), upstream_url, "test-key".into());
    state.request_timeout = request_timeout;
    state
}

#[derive(Clone)]
struct Slow {
    delay_ms: u64,
}

async fn slow_handler(State(state): State<Slow>, Json(_body): Json<Value>) -> Json<Value> {
    tokio::time::sleep(Duration::from_millis(state.delay_ms)).await;
    Json(json!({ "choices": [] }))
}

// ---------------------------------------------------------------------------

/// `/healthz` answers, and reveals counts only — never a stored term.
#[tokio::test]
async fn healthz_is_available_and_leaks_nothing() {
    let state = proxy_state(
        Duration::from_secs(5),
        "http://127.0.0.1:1/v1/chat/completions".into(),
    );
    let base = spawn(app(state)).await;

    let resp = reqwest::get(format!("{base}/healthz")).await.unwrap();
    assert_eq!(resp.status(), 200, "healthz should answer 200");

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["store_terms"], 1);
    assert!(body["version"].is_string());
    assert!(body["uptime_seconds"].is_number());

    let raw = body.to_string();
    assert!(
        !raw.contains("Cartalian"),
        "healthz leaked a stored term: {raw}"
    );
}

/// A liveness probe must not queue behind a long scan: hold the gateway lock (as a slow
/// detection does) and `/healthz` must still answer promptly.
#[tokio::test]
async fn healthz_does_not_wait_for_the_gateway_lock() {
    let state = proxy_state(
        Duration::from_secs(5),
        "http://127.0.0.1:1/v1/chat/completions".into(),
    );
    let gateway = state.gateway.clone();
    let base = spawn(app(state)).await;

    let _held = gateway.lock().await;
    let resp = tokio::time::timeout(
        Duration::from_secs(2),
        reqwest::get(format!("{base}/healthz")),
    )
    .await
    .expect("/healthz blocked behind the gateway lock")
    .unwrap();
    assert_eq!(resp.status(), 200);
}

/// An oversized request body is refused before it can reach the detector.
#[tokio::test]
async fn oversized_body_is_rejected() {
    let state = proxy_state(
        Duration::from_secs(5),
        "http://127.0.0.1:1/v1/chat/completions".into(),
    );
    let base = spawn(app(state)).await;

    // Default cap is 2 MiB; send well past it.
    let huge = "x".repeat(4 * 1024 * 1024);
    let resp = Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "messages": [{ "role": "user", "content": huge }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        413,
        "an oversized body should be rejected with 413"
    );
}

/// A stalled upstream is cut off by the request timeout rather than hanging forever.
#[tokio::test]
async fn stalled_upstream_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let router = Router::new()
            .route("/v1/chat/completions", post(slow_handler))
            .with_state(Slow { delay_ms: 5_000 });
        axum::serve(listener, router).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The proxy gives up after 300ms; the mock would take 5s.
    let state = proxy_state(
        Duration::from_millis(300),
        format!("http://{addr}/v1/chat/completions"),
    );
    let base = spawn(app(state)).await;

    let resp = Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({ "messages": [{ "role": "user", "content": "hello" }] }))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        502,
        "a stalled upstream should surface as 502"
    );
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("timeout"),
        "expected a timeout message, got: {text}"
    );
}

/// The store holds everything you consider private, so it must not be world-readable.
#[cfg(unix)]
#[test]
fn store_file_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.json");

    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    store.save(&path).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "store should be 0600, got {mode:o}");
}

/// ...and an already-loose file is tightened on the next save.
#[cfg(unix)]
#[test]
fn save_tightens_a_pre_existing_loose_file() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.json");

    std::fs::write(&path, "{}").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    Store::default().save(&path).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "an existing 0644 store should be tightened");
}

/// A taught term containing a character JSON escapes (`"`) never appears verbatim in the
/// serialized body, so the fail-closed check must also look at the decoded strings. Here
/// the term rides in a field the redaction walker does not collect (`metadata`).
#[tokio::test]
async fn fail_closed_catches_terms_that_json_escapes() {
    use std::sync::Arc;

    let mut store = Store::default();
    store.teach("the \"Falcon\" project", "ORG", "global");
    let hits = Arc::new(tokio::sync::Mutex::new(0u32));
    let counter = hits.clone();
    let upstream = spawn(axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let counter = counter.clone();
            async move {
                *counter.lock().await += 1;
                axum::Json(serde_json::json!({"choices": []}))
            }
        }),
    ))
    .await;
    let mut state = proxy_state(
        Duration::from_secs(5),
        format!("{upstream}/v1/chat/completions"),
    );
    state.gateway = Arc::new(tokio::sync::Mutex::new(portcullis::Gateway::new(store)));
    let base = spawn(app(state)).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hello"}],
            "metadata": {"note": "about the \"Falcon\" project"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        502,
        "a term in an unwalked field must fail closed"
    );
    assert_eq!(*hits.lock().await, 0, "nothing may reach the upstream");
}
