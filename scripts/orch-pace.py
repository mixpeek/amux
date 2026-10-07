#!/usr/bin/env python3
"""orch-pace: is an orchestration on pace for its deadline?

Usage:
  scripts/orch-pace.py --orchestrator mixpeek-override --lane-prefix gs12- \
      --deadline 2026-10-04T23:59:00-04:00 \
      --proof-prefix "GS12 proof" --proof-prefix "GS12 requirement" [--epic MO-3905] [--json]

Counts the plan's cards on the board (the orchestrator's board plus every lane
whose name starts with --lane-prefix; archived cards excluded), appends a
snapshot to ~/.amux/logs/orch-pace-<orchestrator>.jsonl, and compares the
rate since the previous snapshots with the rate the deadline needs.

Finish lines, all from the board:
  plan:  with --plan-regex, the orchestrator's plan-item cards at VERIFIED.
         This is the real finish line and the verdict is taken from it:
         worker sub-cards come and go as the orchestrator releases work, and
         would move the "work" count without moving the plan (mixpeek-override,
         2026-10-01: "the finish line is the 220 plan items and the 58 proofs").
  work:  every card on the boards that is terminal (done, verified,
         discarded), reported for context
  proof: exact frozen done-line IDs with --epic; otherwise completion cards
         matching --proof-prefix across every executor in the orchestration.
         Moving an assignment never changes the proof or plan denominator.

Pace is measured, never estimated: `rate_6h` is cards CLOSED done or verified
in the last six hours (their closed_at), per hour, and `needed` is open cards
over hours left. Discards leave the open count but are not progress: they are
reported beside the rate so "finishing" by discarding is visible. The verdict
is ON PACE when rate_6h >= needed and BEHIND otherwise; with no closed_at on
any card it is UNMEASURED and says so rather than reading as on pace. Proof has a checkpoint of its own, judged by projecting its 6h rate to it: half the completion
cards verified by 24 hours before the deadline.

Written 2026-10-01 for goal spec 12 (Ethan: "it needs to be finished by
Sunday and verify that we're on pace continuously until then").
"""
import argparse, datetime as dt, json, os, ssl, subprocess, sys, urllib.request

TERMINAL = {"done", "verified", "discarded"}


def amux_url():
    try:
        return subprocess.run(["amux", "url"], capture_output=True, text=True, timeout=10).stdout.strip()
    except Exception:
        return os.environ.get("AMUX_URL", "")


