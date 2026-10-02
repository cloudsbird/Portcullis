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
    /// Remove only from this scope. Omitted: remove from every scope.
    #[serde(default)]
    scope: Option<String>,
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
    let result = check_admin(state, headers);
    if matches!(result, Err((StatusCode::UNAUTHORIZED, _))) {
        state.metrics.blocked("unauthorized_admin");
    }
    result
}

fn check_admin(state: &ProxyState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
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

/// Apply `op` to the gateway and persist the result.
///
/// * **Read-modify-write against the file.** If the store file exists it is re-read
///   first and becomes the starting point, so a term added out of band (`portcullis
///   teach` while serving) is not silently overwritten by this write.
/// * **Disk work off the gateway lock.** Reading and saving can run Argon2 (64 MiB) when
///   encryption is on; doing that under the gateway lock would stall every request, so
///   it runs on the blocking pool and the lock is held only for the in-memory change.
/// * Writers are serialized by `store_write_lock`, so two teaches cannot interleave.
///
/// An unreadable store file (wrong key, corruption) fails the request instead of being
/// overwritten.
async fn mutate_store<T>(
    state: &ProxyState,
    op: impl FnOnce(&mut crate::Gateway) -> T,
) -> Result<T, (StatusCode, String)> {
    let internal = |e: String| (StatusCode::INTERNAL_SERVER_ERROR, e);
    let _writer = state.store_write_lock.lock().await;
    let path = store_path(state);

    let on_disk = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            if std::path::Path::new(&path).exists() {
                crate::Store::load(&path).map(Some)
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| internal(e.to_string()))?
    };

    let (result, snapshot) = {
        let mut gw = state.gateway.lock().await;
        if let Some(store) = on_disk {
            gw.replace_store(store);
        }
        let result = op(&mut gw);
        (result, gw.store.clone())
    };

    tokio::task::spawn_blocking(move || snapshot.save(&path))
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(|e| internal(e.to_string()))?;
    Ok(result)
}

pub(super) async fn teach_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(req): Json<TeachRequest>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    // With scopes enabled, an unknown scope would protect nobody and never say so.
    if !state.scope_tokens.is_empty()
        && !req.scope.eq_ignore_ascii_case(crate::GLOBAL_SCOPE)
        && !state
            .scope_tokens
            .iter()
            .any(|(s, _)| s.eq_ignore_ascii_case(&req.scope))
    {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "unknown scope '{}': not 'global' and not configured",
                req.scope
            ),
        )
            .into_response();
    }

    let (term, label, scope, whole_word) = (
        req.term.clone(),
        req.label.clone(),
        req.scope.clone(),
        req.whole_word,
    );
    let total = match mutate_store(&state, move |gw| {
        gw.teach_with(&term, &label, &scope, whole_word);
        gw.store.deny.len()
    })
    .await
    {
        Ok(total) => total,
        Err(e) => return e.into_response(),
    };
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

    let (term, scope) = (req.term.clone(), req.scope.clone());
    let removed = match mutate_store(&state, move |gw| gw.unteach_in(&term, scope.as_deref())).await
    {
        Ok(removed) => removed,
        Err(e) => return e.into_response(),
    };
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

/// `GET /suggestions[?scope=NAME]`: all candidates, or only those seen in one scope's
/// traffic.
pub(super) async fn suggestions_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(e) = require_admin(&state, &headers) {
        return e.into_response();
    }

    let gw = state.gateway.lock().await;
    let suggestions = match query.get("scope") {
        Some(scope) => gw.suggestions_for(Some(scope)),
        None => gw.suggestions(),
    };
    Json(json!({ "suggestions": suggestions })).into_response()
}
