//! M7: the Prometheus endpoint, end to end.

use axum::{routing::post, Json, Router};
use portcullis::proxy::{parse_scope_tokens, ProxyState};
use portcullis::{Gateway, Store};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

async fn ok_handler(Json(_): Json<Value>) -> Json<Value> {
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

async fn proxy(configure: impl FnOnce(&mut ProxyState)) -> (String, Client) {
    let mut store = Store::default();
    store.teach("Cartalian", "ORG", "global");
    let upstream = spawn(Router::new().route("/v1/chat/completions", post(ok_handler))).await;
    let client = Client::new();
    let mut state = ProxyState::new(
        Gateway::new(store),
        client.clone(),
        format!("{upstream}/v1/chat/completions"),
        "key".into(),
    );
    configure(&mut state);
    (spawn(portcullis::proxy::app(state)).await, client)
}

async fn chat(client: &Client, base: &str, token: Option<&str>, text: &str) -> StatusCode {
    let mut req = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": text}]}));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap().status()
}

async fn scrape(client: &Client, base: &str) -> String {
    client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

#[tokio::test]
async fn counts_requests_redactions_cache_and_upstream_without_leaking_values() {
    let (base, client) = proxy(|_| {}).await;
    let text = "Cartalian wrote to dana.whitfield@example.com";
    assert_eq!(chat(&client, &base, None, text).await, StatusCode::OK);
    assert_eq!(chat(&client, &base, None, text).await, StatusCode::OK); // cache hit
    client
        .get(format!("{base}/no-such-route-abc123"))
        .send()
        .await
        .unwrap();

    let m = scrape(&client, &base).await;
    assert!(
        m.contains("portcullis_http_requests_total{route=\"openai\",status=\"200\"} 2"),
        "{m}"
    );
    assert!(
        m.contains("portcullis_redactions_total{label=\"ORG\"} 2"),
        "{m}"
    );
    assert!(
        m.contains("portcullis_redactions_total{label=\"EMAIL\"} 2"),
        "{m}"
    );
    assert!(
        m.contains("portcullis_delta_cache_total{result=\"hit\"} 1"),
        "{m}"
    );
    assert!(
        m.contains("portcullis_delta_cache_total{result=\"miss\"} 1"),
        "{m}"
    );
    assert!(
        m.contains("portcullis_upstream_responses_total{outcome=\"200\"} 2"),
        "{m}"
    );
    assert!(
        m.contains("portcullis_scan_duration_seconds_count 2"),
        "{m}"
    );
    assert!(m.contains("portcullis_store_terms 1"), "{m}");
    // A random path is folded into one series, not minted as a new label value.
    assert!(m.contains("route=\"other\""), "{m}");
    assert!(
        !m.contains("abc123"),
        "paths must not become label values: {m}"
    );

    // The whole point: no value, however it got in, is ever exposed.
    for secret in ["Cartalian", "dana.whitfield", "example.com"] {
        assert!(!m.contains(secret), "metrics leaked {secret}: {m}");
    }
}

#[tokio::test]
async fn blocked_requests_are_counted_by_reason_and_scope() {
    let (base, client) = proxy(|s| {
        s.scope_tokens = parse_scope_tokens("alpha=tok-alpha").unwrap();
    })
    .await;
    assert_eq!(
        chat(&client, &base, None, "hi").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        chat(&client, &base, Some("tok-alpha"), "hi").await,
        StatusCode::OK
    );
    let m = scrape(&client, &base).await;
    assert!(
        m.contains("portcullis_blocked_total{reason=\"unauthorized_scope\"} 1"),
        "{m}"
    );
    assert!(
        m.contains("portcullis_scope_requests_total{scope=\"alpha\"} 1"),
        "{m}"
    );
}

#[tokio::test]
async fn the_endpoint_can_require_a_bearer_token() {
    let (base, client) = proxy(|s| s.metrics_token = Some("scrape-me".into())).await;
    let status = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let resp = client
        .get(format!("{base}/metrics"))
        .bearer_auth("scrape-me")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/plain; version=0.0.4"));
}