def board(url):
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    with urllib.request.urlopen(f"{url}/api/board?all=1&slim=0", context=ctx, timeout=60) as r:
        return json.load(r)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--orchestrator", required=True)
    ap.add_argument("--lane-prefix", required=True)
    ap.add_argument("--deadline", required=True, help="ISO-8601 with offset")
    ap.add_argument("--proof-prefix", action="append", default=[])
    ap.add_argument("--plan-regex", help="titles of the orchestrator's plan-item cards, e.g. '^GS12 (plan item )?\\d+\\.\\d+'")
    ap.add_argument("--epic", help="use this epic's versioned frozen done line as the exact proof population")
    ap.add_argument("--done-line-file", help="read the done-line API response from this JSON file (tests)")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--board-file", help="read the board from this JSON file instead of the server (tests)")
    ap.add_argument("--now", help="ISO-8601 time to measure at (tests)")
    a = ap.parse_args()

    now = dt.datetime.fromisoformat(a.now) if a.now else dt.datetime.now(dt.timezone.utc)
    deadline = dt.datetime.fromisoformat(a.deadline)
    hours_left = max((deadline - now).total_seconds() / 3600, 0.0)

    url = None if a.board_file else amux_url()
    all_cards = json.load(open(a.board_file)) if a.board_file else board(url)
    cards = [
        c for c in all_cards
        if not c.get("archived")
        and ((c.get("session") or "") == a.orchestrator or (c.get("session") or "").startswith(a.lane_prefix))
        and c.get("type") not in ("epic", "watch", "tripwire")
    ]
    total = len(cards)
    terminal = sum(1 for c in cards if c.get("status") in TERMINAL)
    proof = [c for c in cards
             if any((c.get("title") or "").startswith(p) for p in a.proof_prefix)]
    scope = None
    if a.epic:
        try:
            if a.done_line_file:
                scope = json.load(open(a.done_line_file))
            else:
                ctx = ssl.create_default_context()
                ctx.check_hostname = False
                ctx.verify_mode = ssl.CERT_NONE
                with urllib.request.urlopen(f"{url}/api/contract/done-line/{a.epic}", context=ctx, timeout=30) as response:
                    scope = json.load(response)
            if scope.get("measured") is not True or scope.get("epic") != a.epic or not scope.get("line") or not scope.get("version"):
                raise ValueError("no nonempty versioned frozen done line")
            by_id = {c["id"]: c for c in all_cards}
            ids = [c["id"] for c in scope["line"]]
            if len(ids) != len(set(ids)):
                raise ValueError("duplicate card IDs in frozen done line")
            proof = [by_id.get(id, {"id": id, "status": "missing"}) for id in ids]
            invalid = [c["id"] for c in proof if c.get("archived") or c.get("deleted") or c.get("status") in ("missing", "discarded")]
            if invalid:
                raise ValueError("frozen scope contains missing, archived or discarded cards: " + ", ".join(invalid))
        except Exception as error:
            out = {"measured": False, "n_considered": 0, "verdict": "UNMEASURED", "why_unmeasured": str(error), "epic": a.epic}
            print(json.dumps(out) if a.json else f"UNMEASURED: {error}")
            return 1
    proof_verified = sum(1 for c in proof if c.get("status") == "verified")
    needsyou = sorted(c["id"] for c in cards if c.get("status") == "needsyou")

    snap = {"ts": now.timestamp(), "total": total, "terminal": terminal,
            "proof_total": len(proof), "proof_verified": proof_verified}
    hist_path = os.path.expanduser(f"~/.amux/logs/orch-pace-{a.orchestrator}.jsonl")
    hist = []
    if os.path.exists(hist_path):
        with open(hist_path) as f:
            hist = [json.loads(l) for l in f if l.strip()]
    os.makedirs(os.path.dirname(hist_path), exist_ok=True)
    with open(hist_path, "a") as f:
        f.write(json.dumps(snap) + "\n")

    def closed_ts(c):
        v = c.get("closed_at") or (c.get("entered_state_at") if c.get("status") in TERMINAL else None)
        try:
            return float(v)
        except (TypeError, ValueError):
            try:
                return dt.datetime.fromisoformat(str(v).replace("Z", "+00:00")).timestamp()
            except ValueError:
                return None
    since = now.timestamp() - 6 * 3600
    stamped = [c for c in cards if c.get("status") in TERMINAL and closed_ts(c) is not None]
    recent = [c for c in stamped if closed_ts(c) >= since]
    progressed = sum(1 for c in recent if c.get("status") in ("done", "verified"))
    discarded_6h = sum(1 for c in recent if c.get("status") == "discarded")
    rate = progressed / 6.0 if stamped else None
    open_cards = total - terminal
    needed = open_cards / hours_left if hours_left > 0 else float("inf")
    if rate is None:
        verdict = "UNMEASURED"
    elif rate >= needed:
        verdict = "ON PACE"
    else:
        verdict = "BEHIND"
    plan = None
    if a.plan_regex:
        import re
        rx = re.compile(a.plan_regex)
        items = [c for c in cards if rx.match(c.get("title") or "")]
        live = [c for c in items if c.get("status") != "discarded"]
        ver = [c for c in live if c.get("status") == "verified"]
        ver_6h = sum(1 for c in ver if (closed_ts(c) or c.get("last_verified_at") or 0) and float(closed_ts(c) or c.get("last_verified_at") or 0) >= since)
        p_rate = ver_6h / 6.0
        p_needed = (len(live) - len(ver)) / hours_left if hours_left > 0 else float("inf")
        plan = {"items": len(live), "verified": len(ver), "verified_6h": ver_6h,
                "rate_6h_per_h": round(p_rate, 2), "needed_per_h": round(p_needed, 2),
                "discarded": len(items) - len(live)}
        verdict = "ON PACE" if p_rate >= p_needed else "BEHIND"
    checkpoint = deadline - dt.timedelta(hours=24)
    # Before the checkpoint, project the proof rate to it: a target of zero
    # until the checkpoint passed read ON PACE at 6 of 66 with 33 due in
    # 4.5 hours (2026-10-03), so the verdict could only turn BEHIND too late.
    proof_target = len(proof) // 2
    proof_6h = sum(1 for c in proof if c.get("status") == "verified"
                   and (closed_ts(c) or 0) >= since)
    proof_rate = proof_6h / 6.0
    to_checkpoint = max((checkpoint - now).total_seconds() / 3600, 0.0)
    proof_projected = proof_verified + proof_rate * to_checkpoint
    proof_verdict = "ON PACE" if proof_projected >= proof_target else "BEHIND"

    out = {
        "measured": True, "n_considered": len(proof) if scope else total,
        "done_line": None if scope is None else {"epic": a.epic, "version": scope["version"], "cards": [c["id"] for c in proof]},
        "verdict": verdict, "hours_left": round(hours_left, 1),
        "cards_total": total, "cards_terminal": terminal, "cards_open": open_cards,
        "needed_per_h": round(needed, 2),
        "rate_6h_per_h": None if rate is None else round(rate, 2),
        "discarded_6h": discarded_6h,
        "history_snapshots": len(hist) + 1,
        "proof_verified": proof_verified, "proof_total": len(proof),
        "proof_checkpoint": f"{len(proof)//2} verified by {checkpoint.isoformat()}",
        "proof_verdict": proof_verdict,
        "proof_rate_6h_per_h": round(proof_rate, 2),
        "proof_projected_at_checkpoint": round(proof_projected, 1),
        "needsyou": needsyou,
        "plan": plan,
        "population": f"{a.orchestrator} + {a.lane_prefix}* boards, non-archived, epics/watches/tripwires excluded",
    }
    if a.json:
        print(json.dumps(out))
    else:
        if plan:
            print(f"{verdict}: {plan['verified']}/{plan['items']} plan items verified, "
                  f"{hours_left:.1f}h left -> need {plan['needed_per_h']:.2f}/h, measured {plan['rate_6h_per_h']:.2f}/h "
                  f"({plan['verified_6h']} verified in the last 6h; {plan['discarded']} plan items discarded)")
        r = "unmeasured (no card carries a close time)" if rate is None else f"{rate:.2f}/h done or verified"
        print(f"{'cards' if plan else verdict}: {terminal}/{total} cards terminal, {open_cards} open, "
              f"{hours_left:.1f}h left -> need {needed:.2f}/h, measured {r} over the last 6h "
              f"(plus {discarded_6h} discarded, not counted as progress)")
        print(f"proof: {proof_verified}/{len(proof)} completion cards verified ({proof_verdict}; "
              f"checkpoint {proof_target} by {checkpoint.isoformat()}; measured {proof_rate:.2f}/h, "
              f"projected {proof_projected:.1f} there)")
        print(f"needsyou: {', '.join(needsyou) or 'none'}")
        print(f"population: {out['population']}")
    return 0 if verdict != "BEHIND" and proof_verdict != "BEHIND" else 2


if __name__ == "__main__":
    sys.exit(main())
