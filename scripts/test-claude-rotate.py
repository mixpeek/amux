#!/usr/bin/env python3
"""claude-rotate switches BEFORE the current account hits its limit, and takes
workers parked on Claude's limit prompt back to work (Ethan, 2026-10-15:
"workers shouldn't never be rate limited, u needs to switch login before then";
amux-app sat on the weekly-limit prompt while three switches failed). Loads the
shipped script as a module; no network, no CLI."""
import importlib.util
import os
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.environ["CLAUDE_ROTATE_DIR"] = tempfile.mkdtemp()
os.environ["CLAUDE_ROTATE_USAGE_LOG"] = "off"
os.environ["CLAUDE_ROTATE_LOG"] = os.path.join(os.environ["CLAUDE_ROTATE_DIR"], "log")
spec = importlib.util.spec_from_file_location("rot", os.path.join(ROOT, "scripts", "claude-rotate.py"))
rot = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rot)

PASS = FAIL = 0


def expect(cond, label):
    global PASS, FAIL
    if cond:
        PASS += 1
    else:
        FAIL += 1
        print("FAIL:", label)


def row(email, five, week, resets="2026-10-20T00:00:00-04:00"):
    head = min(rot.SWITCH_AT - five, rot.SWITCH_AT - week)
    return {"email": email, "known": True, "usable": head > rot.MIN_HEADROOM, "headroom": head,
            "five_hour": five, "weekly": week, "weekly_resets": resets}


old = {"switched_at": 0}
# Under the preempt line: stay.
t, why = rot.decide("a", [row("a", 10, 70), row("b", 0, 20, "2026-10-25T00:00:00-04:00")], old)
expect(t is None, f"70% weekly holds ({why})")
# Over the preempt line, another account has more headroom: switch now, before the limit.
rows = [row("a", 10, 88, "2026-10-17T00:00:00-04:00"), row("b", 0, 20, "2026-10-25T00:00:00-04:00")]
rows.sort(key=lambda r: (not r["usable"], r["weekly_resets"]))
t, why = rot.decide("a", rows, old)
expect((t, why) == ("b", "preempt_limit"), f"88% weekly switches early, got {(t, why)}")
# The five-hour window counts too.
rows = [row("a", 90, 30, "2026-10-17T00:00:00-04:00"), row("b", 0, 20, "2026-10-25T00:00:00-04:00")]
t, why = rot.decide("a", rows, old)
expect((t, why) == ("b", "preempt_limit"), f"90% five-hour switches early, got {(t, why)}")
# Over the line but nothing better: hold rather than flap to a worse account.
rows = [row("a", 10, 88, "2026-10-17T00:00:00-04:00"), row("b", 0, 95, "2026-10-25T00:00:00-04:00")]
t, why = rot.decide("a", rows, old)
expect(t is None, f"no account with more headroom: hold, got {(t, why)}")

# Past HOT_AT with nothing cool(): take the roomiest usable account now, even
# inside the switch gap (2026-10-17: 97% five-hour held as "current_is_fine").
rows = [row("a", 95, 49, "2026-10-24T00:00:00-04:00"), row("b", 0, 94, "2026-10-21T00:00:00-04:00"),
        row("c", 38, 85, "2026-10-23T00:00:00-04:00")]
t, why = rot.decide("a", rows, {"switched_at": __import__("time").time() - 60})
expect((t, why) == ("c", "preempt_hot"), f"95% five-hour takes the roomiest account, got {(t, why)}")
t, why = rot.decide("c", [row("c", 40, 86), row("a", 97, 49)], old)
expect(t is None, f"the hot account just left is not a target, got {(t, why)}")

# A target that failed FAIL_BACKOFF_N switches inside the window is skipped
# (2026-10-16: 98 of 101 switches to one account failed, ~4 min each, every 5 min).
import time as _t
rows = [row("a", 10, 30, "2026-10-25T00:00:00-04:00"), row("b", 0, 20, "2026-10-17T00:00:00-04:00"),
        row("c", 0, 25, "2026-10-18T00:00:00-04:00")]
