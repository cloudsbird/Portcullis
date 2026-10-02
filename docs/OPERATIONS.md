# Operations

Running Portcullis as a service: what to set, what to watch, and what is still
missing before it guards someone else's data.

## Health

```bash
curl -s http://127.0.0.1:8080/healthz
```

```json
{ "status": "ok", "version": "0.1.0", "uptime_seconds": 42, "store_terms": 7 }
```

Unauthenticated (so a load balancer can reach it) and deliberately reveals **counts
only** — never a term, a value, or a key. There is a test asserting that a stored term
cannot appear in this response.

Use it as a readiness probe: if `/healthz` answers, the process is up and the store is
loaded.

## Upstream timeouts

A hung provider must not hang your client. Three knobs, all in seconds:

| Variable | Default | Meaning |
|---|---|---|
| `PORTCULLIS_CONNECT_TIMEOUT_SECS` | `10` | connection establishment |
| `PORTCULLIS_READ_TIMEOUT_SECS` | `180` | gap **between** bytes — this is what catches a stalled stream |
| `PORTCULLIS_REQUEST_TIMEOUT_SECS` | `600` | whole call, **non-streaming only** |

The split matters. `reqwest`'s total timeout would kill a long response, so streaming
requests are deliberately **exempt** from `REQUEST_TIMEOUT` and rely on `READ_TIMEOUT`
instead — which allows a genuinely long generation while still failing a dead stream.

**Retries are deliberately narrow.** The proxy retries **once**, and only when the
*connection* failed (`is_connect()`), where the request provably never reached the
provider. It does **not** retry HTTP 5xx or post-send timeouts: against an LLM those can
mean the request was already processed, and a silent retry would double-charge and
generate twice.

## Request body cap

| Variable | Default | Meaning |
|---|---|---|
| `PORTCULLIS_MAX_BODY_BYTES` | `2097152` (2 MiB) | largest accepted request body |

Detection cost grows with input length (measured ≈ O(L^1.3)), so an unbounded body is an
easy way to stall the gateway. Oversized requests get **413** before any work is done.

## Providers that need their own headers

Some providers require a project, tenant or session header on every call. There are two
ways to supply it, and **nothing is forwarded unless you ask** — an unrelated credential
cannot ride along.

```bash
# static: same value on every upstream request
PORTCULLIS_UPSTREAM_HEADERS='{"x-opencode-session":"portcullis-1"}'

# pass-through: forward this header from the incoming client request
PORTCULLIS_FORWARD_HEADERS='x-opencode-session'
```

## Logging

| Variable | Default | Meaning |
|---|---|---|
| `PORTCULLIS_LOG` | `info` | `tracing` filter, e.g. `info,portcullis=debug` |
| `PORTCULLIS_LOG_FORMAT` | text | set to `json` for one JSON object per line |
| `PORTCULLIS_METRICS_TOKEN` | — | require this bearer token on `/metrics` (default: open) |

One line per request with method, path, status and duration. **Bodies and headers are
never logged**, so no prompt content, taught term or credential can reach your logs.

```
2026-10-02T04:12:09Z  INFO portcullis::proxy: request method=POST path=/v1/chat/completions status=200 ms=412
```

## Metrics

`GET /metrics` serves Prometheus text format (`text/plain; version=0.0.4`). It is open by
default, like `/healthz`; set `PORTCULLIS_METRICS_TOKEN` to require
`Authorization: Bearer <token>`. It never waits on the gateway lock, so a scrape is not
queued behind a slow scan.

**No value ever appears in a metric.** Labels come from small fixed sets — route, status,
redaction label, block reason, configured scope names — and every open-ended set is capped at
64 series (the rest fold into `other`). Unknown URLs collapse into `route="other"`, so a
scanner cannot mint series.

