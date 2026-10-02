# Deployment

Everything you need to actually run this. If the earlier docs left you unsure, read
[the mental model](#the-mental-model) first — it is the part that trips people up.

---

## The mental model

Portcullis is a **reverse proxy**. It is not a provider and it does not host a model.
It sits between your client and the provider you already pay for:

```
   ┌─────────┐        (1)              ┌────────────┐        (2)              ┌──────────┐
   │ client  │ ───────────────────────▶ │ Portcullis │ ───────────────────────▶ │ provider │
   │ Hermes  │  thinks this IS the      │   local    │  holds your real key     │ OpenAI   │
   │ Cursor  │  provider                │  :8080     │                          │ OpenRtr  │
   └─────────┘                          └────────────┘                          └──────────┘
          ▲                                                                             │
          └──────────────── real values rehydrated here ◀───────────────────────────────┘
```

Two addresses have to be right, and they are **different things**:

| | Meaning | Example |
|---|---|---|
| **(1) Client → Portcullis** | your client's `base_url` | `http://127.0.0.1:8080/v1` |
| **(2) Portcullis → provider** | `PORTCULLIS_UPSTREAM_URL` | `https://openrouter.ai/api/v1` |

Your client never learns the provider URL or key. Portcullis never needs to know which
client is talking to it. That is the whole design.

**Nothing else changes.** The `model` name your client sends is forwarded **verbatim**, so
it must be a model name your *provider* understands — not a Portcullis concept.

---

## Provider cheat-sheet

Set `PORTCULLIS_UPSTREAM_URL` to either the base URL or the full endpoint — both work
(see [URL handling](#url-handling)). `PORTCULLIS_UPSTREAM_KEY` is that provider's key.

| Provider | `PORTCULLIS_UPSTREAM_URL` | Notes |
|---|---|---|
| **OpenRouter** | `https://openrouter.ai/api/v1` | model names look like `openai/gpt-4o-mini` |
| **OpenAI** | `https://api.openai.com/v1` | |
| **Groq** | `https://api.groq.com/openai/v1` | |
| **Together** | `https://api.together.xyz/v1` | |
| **DeepSeek** | `https://api.deepseek.com/v1` | |
| **Ollama** (local) | `http://localhost:11434/v1` | key can be anything, e.g. `ollama` |
| **vLLM** (local) | `http://localhost:8000/v1` | |
| **LiteLLM** | `http://localhost:4000/v1` | **see the warning below** |
| **opencode-go** | `https://opencode.ai/zen/go/v1` | needs a session header — see below |
| **Azure OpenAI** | full deployment URL incl. `?api-version=…` | left as-is, never appended to |
| **Anthropic** | `https://api.anthropic.com` via `PORTCULLIS_ANTHROPIC_UPSTREAM_URL` | Anthropic protocol; the `/v1/messages` route |

> ⚠️ **Careful with LiteLLM, Portkey and other routers.** Pointing Portcullis at another
> router means your prompt passes through *two* proxies. That **adds** a party to your data
> path instead of subtracting one — which is the opposite of the point. Prefer pointing
> Portcullis straight at the provider.

### Providers that need their own header

`opencode-go` requires an `x-opencode-session` header. Either supply it statically, or
forward it from the client:

```bash
PORTCULLIS_UPSTREAM_HEADERS='{"x-opencode-session":"portcullis-1"}'
# or
PORTCULLIS_FORWARD_HEADERS='x-opencode-session'
```

Nothing is forwarded unless named, so an unrelated credential cannot ride along.

> **Quote the JSON.** In a shell assignment or an `EnvironmentFile`, bash strips the inner
> double quotes from an unquoted `VAR={"k":"v"}`, which makes it invalid JSON. You get a
> clear error at startup if that happens — it fails fast rather than silently dropping the
> header and failing later at the provider.

---

## URL handling

`PORTCULLIS_UPSTREAM_URL` accepts either form:

| You set | Portcullis uses |
|---|---|
| `https://openrouter.ai/api/v1` | `https://openrouter.ai/api/v1/chat/completions` |
| `https://openrouter.ai/api/v1/chat/completions` | unchanged |
| `https://x.openai.azure.com/…/chat/completions?api-version=2024-10-21` | unchanged (a `?` means "trust me") |

The resolved URL is printed at startup, so it is never a mystery:

```
INFO portcullis listening listen=127.0.0.1:8080
     openai_upstream=https://openrouter.ai/api/v1/chat/completions
     anthropic_upstream=https://api.anthropic.com/v1/messages
     store=/var/lib/portcullis/store.json forward_headers=[] admin_api=true
```

**If you get a 404 from the provider, this line is the first thing to check.**

---

## Step by step (same machine as your client)

This is the recommended topology: local means the redaction happens inside the trust
boundary you already control.

### 1. Build the binary

```bash
git clone https://github.com/cloudsbird/Portcullis
cd Portcullis
cargo build --release --features onnx     # omit --features onnx to skip the ML layer
```

Build deps: Rust stable, `pkg-config`, `libssl-dev` (needed to compile the ONNX
bindings). The release binary at `target/release/portcullis` is **~30 MB** and links only
libc/libstdc++ — ONNX Runtime is statically linked, so there is nothing else to install.

### 2. Get the detector model (optional — see below)

```bash
# only if you want the ML layer
pip install gliner2-onnx            # or use the exporter directly
git clone https://github.com/lmoe/gliner2-onnx
cd gliner2-onnx
make onnx-export MODEL=fastino/gliner2-privacy-filter-PII-multi
# → model_out/gliner2-privacy-filter-PII-multi/
```

Point `PORTCULLIS_MODEL_DIR` at that directory. It must contain `gliner2_config.json`,
`tokenizer.json` and an `onnx/` folder.

### 3. Write the environment

```bash
install -d -m 700 /var/lib/portcullis

cat > /etc/portcullis.env <<'EOF'
PORTCULLIS_UPSTREAM_URL=https://openrouter.ai/api/v1
PORTCULLIS_UPSTREAM_KEY=sk-or-v1-...
PORTCULLIS_ADMIN_TOKEN=change-me-to-a-long-random-string
PORTCULLIS_STORE=/var/lib/portcullis/store.json
PORTCULLIS_MODEL_DIR=/opt/portcullis/model
PORTCULLIS_LOG_FORMAT=json
EOF
chmod 600 /etc/portcullis.env
```

### 4. Run it

```bash
set -a; . /etc/portcullis.env; set +a
portcullis serve --bind 127.0.0.1:8080
```

### 5. Verify — do not skip this

```bash
# up, and the store loaded?
curl -s http://127.0.0.1:8080/healthz
# {"status":"ok","version":"0.1.0","uptime_seconds":3,"store_terms":0}

# does a real round trip work? (this spends a fraction of a cent)
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o-mini","messages":[{"role":"user","content":"Say OK"}],"max_tokens":5}'
```

Then teach it something and prove redaction works end to end:

```bash
curl -s -X POST http://127.0.0.1:8080/teach \
  -H "Authorization: Bearer $PORTCULLIS_ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"term":"Acme Corp","label":"ORG"}'

curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"openai/gpt-4o-mini","max_tokens":40,
       "messages":[{"role":"user","content":"Repeat exactly: Acme Corp is in Zurich."}]}'
# The client sees "Acme Corp"; the provider only ever saw <<ORG_1>>.
```

### 6. Point your client at it

You do **not** remove the provider key from your client in every case — the client still
needs *a* key to send, but it is now meaningless. Set the client's base URL to Portcullis
and put any placeholder in the key field where the client demands one.

**Hermes** (`config.yaml`):

```yaml
custom_providers:
  portcullis:
    base_url: http://127.0.0.1:8080/v1
    api_key: not-used-locally
    models: [openai/gpt-4o-mini]
```

**Claude Code:**

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080
export ANTHROPIC_API_KEY=not-used-locally
```

**Any OpenAI SDK:** `base_url="http://127.0.0.1:8080/v1"`.
More clients: [CLIENTS.md](CLIENTS.md).

---

## Running without the ML model

**You do not need the 1.2 GB model to get real value.** Without it, Portcullis runs the
deterministic layers only — the taught-term dictionary and the regex layer (email, phone,
card, IP) — which is exactly the layer that can never be *wrong*, and it does so in
**13 MB of RAM** and with no latency penalty worth measuring.

```bash
PORTCULLIS_MODEL_DIR=/nonexistent portcullis serve --bind 127.0.0.1:8080
# portcullis: detector disabled (failed to load gliner2_config.json)
```

That message is informational, not an error. The ML layer only *adds* recall; it is never
the guarantee. Start here if you want the smallest possible deployment, then add the model
when you want automatic detection of things you have not taught.

| | Deterministic only | + ONNX detector |
|---|---|---|
| RAM | ~13 MB | ~1.58 GB (peak 1.84 GB) |
| Disk | ~30 MB | +1.2 GB model |
| Cold start | instant | ~5 s once |
| Finds | what you taught + known shapes | also novel PII, automatically |

---

## Docker

`deploy/Dockerfile` is a two-stage build; the runtime stage needs only Debian slim plus
ca-certificates, because ONNX Runtime is statically linked.

```bash
docker build -f deploy/Dockerfile -t portcullis .

docker run -d --name portcullis \
  -p 127.0.0.1:8080:8080 \
  -v portcullis-data:/data \
  -v /opt/portcullis/model:/model:ro \
  -e PORTCULLIS_UPSTREAM_URL=https://openrouter.ai/api/v1 \
  -e PORTCULLIS_UPSTREAM_KEY=sk-or-v1-... \
  -e PORTCULLIS_ADMIN_TOKEN=$(openssl rand -hex 24) \
  -e PORTCULLIS_MODEL_DIR=/model \
  -e PORTCULLIS_LOG_FORMAT=json \
  portcullis
```

Or `docker compose -f deploy/docker-compose.yml up -d`.

Note `-p 127.0.0.1:8080:8080` — the port is published to **loopback only**. See the
warning below before changing that.

> **Not build-tested in this repository.** The Dockerfile was written but no Docker daemon
> was available in the development environment, so `docker build` has not been run here.
> Verify it yourself before relying on it; the pieces it depends on (static ONNX Runtime,
> libc/libstdc++-only runtime) *have* been verified with `ldd`.

---

## systemd

`deploy/portcullis.service`:

```ini
[Unit]
Description=Portcullis privacy gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=/etc/portcullis.env
ExecStart=/usr/local/bin/portcullis serve --bind 127.0.0.1:8080
Restart=on-failure
RestartSec=3
# Hardening — it needs almost nothing
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/portcullis
LimitNOFILE=8192

[Install]
WantedBy=multi-user.target
```

```bash
# put the exported model somewhere the service user can read
sudo install -d /opt/portcullis
sudo mv model_out/gliner2-privacy-filter-PII-multi /opt/portcullis/model

sudo install -m 644 deploy/portcullis.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now portcullis
journalctl -u portcullis -f
```

---

## Exposing it beyond loopback — read this first

> ### ⚠️ The `/v1/chat/completions` and `/v1/messages` routes have **no authentication**.
>
> Only the admin surface (`/teach`, `/unteach`, `/terms`, `/suggestions`) is
> token-guarded. Anyone who can reach the proxy port can send prompts through it **using
> your provider key**. Portcullis does not rate-limit, and it does not know who is calling.

That is a deliberate design choice for the single-user case — it is meant to run on
localhost next to your client — but it makes a bind address a security decision:

| Topology | Risk | Recommendation |
|---|---|---|
| `127.0.0.1` (default) | only you | **do this** |
| LAN / `0.0.0.0` | anyone on the network can spend your credits | put it behind a reverse proxy with auth, or add mTLS |
| Public internet | strangers can spend your credits | **don't** without auth in front |

If you must share it, terminate auth in front of it (Caddy/nginx with basic auth or mTLS)
rather than publishing the port. Per-token limits and per-client `scope` are not
implemented yet — see [known gaps](OPERATIONS.md#before-this-guards-someone-elses-data).

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `PORTCULLIS_UPSTREAM_URL is not set` | env not exported into the process | `EnvironmentFile` / `set -a; . file` |
| provider returns **404** | wrong upstream path | check the `openai_upstream=` startup line; use a base URL or full endpoint |
| provider returns **400** about a missing header | provider needs a custom header | `PORTCULLIS_UPSTREAM_HEADERS` or `PORTCULLIS_FORWARD_HEADERS` |
| **401/403** returned to the client | the upstream key is wrong | Portcullis passes the provider's status through unchanged |
| **502** immediately | cannot reach the provider; retried once | check DNS/egress and `PORTCULLIS_CONNECT_TIMEOUT_SECS` |
| **502** after a long wait | upstream exceeded `PORTCULLIS_REQUEST_TIMEOUT_SECS` | raise it, or check provider status |
| **413** | request body over 2 MiB | raise `PORTCULLIS_MAX_BODY_BYTES` |
| client sees a literal `<<ORG_1>>` | the model echoed a placeholder in a field we don't rehydrate (we already fixed `content`, `reasoning_content` and tool-call arguments) | open an issue with the provider's response shape |
| `detector disabled` at startup | model dir missing or incomplete | expected if you skip the model; otherwise check `gliner2_config.json` exists |
| **503** from `/teach` | `PORTCULLIS_ADMIN_TOKEN` unset | set it — the admin surface never runs unauthenticated |
| slow first request, fast afterwards | ONNX model cold load (~5 s) once | expected |

---

## Pre-flight checklist

- [ ] `curl /healthz` returns `{"status":"ok"}`
- [ ] a real chat completion succeeds through the proxy
- [ ] `/teach` then a round trip proves the value is rehydrated for the client
- [ ] the startup log shows the upstream URL you expect
- [ ] the bind address is loopback, **or** auth is terminated in front
- [ ] `PORTCULLIS_ADMIN_TOKEN` is set and long
- [ ] the store path is outside the repo, and is `0600`
- [ ] `PORTCULLIS_STORE` and the model are on persistent storage (not a container's ephemeral layer)