rows.sort(key=lambda r: (not r["usable"], r["weekly_resets"]))  # rank()'s order
t, why = rot.decide("a", rows, old)
expect(t == "b", f"without failures the soonest-expiring account is picked, got {(t, why)}")
nowt = _t.time()
failed = {"switched_at": 0, "switch_failures": {"b": [nowt - 60, nowt - 400, nowt - 700]}}
t, why = rot.decide("a", rows, failed)
expect(t == "c", f"a backed-off account is skipped for the next one, got {(t, why)}")
stale = {"switched_at": 0, "switch_failures": {"b": [nowt - 7 * 3600] * 3}}
t, why = rot.decide("a", rows, stale)
expect(t == "b", f"failures outside the window do not count, got {(t, why)}")
two = {"switched_at": 0, "switch_failures": {"b": [nowt - 60, nowt - 400]}}
# A target whose sign-in mailbox token is dead is skipped on the FIRST tick, not
# after FAIL_BACKOFF_N four-minute failures (2026-10-17: hello@amux.io needs_reauth).
t, why = rot.decide("a", rows, old, unreachable={"B"})
expect(t == "c", f"an account whose sign-in link cannot be read is skipped, got {(t, why)}")
t, why = rot.decide("b", rows, old, unreachable={"b"})
expect(t != "b", f"the current account is never its own target, got {(t, why)}")
expect(rot.decide("a", rows, two)[0] == "b", "two failures are not yet a backoff")
expect(rot.backed_off(failed) == {"b"}, "backed_off names the account")

# The soonest-expiring account is not picked when it is already over the
# preempt line and another is under it: the next tick would switch back out.
rows = [row("v", 0, 93, "2026-10-21T11:00:00-04:00"), row("j", 0, 27, "2026-10-23T02:59:00-04:00"),
        row("m", 70, 18, "2026-10-23T08:00:00-04:00")]
t, why = rot.decide("m", rows, old)
expect(t == "j", f"an account over the preempt line is not a switch target, got {(t, why)}")

# The sign-in link search covers spam (every link to one account landed there).
seen = []
rot.amux_get = lambda path: seen.append(path) or []
rot.magic_link("x@example.com", 0)
expect(seen and "in%3Aanywhere" in seen[0], f"magic-link search includes spam, got {seen[:1]}")

# Stranded workers: running, rate limited, not isolated.
names, iso = rot.stranded([
    {"name": "w1", "running": True, "status": "rate_limited"},
    {"name": "w2", "running": True, "status": "active"},
    {"name": "w3", "running": True, "status": "rate_limited", "isolated": True},
    {"name": "w4", "running": False, "status": "rate_limited"},
])
expect(names == ["w1"] and iso == ["w3"], f"stranded {names} isolated {iso}")
# Only Claude's own limit prompt gets a keypress.
expect(rot.resume_action("What do you want to do?\n 1. Stop and wait for limit to reset\n 2. Wait here") == "1",
       "the limit prompt is answered with 1")
expect(rot.resume_action("❯ fix the deploy gate") is None, "a composer gets no stray keypress")


# A status line far above a recent /usage probe is another process's stale
# number (2026-10-17: probe 13% weekly, status lines 100%, the account ranked
# spent and four browser switches away from it failed).
import time as _t
acct = {"probed": {"seven_day": {"used": 13, "at": _t.time()}}}
expect(rot.contradicts_probe(acct, "seven_day", 100), "100% status line vs 13% probe is rejected")
expect(not rot.contradicts_probe(acct, "seven_day", 18), "a small rise over the probe is kept")
acct_old = {"probed": {"seven_day": {"used": 13, "at": _t.time() - rot.PROBE_TRUST_S - 5}}}
expect(not rot.contradicts_probe(acct_old, "seven_day", 100), "an old probe no longer outranks status lines")
rot.record("p@x", 47, "2026-10-17T23:50:00-04:00", 13, "2026-10-17T21:00:00-04:00", source="usage-probe")
expect(rot.record("p@x", 45, "2026-10-17T23:50:00-04:00", 100, "2026-10-17T20:59:00-04:00") is False,
       "record() refuses the contradicting status line")
rot.record("q@x", 45, "2026-10-17T23:50:00-04:00", 100, "2026-10-17T20:59:00-04:00")
rot.record("q@x", 47, "2026-10-17T23:50:00-04:00", 13, "2026-10-17T21:00:00-04:00", source="usage-probe")
q = rot.load(rot.ACCOUNTS, {})["accounts"]["q@x"]["seven_day"]["used_percentage"]
expect(q == 13, f"a probe lower than a status line's number corrects it, got {q}")
stored = rot.load(rot.ACCOUNTS, {})["accounts"]["p@x"]["seven_day"]["used_percentage"]
expect(stored == 13, f"the probe's 13% stands, got {stored}")

