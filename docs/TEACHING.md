# How teaching works

Teaching is the thing that makes Portcullis different from every other redaction proxy.
This page explains exactly what happens — and, in the last section, what it does **not**
do yet.

## The one-sentence version

**Teaching adds a term to a local list. Nothing retrains, nothing is uploaded, and it
takes effect on the very next request.**

## Why that is enough

There is no model training anywhere in the loop. Portcullis detects in three layers:

| Layer | Catches | Nature |
|---|---|---|
| **Dictionary** (what you taught) | exactly your terms, and only those | **exact, deterministic** |
| **Regex** | email, phone, card-like, IP | exact for known shapes |
| **ONNX model** | names, orgs, addresses you never mentioned | best-effort recall |

The model is the *fuzzy* layer — it guesses. The dictionary is the *guarantee*. When you
teach a term you are not teaching the model; you are adding a rule to the layer that
cannot be wrong. That is why the effect is instant and total, and why a taught term can
never be "forgotten" the way a fine-tune can degrade.

## What happens, step by step

When you teach `Aerolith`:

1. **It goes into the deny list.** `store.deny` gains an entry. Matching is
   case-insensitive, and teaching is **idempotent** — teaching the same term again just
   updates its label rather than duplicating it.
2. **The delta cache is cleared.** This is the important one (design invariant 3). Every
   turn, API clients re-send the whole conversation, and Portcullis caches the redacted
   form of segments it has already scanned. A redaction cached *before* you taught a term
   would otherwise keep leaking it forever. So `teach` wipes the cache — and that is why
   teaching affects **messages already in the conversation**, not just new ones.
3. **The store is written to disk** (`PORTCULLIS_STORE`, default `store.json`), so it
   survives a restart.
4. **From the next request**, every occurrence is replaced with a stable placeholder
   (`<<ORG_1>>`) and rehydrated locally in the reply. The provider never sees the term.

## Three ways to teach it

**1. CLI**

```bash
portcullis --store ./store.json teach Aerolith --label ORG
portcullis --store ./store.json unteach Aerolith
```

**2. HTTP** (bearer-guarded; the endpoint returns `503` if no token is configured, so it
is never accidentally open)

```bash
TOKEN="$PORTCULLIS_ADMIN_TOKEN"
curl -sX POST http://127.0.0.1:8080/teach \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"term":"Aerolith","label":"ORG","scope":"global"}'
```

**3. From what it already found** — see the next section.

## The learning loop (`suggestions`)

Teaching is only useful if you notice what to teach. Portcullis records, during
redaction, every value caught by a **non-dictionary** layer (regex or the model) that it
does not already know about.

```bash
curl -s http://127.0.0.1:8080/suggestions -H "Authorization: Bearer $TOKEN"
```

The buffer is bounded (200 entries) and de-duplicated — re-seeing a term moves it to the
back rather than filling the queue.

The workflow:

```
  send a normal request
        │
        ▼
  GET /suggestions        ← "here is what I hid that you never told me about"
        │
        ▼
  POST /teach  …          ← confirm the ones that matter
        │
        ▼
  they are now deterministic, labelled correctly, forever
```

Concretely, from a real run — you mention a project you never taught:

| | model receives | note |
|---|---|---|
| before teaching | `<<FULL_NAME_1>>` | the model caught it anyway — but **guessed the label** |
| after `POST /teach … ORG` | `<<ORG_1>>` | deterministic, correctly labelled |

The model's recall is a safety net; teaching converts a lucky guess into a guarantee.

## The store file

```json
{
  "deny": [
    {
      "term": "Aerolith",
      "label": "ORG",
      "scope": "global",
      "aliases": [],
      "source": "user"
    }
  ],
  "allow": []
}
```

- **`deny`** — terms to hide. This is what teaching writes to.
- **`allow`** — terms to leave alone even if a detector flags them (an override).
  *There is no API or CLI for this yet* — it is edited by hand in the JSON.
- **`store.json` is the crown jewels.** It is a map of everything you consider private.
  It is `.gitignore`d by default. Keep it that way.

## What teaching does NOT do

Being precise here matters more than looking complete.

| Not done | Detail |
|---|---|
| **Retrain anything** | By design. No model weights change; teaching is a list append. |
| **Encrypt the store** | `store.json` is **plaintext JSON** on disk today. Protect it with filesystem permissions, and keep it off version control. |
| **Enforce `scope`** | `scope` (`global` / `client` / `session`) is **stored and displayed, but not applied** — every taught term currently affects every request. Scoping is a schema slot, not a behaviour yet. |
| **Populate `aliases`** | The lookup honours aliases, but nothing can add one — `teach` always writes an empty list, and there is no CLI/HTTP surface for it. Teach each spelling for now. |
| **Catch variants automatically** | `Aerolith` and `Aerolith Inc.` are different strings. Teach both, or wait for fuzzy expansion (below). |
| **Fuzzy/trained expansion** | The planned tiers — expansion via local embeddings, then a LoRA fine-tune of the detector — are **not implemented**. |

## Status, honestly

| Capability | State |
|---|---|
| `teach` / `unteach` via CLI and HTTP | ✅ implemented, tested |
| Instant effect via cache invalidation | ✅ implemented, tested (`teach_invalidates_cache`, `teach_over_http_invalidates_delta_cache`) |
| Persistence to `store.json` | ✅ implemented |
| Auto-suggest of untaught detections | ✅ implemented, tested |
| Bearer auth on the learning surface | ✅ implemented, tested |
| Re-teach updates a label | ✅ implemented |
| `scope` enforcement | ❌ stored only |
| `aliases` API | ❌ plumbing only |
| Store encryption at rest | ❌ plaintext |
| Embedding expansion / fine-tuning | ❌ not started |

The honest summary: **the guarantee layer works today; the convenience layers are
roadmap.**
