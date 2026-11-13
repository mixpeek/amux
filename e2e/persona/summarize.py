#!/usr/bin/env python3
"""Summarize the persona runs in a folder: the newest result per use case
(uc<N>.json), every round kept in history/, and the router's override rate
from the household's /api/intent/stats snapshots in stats/ plus each run's
used_change. Prints a markdown table; --check exits 1 unless every newest
score is at least 4 and the override rate is under 10%.

    python3 e2e/persona/summarize.py e2e/persona/2026-11-13 [--check]
"""
import glob, json, os, sys

d = sys.argv[1]
check = "--check" in sys.argv
dims = ["finished", "correct", "no_jargon", "steps_like_chatgpt", "trust"]
banned = {"worker", "workers", "session", "sessions", "model", "provider", "tmux", "terminal", "card", "cards", "board", "scope", "yolo"}
rows, ok = [], True
for n in range(1, 10):
    p = os.path.join(d, f"uc{n}.json")
    if not os.path.exists(p):
        rows.append(f"| uc{n} | - | missing | | | | | |")
        ok = False
        continue
    r = json.load(open(p))
    sc = r.get("scores", {})
    hits = sorted({w for w in r.get("jargon_seen", []) if w.strip().lower() in banned})
    low = [k for k in dims if int(sc.get(k, 0)) < 4]
    ok = ok and not low and not hits
    rows.append(f"| uc{n} | {r.get('round')} | " + " | ".join(str(sc.get(k, "")) for k in dims)
                + f" | {', '.join(hits) or 'none'} | [{os.path.basename(p)}]({os.path.basename(p)}) |")
routed = overridden = 0
for p in glob.glob(os.path.join(d, "stats", "*.json")):
    s = json.load(open(p))
    routed += int(s.get("routed") or 0)
    overridden += int(s.get("overridden") or 0)
changes = sum(1 for p in glob.glob(os.path.join(d, "uc*.json")) if json.load(open(p)).get("used_change"))
rate = overridden / routed if routed else None
ok = ok and rate is not None and rate < 0.10
print("| case | round | finished | correct | no jargon | steps like ChatGPT | trust | banned words seen | run |")
print("|---|---|---|---|---|---|---|---|---|")
print("\n".join(rows))
print(f"\nRouter overrides: {overridden} of {routed} routed requests"
      + (f" ({rate:.0%})" if rate is not None else " (nothing routed: not measured)")
      + f"; evaluators who used Change: {changes}.")
if check:
    sys.exit(0 if ok else 1)
