# Using Portcullis with any LLM client

Portcullis is a **proxy**, not a library. That means "does it work with client X?"
reduces to one question: *can client X point its base URL somewhere else?* For almost
every popular client, the answer is yes.

The client keeps sending exactly the requests it always did. Portcullis strips private
values on the way out, and restores them on the way back. The provider never sees them.

> The cloud model sees the **task**, never the **identities**.

## The universal recipe

1. Run Portcullis with your real provider behind it:

   ```bash
   export PORTCULLIS_UPSTREAM_URL="https://api.your-provider.com/v1/chat/completions"
   export PORTCULLIS_UPSTREAM_KEY="<your real provider key>"
   export PORTCULLIS_ADMIN_TOKEN="<a local secret, guards /teach>"
   export PORTCULLIS_STORE="/path/to/store.json"     # keep this OFF the repo
   export PORTCULLIS_MODEL_DIR="/path/to/model"      # ONNX detector (optional)
   portcullis --features onnx serve --bind 127.0.0.1:8080
   ```

2. Point your client's **base URL** at `http://127.0.0.1:8080/v1`.
3. Point your client's **API key** at anything — Portcullis ignores it and uses
   `PORTCULLIS_UPSTREAM_KEY` when talking to the real provider.

## Client matrix

| Client | Protocol | How to point it at Portcullis | Status |
|---|---|---|---|
| **Hermes** | OpenAI | add a `custom_providers` entry with `base_url: http://127.0.0.1:8080/v1` | ✅ |
| **OpenCode** | OpenAI | provider config `baseURL` | ✅ |
| **Cursor / Cline / Continue** | OpenAI | "OpenAI-compatible" base-URL override | ✅ |
| **Aider** | OpenAI | `--openai-api-base` | ✅ |
| **OpenAI SDK / LangChain / LiteLLM** | OpenAI | `base_url=` | ✅ |
| **Claude Code** | Anthropic | `ANTHROPIC_BASE_URL=http://127.0.0.1:8080` | ✅ |
| **Anthropic SDK** | Anthropic | `base_url=` | ✅ |

**OpenAI protocol** = `POST /v1/chat/completions` (implemented).
**Anthropic protocol** = `POST /v1/messages` (implemented).

Both are redacted and forwarded in the **same** protocol — Portcullis does not
translate between them, so tool calls, images and block structure survive intact.

## Streaming

Interactive clients stream (SSE). Portcullis rehydrates the response stream chunk by
chunk, with a carry-over buffer so a placeholder split across two chunks is still
restored correctly. Fail-closed behaviour is unchanged: the assertion still runs over
the full outbound payload *before* anything is forwarded.

## Teaching it (from any client)

The learning surface is plain HTTP with a bearer token, so anything that can `curl`
can teach it:

```bash
TOKEN="<PORTCULLIS_ADMIN_TOKEN>"

# teach a term it must always hide
curl -sX POST http://127.0.0.1:8080/teach \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"term":"Aerolith","label":"ORG"}'

# what does it know?
curl -s http://127.0.0.1:8080/terms -H "Authorization: Bearer $TOKEN"

# what did it spot that you have not taught yet?
curl -s http://127.0.0.1:8080/suggestions -H "Authorization: Bearer $TOKEN"

# forget something
curl -sX POST http://127.0.0.1:8080/unteach \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"term":"Aerolith"}'
```

Teaching invalidates the redaction cache, so a term taught mid-conversation takes effect
on the **very next** request — including on messages already seen.

## Environment variables

| Variable | Required | Purpose |
|---|---|---|
| `PORTCULLIS_UPSTREAM_URL` | yes | Full upstream endpoint (e.g. `.../v1/chat/completions`) |
| `PORTCULLIS_UPSTREAM_KEY` | yes | Bearer key for the upstream provider |
| `PORTCULLIS_ADMIN_TOKEN` | for `/teach` | Guards the learning surface. Unset → those endpoints return **503** (never open) |
| `PORTCULLIS_STORE` | no | Where taught terms are persisted (default `store.json`) |
| `PORTCULLIS_MODEL_DIR` | no | ONNX detector model dir (needs the `onnx` feature) |
| `PORTCULLIS_MODEL` | no | Name of a detector from the registry (see [MODELS.md](MODELS.md)) |
| `PORTCULLIS_MODELS` | no | Path to the model registry (default `portcullis.models.json`) |
| `PORTCULLIS_LABELS` | no | Comma-separated label set for the detector |
| `PORTCULLIS_THRESHOLD` | no | Confidence threshold (default 0.5; the benchmark uses 0.3) |
| `PORTCULLIS_ANTHROPIC_UPSTREAM_URL` | for Claude Code | Default `https://api.anthropic.com/v1/messages` |
| `PORTCULLIS_ANTHROPIC_KEY` | for Claude Code | Anthropic key, sent as `x-api-key` (not a bearer token) |
| `PORTCULLIS_ANTHROPIC_VERSION` | no | Default `2023-06-01`; a client-supplied version wins |

## What Portcullis does *not* do

- It is **not** a network service you expose. Bind it to `127.0.0.1` — it holds your
  decrypted values in memory and on disk.
- The `store.json` file is the crown jewels: it maps your real terms. **Never commit it.**
- Detection is not magic: the dictionary layer is a guarantee, the model layer is
  best-effort recall. Teach the terms that matter.
