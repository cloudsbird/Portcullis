//! Anthropic-compatible `/v1/messages`: request redaction, response and SSE
//! rehydration.

use super::stream::*;
use super::*;

// ---------------------------------------------------------------------------
// Anthropic Messages API redaction helpers
// ---------------------------------------------------------------------------

/// Collect every redactable string from an Anthropic request body.
pub(super) fn collect_anthropic_texts(body: &mut Value, out: &mut Vec<String>) {
    if let Some(system) = body.get_mut("system") {
        collect_anthropic_system(system, out);
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            collect_anthropic_message(msg, out);
        }
    }
}

pub(super) fn collect_anthropic_system(system: &mut Value, out: &mut Vec<String>) {
    match system {
        Value::String(s) => {
            out.push(std::mem::take(s));
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                let Value::Object(map) = block else { continue };
                if map.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(Value::String(s)) = map.get_mut("text") {
                        out.push(std::mem::take(s));
                    }
                }
            }
        }
        _ => {}
    }
}

pub(super) fn collect_anthropic_message(msg: &mut Value, out: &mut Vec<String>) {
    let Value::Object(map) = msg else { return };
    if let Some(content) = map.get_mut("content") {
        collect_anthropic_content(content, out);
    }
}

pub(super) fn collect_anthropic_content(content: &mut Value, out: &mut Vec<String>) {
    match content {
        Value::String(s) => {
            out.push(std::mem::take(s));
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                let Value::Object(map) = block else { continue };
                match map.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(Value::String(s)) = map.get_mut("text") {
                            out.push(std::mem::take(s));
                        }
                    }
                    Some("tool_result") => {
                        if let Some(content) = map.get_mut("content") {
                            collect_anthropic_content(content, out);
                        }
                    }
                    Some("tool_use") => {
                        if let Some(input) = map.get_mut("input") {
                            collect_string_leaves(input, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

pub(super) fn collect_string_leaves(value: &mut Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            out.push(std::mem::take(s));
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                collect_string_leaves(item, out);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                collect_string_leaves(v, out);
            }
        }
        _ => {}
    }
}

/// Place redacted strings back into an Anthropic request body.
pub(super) fn replace_anthropic_texts(body: &mut Value, redacted: &[String], idx: &mut usize) {
    if let Some(system) = body.get_mut("system") {
        replace_anthropic_system(system, redacted, idx);
    }
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            replace_anthropic_message(msg, redacted, idx);
        }
    }
}

pub(super) fn replace_anthropic_system(system: &mut Value, redacted: &[String], idx: &mut usize) {
    match system {
        Value::String(_) => {
            *system = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                let Value::Object(map) = block else { continue };
                if map.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(Value::String(_)) = map.get_mut("text") {
                        map["text"] = Value::String(redacted[*idx].clone());
                        *idx += 1;
                    }
                }
            }
        }
        _ => {}
    }
}

pub(super) fn replace_anthropic_message(msg: &mut Value, redacted: &[String], idx: &mut usize) {
    let Value::Object(map) = msg else { return };
    if let Some(content) = map.get_mut("content") {
        replace_anthropic_content(content, redacted, idx);
    }
}

