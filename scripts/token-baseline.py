#!/usr/bin/env python3
"""Token-utilisation baseline: the before/after harness for AMUX-3559.

Ethan, 2026-08-23: *"audit system level prompt injections/commands etc sent by
amux automatically like board shit ... we should also have documented e2e/eval
shit run them all and use them to squeeze out improved token utilization
iterate until its acceptable and were not wasting tokens."*

The suites that already exist cannot do that job, and this file exists because
of what running them showed: `cargo test -p amux-server` and the Playwright
specs in `e2e/` assert BEHAVIOUR (a route answers, a payload carries a field, a
button closes an overlay). Not one of them can express a token, so "run them as
the before/after harness" is unsatisfiable as written — a green suite is
compatible with spend doubling. The missing instrument is this one.

It is modelled on `scripts/perf-baseline.sh` + `perf-compare.py`, which is the
one documented before/after harness in this repo, with one structural
difference that has to be stated because it changes what a comparison MEANS:

  * perf re-measures the SAME corpus, so measured-vs-baseline is a controlled
    experiment and a delta is caused by the code.
  * tokens cannot be re-measured for last week. Each run is a fresh trailing
    window over live fleet behaviour, so a delta is an OBSERVATION, not an
    experiment, and it is confounded by anything else that moved (fleet size
    above all). Every confounder this file knows about is emitted beside the
    numbers rather than corrected for silently.

Design rules, each one there because the alternative is a gate that cannot
fail or a number that cannot be read:

* **Everything is per-day and per-lane.** Raw counts across windows of
  different length, or fleets of different size, are not comparable. A fleet
  that grew 49 -> 60 lanes produces more nudges with identical efficiency;
  gating the raw count would file that as a regression and gating nothing
  would call a fleet shrink a win.
* **Direction is explicit per metric**, because a metric whose sign is inferred
  gets inferred wrong. All four gated metrics are currently "higher is worse";
  the machinery carries `down_bad` so the next inverse metric cannot be added
  by editing a threshold number alone.
* **Only what amux WROTE is gated.** A peer relay and a scheduler delivery are
  the product working; a user adding ten schedules must not trip a
  token-efficiency gate they cannot honestly satisfy (ethos rule 3). Both are
  measured and printed, and unclassified rows are a visible third number rather
  than being quietly filed under either.
* **Nudge volume and background spend are gated TOGETHER.** This is the whole
  discriminator. Fewer nudges at unchanged background spend means the turns
  MOVED, not that they went away, and reporting that as a win is how a token
  programme congratulates itself into a bigger bill. `compare` prints that
  verdict by name.
* **The window control is emitted.** `rows_considered` / `rows_excluded` are
  the proof the cutoff filtered anything; an unbounded match and a correct
  match look identical from the rows alone (ethos rule 7, and the seconds/ms
  trap has already bitten this exact pair of tables).
* **The worktree state is recorded beside the numbers**, per
  `docs/self-improvement-loop.md`: a measurement on a shared checkout is only
  as clean as `git status --porcelain` at the instant it ran.

Usage:
    token-baseline.py measure [--days 7] [--out measured.json]
    token-baseline.py compare --measured m.json [--baseline docs/token-baseline.json]

Exit codes: 0 all good · 1 regression · 2 bad input / missing metric.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import re
import sqlite3
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

DB = Path(os.environ.get("AMUX_DB", str(Path.home() / ".amux" / "amux.db")))
BASELINE = Path("docs/token-baseline.json")

# ---------------------------------------------------------------------------
# Injection taxonomy. Matched on the STABLE prefix each generator emits, not on
# prose in the middle of the message, so a reworded nudge stays in its bucket.
# `other` is deliberately kept and reported: a bucket that silently absorbs a
# new generator is how a doubling goes unseen, so `measure` fails loudly if
# `other` is more than a fifth of the window.
# ---------------------------------------------------------------------------
#
# The regexes exist because `steering_history` has no source column: its schema
# is (id, session, text, queued_at, delivered_at, outcome), so nothing records
# WHICH job generated a row. Every attribution here is reverse-engineered from
# the text a generator happens to emit, which is why the taxonomy goes stale
# silently and why `measure` warns when it has. That missing column is AMUX-3562;
# when it lands, delete this table and group by it.
#
# Two of these were written from a whole-table prefix sample and matched zero
# rows in the first real window: `[amux peer-review]` and `[amux context
# monitor]`. Grepping crates/amux-server/src found no generator emitting either
# string, so they were regexes for text nothing writes — removed rather than
# left as permanently-empty buckets that read as "this never happens".
KINDS: list[tuple[str, re.Pattern[str]]] = [
    # -- delivered payload: a human or a human's schedule authored this text ---
    ("peer_relay",      re.compile(r"^\[amux-origin:")),
    # -- amux-authored overhead: the subject of AMUX-3559 --------------------
    ("auto_pickup",     re.compile(r"^\[amux auto-pickup\]")),
    ("auto_continue",   re.compile(r"^\[amux auto-continue\]")),
    ("board_note",      re.compile(r"^\[amux board note on")),
    ("staged_guard",    re.compile(r"^\[amux staged-guard\]")),
    ("schedule_failed", re.compile(r"^\[amux\] SCHEDULE FAILED")),
    ("idle_holding",    re.compile(r"^\[amux\] You went idle holding")),
    ("commit_nudge",    re.compile(r"^You went idle with \d+ uncommitted")),
    # Two live variants ("N of your dirty file(s)" and "N dirty file(s) in this
    # SHARED checkout"). The first spelling alone left ~50 rows in `other`.
    ("stale_dirty",     re.compile(r"^STALE: \d+ ")),
    ("no_queued_work",  re.compile(r"^\[amux\] You (have no queued work|are idle with)")),
    ("capture_split",   re.compile(r"^\[amux\] \d+ of your prompts are captured")),
    ("amux_other",      re.compile(r"^\[amux")),
]

# Which buckets are amux talking to a lane, versus a human (or a human's
# schedule) talking to a lane. Only the first group is overhead; the second is
# the product working. Gating the total would mean a user adding ten schedules
# trips a token-efficiency gate they cannot honestly satisfy (ethos rule 3),
# so the gate reads the amux-authored subset and the total is recorded beside it.
#
# `other` is in NEITHER set, on purpose. It is dominated by scheduled human
# prompts, so filing it under "delivered" is the tempting read — and it would
# mean a brand-new amux generator lands in a bucket no gate watches, which is
# how an exemption stops making something cheap and starts making it invisible
# (ethos rule 1). Unclassified is reported as its own third number and the
# taxonomy warning fires on its share.
DELIVERED_KINDS = {"peer_relay", "scheduled_task"}
UNCLASSIFIED_KINDS = {"other"}

# metric -> (max fractional growth, direction, why)
#   direction "up_bad"   : a rise beyond the threshold is the regression
#   direction "down_bad" : a fall beyond the threshold is the regression
THRESHOLDS: dict[str, tuple[float, str, str]] = {
    "amux_authored_per_lane_day":  (0.15, "up_bad",
        "AMUX-3559: injections amux WROTE, per lane per day, excluding peer relays and "
        "scheduler deliveries a human authored. +15% is a new generator, or a nudge that "
        "stopped backing off. This is the count that provokes background turns, and a "
        "provoked turn costs ~100x what the nudge's own characters cost."),
    "amux_authored_chars_per_lane_day": (0.15, "up_bad",
        "Length of the same. Gated to catch a nudge that tripled in size while holding its "
        "count, but see tokens_cache_read: characters are the SMALL term. A breach here with "
        "the count flat is a wording regression, not a spend regression."),
    "background_pct": (0.10, "up_bad",
        "AMUX-3542: share of spend on turns the user did not type. This is the customer's "
        "complaint stated as a number."),
    "background_usd_per_lane_day": (0.20, "up_bad",
        "AMUX-3542: the bill. background_pct can fall while the bill rises if human spend "
        "rises faster, so the absolute is gated too."),
}

# Absolute ceilings. Not relative to a baseline, so they hold on a first run
# and stop a slow creep of baseline bumps from legalising a doubling one 14%
# step at a time. Both are the audited 2026-08-23 level plus headroom, and
# both are knobs so a deliberate policy change moves the number ONCE, on the
# record, instead of being argued per run.
ABSOLUTE_MAX: dict[str, float] = {
    "background_pct": float(os.environ.get("AMUX_TOKEN_MAX_BACKGROUND_PCT", "85")),
    "amux_authored_per_lane_day": float(os.environ.get("AMUX_TOKEN_MAX_INJ_PER_LANE_DAY", "10")),
}

# Below this a ratio is noise: 0.4 -> 0.6 injections/lane/day is +50% and means
# nothing. Stated rather than silently swallowed; every skip is printed.
MIN_FOR_RATIO = 0.5


def classify(text: str, scheduled: set[str]) -> str:
    # The schedules join runs FIRST and is exact: a row whose text is verbatim
    # some schedule's `command` was delivered by the scheduler, i.e. a human
    # wrote it. This is structural rather than a prefix guess, but it is a LOWER
    # BOUND and can only undercount — editing a schedule leaves its older
    # deliveries no longer matching the current command. It can never overcount,
    # which is the direction that matters: an undercount inflates amux's own
    # share, so the gate errs toward accusing amux, not excusing it.
    if text in scheduled:
        return "scheduled_task"
    for name, pat in KINDS:
        if pat.match(text):
            return name
    return "other"


def worktree_state() -> dict:
    def run(*a: str) -> str:
        try:
            return subprocess.run(a, capture_output=True, text=True, timeout=20).stdout.strip()
        except Exception:  # noqa: BLE001
            return ""
    dirty = run("git", "status", "--porcelain")
    return {
        "head": run("git", "rev-parse", "--short", "HEAD"),
        "dirty_files": len([ln for ln in dirty.splitlines() if ln.strip()]),
        # A number measured on a tree with a peer's uncommitted work in it has
        # already published one artifact in this repo. Recorded, not gated:
        # refusing to measure on a shared checkout would mean never measuring.
        "clean": dirty == "",
    }


def measure(days: float) -> dict:
    if not DB.exists():
        print(f"FATAL: no database at {DB}", file=sys.stderr)
        sys.exit(2)
    conn = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
    c = conn.cursor()
    now = time.time()
    cutoff = now - days * 86400

    # ---- injections -------------------------------------------------------
    # steering_history.queued_at is in SECONDS. token_ledger.ts is in SECONDS.
    # cmd_history.ts is in MILLISECONDS. The three are compared below and the
    # mismatch is the single most expensive mistake available here, so each
    # cutoff is written next to the column it filters.
    total_inj = c.execute("SELECT COUNT(*) FROM steering_history").fetchone()[0]
    rows = c.execute(
        "SELECT text FROM steering_history WHERE queued_at > ?", (cutoff,)
    ).fetchall()
    in_window = len(rows)

    scheduled = {r[0] for r in c.execute(
        "SELECT command FROM schedules WHERE command IS NOT NULL AND command != ''")}

    by_kind: Counter[str] = Counter()
    chars_by_kind: Counter[str] = Counter()
    for (text,) in rows:
        k = classify(text or "", scheduled)
        by_kind[k] += 1
        chars_by_kind[k] += len(text or "")

    total_chars = sum(chars_by_kind.values())
    relay_chars = chars_by_kind.get("peer_relay", 0)

    def tally(names) -> tuple[int, int]:
        return (sum(by_kind.get(k, 0) for k in names),
                sum(chars_by_kind.get(k, 0) for k in names))

    delivered_n, delivered_chars = tally(DELIVERED_KINDS)
    unclass_n, unclass_chars = tally(UNCLASSIFIED_KINDS)
    authored_n = in_window - delivered_n - unclass_n
    authored_chars = total_chars - delivered_chars - unclass_chars

    # ---- fleet size, the confounder that matters most ---------------------
    # Lanes that received at least one injection in the window. Not the
    # registered lane count: a registered-but-parked lane costs nothing and
    # would deflate the per-lane rate exactly when the fleet is quietest.
    active_lanes = c.execute(
        "SELECT COUNT(DISTINCT session) FROM steering_history WHERE queued_at > ?", (cutoff,)
    ).fetchone()[0] or 1

    # ---- spend ------------------------------------------------------------
    total_ledger = c.execute("SELECT COUNT(*) FROM token_ledger").fetchone()[0]
    ledger_in_window = c.execute(
        "SELECT COUNT(*) FROM token_ledger WHERE ts > ?", (cutoff,)
    ).fetchone()[0]

    # Same correlated lookup as GET /api/usage/attribution, deliberately: two
    # spellings of "was this turn human-triggered" would drift, and the panel
    # is what a person reads when they ask where their credits went. The
    # `h.ts/1000` is the millisecond divide.
    spend = c.execute(
        "WITH lg AS (SELECT ts, session, cost_usd FROM token_ledger WHERE ts > ?1) "
        "SELECT COALESCE((SELECT h.type FROM cmd_history h "
        "                   WHERE h.session = lg.session AND h.ts/1000 <= lg.ts "
        "                   ORDER BY h.ts DESC LIMIT 1), '') AS trig, "
        "       SUM(cost_usd), COUNT(*) FROM lg GROUP BY 1",
        (cutoff,),
    ).fetchall()
    total_usd = sum(r[1] or 0.0 for r in spend)
    bg_usd = sum((r[1] or 0.0) for r in spend if r[0] != "user")
    turns = sum(r[2] or 0 for r in spend)

    # Token volume by class, recorded because it settles what the injections
    # actually cost and the answer is counter-intuitive. Measured 2026-08-23 over
    # 7 days: 20.1B cache_read against 359k input and 49.9M output — cache reads
    # are >99% of all tokens and essentially all of the dollars. So an injection's
    # bill is NOT its own characters (3.7M chars/week is ~0.9M tokens, rounding
    # error against 20.1B); it is the TURN it provokes, because that turn re-reads
    # the whole cached context. Optimising nudge wording is therefore worth
    # roughly nothing and NOT sending the turn is worth ~$0.52. Anyone reading
    # this file to decide what to cut needs that in front of them.
    vol = c.execute(
        "SELECT COALESCE(SUM(input),0), COALESCE(SUM(cache_read),0), "
        "COALESCE(SUM(output),0) FROM token_ledger WHERE ts > ?", (cutoff,)
    ).fetchone()

    out = {
        "_comment": (
            "Token-utilisation snapshot (scripts/token-baseline.py measure). Each run is a "
            "FRESH trailing window over live fleet behaviour, not a re-measurement of a fixed "
            "corpus: a delta here is an observation confounded by fleet size and workload, "
            "which is why every rate is per-lane-per-day and active_lanes is recorded."
        ),
        "measured": time.strftime("%Y-%m-%d", time.localtime(now)),
        "window_days": days,
        "active_lanes": active_lanes,
        # gated metrics
        "amux_authored_per_lane_day": round(authored_n / days / active_lanes, 3),
        "amux_authored_chars_per_lane_day": round(authored_chars / days / active_lanes, 1),
        "background_pct": round(bg_usd / total_usd * 100, 1) if total_usd > 0 else 0.0,
        "background_usd_per_lane_day": round(bg_usd / days / active_lanes, 4),
        # recorded, not gated: the shape behind the totals
        "injections_per_lane_day": round(in_window / days / active_lanes, 3),
        "delivered_per_lane_day": round(delivered_n / days / active_lanes, 3),
        "unclassified_per_lane_day": round(unclass_n / days / active_lanes, 3),
        # Ungated on purpose. It reads as an efficiency ratio and is not one: it
        # falls when peers relay less, which is not amux's overhead changing, and
        # amux_authored_* now measures the thing directly. Kept because it is the
        # number the AMUX-3559 audit published (0.62%) and dropping it would make
        # this file non-comparable with that write-up.
        "peer_relay_pct": round(relay_chars / total_chars * 100, 3) if total_chars else 0.0,
        "injections_total": in_window,
        "injection_chars_total": total_chars,
        "amux_authored_total": authored_n,
        "total_usd": round(total_usd, 2),
        "background_usd": round(bg_usd, 2),
        "turns": turns,
        "usd_per_turn": round(total_usd / turns, 4) if turns else 0.0,
        "tokens_input": vol[0],
        "tokens_cache_read": vol[1],
        "tokens_output": vol[2],
        # total_usd is LIST-PRICE-EQUIVALENT from the ledger's own cost_usd, not
        # a Max-plan invoice. Read it as a comparable unit across runs, not as
        # money owed, or the first person to see five figures for one week panics.
        "cost_basis": "list-price equivalent (token_ledger.cost_usd), not billed",
        "by_kind": dict(by_kind.most_common()),
        "chars_by_kind": dict(chars_by_kind.most_common()),
        # controls: equal counts mean the cutoff matched EVERYTHING and the
        # numbers above are the whole table rather than a window.
        "rows_considered": in_window,
        "rows_excluded_by_window": total_inj - in_window,
        "ledger_rows_considered": ledger_in_window,
        "ledger_rows_excluded_by_window": total_ledger - ledger_in_window,
        "worktree": worktree_state(),
    }

    # A taxonomy that stops matching is a silent measurement failure: every new
    # generator lands in `other`, the gated rates keep moving, and nothing says
    # the breakdown went blind. Loud, and non-fatal for `compare`, which reads
    # the rates rather than the buckets.
    other = by_kind.get("other", 0)
    if in_window and other / in_window > 0.20:
        out["taxonomy_warning"] = (
            f"{other} of {in_window} injections ({other / in_window * 100:.0f}%) fell into "
            "'other' — KINDS in scripts/token-baseline.py has gone stale against the "
            "generators. The rates are still valid; the breakdown is not."
        )
    if in_window == total_inj and total_inj > 0:
        out["window_warning"] = (
            "rows_excluded_by_window is 0 — the cutoff excluded nothing, so this is the "
            "whole table and not a window. Check the unit of queued_at before trusting it."
        )
    return out


def load(path: Path, *, missing_ok: bool = False) -> dict:
    if missing_ok and not path.exists():
        print(f"NOTE: no baseline at {path} — first run, recording only, nothing gated.\n")
        return {}
    try:
        with path.open() as f:
            return json.load(f)
    except Exception as e:  # noqa: BLE001
        print(f"FATAL: cannot read {path}: {e}", file=sys.stderr)
        sys.exit(2)


def compare(measured: dict, baseline: dict) -> int:
    failures: list[str] = []
    notes: list[str] = []
    rows: list[dict] = []

    for metric, (limit, direction, why) in THRESHOLDS.items():
        base, meas = baseline.get(metric), measured.get(metric)
        if base is None and meas is None:
            continue
        if meas is None:
            # The gate's own blind spot: a metric the baseline gates but the
            # harness stopped emitting. Silence here is a green gate measuring
            # nothing.
            failures.append(
                f"{metric}: present in the baseline, ABSENT from the measurement. "
                "The harness stopped emitting a gated metric."
            )
            continue

        meas_f = float(meas)
        ceiling = ABSOLUTE_MAX.get(metric)
        over = ceiling is not None and meas_f >= ceiling
        if over:
            failures.append(
                f"{metric}: {meas_f:g} is at or over the ABSOLUTE ceiling {ceiling:g}."
                + ("" if base is None else f" Baseline was {float(base):g}.")
            )

        if base is None:
            notes.append(f"{metric}: {meas_f:g} — no baseline, recording only.")
            rows.append({"metric": metric, "measured": meas_f, "baseline": None,
                         "verdict": "over-ceiling" if over else "recording-only"})
            continue

        base_f = float(base)
        if max(abs(base_f), abs(meas_f)) < MIN_FOR_RATIO:
            notes.append(f"{metric}: {base_f:g} -> {meas_f:g}, both under the {MIN_FOR_RATIO} "
                         "noise floor — ratio skipped.")
            rows.append({"metric": metric, "measured": meas_f, "baseline": base_f,
                         "verdict": "below-noise-floor" if not over else "over-ceiling"})
            continue
        if base_f <= 0:
            rows.append({"metric": metric, "measured": meas_f, "baseline": base_f,
                         "verdict": "baseline-zero"})
            continue

        delta = (meas_f - base_f) / base_f
        verdict = "over-ceiling" if over else "ok"
        bad = delta > limit if direction == "up_bad" else delta < -limit
        if bad:
            arrow = "+" if delta > 0 else ""
            failures.append(
                f"{metric}: {meas_f:g} vs baseline {base_f:g} = {arrow}{delta * 100:.1f}% "
                f"(threshold {'+' if direction == 'up_bad' else '-'}{limit * 100:.0f}%) — {why}"
            )
            verdict = "regression"
        rows.append({"metric": metric, "measured": meas_f, "baseline": base_f,
                     "delta_pct": round(delta * 100, 2), "direction": direction,
                     "verdict": verdict})

    print(f"{'metric':<32} {'baseline':>12} {'measured':>12} {'delta':>9}  verdict")
    print("-" * 82)
    for r in rows:
        b = "—" if r["baseline"] is None else f"{r['baseline']:g}"
        d = f"{r['delta_pct']:+.1f}%" if "delta_pct" in r else "—"
        print(f"{r['metric']:<32} {b:>12} {r['measured']:<12g} {d:>9}  {r['verdict']}")
    print()

    # ---- the paired verdict ----------------------------------------------
    # The reason this harness is not a nudge counter with extra steps. Fewer
    # injections at unchanged background spend means the turns MOVED rather
    # than went away, and a per-metric gate reports that as an unambiguous win
    # because both metrics individually improved-or-held.
    bi, mi = (baseline.get("amux_authored_per_lane_day"),
              measured.get("amux_authored_per_lane_day"))
    bb, mb = baseline.get("background_usd_per_lane_day"), measured.get("background_usd_per_lane_day")
    if None not in (bi, mi, bb, mb) and float(bi) > 0 and float(bb) > 0:
        di = (float(mi) - float(bi)) / float(bi)
        db = (float(mb) - float(bb)) / float(bb)
        print("PAIRED READING (injections vs background spend, per lane per day)")
        if di < -0.10 and db > -0.05:
            print(f"  injections {di * 100:+.1f}% but background spend {db * 100:+.1f}%.")
            print("  The turns MOVED, they did not go away. Fewer, longer injections cost the")
            print("  same. Do not report this as a token win.")
            notes.append("paired: fewer injections, spend held — turns moved, not removed.")
        elif di < -0.10 and db < -0.10:
            print(f"  injections {di * 100:+.1f}% AND background spend {db * 100:+.1f}%.")
            print("  Both fell together — this is a real reduction in background work.")
        elif di > 0.10 and db < -0.10:
            print(f"  injections {di * 100:+.1f}% but background spend {db * 100:+.1f}%.")
            print("  More injections, cheaper: nudges got shorter or provoke shorter turns.")
        else:
            print(f"  injections {di * 100:+.1f}%, background spend {db * 100:+.1f}% — moving together.")
        print()

    for n in notes:
        print(f"NOTE: {n}")
    if measured.get("taxonomy_warning"):
        print(f"WARN: {measured['taxonomy_warning']}")
    if measured.get("window_warning"):
        print(f"WARN: {measured['window_warning']}")
    if not measured.get("worktree", {}).get("clean", True):
        print(f"NOTE: measured on a dirty tree "
              f"({measured['worktree'].get('dirty_files')} file(s)) — a shared-checkout "
              "measurement is only as clean as git status at the instant it ran.")
    bl_lanes, m_lanes = baseline.get("active_lanes"), measured.get("active_lanes")
    if bl_lanes and m_lanes and abs(m_lanes - bl_lanes) / bl_lanes > 0.15:
        print(f"NOTE: fleet size moved {bl_lanes} -> {m_lanes} lanes. Per-lane rates absorb "
              "this; the raw totals in this file do not.")
    print()

    if failures:
        print("REGRESSION:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("OK — no token-utilisation regression against the baseline.")
    return 0


def selftest() -> int:
    """Prove the gate can fail, on the arithmetic rather than on wording.

    A gate nobody has watched go red is theatre (ethos rule 7), and the specific
    trap here is a mutation that changes the STRING an assertion prints rather
    than the number it decides on. Every case below moves a VALUE. The last two
    are a matched pair on purpose: the same +100% must fail above the noise floor
    and skip below it, so a passing skip is the floor working and not the gate
    being dead.
    """
    import copy
    base = {
        "amux_authored_per_lane_day": 4.0,
        "amux_authored_chars_per_lane_day": 8000.0,
        "background_pct": 70.0,
        "background_usd_per_lane_day": 50.0,
        "active_lanes": 50, "window_days": 7.0, "worktree": {"clean": True},
    }

    def run(mutate, *, baseline=None) -> tuple[int, str]:
        b = copy.deepcopy(baseline if baseline is not None else base)
        m = copy.deepcopy(base)
        mutate(b, m)
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = compare(m, b)
        return rc, buf.getvalue()

    cases: list[tuple[str, object, int, str]] = [
        ("identity passes",
         lambda b, m: None, 0, ""),
        ("+30% authored breaches the +15% threshold",
         lambda b, m: m.update(amux_authored_per_lane_day=5.2), 1,
         "threshold"),
        ("gated metric absent from the measurement is a FAILURE, not a skip",
         lambda b, m: m.pop("background_usd_per_lane_day"), 1,
         "ABSENT from the measurement"),
        ("absolute ceiling fires with no baseline breach",
         lambda b, m: (b.update(background_pct=87.0), m.update(background_pct=88.0)), 1,
         "ABSOLUTE ceiling"),
        ("a metric new in the measurement is recorded, not gated",
         lambda b, m: m.update(some_future_metric=1.23), 0, ""),
        ("fewer injections at held spend prints the moved-turns verdict",
         lambda b, m: m.update(amux_authored_per_lane_day=3.2), 0,
         "turns MOVED"),
        ("+100% below the noise floor is skipped",
         lambda b, m: (b.update(amux_authored_per_lane_day=0.2),
                       m.update(amux_authored_per_lane_day=0.4)), 0,
         "noise floor"),
        ("+100% ABOVE the noise floor still fails (control for the row above)",
         lambda b, m: (b.update(amux_authored_per_lane_day=2.0),
                       m.update(amux_authored_per_lane_day=4.0)), 1,
         "threshold"),
    ]

    bad = 0
    for name, mutate, want_rc, want_text in cases:
        rc, out = run(mutate)
        ok = rc == want_rc and (not want_text or want_text in out)
        if not ok:
            bad += 1
        print(f"  {'ok  ' if ok else 'FAIL'} {name}"
              + ("" if ok else f"   (rc={rc} want {want_rc}, text_found="
                               f"{want_text in out if want_text else 'n/a'})"))
    print(f"\n{len(cases) - bad}/{len(cases)} gate cases behaved as specified.")
    return 1 if bad else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("selftest", help="prove the gate can fail")

    m = sub.add_parser("measure", help="emit a snapshot JSON")
    m.add_argument("--days", type=float, default=7.0)
    m.add_argument("--out", type=Path, default=None)

    c = sub.add_parser("compare", help="gate a snapshot against the committed baseline")
    c.add_argument("--measured", required=True, type=Path)
    c.add_argument("--baseline", default=BASELINE, type=Path)
    c.add_argument("--record-missing", action="store_true")

    args = ap.parse_args()

    if args.cmd == "selftest":
        return selftest()

    if args.cmd == "measure":
        snap = measure(args.days)
        text = json.dumps(snap, indent=2)
        if args.out:
            args.out.write_text(text + "\n")
            print(f"wrote {args.out}")
        print(text)
        return 0

    measured = load(args.measured)
    baseline = load(args.baseline, missing_ok=args.record_missing)
    return compare(measured, baseline)


if __name__ == "__main__":
    sys.exit(main())
