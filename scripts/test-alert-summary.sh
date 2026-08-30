#!/usr/bin/env bash
# AMUX-3619 — the fire alarm must report the WORST channel outcome, not the best.
#
# `amux alert` has keyed off the server's `delivered_any` since AMUX-3151, so the
# ZERO-channel case ("reached nobody") already fails loudly. The case that was
# not handled is PARTIAL delivery: on 2026-08-24 a real disk escalation reached
# 1 of 3 channels — push had no subscriptions, iMessage failed on a missing
# Automation permission — and the CLI printed a bare `alert delivered →`.
#
# The per-channel detail was always in that line. What was wrong is that the
# SUMMARY reported the best outcome while the details reported the worst, which
# is the one-output-two-states shape landing on the fire alarm itself.
#
# It also flags an UNPINNED email destination: with AMUX_OWNER_EMAIL unset the
# server falls back to "the freshest connected account" and mails it TO ITSELF,
# so the destination can change without anyone touching alert config. That is not
# a failure and it is not a clean success.
#
# Drives the SHIPPED python block extracted from `amux`, never a copy — a copy
# passes forever while the real one rots.
#
# Exit 0 = pass, 1 = failure.
set -uo pipefail
cd "$(dirname "$0")/.."
command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not installed"; exit 0; }

python3 - <<'PY'
import json, subprocess, sys

src = open('amux').read()
start = "ch = d.get('channels', {})"
end   = "print('alert delivered →', detail)"
if start not in src or end not in src:
    sys.exit("FAIL — the alert summary block moved or was renamed; this test is now blind")
i = src.index(start); j = src.index(end, i) + len(end)
block = src[i:j]

def run(payload):
    prog = "import sys, json\nd = json.loads(sys.argv[1])\n" + block
    r = subprocess.run([sys.executable, "-c", prog, json.dumps(payload)],
                       capture_output=True, text=True)
    return r.returncode, (r.stdout + r.stderr)

fails = 0
def ok(cond, msg):
    global fails
    if cond: print("  ok   — " + msg)
    else:    fails += 1; print("  FAIL — " + msg)

# 1. Every channel delivered: the clean line, exit 0.
code, out = run({"delivered_any": True,
                 "channels": {"email": "email via ethan@mixpeek.com", "push": "sent", "sms": "imessage"}})
ok(code == 0 and "alert delivered" in out and "PARTLY" not in out,
   "all channels delivered -> a clean 'alert delivered', exit 0")

# 2. THE CASE THIS EXISTS FOR, verbatim from the 2026-08-24 escalation.
code, out = run({"delivered_any": True, "channels": {
    "email": "email via info@mixpeek.com -> info@mixpeek.com [UNPINNED: freshest connected account (set AMUX_OWNER_EMAIL)]",
    "push":  "error: no push subscriptions — nobody is registered to receive it",
    "sms":   "failed: imessage error: 54:97: execution error"}})
ok("PARTLY DELIVERED" in out, "1-of-3 must NOT lead with a bare 'delivered'")
ok("2 of 3" in out and "push" in out and "sms" in out, "it must name HOW MANY failed and WHICH")
ok("AMUX_OWNER_EMAIL" in out, "an unpinned destination must name the knob that pins it")

# 3. Reached nobody: unchanged, still exit 3. The regression guard on AMUX-3151.
code, out = run({"delivered_any": False, "channels": {"push": "error: none", "sms": "failed: x"}})
ok(code == 3 and "ALERT FAILED" in out,
   "zero channels still exits 3 — partial reporting must not soften the total failure")

# 4. CONTROL. A clean delivery must not be reported as partial, or the warning
#    becomes noise on every alert and stops being read, which is the defect one
#    layer along.
code, out = run({"delivered_any": True, "channels": {"email": "email via ethan@mixpeek.com"}})
ok(code == 0 and "PARTLY" not in out and "AMUX_OWNER_EMAIL" not in out,
   "a single clean channel is NOT partial and NOT unpinned")

print()
sys.exit(1 if fails else 0)
PY
