#!/usr/bin/env python3
"""PIIMB masking metrics for Portcullis.

Implements the PII Masking Benchmark's ranking metrics exactly as specified at
https://huggingface.co/datasets/piimb/pii-masking-benchmark#metrics

    precision = |pred AND gold| / |pred|      (character level)
    recall    = |pred AND gold| / |gold|
    F1        = 2PR / (P + R)
    F2        = 5PR / (4P + R)                <- the leaderboard metric
    FPR       = |pred minus gold| / |non-gold|

Notes on fidelity:
  * Character level, not entity level.
  * Overlapping or consecutive spans are merged before scoring.
  * Evaluation is LABEL-AGNOSTIC: only character positions count.
  * Micro-averaged within a task (counts accumulated across documents).
  * The detector receives the task's own label list, and runs at threshold 0.3,
    matching how the benchmark runs GLiNER/GLiNER2 models.

Usage:
    python run.py --binary ./target/release/portcullis \\
                  --model-dir ./model --sample 2000 --out results.json
"""

import argparse
import json
import os
import random
import subprocess
import sys
import tempfile
import time
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path

PARQUET_URL = (
    "https://huggingface.co/api/datasets/piimb/pii-masking-benchmark"
    "/parquet/sentences/test/0.parquet"
)
DEFAULT_TASKS = ["ai4privacy-en", "gretel", "nemotron-pii", "privy"]


def ensure_parquet(path: Path) -> Path:
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        print(f"downloading {PARQUET_URL}")
        urllib.request.urlretrieve(PARQUET_URL, path)
    return path


def load_rows(path: Path):
    import pyarrow.parquet as pq

    table = pq.read_table(path, columns=["uid", "task_name", "text", "entities"])
    return table.to_pylist()


def merge_spans(spans):
    """Merge overlapping or consecutive [start, end) spans (PIIMB does this)."""
    spans = sorted((int(s), int(e)) for s, e in spans if int(e) > int(s))
    out = []
    for s, e in spans:
        if out and s <= out[-1][1]:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return out


def mask_chars(text_len, spans):
    m = bytearray(text_len)
    for s, e in spans:
        s = max(0, s)
        e = min(text_len, e)
        for i in range(s, e):
            m[i] = 1
    return m


def run_detector(binary, model_dir, texts, labels, threshold):
    """Run `portcullis detect` once over all texts; returns {index: [entities]}."""
    with tempfile.TemporaryDirectory() as td:
        inp = Path(td) / "in.jsonl"
        outp = Path(td) / "out.jsonl"
        with inp.open("w", encoding="utf-8") as f:
            for i, t in enumerate(texts):
                f.write(json.dumps({"id": i, "text": t}) + "\n")
        env = dict(os.environ)
        if model_dir:
            env["PORTCULLIS_MODEL_DIR"] = model_dir
        with inp.open("rb") as fi, outp.open("wb") as fo:
            subprocess.run(
                [
                    binary, "detect",
                    "--labels", ",".join(labels),
                    "--threshold", str(threshold),
                ],
                stdin=fi, stdout=fo, check=True, env=env,
            )
        preds = {}
        with outp.open(encoding="utf-8") as f:
            for line in f:
                d = json.loads(line)
                preds[d["id"]] = d.get("entities") or []
        return preds