pub(super) fn replace_anthropic_content(content: &mut Value, redacted: &[String], idx: &mut usize) {
    match content {
        Value::String(_) => {
            *content = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                let Value::Object(map) = block else { continue };
                match map.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(Value::String(_)) = map.get_mut("text") {
                            map["text"] = Value::String(redacted[*idx].clone());
                            *idx += 1;
                        }
                    }
                    Some("tool_result") => {
                        if let Some(content) = map.get_mut("content") {
                            replace_anthropic_content(content, redacted, idx);
                        }
                    }
                    Some("tool_use") => {
                        if let Some(input) = map.get_mut("input") {
                            replace_string_leaves(input, redacted, idx);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

pub(super) fn replace_string_leaves(value: &mut Value, redacted: &[String], idx: &mut usize) {
    match value {
        Value::String(_) => {
            *value = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                replace_string_leaves(item, redacted, idx);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                replace_string_leaves(v, redacted, idx);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages API handler
// ---------------------------------------------------------------------------

pub async fn anthropic_messages(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match handle_anthropic(state, headers, body).await {
        Ok(resp) => resp,
        Err((status, msg)) => (status, msg).into_response(),
    }
}

pub(super) async fn handle_anthropic(
    state: ProxyState,
    headers: HeaderMap,
    body: Value,
) -> Result<Response, (StatusCode, String)> {
    // Collect every redactable string while preserving all other fields.
    let mut redacted_body = body.clone();
    let mut texts_to_redact: Vec<String> = Vec::new();
    collect_anthropic_texts(&mut redacted_body, &mut texts_to_redact);

    // Redact all text-bearing strings in one batch (better cache hit rate).
    // Detection is CPU-bound and synchronous, so it runs on the blocking pool
    // rather than stalling an async worker thread.
    let (redacted_texts, vault) = {
        let gateway = state.gateway.clone();
        let texts = texts_to_redact.clone();
        tokio::task::spawn_blocking(move || {
            let mut gw = gateway.blocking_lock();
            let mut vault = Vault::new();
            let out = gw.process_with(&mut vault, &texts);
            (out, vault)
        })
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("redaction task failed: {e}"),
            )
        })?
    };

    // Place redacted strings back into the request copy.
    {
        let mut idx = 0;
        replace_anthropic_texts(&mut redacted_body, &redacted_texts, &mut idx);
    }

    // Invariant 4: fail-closed outbound assertion over the ENTIRE assembled body.
    {
        let gw = state.gateway.lock().await;
        let assembled = serde_json::to_string(&redacted_body)
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        gw.assert_clean(&[assembled])
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    }

    let streaming = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Anthropic uses `x-api-key` and `anthropic-version`, not Bearer auth.
    let version = headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| state.anthropic_version.clone());

    let upstream_resp = send_upstream(&state, streaming, &headers, || {
        state
            .client
            .post(&state.anthropic_upstream_url)
            .header("x-api-key", &state.anthropic_upstream_key)
            .header("anthropic-version", &version)
            .header("content-type", "application/json")
            .json(&redacted_body)
    })
    .await?;

    if streaming {
        return anthropic_stream_response(Arc::new(vault), upstream_resp).await;
    }

    let status = upstream_resp.status();
    let mut upstream_json: Value = upstream_resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    rehydrate_anthropic_response(&mut upstream_json, &vault);

    Ok((status, Json(upstream_json)).into_response())
}

pub(super) fn rehydrate_anthropic_response(body: &mut Value, vault: &Vault) {
    let Some(content) = body.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for block in content.iter_mut() {
        let Value::Object(map) = block else { continue };
        match map.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(Value::String(s)) = map.get_mut("text") {
                    *s = vault.restore(s);
                }
            }
            Some("tool_use") => {
                if let Some(input) = map.get_mut("input") {
                    rehydrate_string_leaves(input, vault);
                }
            }
            _ => {}
        }
    }
}

pub(super) fn rehydrate_string_leaves(value: &mut Value, vault: &Vault) {
    match value {
        Value::String(s) => *s = vault.restore(s),
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                rehydrate_string_leaves(item, vault);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                rehydrate_string_leaves(v, vault);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages API streaming SSE path
// ---------------------------------------------------------------------------

/// Forward an Anthropic streaming upstream response, rehydrating text and
/// input_json deltas while reusing the placeholder-level carry buffer.
pub(super) async fn anthropic_stream_response(
    vault: Arc<Vault>,
    upstream_resp: reqwest::Response,
) -> Result<Response, (StatusCode, String)> {
    let status = upstream_resp.status();
    let content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();

    let bytes_stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
        Box::pin(upstream_resp.bytes_stream());

    let stream = futures::stream::try_unfold(
        (String::new(), StreamCarry::default(), bytes_stream, vault),
        |(mut buf, mut carry, mut stream, vault)| async move {
            loop {
                if let Some(event_end) = buf.find("\n\n") {
                    let after = buf.split_off(event_end + 2);
                    let event = std::mem::replace(&mut buf, after);
                    let processed = process_anthropic_sse_event(&event, &mut carry, &vault);
                    return Ok::<_, std::io::Error>(Some((
                        Bytes::from(processed),
                        (buf, carry, stream, vault),
                    )));
                }

                match stream.try_next().await {
                    Ok(Some(chunk)) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    Ok(None) => {
                        let mut tail = if buf.is_empty() {
                            String::new()
                        } else {
                            let processed = process_anthropic_sse_event(&buf, &mut carry, &vault);
                            buf.clear();
                            processed
                        };
                        tail.push_str(&flush_anthropic_carry(&mut carry, &vault));
                        if tail.is_empty() {
                            return Ok(None);
                        }
                        return Ok(Some((Bytes::from(tail), (buf, carry, stream, vault))));
                    }
                    Err(e) => {
                        return Err(std::io::Error::other(e));
                    }
                }
            }
        },
    );

    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(axum::http::header::CONTENT_TYPE, ct.as_bytes());
    }
    builder
        .body(Body::from_stream(stream))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// Rehydrate the text-bearing fields inside one Anthropic SSE event.
///
/// `carry` holds back a partial placeholder that spans two events; it is
/// flushed before `message_stop` and again at end of stream.
pub(super) fn process_anthropic_sse_event(
    event: &str,
    carry: &mut StreamCarry,
    vault: &Vault,
) -> String {
    let mut out = String::new();

    // If this event carries `message_stop`, flush any held-back placeholder
    // text before emitting the stop event.
    let is_message_stop = event.lines().any(|line| {
        line.strip_prefix("data: ")
            .and_then(|payload| serde_json::from_str::<Value>(payload).ok())
            .and_then(|v| {
                v.get("type")
                    .and_then(|t| t.as_str().map(|s| s == "message_stop"))
            })
            .unwrap_or(false)
    });
    if is_message_stop {
        out.push_str(&flush_anthropic_carry(carry, vault));
    }

    for line in event.lines() {
        if let Some(payload) = line.strip_prefix("data: ") {
            match serde_json::from_str::<Value>(payload) {
                Ok(mut value) => {
                    rehydrate_anthropic_sse_delta(&mut value, carry, vault);
                    out.push_str("data: ");
                    match serde_json::to_string(&value) {
                        Ok(serialized) => out.push_str(&serialized),
                        Err(_) => out.push_str(payload),
                    }
                }
                Err(_) => out.push_str(line),
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    // Each SSE event is terminated by a blank line.
    out.push('\n');
    out
}

/// Apply `Gateway::rehydrate` to text-bearing Anthropic streaming deltas.
pub(super) fn rehydrate_anthropic_sse_delta(
    value: &mut Value,
    carry: &mut StreamCarry,
    vault: &Vault,
) {
    if value.get("type").and_then(|t| t.as_str()) != Some("content_block_delta") {
        return;
    }
    let Some(delta) = value.get_mut("delta") else {
        return;
    };
    let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match delta_type {
        "text_delta" => {
            if let Some(Value::String(content)) = delta.get_mut("text") {
                let rehydrated = rehydrate_with_carry(content, &mut carry.content, vault);
                *content = rehydrated;
            }
        }
        "input_json_delta" => {
            if let Some(Value::String(partial_json)) = delta.get_mut("partial_json") {
                let rehydrated = rehydrate_with_carry(partial_json, &mut carry.args, vault);
                *partial_json = rehydrated;
            }
        }
        _ => {}
    }
}

/// Emit held-back Anthropic placeholder text as final synthetic deltas.
pub(super) fn flush_anthropic_carry(carry: &mut StreamCarry, vault: &Vault) -> String {
    if carry.content.is_empty() && carry.args.is_empty() {
        return String::new();
    }
    let mut events = Vec::new();
    if !carry.content.is_empty() {
        let held = std::mem::take(&mut carry.content);
        let payload = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": vault.restore(&held) }
        });
        events.push(format!("event: content_block_delta\ndata: {payload}\n\n"));
    }
    if !carry.args.is_empty() {
        let held = std::mem::take(&mut carry.args);
        let payload = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": vault.restore(&held) }
        });
        events.push(format!("event: content_block_delta\ndata: {payload}\n\n"));
    }
    events.join("")
}
