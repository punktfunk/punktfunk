#!/usr/bin/env python3
"""Compare criterion benchmark results against a committed baseline and report (soft-warn).

Reads the median time of every benchmark from target/criterion/**/new/estimates.json (written by
`cargo bench -p punktfunk-core`), diffs each against scripts/bench/baseline.json, and prints a
markdown table (also appended to $GITHUB_STEP_SUMMARY in CI). A metric slower than the baseline by
more than the regression threshold is flagged ⚠, but the script ALWAYS exits 0 — CI timing is noisy
(shared hardware), so this is report-only by design; the dedicated Tier-3 GPU runner
(scripts/bench/gpu-stream.sh) compares against a tighter threshold, also report-only. Use
--update to (re)write the baseline from the current run.

  python3 scripts/bench/compare.py [--threshold 0.5] [--update]
"""
import argparse
import glob
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BASELINE = os.path.join(ROOT, "scripts", "bench", "baseline.json")


def collect():
    """name -> median nanoseconds, for every criterion benchmark in target/criterion."""
    out = {}
    for est in glob.glob(os.path.join(ROOT, "target", "criterion", "**", "new", "estimates.json"),
                         recursive=True):
        # .../target/criterion/<group>/<id...>/new/estimates.json  ->  "<group>/<id...>"
        rel = os.path.relpath(est, os.path.join(ROOT, "target", "criterion"))
        name = os.path.dirname(os.path.dirname(rel))  # strip "/new/estimates.json"
        if name == "report":
            continue
        with open(est) as f:
            data = json.load(f)
        out[name.replace(os.sep, "/")] = data["median"]["point_estimate"]
    return out


def human(ns):
    for unit, scale in (("s", 1e9), ("ms", 1e6), ("µs", 1e3), ("ns", 1.0)):
        if ns >= scale:
            return f"{ns / scale:.2f} {unit}"
    return f"{ns:.2f} ns"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--threshold", type=float, default=0.5,
                    help="fractional slowdown vs baseline that flags a regression (0.5 = plus 50 pct)")
    ap.add_argument("--update", action="store_true", help="overwrite the baseline from this run")
    args = ap.parse_args()

    current = collect()
    if not current:
        print("no criterion results found — run `cargo bench -p punktfunk-core` first", file=sys.stderr)
        return 0  # soft

    if args.update:
        os.makedirs(os.path.dirname(BASELINE), exist_ok=True)
        with open(BASELINE, "w") as f:
            json.dump({k: current[k] for k in sorted(current)}, f, indent=2)
            f.write("\n")
        print(f"wrote baseline ({len(current)} benchmarks) -> {BASELINE}")
        return 0

    baseline = {}
    if os.path.exists(BASELINE):
        with open(BASELINE) as f:
            baseline = json.load(f)

    lines = ["| benchmark | baseline | current | Δ |", "|---|---:|---:|---:|"]
    regressions = []
    for name in sorted(current):
        cur = current[name]
        base = baseline.get(name)
        if base is None:
            lines.append(f"| {name} | — | {human(cur)} | _new_ |")
            continue
        delta = (cur - base) / base
        flag = " ⚠" if delta > args.threshold else (" ✅" if delta < -args.threshold else "")
        lines.append(f"| {name} | {human(base)} | {human(cur)} | {delta:+.1%}{flag} |")
        if delta > args.threshold:
            regressions.append((name, delta))

    table = "\n".join(lines)
    header = "## Benchmark vs baseline (report-only)\n"
    print(header + table)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(header + table + "\n")

    if regressions:
        print(f"\n⚠ {len(regressions)} benchmark(s) regressed > {args.threshold:+.0%} vs baseline "
              f"(report-only; verify on the dedicated runner before acting):")
        for name, d in regressions:
            print(f"   {name}: {d:+.1%}")
    return 0  # always soft


if __name__ == "__main__":
    sys.exit(main())
