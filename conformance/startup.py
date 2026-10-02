#!/usr/bin/env python3
"""Measure time to the first prompt, without supplying input or a valid backend.

Usage: python3 conformance/startup.py target/release/moly [sample-count]
This is an observational benchmark, not a portable CI latency threshold.
"""

import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import threading
import time

binary = Path(sys.argv[1]).resolve()
count = int(sys.argv[2]) if len(sys.argv) > 2 else 25
if count < 1:
    raise SystemExit("sample-count must be positive")
samples = []
for _ in range(count):
    start = time.perf_counter_ns()
    with subprocess.Popen(
        [str(binary), "--connect", "deliberately-nonexistent-endpoint"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env={**os.environ, "MOLY_MODEL_ENDPOINT": "invalid-before-first-input"},
    ) as process:
        deadline = threading.Timer(5, process.kill)
        deadline.start()
        try:
            assert process.stdout is not None
            assert process.stdin is not None
            # No user input has been sent, so this measures the startup boundary.
            prompt = process.stdout.read(6)
            elapsed = (time.perf_counter_ns() - start) / 1_000_000
            if prompt != b"moly> ":
                raise RuntimeError(f"unexpected prompt: {prompt!r}")
            _, diagnostics = process.communicate(b"/quit\n", timeout=5)
            if process.returncode != 0 or diagnostics:
                raise RuntimeError(f"pre-backend exit failed: {diagnostics!r}")
            samples.append(elapsed)
        finally:
            deadline.cancel()
            if process.poll() is None:
                process.kill()
                process.communicate()
print(json.dumps({
    "binary": str(binary),
    "samples": count,
    "first_prompt_ms": {
        "min": min(samples),
        "median": statistics.median(samples),
        "max": max(samples),
    },
}, indent=2))
