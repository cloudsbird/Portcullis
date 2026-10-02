//! In-chat learning surface: `/teach`, `/unteach`, `/terms`, `/suggestions`.

use super::*;

// ---------------------------------------------------------------------------
// In-chat learning surface (M2)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct TeachRequest {
    term: String,
    label: String,
    #[serde(default = "default_scope")]
    scope: String,
    /// Match only at word boundaries.
    #[serde(default)]
    whole_word: bool,
}

#[derive(Deserialize)]
pub(super) struct UnteachRequest {
    term: String,
}

pub(super) fn default_scope() -> String {
    "global".into()
}

/// Resolve the effective store path for persistence.
pub(super) fn store_path(state: &ProxyState) -> String {
    state.store_path.clone().unwrap_or_else(|| {
        std::env::var("PORTCULLIS_STORE").unwrap_or_else(|_| "store.json".into())
    })
}

/// Constant-time equality check for bearer tokens.
pub(super) fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = 0u8;
    for i in 0..max {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0 && a.len() == b.len()
}

/// Enforce `Authorization: Bearer <PORTCULLIS_ADMIN_TOKEN>`.
///
/// Returns 503 if no token is configured, 401 on mismatch or missing header.
pub(super) fn require_admin(
    state: &ProxyState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, String)> {
    let token: String = match state.admin_token.as_deref() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => match std::env::var("PORTCULLIS_ADMIN_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "admin token not configured".into(),
                ))
            }
        },
    };

    let header = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
    let provided = match header {
        Some(h) if h.starts_with("Bearer ") => &h["Bearer ".len()..],
        _ => {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing or malformed bearer token".into(),
            ))
        }
    };

    if !constant_time_eq(provided, &token) {
        return Err((StatusCode::UNAUTHORIZED, "invalid bearer token".into()));
    }
    Ok(())
}

pub(super) async fn teach_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(req): Json<TeachRequest>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let path = store_path(&state);
    let mut gw = state.gateway.lock().await;
    gw.teach_with(&req.term, &req.label, &req.scope, req.whole_word);
    if let Err(e) = gw.store.save(&path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let total = gw.store.deny.len();
    Json(json!({ "ok": true, "taught": req.term, "total": total })).into_response()
}

pub(super) async fn unteach_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(req): Json<UnteachRequest>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let path = store_path(&state);
    let mut gw = state.gateway.lock().await;
    let removed = gw.unteach(&req.term);
    if let Err(e) = gw.store.save(&path) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    Json(json!({ "ok": true, "removed": removed })).into_response()
}

pub(super) async fn terms_handler(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let gw = state.gateway.lock().await;
    let terms: Vec<Value> = gw
        .store
        .deny
        .iter()
        .map(|e| json!({"term": e.term, "label": e.label, "scope": e.scope}))
        .collect();
    Json(json!({ "terms": terms })).into_response()
}

pub(super) async fn suggestions_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let gw = state.gateway.lock().await;
    let suggestions = gw.suggestions();
    Json(json!({ "suggestions": suggestions })).into_response()
}
