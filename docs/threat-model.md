# Threat model

## What Portcullis protects

Private values in the prompt never leave the machine in the clear. The cloud model sees
stable placeholders (`<<EMAIL_1>>`) instead of real values; real values are restored
locally in the response.

## The crown jewels

`store.json` — the learned entity store — is **the most sensitive artifact**. It is,
effectively, a list of everything you consider private. Therefore:

- it is **`.gitignore`d** and must never be committed
- it must be **encrypted at rest** (M1) and access-controlled
- it must **never be synced** anywhere

The in-memory delta cache stores `sha256(original) -> redacted`; it holds **no raw
originals**, so the cache itself is not a plaintext corpus.

## Trust boundaries

- **Before:** prompt → cloud LLM (provider sees everything).
- **After:** prompt → Portcullis (local) → cloud LLM (provider sees placeholders).
- Portcullis is meant to be run **locally**, so it *subtracts* a party from your data
  path rather than adding one. A hosted Portcullis would defeat the purpose.

## What Portcullis does NOT solve

- **Inference.** `<<ORG_1>> is laying off 30%` still points at the org. Redaction shrinks
  the leak surface; it does not eliminate it.
- **Detection completeness.** A value the detector misses is missed. This is why the
  deterministic store and a second detector matter more than the scan schedule.
- **Local compromise.** If your machine is compromised, the store is readable.
- **Other egress paths.** Search queries, embeddings, STT, browser — separate surfaces,
  not covered here.

## Failure modes we actively defend against

1. **Raw echo** — outbound is never the original segment (invariant 1).
2. **Bypass** — no segment is trusted by position or type (invariant 2).
3. **Stale redaction** — teaching a term invalidates cached redactions (invariant 3).
4. **Silent leak** — a fail-closed outbound assertion blocks a request that still
   contains a protected term or a malformed placeholder (invariant 4).
5. **Prompt-cache sabotage** — redaction is a pure, stable function of the text, and
   unchanged segments are re-emitted byte-identically.
