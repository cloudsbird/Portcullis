# Threat model

## What Portcullis protects

Private values in the prompt never leave the machine in the clear. The cloud model sees
stable placeholders (`<<EMAIL_1>>`) instead of real values; real values are restored
locally in the response.

## The crown jewels

`store.json` — the learned entity store — is **the most sensitive artifact**. It is,
effectively, a list of everything you consider private. Therefore:

- it is **`.gitignore`d** and must never be committed
- it should be **encrypted at rest** ([ENCRYPTION.md](ENCRYPTION.md)) and access-controlled
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
   The assertion checks **taught terms only**. It deliberately does not re-run the regex
   or ML detectors over the assembled body: JSON is full of numbers (timestamps, token
   counts) that look like phone numbers, so it would reject ordinary requests. A value
   that only a detector could have caught, and missed, is not stopped here.
   Text that is JSON (tool results, tool-call arguments) is also checked in decoded
   form, so a term after an escaped newline or written with `\u` escapes is caught.
   Binary payloads (base64 images, audio, files) are left out of the check: they are
   forwarded unchanged and were never redacted, and a megabyte of base64 contains almost
   any short term by chance. Text *inside* an image is therefore not protected.
   "Malformed placeholder" means a placeholder-shaped `<<LABEL_N` with no `>>`; a bare
   `<<` (a shell heredoc) is ordinary text.
   An ML detector **failure** (as opposed to a miss) is fail-open by default: the
   segment keeps dictionary and regex coverage, the failure is counted in
   `portcullis_detector_errors_total`, and the scan is not cached so the next turn
   retries it. `PORTCULLIS_DETECTOR_FAIL_CLOSED=1` blocks the request instead.
5. **Prompt-cache sabotage** — redaction is a pure, stable function of the text, and
   unchanged history is redacted and numbered identically on every turn.