| Metric | Type | Labels | Answers |
|---|---|---|---|
| `portcullis_http_requests_total` | counter | `route`, `status` | traffic and error rate |
| `portcullis_http_request_duration_seconds` | histogram | `route` | latency to response headers (a stream's body time is not included) |
| `portcullis_redactions_total` | counter | `label` | how much is being hidden, by kind |
| `portcullis_delta_cache_total` | counter | `result` | delta-cache hit rate |
| `portcullis_blocked_total` | counter | `reason` | `residual_term` / `malformed_placeholder` (fail-closed), `unauthorized_scope`, `unauthorized_admin` |
| `portcullis_scan_duration_seconds` | histogram | — | detection cost per request |
| `portcullis_gateway_lock_wait_seconds` | histogram | — | **queueing** behind the single gateway lock; rising means detection is your bottleneck |
| `portcullis_upstream_responses_total` | counter | `outcome` | provider status codes, `timeout`, `error` |
| `portcullis_upstream_duration_seconds` | histogram | — | provider latency |
| `portcullis_scope_requests_total` | counter | `scope` | per-client traffic (only with scopes enabled) |
| `portcullis_store_terms` | gauge | — | taught terms |
| `portcullis_uptime_seconds`, `portcullis_build_info` | gauge | `version` | process basics |

```yaml
# prometheus.yml
scrape_configs:
  - job_name: portcullis
    static_configs: [{ targets: ["127.0.0.1:8080"] }]
    # authorization: { credentials_file: /etc/prometheus/portcullis.token }   # if a token is set
```

Worth alerting on: `increase(portcullis_blocked_total{reason=~"residual_term|malformed_placeholder"}[5m]) > 0`
(the fail-closed assertion fired — something nearly leaked), a sustained non-zero
`portcullis_gateway_lock_wait_seconds`, and 5xx or `timeout` in `portcullis_upstream_responses_total`.

Not implemented: **OTLP / distributed tracing.** Metrics are Prometheus-only, and logs remain
structured JSON (`PORTCULLIS_LOG_FORMAT=json`).

## Concurrency

Detection is CPU-bound and synchronous. It runs on the **blocking pool**
(`spawn_blocking`), not on an async worker, so a slow scan cannot stall the runtime and
starve unrelated requests.

That said, a single `Gateway` sits behind one mutex, so **detection itself serialises**:
one scan at a time (response rehydration no longer takes the lock — each request restores
from its own vault — so streaming responses never wait on someone else's scan). Measured throughput is ~0.28 s/sentence on the reference host, so
expect roughly 3–4 scans/second, and remember a 4,000-character prompt is ~2.9 s.

The delta cache absorbs most of the real cost in a conversation — only the *new* text in
each turn is scanned — so steady-state chat traffic is materially faster than that
worst case. But if you need genuine parallel detection, that is a scheduling change (a
pool of detector instances), not a config knob, and it is not implemented today.

## Running it

```bash
# store outside the repo, 0600 by default
install -d -m 700 /var/lib/portcullis
export PORTCULLIS_UPSTREAM_URL=https://api.openai.com/v1/chat/completions
export PORTCULLIS_UPSTREAM_KEY=sk-...
export PORTCULLIS_ADMIN_TOKEN=$(head -c 32 /dev/urandom | base64)
export PORTCULLIS_STORE=/var/lib/portcullis/store.json
export PORTCULLIS_MODEL_DIR=/opt/portcullis/model
export PORTCULLIS_LOG_FORMAT=json

portcullis serve --bind 127.0.0.1:8080
```

Keep it on **loopback**. It is a local privacy gateway — exposing it publicly would make
it an open redaction proxy for your provider key.

## Full environment reference

| Variable | Default | Purpose |
|---|---|---|
| `PORTCULLIS_UPSTREAM_URL` | *(required)* | OpenAI-compatible upstream |
| `PORTCULLIS_UPSTREAM_KEY` | — | bearer token for that upstream |
| `PORTCULLIS_ANTHROPIC_UPSTREAM_URL` | `https://api.anthropic.com/v1/messages` | Anthropic upstream |
| `PORTCULLIS_ANTHROPIC_KEY` | — | sent as `x-api-key` |
| `PORTCULLIS_ANTHROPIC_VERSION` | `2023-06-01` | default when the client sends none |
| `PORTCULLIS_ADMIN_TOKEN` | — | guards `/teach`, `/unteach`, `/terms`, `/suggestions`; **unset → 503** |
| `PORTCULLIS_STORE` | `store.json` | learned-term store, written `0600` |
| `PORTCULLIS_MODEL` | — | named detector from the registry ([MODELS.md](MODELS.md)) |
| `PORTCULLIS_MODELS` | `portcullis.models.json` | registry path |
| `PORTCULLIS_MODEL_DIR` | `./model` | detector dir when not using a named model |
| `PORTCULLIS_LABELS` | detector default | comma-separated label set |
| `PORTCULLIS_THRESHOLD` | `0.5` | detector confidence cut-off |
| `PORTCULLIS_MAX_BODY_BYTES` | `2097152` | request body cap |
| `PORTCULLIS_FORWARD_HEADERS` | — | comma-separated **client** headers to pass upstream (opt-in allowlist) |
| `PORTCULLIS_UPSTREAM_HEADERS` | — | JSON object of static headers added to every upstream request |
| `PORTCULLIS_CONNECT_TIMEOUT_SECS` | `10` | see above |
| `PORTCULLIS_READ_TIMEOUT_SECS` | `180` | see above |
| `PORTCULLIS_REQUEST_TIMEOUT_SECS` | `600` | see above |
| `PORTCULLIS_LOG` | `info` | log filter |
| `PORTCULLIS_LOG_FORMAT` | text | `json` for structured logs |

## Before this guards someone else's data

What is closed:

- ✅ upstream timeouts (connect / read / request), with streaming handled correctly
- ✅ narrow, safe connect-only retry
- ✅ request body cap
- ✅ `/healthz` that leaks nothing
- ✅ detection off the async worker thread
- ✅ structured logs that never contain prompt content
- ✅ store written `0600`

What is **not** closed, and I would not pretend otherwise:

1. **Only one provider has been exercised.** The live run covers an OpenAI-compatible
   endpoint. The Anthropic `/v1/messages` path is still mock-only — no live Anthropic key
   was available. Its SSE shapes are different, so treat it as unverified against reality.
2. **The store is encrypted only if you set a key.** Without one it is `0600` plaintext JSON,
   a map of everything you consider private. With one see [ENCRYPTION.md](ENCRYPTION.md) —
   the running process still holds it decrypted.
3. **Scope isolation is opt-in, and is selection, not a security boundary.** See
   [SCOPES.md](SCOPES.md): per-client tokens pick which terms apply, but the operator,
   the store file, the provider key and the detector are shared.
4. **Detection serialises** (see Concurrency), so throughput is bounded.
5. **No OTLP / tracing export.** Prometheus metrics exist (see Metrics above); distributed tracing does not.

## Live end-to-end test

`scripts/e2e_live.sh` runs the whole path against a **real** provider: real TLS, real auth,
real SSE framing, real error shapes. It is opt-in, because it sends traffic to a third
party and costs a fraction of a cent:

```bash
PORTCULLIS_E2E_UPSTREAM_URL=https://api.openai.com/v1/chat/completions \
PORTCULLIS_E2E_UPSTREAM_KEY=sk-... \
PORTCULLIS_E2E_MODEL=gpt-4o-mini \
  scripts/e2e_live.sh
```

It asserts: health, the teach surface, a non-streaming round trip, a **streaming** round
trip (frames, `[DONE]`, rehydration, and no leaked or half-split placeholder), that the
provider received a **placeholder rather than the real value**, and that an upstream error
surfaces as a status rather than a hang.

**It earned its keep.** The first live run found two real gaps that no mock had caught:

- **`reasoning_content` was not rehydrated.** Reasoning models stream a second text field;
  a placeholder the model *reasoned* about came back to the client as a literal
  `<<ORG_1>>`. Fixed for both the streaming and non-streaming paths (and non-streaming
  tool-call arguments, which were also unhandled).
- **Custom provider headers were impossible.** The proxy forwarded nothing from the client,
  so any provider requiring a header (e.g. `x-opencode-session`) simply could not be
  reached. Fixed with the opt-in allowlist above.