def score(rows, preds):
    tp = gold_n = pred_n = nonpii = 0
    for i, row in enumerate(rows):
        text = row["text"] or ""
        n = len(text)
        gold = merge_spans([(e["start"], e["end"]) for e in (row["entities"] or [])])
        pred = merge_spans([(p["start"], p["end"]) for p in preds.get(i, [])])
        gm = mask_chars(n, gold)
        pm = mask_chars(n, pred)
        tp += sum(1 for a, b in zip(gm, pm) if a and b)
        gold_n += sum(gm)
        pred_n += sum(pm)
        nonpii += n - sum(gm)

    p = tp / pred_n if pred_n else 0.0
    r = tp / gold_n if gold_n else 0.0
    f1 = 2 * p * r / (p + r) if (p + r) else 0.0
    f2 = 5 * p * r / (4 * p + r) if (4 * p + r) else 0.0
    fpr = (pred_n - tp) / nonpii if nonpii else 0.0
    return {
        "precision": p, "recall": r, "f1": f1, "f2": f2, "fpr": fpr,
        "tp_chars": tp, "gold_chars": gold_n, "pred_chars": pred_n,
        "nonpii_chars": nonpii,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="./target/release/portcullis")
    ap.add_argument("--model-dir", default=None,
                    help="sets PORTCULLIS_MODEL_DIR for the detector")
    ap.add_argument("--parquet", default=".cache/piimb-sentences.parquet")
    ap.add_argument("--tasks", default=",".join(DEFAULT_TASKS))
    ap.add_argument("--sample", type=int, default=0,
                    help="sentences to sample per task (0 = all)")
    ap.add_argument("--seed", type=int, default=13)
    ap.add_argument("--threshold", type=float, default=0.3,
                    help="PIIMB runs GLiNER/GLiNER2 at 0.3 to prioritise recall")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    tasks = [t.strip() for t in args.tasks.split(",") if t.strip()]
    rows = load_rows(ensure_parquet(Path(args.parquet)))
    by_task = defaultdict(list)
    for r in rows:
        by_task[r["task_name"]].append(r)

    rng = random.Random(args.seed)
    results = {}
    for task in tasks:
        pool = by_task.get(task, [])
        if not pool:
            print(f"!! no rows for task {task}", file=sys.stderr)
            continue
        # The model receives the task's own label list (per the PIIMB spec), so
        # take it from the full task, not just the sample.
        labels = sorted({e["label"] for r in pool for e in (r["entities"] or [])})
        sample = pool if args.sample <= 0 else rng.sample(pool, min(args.sample, len(pool)))
        texts = [r["text"] or "" for r in sample]

        t0 = time.time()
        preds = run_detector(args.binary, args.model_dir, texts, labels, args.threshold)
        elapsed = time.time() - t0
        s = score(sample, preds)
        s.update({
            "task": task, "n_sentences": len(sample), "n_labels": len(labels),
            "labels": labels, "threshold": args.threshold,
            "seconds": round(elapsed, 1),
            "sentences_per_second": round(len(sample) / elapsed, 2) if elapsed else None,
        })
        results[task] = s
        print(f"{task:18s} n={len(sample):6d}  P={s['precision']:.3f} "
              f"R={s['recall']:.3f}  F1={s['f1']:.3f}  F2={s['f2']:.3f}  "
              f"FPR={s['fpr']:.3f}  ({s['seconds']}s)")

    if results:
        avg_f2 = sum(s["f2"] for s in results.values()) / len(results)
        avg_f1 = sum(s["f1"] for s in results.values()) / len(results)
        avg_p = sum(s["precision"] for s in results.values()) / len(results)
        avg_r = sum(s["recall"] for s in results.values()) / len(results)
        print(f"\nAvg F2 = {avg_f2:.4f}   Avg F1 = {avg_f1:.4f}   "
              f"Avg P = {avg_p:.4f}   Avg R = {avg_r:.4f}")
        payload = {
            "benchmark": "PIIMB (piimb/pii-masking-benchmark), sentences/test",
            "metric": "character-level, label-agnostic, micro-averaged per task",
            "seed": args.seed, "sample_per_task": args.sample,
            "threshold": args.threshold, "tasks": results,
            "avg_f2": avg_f2, "avg_f1": avg_f1,
            "avg_precision": avg_p, "avg_recall": avg_r,
        }
        if args.out:
            Path(args.out).parent.mkdir(parents=True, exist_ok=True)
            Path(args.out).write_text(json.dumps(payload, indent=2))
            print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
