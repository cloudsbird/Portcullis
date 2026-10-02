# M1 Brief — the detector boundary and the proxy

> Read this before touching M1. It carries the context the code alone doesn't.

## What Portcullis is

A local-first, teachable PII gateway. The cloud model must see the **task**, never the
**identities**. See [`README.md`](../README.md) and [`threat-model.md`](threat-model.md).

## The contract — do not weaken

[`tests/invariants.rs`](../tests/invariants.rs) holds four invariants. They **are** the
safety contract. M1 must keep them passing and must not change their meaning:

1. **Never echo a raw segment** — outbound is freshly-redacted or cached-redacted.
2. **No bypass** — every outbound segment was scanned (now, or its exact bytes earlier).
3. **Invalidate on policy change** — `teach` / `unteach` clears the delta cache.
4. **Fail closed** — `assert_clean` runs before anything leaves.

## Measured facts to design around (do not re-derive)

- GLiNER2-PII (`fastino/gliner2-privacy-filter-PII-multi`, 307M, Apache-2.0) runs **CPU-only**:
  ~160 ms @80 chars · ~330 ms @500 · ~1.2 s @2000 · ~2.9 s @4000 (≈ **O(L^1.3)**).
- **Design rule: scan the DELTA, not the context.** API clients resend the whole
  conversation every turn; only genuinely new segments need scanning. (Measured: 4× faster
  at 12 turns, and the gap widens.)
- Redaction must be **deterministic and stable** — same value → same placeholder — because
  that preserves the provider's prompt cache *and* keeps the model coherent across turns.
- The ONNX export is **fragmented** into encoder / span_rep / count_embed / classifier and
  needs decode logic. In Rust, `gliner2-rs` (on `ort`) already implements that decode.

## M1 deliverables

1. **`detect`** — a GLiNER2-PII detector implementing the existing `Detector` trait, using
   `ort`, behind a cargo feature (`onnx`) so the core still builds without it.
   The deterministic dictionary layer stays **first**.
2. **`proxy`** — an OpenAI-compatible `POST /v1/chat/completions` (axum):
   intercept → [`Gateway::process`] each message segment → `assert_clean` → forward upstream
   → stream back → `rehydrate`.
3. **`teach`** — an HTTP surface for `teach` / `unteach` / auto-suggest, wired to cache
   invalidation.
4. **Tests** — extend `tests/invariants.rs` to cover the proxy path end-to-end.

## Non-negotiables

- No raw segment ever leaves (invariant 1).
- `assert_clean` runs **before** forwarding (invariant 4).
- A policy change **invalidates** the cache (invariant 3).
- It fails **closed**, never open.
