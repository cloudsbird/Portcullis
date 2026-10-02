# Benchmarks

Reproducible measurements of Portcullis, with the raw scripts and the raw output
checked in alongside the numbers.

## PIIMB — PII Masking Benchmark

**What it measures.** PIIMB scores **zero-shot PII masking** — how well a model masks
PII out of the box, with no fine-tuning and no label customisation. Its ranking metric is
**masking F2**, computed at the **character level**.

Source: [`piimb/pii-masking-benchmark`](https://huggingface.co/datasets/piimb/pii-masking-benchmark)
(CC BY-NC 4.0 — non-commercial).

### The metric (from the benchmark's own spec)

```
precision = |pred ∩ gold| / |pred|          character level, not entity level
recall    = |pred ∩ gold| / |gold|
F1        = 2PR / (P + R)
F2        = 5PR / (4P + R)                  <- the leaderboard metric
FPR       = |pred \ gold| / |non-gold|
```

Overlapping or consecutive spans are merged before scoring. Evaluation is
**label-agnostic** — only character positions count. Scores are **micro-averaged** within
a task; `Avg` is the simple mean across tasks.

### How we run it (matching the benchmark's own protocol)

To stay comparable with the published leaderboard, we run Portcullis the way PIIMB runs
its GLiNER/GLiNER2 entries:

- the detector receives the **task's own label list** (not our built-in set)
- confidence **threshold 0.3** (the benchmark uses 0.3 to prioritise recall)
- all four leaderboard tasks: `ai4privacy-en` (OpenPII), `gretel`, `nemotron-pii`, `privy`
- `sentences` subset, `test` split

### Reproduce

```bash
cargo build --release --features onnx

python benchmarks/pii_masking/run.py \
  --binary ./target/release/portcullis \
  --model-dir ./model \
  --sample 2000 \
  --out benchmarks/pii_masking/results.json
```

Requires Python with `pyarrow` (the script downloads the dataset itself).
`--sample 0` runs the full set (≈9 h on the reference host, ~0.28 s/sentence).

Results are in **[RESULTS.md](RESULTS.md)**; the raw run output is in
[`pii_masking/results.json`](pii_masking/results.json).

### Honest limitations

- **Sample size.** The published run uses a random sample per task, stated in
  RESULTS.md, not the full set.
- **Label vocabulary matters a lot.** Tasks differ in how they name labels — `gretel` and
  `nemotron-pii` use natural names (`first_name`), while `privy` and `ai4privacy-en` use
  ALL-CAPS Presidio-style names (`IBAN_CODE`, `GIVENNAME`). A model trained on natural
  label names is being asked to generalise to abbreviations. This affects every model on
  the leaderboard equally, but it is the dominant factor in the spread we see.
- **Hardware and threshold are stated** with the results; F2 is sensitive to threshold.
- **Honor system.** The split is public; we have not trained or fine-tuned on it.
- This is a *detector* benchmark. It does not measure anything gateway-specific
  (cache invalidation, rehydration, fail-closed), which is what our own invariant tests
  cover.