# A switch to an account with a stored login never opens the browser, and a
# failed swap puts the previous login back.
kc = {(rot.CRED_SVC, rot.CRED_ACCT): '{"claudeAiOauth": {"refreshToken": "old"}, "mcpOAuth": {"m": 1}}'}
rot._kc_read = lambda s, a: kc.get((s, a))
rot._kc_write = lambda s, a, v: kc.__setitem__((s, a), v) or True
rot.capture_login = lambda: None
rot.resume_stranded = lambda force=False: None
rot.resume_auth_lost = lambda: None
rot.CLAUDE_JSON = os.path.join(os.environ["CLAUDE_ROTATE_DIR"], "claude.json")
rot.save(rot.CLAUDE_JSON, {"oauthAccount": {"emailAddress": "a@x"}})
expect(rot.stored_switch("b@x", "a@x", "preempt_limit", _t.time()) is None, "no stored login: browser fallback")
kc[(rot.STORE_SVC, "b@x")] = '{"claudeAiOauth": {"refreshToken": "new"}, "oauthAccount": {"emailAddress": "b@x"}}'
rot.cli_email = lambda: rot.load(rot.CLAUDE_JSON, {})["oauthAccount"]["emailAddress"]
rot.probe = lambda: {"seven_day": {}}
expect(rot.stored_switch("b@x", "a@x", "preempt_limit", _t.time()) == 0, "stored login switches by CLI")
import json as _j
cred = _j.loads(kc[(rot.CRED_SVC, rot.CRED_ACCT)])
expect(cred["claudeAiOauth"]["refreshToken"] == "new" and cred["mcpOAuth"] == {"m": 1},
       "the swap replaces only claudeAiOauth and keeps MCP logins")
rot.probe = lambda: None
kc[(rot.STORE_SVC, "a@x")] = '{"claudeAiOauth": {"refreshToken": "a"}, "oauthAccount": {"emailAddress": "a@x"}}'
before = kc[(rot.CRED_SVC, rot.CRED_ACCT)]
expect(rot.stored_switch("a@x", "b@x", "preempt_limit", _t.time()) is None, "an unproven swap falls back")
expect(kc[(rot.CRED_SVC, rot.CRED_ACCT)] == before and rot.current_email() == "b@x",
       "and the previous login is restored")


# Nothing under the preempt line: the roomiest account wins over the
# soonest-resetting nearly full one.
rows = [row("a", 99, 26, "2026-10-17T21:00:00-04:00"), row("b", 53, 95, "2026-10-23T03:00:00-04:00"),
        row("c", 0, 90, "2026-10-24T02:00:00-04:00")]
rows.sort(key=lambda r: (not r["usable"], r["weekly_resets"]))
t2, why = rot.decide("a", rows, {"switched_at": 0})
expect(t2 == "c", f"spent current takes the roomiest target, got {(t2, why)}")

# A working browser sign-in clears backoffs caused by the browser, and keeps
# the one caused by an unreadable sign-in link.
st = {"switch_failures": {"h@x": [1, 2, 3], "m@x": [4, 4, 4], "t@x": [5]}, "account_failures": ["m@x"]}
cleared = rot.forgive_browser_failures(st, "t@x")
expect(cleared == ["h@x"] and list(st["switch_failures"]) == ["m@x"], f"browser failures forgiven, got {st}")


# 4% remaining is the latest switch point: 96% used is spent, 95% is not.
expect(not row("a", 96, 10)["usable"] and row("a", 95, 10)["usable"], "96% used is spent, 95% is usable")
t3, why = rot.decide("a", [row("a", 10, 96), row("b", 0, 20)], {"switched_at": __import__("time").time()})
expect((t3, why) == ("b", "current_exhausted"), f"96% weekly switches even inside the switch gap, got {(t3, why)}")
# A backed-off account is the last resort, never no account at all; a dead
# mailbox is still skipped.
now_s = __import__("time").time()
st = {"switch_failures": {"h": [now_s, now_s, now_s]}}
t4, why = rot.decide("t", [row("t", 98, 74), row("h", 0, 23)], st)
expect(t4 == "h", f"backed-off h is used when it is the only room, got {(t4, why)}")
t5, why = rot.decide("t", [row("t", 98, 74), row("h", 0, 23)], st, unreachable=["h"])
expect(t5 is None, f"a dead mailbox stays skipped, got {(t5, why)}")


