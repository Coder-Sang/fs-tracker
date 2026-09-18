#!/usr/bin/env python3
"""Run repeatable wall-time benchmarks against a release fs-tracker binary."""

from __future__ import annotations

import argparse
import json
import platform
import shutil
import statistics
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/release/fs-tracker"))
    parser.add_argument("--base-dir", type=Path, default=Path("/tmp"))
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def prepare(root: Path, case: str) -> list[str]:
    root.mkdir()
    if case == "read_scan":
        for index in range(1000):
            (root / f"file-{index:04}.txt").write_bytes((f"line {index}\n" * 8).encode())
        source = "from pathlib import Path; [p.read_bytes() for p in Path(__import__('sys').argv[1]).iterdir()]"
    elif case == "small_edits":
        for index in range(200):
            (root / f"file-{index:04}.txt").write_text(f"before {index}\n")
        source = "from pathlib import Path; [(p.write_text('after\\n')) for p in Path(__import__('sys').argv[1]).iterdir()]"
    elif case == "large_edit":
        (root / "large.bin").write_bytes(b"a" * (16 * 1024 * 1024))
        source = "from pathlib import Path; Path(__import__('sys').argv[1], 'large.bin').write_bytes(b'b' * (16 * 1024 * 1024))"
    else:
        raise ValueError(case)
    return ["python3", "-c", source, str(root)]


def one_run(binary: Path, base: Path, case: str, tracked: bool) -> dict[str, Any]:
    run_dir = Path(tempfile.mkdtemp(prefix=f"fs-tracker-{case}-", dir=base))
    root = run_dir / "root"
    output = run_dir / "result"
    command = prepare(root, case)
    argv = command
    if tracked:
        argv = [str(binary), "run", "--root", f"main={root}", "--output", str(output), "--", *command]
    started = time.perf_counter_ns()
    completed = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    elapsed_ms = (time.perf_counter_ns() - started) / 1_000_000
    if completed.returncode != 0:
        raise RuntimeError(f"{case} failed ({completed.returncode}): {completed.stderr.decode(errors='replace')}")
    result: dict[str, Any] = {"wall_ms": elapsed_ms}
    if tracked:
        report = json.loads((output / "report.json").read_text())
        result["tracker_metrics"] = report["metrics"]
        result["state"] = report["state"]
    shutil.rmtree(run_dir)
    return result


def summarize(values: list[dict[str, Any]]) -> dict[str, Any]:
    wall = [value["wall_ms"] for value in values]
    result: dict[str, Any] = {
        "runs": values,
        "median_wall_ms": statistics.median(wall),
        "min_wall_ms": min(wall),
        "max_wall_ms": max(wall),
    }
    if "tracker_metrics" in values[0]:
        result["median_notifications"] = statistics.median(
            value["tracker_metrics"]["notifications_received"] for value in values
        )
        result["median_captured_bytes"] = statistics.median(
            value["tracker_metrics"]["captured_bytes"] for value in values
        )
    return result


def main() -> int:
    args = parse_args()
    if args.iterations < 1:
        raise SystemExit("--iterations must be positive")
    binary = args.binary.resolve()
    base = args.base_dir.resolve()
    if not binary.is_file():
        raise SystemExit(f"binary does not exist: {binary}")
    if not base.is_dir():
        raise SystemExit(f"base directory does not exist: {base}")
    results: dict[str, Any] = {
        "environment": {
            "architecture": platform.machine(),
            "kernel": platform.release(),
            "base_dir": str(base),
            "iterations": args.iterations,
        },
        "cases": {},
    }
    for case in ("read_scan", "small_edits", "large_edit"):
        baseline = [one_run(binary, base, case, False) for _ in range(args.iterations)]
        tracked = [one_run(binary, base, case, True) for _ in range(args.iterations)]
        baseline_summary = summarize(baseline)
        tracked_summary = summarize(tracked)
        results["cases"][case] = {
            "baseline": baseline_summary,
            "tracked": tracked_summary,
            "median_overhead_ratio": tracked_summary["median_wall_ms"]
            / baseline_summary["median_wall_ms"],
        }
    encoded = json.dumps(results, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(encoded)
    print(encoded, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
