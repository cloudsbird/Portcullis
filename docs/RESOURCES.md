# Resource requirements

All numbers below are **measured on this host** — a 4 vCPU AMD EPYC 9634 with 7.8 GB
RAM and AVX-512, no GPU. Reproduce with [`scripts/measure.py`](../scripts/measure.py)
and [`scripts/measure_server.sh`](../scripts/measure_server.sh).

## Two profiles

Portcullis runs in one of two configurations. The difference is entirely the detector
stack: the dictionary + regex layers are free, the ML model is not.

| Profile | Build | Steady RSS | Peak RSS | Needs a model? |
|---|---|---|---|---|
| **Deterministic only** — dictionary (taught terms) + regex (email/phone/card/IP) | `cargo build --release` | **13 MB** | 13 MB | no |
| **Full** — adds the GLiNER2-PII ONNX detector | `cargo build --release --features onnx` | **~1.58 GB** | **~1.84 GB** | yes, ~1.2 GB on disk |

Measured `VmRSS` of the running proxy at steady state:
`13,500 kB` (deterministic) vs `1,614,164 kB` (full). Peak `VmHWM` for the full
profile was `1,879,452 kB` — that is the number to budget for.

> **There is no GPU requirement.** Everything runs on CPU.

## Disk

| Artifact | Size |
|---|---|
| `portcullis` release binary (stripped, LTO) | ~29 MB |
| Model — `encoder.onnx` | 1.1 GB |
| Model — `span_rep.onnx` | 64 MB |
| Model — `count_embed.onnx` | 41 MB |
| Model — `classifier.onnx` (classification only; not used for NER) | 4.6 MB |
| **Model total** | **~1.2 GB** |

## CPU and latency

The deterministic layer is effectively free (microseconds). The ONNX encoder is the
entire CPU cost.

**Cold start** (loading the model into the process): **~5.3 s** on this host. A
one-shot `portcullis scan` pays this every time; the `serve` proxy pays it **once** and
stays warm.

**Per-scan latency, warm, CPU** (GLiNER2-PII, ~7 labels):

| Input length | Latency (p50) |
|---|---|
| ~60 chars | ~135 ms |
| ~220 chars | ~200 ms |
| ~500 chars | ~330 ms |
| ~1000 chars | ~590 ms |
| ~2000 chars | ~1.9 s |
| ~4000 chars | ~2.9 s |

Scaling is roughly **O(L^1.3)** — superlinear but not quadratic. Label count is nearly
free (2 vs 8 labels at 1000 chars: 531 ms vs 586 ms).

**The design keeps this off the hot path.** API clients are stateless: every turn
re-sends the whole conversation. Portcullis scans only the **delta** (new segments) and
re-emits unchanged ones byte-identically, so a 12-turn conversation costs about a
quarter of a naive full rescan (measured 4.0× faster: 15.7 s → 4.0 s cumulative).

## Sizing guidance

| Deployment | vCPU | RAM | Disk |
|---|---|---|---|
| Deterministic layer only | 1 | **64 MB** | 40 MB |
| Full (ONNX detector) | **2** (4 comfortable) | **2 GB** to be safe (peak observed 1.84 GB) | **1.3 GB** |

Notes:

- ONNX Runtime parallelises intra-op work across cores; more cores help throughput, not
  much single-request latency. Bound it with `OMP_NUM_THREADS` if you share the host.
- Budget the RAM for the **process**; the provider client (Hermes, Claude Code) is
  separate.
- If RAM is tight, run the deterministic profile only — it still covers your taught
  terms (the exact guarantee) plus structured PII, at 13 MB.
- Only one model load happens per process, regardless of request count.
