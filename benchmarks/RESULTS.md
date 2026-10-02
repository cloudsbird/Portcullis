# Benchmark results

## PIIMB — PII Masking Benchmark

> **Avg F2 = 0.819** — rank **#4 of 20** published entries.

Detector: `fastino/gliner2-privacy-filter-PII-multi` (307M), run through Portcullis's
in-process ONNX detector. CPU only, no GPU.

Setup: `sentences` / `test` subset, **2,000-sentence random sample per task** (seed 13),
confidence **threshold 0.3**, and the **task's own label list** — exactly how PIIMB runs
its GLiNER/GLiNER2 entries. Metric: character-level, label-agnostic, spans merged before
scoring, micro-averaged per task.

### Results

| Task | n | Precision | Recall | F1 | **F2** | FPR |
|---|---|---|---|---|---|---|
| gretel | 2,000 | 0.887 | 0.945 | 0.915 | **0.933** | 0.044 |
| ai4privacy-en (OpenPII) | 2,000 | 0.756 | 0.908 | 0.825 | **0.873** | 0.084 |
| nemotron-pii | 2,000 | 0.754 | 0.875 | 0.810 | **0.848** | 0.045 |
| privy | 2,000 | 0.316 | 0.823 | 0.457 | **0.623** | 0.135 |
| **Average** | | **0.678** | **0.888** | **0.752** | **0.819** | |

Raw output: [`pii_masking/results.json`](pii_masking/results.json).

### Where that ranks

Against the 20 entries published on the PIIMB leaderboard:

| Rank | Model | Avg F2 |
|---|---|---|
| 1 | OpenMed-PII-SuperClinical-Large-434M | 0.887 |
| 2 | OpenMed-PII-SuperClinical-Small-44M | 0.870 |
| 3 | nvidia/gliner-PII | 0.831 |
| **4** | **Portcullis (fastino/gliner2-privacy-filter-PII-multi)** | **0.819** ⬅ |
| 5 | knowledgator/gliner-pii-large-v1.0 | 0.814 |
| 6 | OpenMed/privacy-filter-nemotron | 0.783 |
| … | *(14 more entries down to 0.420)* | |

Notable comparisons:

- **`openai/privacy-filter` — 0.708.** We are **+0.111** above it.
- **`knowledgator/gliner-pii-base-v1.0` — 0.738.** We are **+0.081** above it.
- **`urchade/gliner_multi_pii-v1` — 0.711.** We are **+0.108** above it.
- **`nvidia/gliner-PII` — 0.831.** Just **−0.012** below the top non-clinical entry.

### How to read these numbers

Character-level **F2** weights recall twice as heavily as precision, because *missing* PII
is worse than over-masking. The distribution across all 20 published entries:

| Band | Avg F2 | Meaning |
|---|---|---|
| SOTA | **≥ 0.87** | top 2 (both domain-tuned clinical models) |
| Strong | **≥ 0.83** | top 3–4 |
| Upper quartile | ≥ 0.78 | |
| Median | **0.72** | half the field is here or below |
| Bottom quartile | ≤ 0.67 | |

So **0.819 is a "strong" score** — top-quartile comfortably, within 1.2 points of the best
general-purpose model, and well clear of the median.

### Per-task comparison vs the #3 model

We win two of four tasks outright:

| Task | Portcullis | nvidia/gliner-PII | Δ |
|---|---|---|---|
| gretel | **0.933** | 0.920 | **+0.013** |
| nemotron-pii | **0.848** | 0.772 | **+0.076** |
| ai4privacy-en | 0.873 | **0.918** | −0.045 |
| privy | 0.623 | **0.714** | −0.091 |

Its higher average comes from being better on exactly the two tasks where we are weakest,
not from winning everywhere.

### The finding worth more than the score

The spread here is driven mostly by **label vocabulary**, not detection capability:

| Task | Label style | Examples | Our F2 |
|---|---|---|---|
| gretel, nemotron-pii | natural names | `first_name`, `account_number`, `street_address` | **0.933 / 0.848** |
| ai4privacy-en, privy | ALL-CAPS / Presidio style | `GIVENNAME`, `TELEPHONENUM`, `IBAN_CODE`, `US_SSN` | **0.873 / 0.623** |

The model was trained on natural-language label names, and is being asked to generalise to
abbreviations. Every GLiNER entry on the leaderboard faces the same handicap — but the
practical lesson is that **choosing the right label names moves the score more than
choosing a different model would**. That is why label sets are configurable
(`PORTCULLIS_LABELS`) and why label presets are the next thing worth building.

### Privacy→recall trade-off, stated plainly

Recall is high across the board (0.823–0.945) and we deliberately run at threshold 0.3 to
prioritise it. The cost is precision — worst on privy (0.316), where the model masks too
much. In Portcullis's actual design that is the *safe* direction of error: over-masking a
span sends a placeholder the model can work around, whereas under-masking sends a real
identity to the provider. The gateway's rehydration and fail-closed assertion mean a
false positive costs coherence, not privacy.

### Limitations

- **Sampled, not exhaustive.** 2,000 of 18,538 / 19,601 / 77,907 / 5,363 sentences per
  task. `--sample 0` runs the full set (≈9 h on the reference host).
- **Threshold-sensitive.** 0.3 is PIIMB's recall-oriented setting; higher thresholds raise
  precision and lower recall.
- **Hardware:** 4 vCPU AMD EPYC 9634, no GPU. Latency is in
  [`../docs/RESOURCES.md`](../docs/RESOURCES.md).
- **Detector only.** PIIMB cannot measure what the *gateway* adds — cache invalidation,
  rehydration, fail-closed. Those are covered by `tests/invariants.rs`.
- **Honor system.** The test split is public; we have not trained or fine-tuned on it.
- **Not an official leaderboard submission.** We have not contacted the PIIMB maintainers;
  this is our own reproducible run of their published metric on their published data.
- Dataset licence is **CC BY-NC 4.0** (non-commercial).
