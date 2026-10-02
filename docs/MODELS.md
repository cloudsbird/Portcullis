# Choosing a model

Portcullis is not tied to one detector. Any GLiNER2 checkpoint can be dropped in, and
each one can carry its own label set and threshold.

## Why this matters — our own numbers first

We benchmarked the default detector on PIIMB (see [`../benchmarks/RESULTS.md`](../benchmarks/RESULTS.md)):

| Task | Label style | Our F2 |
|---|---|---|
| gretel | natural names (`first_name`, `account_number`) | **0.933** |
| nemotron-pii | natural names | **0.848** |
| ai4privacy-en | ALL-CAPS (`GIVENNAME`, `TELEPHONENUM`) | **0.873** |
| privy | Presidio-style (`IBAN_CODE`, `US_SSN`) | **0.623** |

Same model, same weights — **the label vocabulary moves the score more than the model
does.** And across the 20 published PIIMB entries scores range from 0.42 to 0.89: a 44M
clinical model beats a 434M general one on clinical text, and the smallest entries run in
a fraction of the memory.

So there are two real reasons to switch: **domain fit**, and **resource budget**.

## The registry

Model selection is a small JSON file, looked for at `portcullis.models.json` (override
with `--models` or `PORTCULLIS_MODELS`). Start from
[`../portcullis.models.example.json`](../portcullis.models.example.json):

```json
{
  "default": "general",
  "models": {
    "general": {
      "dir": "./model",
      "labels": ["person", "full_name", "email", "phone_number"],
      "threshold": 0.5,
      "description": "GLiNER2-PII (307M). Balanced default."
    },
    "clinical": {
      "dir": "./models/openmed-clinical",
      "threshold": 0.3
    }
  }
}
```

Each entry carries **three things that must agree**: `dir`, `labels`, `threshold`. That is
the point of bundling them — a model trained on `first_name` should not be asked for
`GIVENNAME`, and a recall-oriented model should not be run at a precision-oriented
threshold.

| Field | Required | Meaning |
|---|---|---|
| `dir` | yes | directory holding the ONNX graphs + tokenizer |
| `labels` | no | label names to ask for; omitted → the detector default |
| `threshold` | no | confidence cut-off; omitted → 0.5 |
| `description` | no | shown by `portcullis models` |
| `family` | no | defaults to `gliner2`; anything else is rejected at load |

## Selecting one

```bash
portcullis models                       # list what the registry defines
portcullis --model clinical serve       # one invocation
PORTCULLIS_MODEL=clinical portcullis serve   # every invocation
```

`portcullis models` prints each entry with its directory, label count, threshold and
description, and marks the default:

```
registry: ./portcullis.models.json

  general  (default)
      dir:       ./model
      labels:    4 (person, full_name, email, phone_number)
      threshold: 0.5
      GLiNER2-PII (307M). Balanced default.
```

If no registry exists, the command says so and shows how to point at a directory
directly instead — it never fails silently.

## Precedence

Highest wins:

1. `--model NAME` / `PORTCULLIS_MODEL=NAME` → that registry entry (`dir` + `labels` + `threshold`)
2. `--model-dir DIR` / `PORTCULLIS_MODEL_DIR=DIR` → that directory, with `--labels` /
   `PORTCULLIS_LABELS` and `--threshold` / `PORTCULLIS_THRESHOLD`
3. otherwise `./model` with detector defaults

Explicit flags override a preset:

```bash
# preset supplies the dir and labels; the flag tightens the threshold
portcullis --model general --threshold 0.7 scan "..."
```

Naming a model that does not exist is an **error that lists the alternatives**, and
naming one with no readable registry fails immediately — rather than quietly falling back
to a different model than you asked for.

## What is supported today

**Family `gliner2` only** — a GLiNER2 checkpoint exported to the fragmented ONNX layout:

```
dir/
├── encoder.onnx        encoder graph
├── span_rep.onnx       span representation graph
├── count_embed.onnx    label-embedding transform
├── tokenizer.json
└── gliner2_config.json max_width + special_tokens + onnx file map
```

Other families are **rejected at registry load**, with a clear message, so you get told up
front instead of hitting a confusing shape error at inference.

Not yet supported, and honestly not free to add:

| Family | Why not |
|---|---|
| GLiNER **v1** (`nvidia/gliner-PII`, `urchade/gliner_multi_pii-v1`) | a fused single-graph export and a different decode path |
| Token-classification models (BERT/DeBERTa PII fine-tunes) | different output shape entirely — token labels, not spans |

Today the practical set of swappable models is **GLiNER2 checkpoints** (e.g. the
`fastino/gliner2-*` and `lmoe/gliner2-*-onnx` families). Export tooling is upstream
([`lmoe/gliner2-onnx`](https://github.com/lmoe/gliner2-onnx), MIT).

## Adding a model — checklist

1. Export the checkpoint to the GLiNER2 ONNX layout above.
2. Add an entry to the registry: `dir`, `labels` (the names **that model** was trained
   on), and a `threshold`.
3. Sanity-check it end to end:
   ```bash
   portcullis --model mymodel scan "Email daniel@example.com about Cartalian."
   ```
4. If it matters, benchmark it:
   ```bash
   python benchmarks/pii_masking/run.py --binary ./target/release/portcullis \
     --model-dir ./models/mymodel --sample 500
   ```

## Resource impact

Model choice changes the memory profile directly — see
[`RESOURCES.md`](RESOURCES.md). The default 307M detector is ~1.58 GB resident; a small
export can be tens of megabytes. On a constrained host, pick a smaller model, or run the
deterministic dictionary + regex layers alone (13 MB) and skip the ONNX detector entirely.