# A five-hour status line well above the last probe is kept: that window can
# climb 15 points between probes (2026-10-17 22:17, 86% rejected against 73%).
rot.record("f@x", 73, "2099-01-01T05:00:00-04:00", 20, "2099-01-05T00:00:00-04:00", source="usage-probe")
expect(rot.record("f@x", 86, "2099-01-01T05:00:00-04:00", 21, "2099-01-05T00:00:00-04:00") is True,
       "a fast-rising five-hour status line is accepted")


# The timer screen newer Claude Code shows on a limit is resumed with Escape,
# word-by-word colour codes included (studio-plg's pane, 2026-10-17).
timer = "\x1b[38;5;231mContinuing\x1b[39m \x1b[38;5;231mautomatically\x1b[39m at 1:40am \x1b[38;5;231m·\x1b[39m esc to cancel"
expect(rot.resume_action(timer) == "Escape", f"timer screen resumes with Escape, got {rot.resume_action(timer)!r}")
expect(rot.resume_action("Stop and wait for limit to reset") == "1", "the menu still answers 1")
expect(rot.resume_action("working on it") is None, "no keys for a worker that is not parked")
shortly = "  \u26a0 Usage limit reached \u00b7 limit resets 1pm\n    Continuing shortly \u00b7 esc to cancel\n  \u23f5\u23f5 bypass permissions on"
expect(rot.resume_action(shortly) == "Escape", f"the 'Continuing shortly' timer resumes with Escape, got {rot.resume_action(shortly)!r}")


# An installed copy that differs from origin/main says so; the shipped script
# agrees with itself through a repo whose origin/main holds the same bytes.
import subprocess as _sp
_repo = tempfile.mkdtemp()
_sp.run(["git", "init", "-q", "-b", "main", _repo]); os.makedirs(os.path.join(_repo, "scripts"))
open(os.path.join(_repo, "scripts", "claude-rotate.py"), "w").write("different\n")
_sp.run(["git", "-C", _repo, "add", "."]); _sp.run(["git", "-C", _repo, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "x"])
_sp.run(["git", "-C", _repo, "update-ref", "refs/remotes/origin/main", "HEAD"])
rot.REPO = _repo
d = rot.install_drift()
expect(d is not None and d["lines_differing"] > 0, f"drift from origin/main is reported, got {d}")
open(os.path.join(_repo, "scripts", "claude-rotate.py"), "w").write(open(os.path.join(ROOT, "scripts", "claude-rotate.py")).read())
_sp.run(["git", "-C", _repo, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qam", "y"])
_sp.run(["git", "-C", _repo, "update-ref", "refs/remotes/origin/main", "HEAD"])
expect(rot.install_drift() is None, "an install equal to origin/main reports nothing")

# A mailbox that recovers forgives the switch failures it caused while dead
# (2026-10-17: hello@amux.io sat in a 6h backoff after its mailbox came back).
st = {"switch_failures": {"h@x": [100.0, 200.0], "o@x": [150.0]}}
expect(rot.forgive_recovered_mailboxes(st, {"h@x"}, at_s=300.0) == [], "nothing is forgiven while the mailbox is dead")
expect(st["switch_failures"]["h@x"] == [100.0, 200.0], "failures are kept while dead")
expect(rot.forgive_recovered_mailboxes(st, None, at_s=400.0) == [], "an unmeasured health read forgives nothing")
expect("h@x" in st["mailbox_dead_seen"], "an unmeasured read keeps the dead record")
st["switch_failures"]["h@x"].append(350.0)  # failed after it was last seen dead: not the mailbox's fault
expect(rot.forgive_recovered_mailboxes(st, set(), at_s=500.0) == ["h@x"], "recovery clears the account")
expect(st["switch_failures"]["h@x"] == [350.0], f"only failures from while dead are dropped, got {st['switch_failures']}")
expect(st["switch_failures"]["o@x"] == [150.0], "an account that was never dead keeps its failures")
expect("h@x" not in st["mailbox_dead_seen"], "the dead record is consumed")

print(f"test-claude-rotate: {PASS} passed, {FAIL} failed")
sys.exit(1 if FAIL else 0)
