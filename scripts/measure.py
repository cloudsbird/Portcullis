#!/usr/bin/env python3
"""Measure wall time and peak RSS of a child process (portable; no GNU `time`).

Usage:
    python3 scripts/measure.py ./target/release/portcullis --store /tmp/s.json scan "text"

Used to produce the numbers in docs/RESOURCES.md.
"""
import resource
import subprocess
import sys
import time

cmd = sys.argv[1:]
if not cmd:
    print(__doc__)
    raise SystemExit(2)

t0 = time.perf_counter()
subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
wall = time.perf_counter() - t0
peak_kb = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
print(f"wall={wall:.3f}s  peak_rss={peak_kb / 1024:.1f} MB")
