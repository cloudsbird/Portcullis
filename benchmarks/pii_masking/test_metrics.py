#!/usr/bin/env python3
"""Known-answer tests for the PIIMB metric implementation in run.py.

If the scoring code is wrong, the published number is worthless — so the metric
is pinned with hand-computed cases.

Run:  python benchmarks/pii_masking/test_metrics.py
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from run import merge_spans, score  # noqa: E402


def row(text, gold):
    return {"text": text, "entities": [{"start": s, "end": e, "label": "X"} for s, e in gold]}


def preds(span_lists):
    return {i: [{"start": s, "end": e, "label": "Y"} for s, e in spans]
            for i, spans in enumerate(span_lists)}


def close(a, b, tol=1e-9):
    return abs(a - b) < tol


def check(name, got, want):
    ok = all(close(got[k], v) for k, v in want.items())
    status = "ok  " if ok else "FAIL"
    print(f"{status} {name}")
    if not ok:
        for k, v in want.items():
            mark = "" if close(got[k], v) else "   <--"
            print(f"       {k}: want {v:.6f} got {got[k]:.6f}{mark}")
    return ok


results = []

# --- 1. Partial overlap -----------------------------------------------------
# text 10 chars; gold = chars 0-2 (3 chars); pred = chars 1-3 (3 chars)
# overlap = 2 chars -> P = 2/3, R = 2/3, F1 = 2/3, F2 = 2/3, FPR = 1/7
results.append(check(
    "partial overlap",
    score([row("abcdefghij", [(0, 3)])], preds([[(1, 4)]])),
    {"precision": 2/3, "recall": 2/3, "f1": 2/3, "f2": 2/3, "fpr": 1/7},
))

# --- 2. Perfect match -------------------------------------------------------
results.append(check(
    "exact match",
    score([row("abcdefghij", [(2, 5)])], preds([[(2, 5)]])),
    {"precision": 1.0, "recall": 1.0, "f1": 1.0, "f2": 1.0, "fpr": 0.0},
))

# --- 3. Everything predicted, nothing gold (pure false positives) ------------
result = score([row("abcdefghij", [])], preds([[(0, 2)]]))
expected = {"precision": 0.0, "recall": 0.0, "f1": 0.0, "f2": 0.0, "fpr": 0.2}
# recall is undefined (no gold); implementation reports 0.0
results.append(check("pure false positive", result, expected))

# --- 4. Nothing predicted, gold present (pure miss) --------------------------
result = score([row("abcdefghij", [(0, 4)])], preds([[]]))
results.append(check(
    "pure miss",
    result,
    {"precision": 0.0, "recall": 0.0, "f1": 0.0, "f2": 0.0, "fpr": 0.0},
))

# --- 5. Consecutive gold spans are merged ------------------------------------
results.append(check(
    "consecutive spans merged",
    score([row("abcdefghij", [(0, 3), (3, 6)])], preds([[(0, 6)]])),
    {"precision": 1.0, "recall": 1.0, "f2": 1.0, "fpr": 0.0},
))

# --- 6. Overlapping predictions collapse (no double counting) ----------------
# pred [(0,4),(2,6)] merges to [(0,6)] = 6 chars, gold [(0,6)] -> perfect
results.append(check(
    "overlapping predictions merged",
    score([row("abcdefghij", [(0, 6)])], preds([[(0, 4), (2, 6)]])),
    {"precision": 1.0, "recall": 1.0, "f2": 1.0},
))

# --- 7. Micro-averaging across documents -------------------------------------
# doc A: gold 0-4 (4), pred 0-4 -> tp 4. doc B: gold 0-1 (1), pred none -> tp 0.
# micro: tp=4, gold=5, pred=4 -> P=1.0, R=0.8
results.append(check(
    "micro-averaged across docs",
    score([row("abcdefghij", [(0, 4)]), row("abcdefghij", [(0, 1)])], preds([[(0, 4)], []])),
    {"precision": 1.0, "recall": 0.8},
))

# --- 8. Out-of-range predictions are clamped ---------------------------------
result = score([row("abc", [(0, 3)])], preds([[(0, 99)]]))
results.append(check("out-of-range clamped", result, {"precision": 1.0, "recall": 1.0}))

# --- 9. merge_spans primitives ----------------------------------------------
merged_ok = (
    merge_spans([(5, 7), (0, 3)]) == [[0, 3], [5, 7]]
    and merge_spans([(0, 3), (3, 6)]) == [[0, 6]]      # consecutive
    and merge_spans([(0, 5), (2, 8)]) == [[0, 8]]      # overlapping
    and merge_spans([(4, 4)]) == []                    # empty span dropped
)
print(f"{'ok  ' if merged_ok else 'FAIL'} merge_spans primitives")
results.append(merged_ok)

print()
if all(results):
    print(f"all {len(results)} metric checks passed")
else:
    print(f"{sum(1 for r in results if not r)} of {len(results)} FAILED")
    sys.exit(1)
