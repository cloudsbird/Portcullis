# Portcullis

[![CI](https://github.com/cloudsbird/Portcullis/actions/workflows/ci.yml/badge.svg)](https://github.com/cloudsbird/Portcullis/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)

Local-first, teachable PII gateway — redact before the cloud LLM ever sees it.

Portcullis is a reverse proxy that sits between your LLM client and your provider. It
strips private values out of the prompt, forwards the sanitised prompt, and restores the
real values in the reply. The provider sees the task; it never sees the identities — not
as a policy promise, but because those bytes were never sent.

```
  ┌──────────┐   redact    ┌────────────┐   sanitised   ┌───────────┐
  │  client  │────────────▶│ Portcullis │──────────────▶│  provider │
  │          │◀────────────│   :8080    │◀──────────────│           │
  └──────────┘  rehydrate  └────────────┘   completion  └───────────┘
   Hermes,                  local,                       OpenAI,
   Cursor, any              in-process                   OpenRouter,
   OpenAI-style client                                   Ollama, …
```

That upgrades a provider's *"we don't train on your data"* into a structural fact: **the
provider never received it.**

What makes it different from the existing redaction proxies is a **learning loop**. You
teach it your words — a client name, an internal project, a colleague — and it remembers
them for every future request, across sessions. There is no retraining: teaching appends
to a local list, and takes effect on the very next request.

## Table of Contents

- [Security](#security)
- [Background](#background)
- [Install](#install)
- [Usage](#usage)
- [How it works](#how-it-works)
- [Configuration](#configuration)
- [Model selection](#model-selection)
- [Benchmark](#benchmark)
- [Status](#status)
- [Documentation](#documentation)
- [HTTP API](#http-api)
- [Maintainers](#maintainers)
- [Credits](#credits)
- [Contributing](#contributing)
- [License](#license)

## Security

Portcullis is a security tool, so its own posture matters as much as its function.

**The proxy endpoints are deliberately unauthenticated.** `/v1/chat/completions` and
`/v1/messages` accept anyone who can reach the port, and forward using *your* provider
key. Only the admin surface (`/teach`, `/unteach`, `/terms`, `/suggestions`) requires a
bearer token. The intended deployment is loopback, beside your client. Binding it to a
LAN or public interface is a security decision, not a configuration detail — put
authentication in front of it first. See
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md#exposing-it-beyond-loopback).

**The learned store is plaintext unless you set a key.** It is written `0600`, gitignored,
and never leaves the machine — but it is a map of everything you consider private. Set
`PORTCULLIS_STORE_KEY_FILE` to encrypt it at rest (Argon2id + AES-256-GCM); see
[docs/ENCRYPTION.md](docs/ENCRYPTION.md). Either way, treat it as a secret.

**What it does not protect against.** Prompt injection that convinces the model to reveal
a placeholder verbatim, values the detector never recognises, and metadata (timing,
volume, the fact that you redact at all) are all out of scope.
[docs/threat-model.md](docs/threat-model.md) states the boundaries precisely.

To report a vulnerability, please open a private security advisory rather than a public
issue. See [SECURITY.md](SECURITY.md).

## Background

Redaction proxies are not new. Microsoft Presidio, LLM Guard, contextio and pii-proxy all
do broadly this. They share one property: they are **configuration-driven**. To protect a
new term you edit YAML, JSON, or a console, usually out of band, usually as an
administrator.

Portcullis was built around the hypothesis that the missing piece is not detection
quality but **the loop** — the ability to say "this, here, in this conversation, is
private" and have it stick. Everything else follows from that: a deterministic
placeholder scheme so the provider's prompt cache is not invalidated, a delta-scanned
cache so cost tracks new text rather than total context, and four safety invariants
expressed as executable tests.

For the landscape and where the gaps are, see [docs/prior-art.md](docs/prior-art.md).

## Install

### Dependencies

- **Rust** (stable) and `cargo`
- `pkg-config` and `libssl-dev` — required to compile the ONNX bindings, build-time only
- *Optional:* the detector model, ~1.2 GB on disk and ~1.6 GB resident. **Not required** —
  without it Portcullis runs the deterministic layers alone, in ~13 MB of RAM.

```bash
git clone https://github.com/cloudsbird/Portcullis
cd Portcullis

# with the ML detector (default configuration)
cargo build --release --features onnx

# or the lightweight, deterministic-only build
cargo build --release
```

The release binary is ~30 MB and links only libc/libstdc++ — ONNX Runtime is statically
linked, so there is no shared library to install.

### The detector model

Gitignored, because it is 1.2 GB. Export it from the upstream checkpoint with
[`lmoe/gliner2-onnx`](https://github.com/lmoe/gliner2-onnx):

```bash
git clone https://github.com/lmoe/gliner2-onnx && cd gliner2-onnx
make onnx-export MODEL=fastino/gliner2-privacy-filter-PII-multi
# → model_out/gliner2-privacy-filter-PII-multi/
```

Point `PORTCULLIS_MODEL_DIR` at the result. Skipping this is a supported configuration,
not a degraded one — see [Model selection](#model-selection).

### Docker

```bash
docker build -f deploy/Dockerfile -t portcullis .
```

Also available: `deploy/docker-compose.yml` and a hardened `deploy/portcullis.service`
systemd unit. The image has not been build-tested in CI (no Docker daemon in the
development environment); the underlying facts it relies on have been verified with `ldd`.

## Usage

### CLI

The fastest way to see what the provider would receive:

```bash
cargo build --release

# teach a term it must always hide
portcullis --store ./store.json teach Cartalian --label ORG

# print the redacted form, then the rehydrated form
portcullis --store ./store.json scan \
  "Email daniel.pratt@northwind-logistics.com about Cartalian."
```

```
-- what the cloud model sees --
Email <<EMAIL_1>> about <<ORG_1>>.
-- rehydrated (what you see) --
Email daniel.pratt@northwind-logistics.com about Cartalian.
```

`store.json` is a map of everything you consider private. It is `.gitignore`d by default;
keep it out of version control.

### As a proxy

```bash
export PORTCULLIS_UPSTREAM_URL=https://openrouter.ai/api/v1
export PORTCULLIS_UPSTREAM_KEY=sk-or-v1-...
export PORTCULLIS_ADMIN_TOKEN=$(openssl rand -hex 24)
export PORTCULLIS_STORE=/var/lib/portcullis/store.json

portcullis serve --bind 127.0.0.1:8080
```

Then point your client at it. The client never learns the real provider URL or key.

```yaml
# Hermes — config.yaml
custom_providers:
  portcullis:
    base_url: http://127.0.0.1:8080/v1
    api_key: not-used-locally
    models: [openai/gpt-4o-mini]
```

```bash
# Claude Code
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080
export ANTHROPIC_API_KEY=not-used-locally
```

`PORTCULLIS_UPSTREAM_URL` accepts either a base URL or a full endpoint. The resolved URL
is printed at startup, which is the first thing to check if the provider returns a 404.
Supported clients and their exact settings: [docs/CLIENTS.md](docs/CLIENTS.md).

### Teaching it at runtime

```bash
curl -X POST http://127.0.0.1:8080/teach \
  -H "Authorization: Bearer $PORTCULLIS_ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"term":"Project Loki","label":"ORG"}'
```

Add `"whole_word": true` (CLI: `--whole-word`) so a short term such as `Ann` only matches as a
whole word and leaves `Announcement` alone.

`/suggestions` returns terms the detector found but that you have not taught — the review
queue for keeping the store current. Teaching clears the delta cache, so it applies
immediately, including to messages the provider has already seen.

## How it works

Four invariants are enforced as tests in
[`tests/invariants.rs`](tests/invariants.rs). If they go red, Portcullis can leak.

1. **Never echo a raw segment.** Outbound bytes are either freshly redacted or the cached
   *redacted* form of a segment seen before — never the original.
2. **No bypass.** Every outbound segment was scanned: now, or its exact bytes earlier. A
   system prompt carries injected memory, so it holds PII too.
3. **Invalidate on policy change.** `teach` and `unteach` clear the delta cache. A
   redaction cached before a term was taught would otherwise leak that term forever.
4. **Fail closed.** The assembled payload is asserted clean before it leaves. If any
   detector errors, the prompt is not sent.

Detection runs in three layers, merged with a fixed priority (dictionary → regex → model),
so the ML layer can only *add* recall and can never override a deterministic match.

LLM APIs are stateless: every turn, the client re-sends the whole conversation. Portcullis
scans only the **delta** and re-emits unchanged segments **byte-identically**, so it stays
invisible to the provider's prompt cache — deterministic redaction saves money instead of
costing it. The same value always maps to the same placeholder, which also keeps the model
coherent.

Full request lifecycle, diagrams and the component map:
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Configuration

Everything is environment variables. The ones that matter most:

| Variable | Default | Purpose |
|---|---|---|
| `PORTCULLIS_UPSTREAM_URL` | *(required)* | the provider to forward to |
| `PORTCULLIS_UPSTREAM_KEY` | — | your real provider key |
| `PORTCULLIS_ADMIN_TOKEN` | — | guards the teaching surface; **unset → 503** |
| `PORTCULLIS_SCOPE_TOKENS` | — | `scope=token,…` — per-client isolation ([SCOPES.md](docs/SCOPES.md)) |
| `PORTCULLIS_STORE` | `store.json` | learned terms, written `0600` |
| `PORTCULLIS_STORE_KEY_FILE` | — | passphrase file that encrypts the store at rest ([ENCRYPTION.md](docs/ENCRYPTION.md)); `PORTCULLIS_STORE_KEY` for inline |
| `PORTCULLIS_MODEL` | — | named detector from the registry |
| `PORTCULLIS_MODEL_DIR` | `./model` | detector directory |
| `PORTCULLIS_THRESHOLD` | `0.5` | detector confidence cut-off |
| `PORTCULLIS_LOG_FORMAT` | text | `json` for structured logs |

Full reference, including timeouts, body limits, header forwarding and the health
endpoint: [docs/OPERATIONS.md](docs/OPERATIONS.md). Provider-by-provider setup:
[docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

## Model selection

Any GLiNER2 checkpoint exported to the fragmented ONNX layout can be selected by name,
and each entry carries the three things that have to agree — model directory, label set,
and threshold:

```json
{
  "default": "general",
  "models": {
    "general": { "dir": "./model", "threshold": 0.5 },
    "recall":  { "dir": "./model", "threshold": 0.3 }
  }
}
```

```bash
portcullis models                  # list what is configured
portcullis --model recall serve    # select one
```

Labels belong to the model: our own benchmark showed the label vocabulary moves the score
more than the weights do, so a preset never separates the two. GLiNER v1 models and
token-classification models are not supported yet and are rejected at load rather than
failing obscurely later. See [docs/MODELS.md](docs/MODELS.md).

## Benchmark

Measured on the
[PII Masking Benchmark](https://huggingface.co/datasets/piimb/pii-masking-benchmark)
(PIIMB) — character-level masking F2, label-agnostic, micro-averaged per task, using the
benchmark's own protocol (the task's label list, threshold 0.3).

| Task | Precision | Recall | **F2** |
|---|---|---|---|
| gretel | 0.887 | 0.945 | **0.933** |
| ai4privacy-en (OpenPII) | 0.756 | 0.908 | **0.873** |
| nemotron-pii | 0.754 | 0.875 | **0.848** |
| privy | 0.316 | 0.823 | **0.623** |
| **Average** | 0.678 | 0.888 | **0.819** |

**Avg F2 = 0.819, which is 4th of 20 published entries.** Across all 20 the range is
0.42–0.89, so read the number against these bands: **≥ 0.87** is SOTA, **≥ 0.83** strong,
**0.72** is the median, **≤ 0.67** the bottom quartile. For calibration, 0.819 beats
`openai/privacy-filter` (0.708) by 0.111 and `knowledgator/gliner-pii-base-v1.0` (0.738) by
0.081, and it wins outright on two of the four tasks against `nvidia/gliner-PII`, the
best-ranked non-clinical entry.

Reproduce it:

```bash
cargo build --release --features onnx
python benchmarks/pii_masking/run.py --binary ./target/release/portcullis \
  --model-dir ./model --sample 2000 --out benchmarks/pii_masking/results.json
```

The scoring implementation is pinned by known-answer tests
([`benchmarks/pii_masking/test_metrics.py`](benchmarks/pii_masking/test_metrics.py)). Raw
output is committed. Full numbers, methodology and limitations:
[benchmarks/RESULTS.md](benchmarks/RESULTS.md).

This benchmarks the **detector**. PIIMB cannot measure what the gateway adds — cache
invalidation, rehydration, fail-closed — which is what `tests/invariants.rs` covers.

## Status

Usable today for a single user, on loopback, in front of your own traffic.

**Verified:** 40 tests by default (**41** with `--features onnx`), run on every push. The
safety contract in `tests/invariants.rs`, the OpenAI and Anthropic proxy paths, SSE
streaming including placeholders split across events, the hardening surface (timeouts,
body limits, health, store permissions), and a golden test proving the Rust ONNX detector
reproduces the reference Python runtime. The full path has also been exercised against a
[**live** provider](scripts/e2e_live.sh), which found and closed two integration gaps that
no mock had caught.

**Not ready to guard someone else's data yet:**

- **The store is encrypted at rest only if you set a key.** Without one it is `0600` plaintext
  JSON. With one, a running process still holds it decrypted in memory
  ([docs/ENCRYPTION.md](docs/ENCRYPTION.md)).
- **Scope isolation is opt-in and is selection, not a boundary.** With per-client tokens
  configured ([docs/SCOPES.md](docs/SCOPES.md)) each client only gets `global` terms plus its
  own; without them every term applies to every request.
- **Only an OpenAI-compatible provider has been exercised live.** The Anthropic
  `/v1/messages` path is mock-tested only.
- **Detection serialises** behind one lock, so throughput is bounded (~3–4 scans/second on
  the reference host; far better in practice because only new text is scanned).
- **No metrics or tracing export** — structured logs only.

All of these are stated in full, with reproduction steps, in
[docs/OPERATIONS.md](docs/OPERATIONS.md#before-this-guards-someone-elses-data).

### Roadmap

| | Milestone |
|---|---|
| ✅ | **M0** — core library and CLI; deterministic dictionary and regex detection; delta-scan cache; stable vault; the four invariants as tests |
| ✅ | **M1** — native in-process ONNX detector, no Python at runtime; OpenAI-compatible proxy that asserts the whole payload before forwarding |
| ✅ | **M2** — in-chat `teach` / `unteach` / `terms` / `suggestions` over HTTP, with cache invalidation and bearer auth |
| ✅ | **M3** — SSE streaming with a placeholder-level carry buffer; the Anthropic Messages API; a per-client setup matrix |
| ✅ | **M4** — packaging: Docker, compose, systemd, and a deployment guide |
| ✅ | **M5** — per-client `scope` enforcement via per-client tokens; multi-tenant isolation demonstrated in `tests/scopes.rs` |
| ✅ | **M6** — encryption at rest for the learned store (Argon2id + AES-256-GCM, opt-in) |
| | **M7** — metrics export (Prometheus/OTLP) |

## Documentation

| Document | What it covers |
|---|---|
| [DEPLOYMENT.md](docs/DEPLOYMENT.md) | the two addresses, provider cheat-sheet, Docker, systemd, troubleshooting |
| [OPERATIONS.md](docs/OPERATIONS.md) | timeouts, limits, health, logging, concurrency, and the remaining gaps |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | request lifecycle, diagrams, the component map |
| [MODELS.md](docs/MODELS.md) | the detector registry, precedence, supported families |
| [SCOPES.md](docs/SCOPES.md) | per-client isolation: tokens, what is and is not isolated |
| [ENCRYPTION.md](docs/ENCRYPTION.md) | encrypting the store: setup, format, rotation, limits |
| [TEACHING.md](docs/TEACHING.md) | how the learning loop works, and what it does not do yet |
| [EXAMPLE.md](docs/EXAMPLE.md) | a real before/after, including the learning loop |
| [RESOURCES.md](docs/RESOURCES.md) | measured CPU, RAM, disk and latency |
| [CLIENTS.md](docs/CLIENTS.md) | Hermes, OpenCode, Claude Code, Cursor, Aider and others |
| [threat-model.md](docs/threat-model.md) | what it does and does not protect |
| [prior-art.md](docs/prior-art.md) | the landscape, and the one gap this fills |

## HTTP API

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `POST` | `/v1/chat/completions` | none | OpenAI-compatible; redact, forward, rehydrate (streaming supported) |
| `POST` | `/v1/messages` | none | Anthropic Messages API equivalent |
| `POST` | `/teach` | bearer | add a term to the store |
| `POST` | `/unteach` | bearer | remove a term |
| `GET` | `/terms` | bearer | list the store |
| `GET` | `/suggestions` | bearer | terms detected but not yet taught |
| `GET` | `/healthz` | none | liveness; returns counts only, never a term or a value |

## Maintainers

[cloudsbird](https://github.com/cloudsbird) ([leunardus.vederis714@gmail.com](mailto:leunardus.vederis714@gmail.com))

## Credits

- [GLiNER2](https://github.com/fastino-ai/GLiNER2) by Fastino AI — the model architecture
- [`fastino/gliner2-privacy-filter-PII-multi`](https://huggingface.co/fastino/gliner2-privacy-filter-PII-multi) — the default detector checkpoint (Apache-2.0)
- [`lmoe/gliner2-onnx`](https://github.com/lmoe/gliner2-onnx) (MIT) — the ONNX export tooling this project depends on
- [PII Masking Benchmark](https://huggingface.co/datasets/piimb/pii-masking-benchmark) — the evaluation suite the numbers above come from (dataset CC BY-NC 4.0)
- [ONNX Runtime](https://github.com/microsoft/onnxruntime), [axum](https://github.com/tokio-rs/axum), [reqwest](https://github.com/seanmonstar/reqwest) — the runtime foundations

## Contributing

Issues and pull requests are welcome. For anything substantial, please open an issue
first so the approach can be agreed before code is written.

```bash
cargo test                                  # 40 tests
cargo build --features onnx
PORTCULLIS_MODEL_DIR=./model cargo test --features onnx    # 41
```

Two conventions worth knowing before you send a patch:

- **Behaviour change means test change.** The safety invariants in `tests/invariants.rs`
  are a contract, not documentation. A change that weakens them will not be merged.
- **Claims need evidence.** If you add a performance or quality number to the docs, add
  the script that reproduces it, as `benchmarks/` and `docs/RESOURCES.md` do.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide.

## License

[Apache-2.0](LICENSE) © Vederis Leunardus
