//! OpenAI-compatible `/v1/chat/completions`: request redaction, response and
//! SSE rehydration.

use super::stream::*;
use super::*;

/// Walk a message and collect every redactable text string into `out`,
/// replacing each collected string with an empty placeholder in `msg`.
/// All other fields are left untouched so they survive forwarding.
pub(super) fn collect_message_texts(msg: &mut Value, out: &mut Vec<String>) {
    // `content` may be a plain string or an array of multimodal parts.
    if let Some(content) = msg.get_mut("content") {
        collect_content_texts(content, out);
    }

    // `tool_calls[*].function.arguments` is text-bearing JSON.
    if let Some(tool_calls) = msg.get_mut("tool_calls") {
        collect_tool_calls_texts(tool_calls, out);
    }

    // `name` and `tool_call_id` are protocol identifiers the provider matches on, so
    // they are forwarded unchanged rather than replaced with placeholders. They are
    // still covered by the fail-closed assertion over the whole outbound body: a
    // taught term appearing in one stops the request instead of leaving the machine.
}

pub(super) fn collect_content_texts(content: &mut Value, out: &mut Vec<String>) {
    match content {
        Value::String(s) => {
            out.push(std::mem::take(s));
        }
        Value::Array(parts) => {
            for part in parts {
                if let Value::Object(map) = part {
                    for key in &["text", "input_text", "output_text"] {
                        if let Some(Value::String(s)) = map.get_mut(*key) {
                            out.push(std::mem::take(s));
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

pub(super) fn collect_tool_calls_texts(tool_calls: &mut Value, out: &mut Vec<String>) {
    let Value::Array(calls) = tool_calls else {
        return;
    };
    for call in calls {
        let Value::Object(call_map) = call else {
            continue;
        };
        let Some(Value::Object(func_map)) = call_map.get_mut("function") else {
            continue;
        };
        if let Some(Value::String(args)) = func_map.get_mut("arguments") {
            out.push(std::mem::take(args));
        }
    }
}

/// Place redacted strings back into `msg`, following the same walk order as
/// `collect_message_texts`.
pub(super) fn replace_message_texts(msg: &mut Value, redacted: &[String], idx: &mut usize) {
    if let Some(content) = msg.get_mut("content") {
        replace_content_texts(content, redacted, idx);
    }
    if let Some(tool_calls) = msg.get_mut("tool_calls") {
        replace_tool_calls_texts(tool_calls, redacted, idx);
    }
}

pub(super) fn replace_content_texts(content: &mut Value, redacted: &[String], idx: &mut usize) {
    match content {
        Value::String(s) => {
            *s = redacted[*idx].clone();
            *idx += 1;
        }
        Value::Array(parts) => {
            for part in parts {
                if let Value::Object(map) = part {
                    for key in &["text", "input_text", "output_text"] {
                        if let Some(Value::String(_)) = map.get_mut(*key) {
                            map[*key] = Value::String(redacted[*idx].clone());
                            *idx += 1;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

pub(super) fn replace_tool_calls_texts(
    tool_calls: &mut Value,
    redacted: &[String],
    idx: &mut usize,
) {
    let Value::Array(calls) = tool_calls else {
        return;
    };
    for call in calls {
        let Value::Object(call_map) = call else {
            continue;
        };
        let Some(Value::Object(func_map)) = call_map.get_mut("function") else {
            continue;
        };
        if let Some(Value::String(_)) = func_map.get_mut("arguments") {
            func_map["arguments"] = Value::String(redacted[*idx].clone());
            *idx += 1;
        }
    }
}

pub async fn chat_completions(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match handle(state, headers, body).await {
        Ok(resp) => resp,
        Err((status, msg)) => (status, msg).into_response(),
    }
}

pub(super) async fn handle(
    state: ProxyState,
    headers: HeaderMap,
    body: Value,
) -> Result<Response, (StatusCode, String)> {
    // Who is asking decides which taught terms apply. Rejected before any body is
    // read or forwarded.
    let scope = resolve_scope(&state, &headers)?;

    let messages = body.get("messages").and_then(|m| m.as_array()).ok_or((
        StatusCode::BAD_REQUEST,
        "missing or invalid messages".to_string(),
    ))?;

    // Collect every redactable string from the original messages while
    // preserving all other fields.
    let mut redacted_messages: Vec<Value> = Vec::with_capacity(messages.len());
    let mut texts_to_redact: Vec<String> = Vec::new();
    for msg in messages.iter() {
        let mut msg = msg.clone();
        collect_message_texts(&mut msg, &mut texts_to_redact);
        redacted_messages.push(msg);
    }

    // Redact all text-bearing strings in one batch (better cache hit rate).
    // Detection is CPU-bound and synchronous, so it runs on the blocking pool
    // rather than stalling an async worker thread.
    let (redacted_texts, vault) = {
        let gateway = state.gateway.clone();
        let texts = texts_to_redact.clone();
        let scope = scope.clone();
        tokio::task::spawn_blocking(move || {
            let waited = Instant::now();
            let mut gw = gateway.blocking_lock();
            let lock_wait = waited.elapsed().as_secs_f64();
            let scanning = Instant::now();
            let mut vault = Vault::new();
            let out = gw.process_scoped(scope.as_deref(), &mut vault, &texts);
            gw.metrics()
                .scan(lock_wait, scanning.elapsed().as_secs_f64());
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

    // Place redacted strings back into the message copies.
    {
        let mut idx = 0;
        for msg in &mut redacted_messages {
            replace_message_texts(msg, &redacted_texts, &mut idx);
        }
    }

    let mut upstream_body = body.clone();
    upstream_body["messages"] = json!(redacted_messages);

    // Invariant 4: fail-closed outbound assertion — run over the ENTIRE assembled
    // payload, not only the strings we knew to redact. If the walker failed to
    // collect a field that carries a protected term, this is what stops it leaving.
    {
        let gw = state.gateway.lock().await;
        let assembled = serde_json::to_string(&upstream_body)
            .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
        gw.assert_clean_scoped(scope.as_deref(), &[assembled])
            .map_err(|e| {
                state.metrics.blocked(if e.contains("residual") {
                    "residual_term"
                } else {
                    "malformed_placeholder"
                });
                (StatusCode::BAD_GATEWAY, e)
            })?;
    }

    let streaming = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let upstream_resp = send_upstream(&state, streaming, &headers, || {
        state
            .client
            .post(&state.upstream_url)
            .bearer_auth(&state.upstream_key)
            .json(&upstream_body)
    })
    .await?;

    if streaming {
        return stream_response(Arc::new(vault), upstream_resp).await;
    }

    let status = upstream_resp.status();
    let mut upstream_json: Value = upstream_resp
        .json()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    // Rehydrate assistant text locally before returning to the caller. Covers
    // content, reasoning content, and tool-call arguments.
    {
        if let Some(choices) = upstream_json
            .get_mut("choices")
            .and_then(|c| c.as_array_mut())
        {
            for choice in choices.iter_mut() {
                if let Some(msg) = choice.get_mut("message") {
                    rehydrate_response_message(msg, &vault);
                }
            }
        }
    }

    Ok((status, Json(upstream_json)).into_response())
}

// ---------------------------------------------------------------------------
// Server-Sent Events streaming path
// ---------------------------------------------------------------------------

/// Forward a streaming upstream response to the caller, rehydrating every
/// `data:` line chunk by chunk while buffering incomplete SSE events so that
/// placeholders split across TCP chunks are still restored correctly.
pub(super) async fn stream_response(
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
                // Emit a complete SSE event as soon as we have the blank-line
                // delimiter. `carry` additionally holds back a partial
                // placeholder that spans two events.
                if let Some(event_end) = buf.find("\n\n") {
                    let after = buf.split_off(event_end + 2);
                    let event = std::mem::replace(&mut buf, after);
                    let processed = process_sse_event(&event, &mut carry, &vault);
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
                            let processed = process_sse_event(&buf, &mut carry, &vault);
                            buf.clear();
                            processed
                        };
                        tail.push_str(&flush_carry(&mut carry, &vault));
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

/// Emit any held-back text as a final synthetic delta event.
pub(super) fn flush_carry(carry: &mut StreamCarry, vault: &Vault) -> String {
    if carry.content.is_empty() && carry.args.is_empty() && carry.reasoning.is_empty() {
        return String::new();
    }
    let mut deltas = Vec::new();
    if !carry.content.is_empty() {
        let held = std::mem::take(&mut carry.content);
        deltas.push(json!({
            "index": 0,
            "delta": { "content": vault.restore(&held) },
            "finish_reason": null
        }));
    }
    if !carry.reasoning.is_empty() {
        let held = std::mem::take(&mut carry.reasoning);
        deltas.push(json!({
            "index": 0,
            "delta": { "reasoning_content": vault.restore(&held) },
            "finish_reason": null
        }));
    }
    if !carry.args.is_empty() {
        let held = std::mem::take(&mut carry.args);
        deltas.push(json!({
            "index": 0,
            "delta": { "tool_calls": [ {
                "index": 0,
                "function": { "arguments": vault.restore(&held) }
            } ] },
            "finish_reason": null
        }));
    }
    let payload = json!({ "object": "chat.completion.chunk", "choices": deltas });
    format!("data: {payload}\n\n")
}

/// Rehydrate the text-bearing fields inside one SSE event.
///
/// `carry` holds back a partial placeholder that spans two events; it is
/// flushed before `[DONE]` and again at end of stream.
pub(super) fn process_sse_event(event: &str, carry: &mut StreamCarry, vault: &Vault) -> String {
    let mut out = String::new();
    for line in event.lines() {
        if let Some(payload) = line.strip_prefix("data: ") {
            if payload == "[DONE]" {
                // Flush anything still held back before terminating.
                out.push_str(&flush_carry(carry, vault));
                out.push_str(line);
            } else {
                match serde_json::from_str::<Value>(payload) {
                    Ok(mut value) => {
                        rehydrate_sse_delta(&mut value, carry, vault);
                        out.push_str("data: ");
                        match serde_json::to_string(&value) {
                            Ok(serialized) => out.push_str(&serialized),
                            Err(_) => out.push_str(payload),
                        }
                    }
                    Err(_) => out.push_str(line),
                }
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

/// Apply `Gateway::rehydrate` to every text-bearing delta field in a streaming
/// chunk: `choices[].delta.content`, `choices[].delta.reasoning_content`, and
/// `choices[].delta.tool_calls[].function.arguments`.
pub(super) fn rehydrate_sse_delta(value: &mut Value, carry: &mut StreamCarry, vault: &Vault) {
    let Some(choices) = value.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return;
    };
    for choice in choices.iter_mut() {
        let Some(delta) = choice.get_mut("delta") else {
            continue;
        };
        if let Some(Value::String(content)) = delta.get_mut("content") {
            let rehydrated = rehydrate_with_carry(content, &mut carry.content, vault);
            *content = rehydrated;
        }
        // Reasoning models stream a second text field. Without this, a
        // placeholder the model *reasoned* about comes back to the client as a
        // literal `<<ORG_1>>`.
        if let Some(Value::String(reasoning)) = delta.get_mut("reasoning_content") {
            let rehydrated = rehydrate_with_carry(reasoning, &mut carry.reasoning, vault);
            *reasoning = rehydrated;
        }
        if let Some(tool_calls) = delta.get_mut("tool_calls") {
            rehydrate_tool_calls(tool_calls, &mut carry.args, vault);
        }
    }
}

/// Rehydrate the text-bearing fields of a complete (non-streaming) assistant
/// message: content, reasoning content, and tool-call arguments.
pub(super) fn rehydrate_response_message(msg: &mut Value, vault: &Vault) {
    for field in ["content", "reasoning_content"] {
        if let Some(Value::String(text)) = msg.get_mut(field) {
            *text = vault.restore(text);
        }
    }
    if let Some(calls) = msg.get_mut("tool_calls").and_then(|c| c.as_array_mut()) {
        for call in calls.iter_mut() {
            if let Some(Value::String(args)) = call
                .get_mut("function")
                .and_then(|f| f.get_mut("arguments"))
            {
                *args = vault.restore(args);
            }
        }
    }
}

pub(super) fn rehydrate_tool_calls(tool_calls: &mut Value, pending: &mut String, vault: &Vault) {
    let Some(calls) = tool_calls.as_array_mut() else {
        return;
    };
    for call in calls.iter_mut() {
        let Some(func) = call.get_mut("function") else {
            continue;
        };
        if let Some(Value::String(args)) = func.get_mut("arguments") {
            let rehydrated = rehydrate_with_carry(args, pending, vault);
            *args = rehydrated;
        }
    }
}
