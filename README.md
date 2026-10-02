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

## Benchmark

On the **[PII Masking Benchmark](https://huggingface.co/datasets/piimb/pii-masking-benchmark)**
(PIIMB) — character-level masking **F2**, label-agnostic, micro-averaged per task:

> ## Avg F2 = 0.819 — **4th of 20 published entries**

| Task | Precision | Recall | **F2** |
|---|---|---|---|
| gretel | 0.887 | 0.945 | **0.933** |
| ai4privacy-en (OpenPII) | 0.756 | 0.908 | **0.873** |
| nemotron-pii | 0.754 | 0.875 | **0.848** |
| privy | 0.316 | 0.823 | **0.623** |
| **Average** | 0.678 | 0.888 | **0.819** |

**What counts as a good value.** Across all 20 published entries the range is
**0.42 – 0.89**, so read it as:

| Band | Avg F2 |
|---|---|
| SOTA | **≥ 0.87** |
| Strong | **≥ 0.83** |
| Upper quartile | ≥ 0.78 |
| **Median** | **0.72** |
| Bottom quartile | ≤ 0.67 |

**Where 0.819 lands: top quartile, within 1.2 points of the best general-purpose model.**
For calibration it beats `openai/privacy-filter` (**0.708**) by 0.111, and
`knowledgator/gliner-pii-base-v1.0` (**0.738**) by 0.081.

It also wins outright on two of the four tasks — gretel (**0.933** vs 0.920) and
nemotron-pii (**0.848** vs 0.772) — against `nvidia/gliner-PII`, the highest-ranked
non-clinical entry.

Reproduce:

```bash
cargo build --release --features onnx
python benchmarks/pii_masking/run.py --binary ./target/release/portcullis \
  --model-dir ./model --sample 2000 --out benchmarks/pii_masking/results.json
```

Full numbers, methodology and honest limitations: **[benchmarks/RESULTS.md](benchmarks/RESULTS.md)**.

> This benchmarks the **detector**. PIIMB cannot measure what the gateway adds — cache
> invalidation, rehydration, fail-closed. Those are covered by `tests/invariants.rs`.

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

**Production readiness.** The operational gaps are closed: upstream connect/read/request
timeouts (with streaming handled separately), a request-body cap, an unauthenticated
`/healthz` that leaks nothing, detection moved off the async worker thread, structured
logs that never contain prompt content, and a store written `0600`.

**What is still missing before this should guard someone else's data:** the store is not
encrypted at rest, `scope` is recorded but not enforced, and only an OpenAI-compatible
provider has been exercised live — the Anthropic path is still mock-only. Those are stated
in full in [`docs/OPERATIONS.md`](docs/OPERATIONS.md#before-this-guards-someone-elses-data).

The path has been exercised end to end against a **live** provider
([`scripts/e2e_live.sh`](scripts/e2e_live.sh)), which found and closed two real integration
gaps: `reasoning_content` was not rehydrated, and providers requiring a custom header could
not be reached at all.

### Milestones

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

- [**docs/ARCHITECTURE.md**](docs/ARCHITECTURE.md) — how a prompt flows end to end (diagrams)
- [**docs/MODELS.md**](docs/MODELS.md) — choosing a detector: the registry, precedence, and what's supported
- [**docs/TEACHING.md**](docs/TEACHING.md) — how the learning loop works, and what it does *not* do yet
- [**docs/EXAMPLE.md**](docs/EXAMPLE.md) — what the provider actually sees (real before/after, including the learning loop)
- [**docs/RESOURCES.md**](docs/RESOURCES.md) — measured CPU, RAM, disk and latency
- [**docs/CLIENTS.md**](docs/CLIENTS.md) — Hermes, OpenCode, Claude Code, Cursor, Aider and more
- [docs/prior-art.md](docs/prior-art.md) — the landscape, and the one gap this fills
- [docs/threat-model.md](docs/threat-model.md) — what it does and does not protect
- [docs/M1-BRIEF.md](docs/M1-BRIEF.md) — the implementation brief

## License

Apache-2.0.
