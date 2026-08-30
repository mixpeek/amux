#!/usr/bin/env python3
"""RR-0117b — regression detection thresholds.

Compares a MEASURED perf JSON (the line `scripts/perf-baseline.sh` prints, plus
whatever else the caller merges in, e.g. `binary_bytes`) against the committed
baseline in `docs/perf-baseline.json`, and exits non-zero on a breach.

Design rules this file is built to, each one there because the alternative is
a gate that cannot fail:

* **Thresholds are stated, per metric, in `THRESHOLDS` below** — not inferred,
  not tuned to make a run green. Every number here traces to the plan's
  RR-0117b line: latency p95 +10%, RSS +20%, binary size +20%.
* **A breach prints the measured number, the baseline number, the delta and
  the threshold.** "Regression detected" without the numbers is a message that
  sends someone to re-measure by hand.
* **A metric present in the measurement but absent from the baseline is
  RECORDED, NOT GATED, and says so.** Gating against a baseline that does not
  exist would either fail every first run or silently pass everything; both
  are worse than an explicit "no baseline: recording only" line. This is the
  honest state for CI-linux numbers whose baseline was measured on a
  developer's macOS box — a cross-platform comparison is not a regression
  signal, it is noise with a p-value.
* **A metric in the BASELINE but missing from the MEASUREMENT is a failure**,
  not a pass. A gate silently skipping the metric it was written for is the
  purest form of theatre: the harness stops emitting `board_avg_ms`, the gate
  goes green forever, and nobody learns anything.

Usage:
    perf-compare.py --measured measured.json [--baseline docs/perf-baseline.json]
                    [--record-missing] [--json-out report.json]

Exit codes: 0 all good · 1 threshold breach · 2 bad input / missing metric.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# The thresholds. Every entry: metric -> (max fractional growth, why).
#
# `worst_ms` is the tail proxy. The plan says "p95"; the harness
# (scripts/perf-baseline.sh) takes 5 samples and reports avg + worst, and a
# p95 computed from 5 samples is not a p95 — it is the max with a misleading
# name. So the gate applies the tail threshold to the number that actually
# exists and says which one it used. Replacing this with a real percentile is
# RR-0117g's job; until then the gate is honest about what it measured.
# ---------------------------------------------------------------------------
THRESHOLDS: dict[str, tuple[float, str]] = {
    "dashboard_avg_ms":   (0.10, "RR-0117b: latency +10% fails"),
    "dashboard_worst_ms": (0.10, "RR-0117b: tail latency +10% fails (worst-of-5 stands in for p95)"),
    "health_avg_ms":      (0.10, "RR-0117b: latency +10% fails"),
    "health_worst_ms":    (0.10, "RR-0117b: tail latency +10% fails (worst-of-5 stands in for p95)"),
    "board_avg_ms":       (0.10, "RR-0117b: latency +10% fails"),
    "board_worst_ms":     (0.10, "RR-0117b: tail latency +10% fails (worst-of-5 stands in for p95)"),
    "board_full_avg_ms":  (0.10, "RR-0117b: latency +10% fails"),
    "workers_avg_ms":     (0.10, "RR-0117b: latency +10% fails"),
    "search_avg_ms":      (0.10, "RR-0110: FTS5 query latency"),
    "rss_mb":             (0.20, "RR-0117b: RSS +20% fails"),
    # Dirty is LIVE heap — the number a leak moves, immune to the freed-page
    # retention that makes RSS allocator weather (AMUX-3488). Tighter delta
    # than RSS on purpose: this one is stable enough to hold it.
    "dirty_mb":           (0.20, "AMUX-3488: live-heap +20% fails — not allocator weather"),
    "binary_bytes":       (0.20, "RR-0117b: binary size +20% growth blocks merge"),
    "board_default_bytes": (0.20, "payload size: a silent 20% growth is a mobile regression (amux is mobile-first)"),
}

# Absolute ceilings from the plan's §Performance targets. These are NOT
# relative to a baseline: they hold no matter what the baseline drifted to,
# which is what stops a slow creep of baseline bumps from legalising a
# 2-second dashboard one 9% step at a time.
ABSOLUTE_MAX: dict[str, float] = {
    "dashboard_avg_ms": 500,
    "health_avg_ms": 50,
    "board_avg_ms": 200,
    "search_avg_ms": 50,   # RR-0110: "FTS5 over 10k entities returns < 50ms"
    # The RSS ceiling moved ONCE, on the record, from the plan's 200: the
    # server outgrew it (66MB on 2026-08-09 -> 220MB in CI on a frozen corpus
    # by 08-22) and the nightly perf leg died on it for its whole silent
    # streak (AMUX-2872). The growth is being attributed under AMUX-3488 —
    # tighten this back if that finds a leak. Same knob as perf-baseline.sh
    # so the two spellings of this ceiling cannot drift apart; the anti-creep
    # purpose above survives because the number is again fixed.
    "rss_mb": float(os.environ.get("PERF_RSS_MAX_MB", "280")),
    # Live-heap ceiling (AMUX-3488): measured 30-45MB on 2026-08-22, fresh
    # boot and post-battery alike. 250 is generous until the first CI (Linux)
    # reading lands — tighten it then. Same knob as perf-baseline.sh.
    "dirty_mb": float(os.environ.get("PERF_DIRTY_MAX_MB", "250")),
}

# Noise floor. Below this, a percentage is meaningless: 2ms -> 3ms is +50% and
# means nothing on a shared CI runner. Stated rather than silently swallowed —
# every skipped comparison is printed.
MIN_MS_FOR_RATIO = 10.0


def load(path: Path, *, missing_ok: bool = False) -> dict:
    if missing_ok and not path.exists():
        # A baseline file that does not exist yet is the FIRST RUN, which is a
        # real state and not an error: there is nothing to compare against, so
        # everything is recorded and nothing is gated. Exiting 2 here would
        # make the first nightly red for the only reason it cannot fix itself;
        # passing silently would hide that no comparison happened.
        print(f"NOTE: no baseline at {path} — first run, recording only, nothing gated.\n")
        return {}
    try:
        with path.open() as f:
            return json.load(f)
    except Exception as e:  # noqa: BLE001
        print(f"FATAL: cannot read {path}: {e}", file=sys.stderr)
        sys.exit(2)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--measured", required=True, type=Path)
    ap.add_argument("--baseline", default=Path("docs/perf-baseline.json"), type=Path)
    ap.add_argument(
        "--record-missing",
        action="store_true",
        help="metrics (or the whole baseline file) absent are reported as 'recording only' rather than gated",
    )
    ap.add_argument("--json-out", type=Path, default=None)
    args = ap.parse_args()

    measured = load(args.measured)
    # A missing baseline is only tolerable when the caller has said it is
    # recording rather than gating; otherwise a typo in --baseline would
    # silently turn the gate off.
    baseline = load(args.baseline, missing_ok=args.record_missing)

    rows = []
    failures = []
    recording = []
    skipped_noise = []
    missing_from_measurement = []

    for metric, (limit, why) in THRESHOLDS.items():
        base = baseline.get(metric)
        meas = measured.get(metric)

        if base is None and meas is None:
            continue
        if meas is None:
            # The gate's own blind spot: a metric the baseline gates but the
            # harness stopped emitting. Silence here would be a green gate
            # measuring nothing.
            missing_from_measurement.append(metric)
            continue
        # The ABSOLUTE ceiling is checked FIRST and does not need a baseline:
        # it is a plan target, not a relative measure, so it holds on a first
        # run, on a different corpus, and on a different machine. Checking it
        # only in the has-a-baseline branch would make the first run a gate
        # that cannot fail — and the first run is exactly when a target is
        # most likely to be missed.
        ceiling = ABSOLUTE_MAX.get(metric)
        over_ceiling = ceiling is not None and float(meas) >= ceiling
        if over_ceiling:
            failures.append(
                f"{metric}: {float(meas):g} exceeds the ABSOLUTE ceiling {ceiling:g} "
                f"(plan §Performance targets)."
                + ("" if base is None else f" Baseline was {float(base):g}.")
            )

        if base is None:
            recording.append((metric, meas))
            rows.append({
                "metric": metric, "measured": meas, "baseline": None,
                "verdict": "over-ceiling" if over_ceiling else "recording-only",
            })
            continue

        base_f, meas_f = float(base), float(meas)
        verdict = "over-ceiling" if over_ceiling else "ok"

        if metric.endswith("_ms") and max(base_f, meas_f) < MIN_MS_FOR_RATIO:
            skipped_noise.append((metric, base_f, meas_f))
            rows.append({"metric": metric, "measured": meas_f, "baseline": base_f,
                         "verdict": "below-noise-floor" if verdict == "ok" else verdict})
            continue

        if base_f <= 0:
            rows.append({"metric": metric, "measured": meas_f, "baseline": base_f, "verdict": "baseline-zero"})
            continue

        delta = (meas_f - base_f) / base_f
        if delta > limit:
            failures.append(
                f"{metric}: {meas_f:g} vs baseline {base_f:g} = {delta * 100:+.1f}% "
                f"(threshold +{limit * 100:.0f}%) — {why}"
            )
            verdict = "regression"
        rows.append({
            "metric": metric, "measured": meas_f, "baseline": base_f,
            "delta_pct": round(delta * 100, 2), "threshold_pct": limit * 100,
            "verdict": verdict,
        })

    # ---- report ----------------------------------------------------------
    print(f"{'metric':<22} {'measured':>12} {'baseline':>12} {'delta':>9}  verdict")
    print("-" * 72)
    for r in rows:
        b = "-" if r["baseline"] is None else f"{r['baseline']:g}"
        d = f"{r['delta_pct']:+.1f}%" if "delta_pct" in r else "-"
        print(f"{r['metric']:<22} {r['measured']:>12g} {b:>12} {d:>9}  {r['verdict']}")

    if skipped_noise:
        print("\nBelow the noise floor (a percentage on single-digit ms is not a signal):")
        for m, b, x in skipped_noise:
            print(f"  {m}: baseline {b:g}ms, measured {x:g}ms — under {MIN_MS_FOR_RATIO:g}ms, ratio not gated")

    if recording:
        print("\nNo baseline recorded for these — RECORDING ONLY, not gated:")
        for m, x in recording:
            print(f"  {m} = {x!r}")
        print(f"  Commit them into {args.baseline} to start gating them.")
        if not args.record_missing:
            print("  (pass --record-missing to make this explicit rather than incidental)")

    if missing_from_measurement:
        print("\nFATAL: the baseline gates these metrics but the measurement did not emit them:")
        for m in missing_from_measurement:
            print(f"  {m}")
        print("  A gate that silently skips its own metric is green forever. Fix the harness or drop the baseline key.")

    if args.json_out:
        args.json_out.write_text(json.dumps({
            "rows": rows,
            "failures": failures,
            "recording_only": [m for m, _ in recording],
            "missing_from_measurement": missing_from_measurement,
        }, indent=2))

    if missing_from_measurement:
        return 2
    if failures:
        print("\n" + "=" * 72)
        print("PERFORMANCE REGRESSION — CI FAILS")
        print("=" * 72)
        for f in failures:
            print(f"  {f}")
        return 1
    print("\nAll gated metrics within thresholds.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
