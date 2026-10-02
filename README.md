# Portcullis

**The redaction engine that learns your words.**

Portcullis is a local-first PII gateway for cloud LLMs. It sits between your agent and the
model: private details are stripped from the prompt **before it leaves your machine**, and
restored in the answer that comes home.

> The cloud model sees the **task**, never the **identities**.

It upgrades a provider's *"we won't store it"* promise into a structural fact:
**the provider never received it.**

## The gap it fills

Good redaction proxies already exist — Presidio, LLM Guard, contextio, pii-proxy. Every
one of them is **config-driven**: you edit YAML/JSON/console to add a term.

Portcullis is built around the one thing they lack — a **learning loop**. You *teach it
what's sensitive*, and it remembers.

## Design invariants

Enforced as tests in [`tests/invariants.rs`](tests/invariants.rs). If they go red,
Portcullis can leak.

1. **Never echo a raw segment.** Outbound bytes are either freshly redacted or the cached
   *redacted* form of a segment seen before — never the original.
2. **No bypass.** Every outbound segment was scanned: now, or its exact bytes earlier.
   (Your system prompt carries injected memory, so it holds PII too.)
3. **Invalidate on policy change.** `teach` / `unteach` clears the delta cache — a
   redaction cached *before* a term was taught would otherwise leak that term forever.
4. **Fail closed.** The assembled payload is asserted clean before it leaves.

## Why prompt caching matters

Every turn, the client re-sends the whole conversation. Portcullis scans only the
**delta** (what's new) and re-emits unchanged segments **byte-identically**, so it stays
invisible to the provider's prompt cache — deterministic redaction *saves* you money
instead of costing it. Determinism also keeps the model coherent: the same value always
gets the same placeholder.

## Quickstart

```bash
cargo build --release

# teach a term it must always hide
portcullis --store ./store.json teach Cartalian --label ORG

# see exactly what the cloud model would receive
portcullis --store ./store.json scan \
  "Email daniel.pratt@northwind-logistics.com about Cartalian."
```

> Keep `store.json` out of version control — it is a map of everything you consider
> private. It is `.gitignore`d by default.

## Status

- **M0** — core library + CLI · deterministic dictionary + regex detection · delta-scan
  cache · stable vault · the four invariants as passing tests.
- **M1** — native in-process ONNX GLiNER2-PII detector (no Python at runtime) + an
  OpenAI-compatible proxy that redacts every text-bearing field and asserts the whole
  payload before forwarding.
- **M2** — in-chat `teach` / `unteach` / `terms` / `suggestions` over HTTP, with cache
  invalidation and bearer auth.
- **M3** — works with **any** client: SSE streaming (with a placeholder-level carry
  buffer), the Anthropic Messages API (`/v1/messages`), and a per-client setup matrix.
- **M4** — packaging and deployment recipes.

**28 tests**, organised as the contract: `tests/invariants.rs` is the safety contract,
then `proxy`, `m2_teach`, `streaming`, `anthropic` and `onnx_detector`.

## Docs

- [**docs/TEACHING.md**](docs/TEACHING.md) — how the learning loop works, and what it does *not* do yet
- [**docs/EXAMPLE.md**](docs/EXAMPLE.md) — what the provider actually sees (real before/after, including the learning loop)
- [**docs/RESOURCES.md**](docs/RESOURCES.md) — measured CPU, RAM, disk and latency
- [**docs/CLIENTS.md**](docs/CLIENTS.md) — Hermes, OpenCode, Claude Code, Cursor, Aider and more
- [docs/prior-art.md](docs/prior-art.md) — the landscape, and the one gap this fills
- [docs/threat-model.md](docs/threat-model.md) — what it does and does not protect
- [docs/M1-BRIEF.md](docs/M1-BRIEF.md) — the implementation brief

## License

Apache-2.0.
