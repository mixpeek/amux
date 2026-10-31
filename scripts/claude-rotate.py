#!/usr/bin/env python3
"""Keep every Claude subscription account fully used by switching the one
machine-wide Claude CLI login to the account whose capacity expires soonest.

All workers share one login (the CLI's default credential), and a running
worker picks up a new login without a restart, so a switch disrupts nothing.

Subcommands:
  statusline     Claude Code status line command. Records the live rate_limits
                 reading under the logged-in account and prints a short line.
  pick [--json]  Rank accounts from the saved readings.
  tick           Probe usage, then switch when the policy says so. Every few minutes.
  probe          Read the logged-in account's usage from /usage in a temporary CLI.
  switch EMAIL   Log the CLI into EMAIL: `claude auth login --email` with the
                 browser page sent to the Chrome profile that holds the
                 accounts, email sign-in link read from the forwarded inbox.
  record EMAIL 5H 5H_RESET WK WK_RESET   Save a reading by hand (ISO times).
  status         Show the current account, its reading and the last switch.
  history [H]    Per-account usage history from the amux DB over the last H
                 hours (default 168): latest reading, burn rate, hours to full.
  on | off       Opt this machine in to (or out of) automatic switching.

Automatic switching is opt-in and off by default: tick only switches when
~/.amux/claude-usage/enabled exists (`on` writes it; the installer only with
CLAUDE_ROTATE_ENABLE=1)
or CLAUDE_ROTATE_ENABLED=1. CLAUDE_ROTATE_ENABLED=0 or a `paused` file there
turns it off even when enabled. Manual `switch EMAIL` always works.

State lives in ~/.amux/claude-usage (accounts.json, state.json). Every
accepted reading is also stored in the amux DB (table claude_account_usage,
GET /api/usage/accounts); each tick backfills any account the DB is behind on.
Every decision
is one JSON line in ~/.amux/logs/claude-rotate.log with a `verdict`.

Policy, readings for an account that is not logged in can only fall, so a
stale reading is a safe upper bound, and a window whose reset has passed counts
as 0. An account is usable while both windows have MIN_HEADROOM below
SWITCH_AT. Among usable accounts the one whose weekly window resets first
wins, since its headroom is lost at that reset. tick switches when the current
account stops being usable, or when another usable account's weekly reset is
earlier than the current one's (spend expiring capacity first). Unknown
accounts are used only after every known one.
"""
import datetime as dt
import fcntl
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

HOME = os.path.expanduser("~")
DIR = os.environ.get("CLAUDE_ROTATE_DIR", os.path.join(HOME, ".amux", "claude-usage"))
ACCOUNTS = os.path.join(DIR, "accounts.json")
STATE = os.path.join(DIR, "state.json")
LOCK = os.path.join(DIR, "switch.lock")
LOG = os.environ.get("CLAUDE_ROTATE_LOG", os.path.join(HOME, ".amux", "logs", "claude-rotate.log"))
CLAUDE_JSON = os.environ.get("CLAUDE_ROTATE_CLAUDE_JSON", os.path.join(HOME, ".claude.json"))
# SWITCH BY 4% REMAINING (Ethan, 2026-10-17: "make sure the login change
# happens [when] we're at 4% remaining"). An account is spent once either
# window has MIN_HEADROOM or less left under SWITCH_AT, so the current one is
# left at 96% used at the latest and no target is picked at or above it.
SWITCH_AT = float(os.environ.get("CLAUDE_SWITCH_AT", "100"))
MIN_HEADROOM = float(os.environ.get("CLAUDE_MIN_HEADROOM", "4"))
# SWITCH BEFORE THE LIMIT (Ethan, 2026-10-15: "workers shouldn't never be rate
# limited, u needs to switch login before then"). Switching only once the
# current account was exhausted (SWITCH_AT) let amux-app hit the weekly limit:
# getmixpeek ran out at 22:45, three email sign-ins failed (about 4 min each)
# and the switch landed at 23:03. Once either window of the current account
# passes PREEMPT_AT, move to a usable account with more headroom. Targets stay
# usable up to SWITCH_AT, so the last few percent of every account still get
# spent, just not by the account the fleet is standing on.
PREEMPT_AT = float(os.environ.get("CLAUDE_PREEMPT_AT", "85"))
# A preempt target must sit this far under PREEMPT_AT in BOTH windows. On
# 2026-10-16 joinmixpeek (5h 86%) preempted to mixpeek (5h 81%), which hit 85%
# twenty minutes later and preempted straight back; both 5h windows reset at
# 13:00, joinmixpeek ran out at 12:17 and the fleet stopped.
PREEMPT_MARGIN = float(os.environ.get("CLAUDE_PREEMPT_MARGIN", "20"))
# Past HOT_AT the current account is minutes from the limit, so any usable
# account with more headroom beats holding. 2026-10-17: ethan@trymixpeek.com
# reached 97% of its 5h window with "current_is_fine" because no other account
# was cool() (every weekly window sat at 85% or more).
HOT_AT = float(os.environ.get("CLAUDE_HOT_AT", "95"))
# A worker already parked on Claude's limit prompt is resumed at most this often.
RESUME_GAP_S = int(os.environ.get("CLAUDE_ROTATE_RESUME_GAP_S", "600"))
# Readings arriving this soon after a switch may still describe the previous
# account (the CLI reports headers from its last response), so they are dropped.
SETTLE_S = int(os.environ.get("CLAUDE_ROTATE_SETTLE_S", "180"))
MIN_SWITCH_GAP_S = int(os.environ.get("CLAUDE_ROTATE_MIN_GAP_S", "900"))
CHROME_PROFILE = os.environ.get("CLAUDE_ROTATE_CHROME_PROFILE", "Profile 16")
CLAUDE = os.environ.get("CLAUDE_ROTATE_CLAUDE", "claude")
# The probe CLI starts here; it must be a folder the owner already trusts.
PROBE_CWD = os.environ.get("CLAUDE_ROTATE_PROBE_CWD", os.path.join(HOME, "Dev"))
SWITCH_TIMEOUT_S = int(os.environ.get("CLAUDE_ROTATE_SWITCH_TIMEOUT_S", "240"))


def now():
    return dt.datetime.now().astimezone()


def log(verdict, **fields):
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    line = json.dumps({"ts": now().isoformat(timespec="seconds"), "verdict": verdict, **fields})
    with open(LOG, "a") as f:
        f.write(line + "\n")
    return line


def load(path, default):
    try:
        with open(path) as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


def save(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".tmp-")
    with os.fdopen(fd, "w") as f:
        json.dump(data, f, indent=2)
    os.replace(tmp, path)


class locked:
    """Exclusive lock on a file; blocking unless nowait."""

    def __init__(self, path, nowait=False):
        self.path, self.nowait = path, nowait

    def __enter__(self):
        os.makedirs(os.path.dirname(self.path), exist_ok=True)
        self.f = open(self.path, "a")
        try:
            fcntl.flock(self.f, fcntl.LOCK_EX | (fcntl.LOCK_NB if self.nowait else 0))
        except BlockingIOError:
            self.f.close()
            return False
        return True

    def __exit__(self, *exc):
        if not self.f.closed:
            fcntl.flock(self.f, fcntl.LOCK_UN)
            self.f.close()


def current_email():
    """The logged-in account, from the CLI's own config (no credential read)."""
    acct = (load(CLAUDE_JSON, {}) or {}).get("oauthAccount") or {}
    return acct.get("emailAddress")


def cli_email():
    """The logged-in account as the CLI reports it."""
    try:
        out = subprocess.run([CLAUDE, "auth", "status", "--json"], capture_output=True, text=True, timeout=30).stdout
        return json.loads(out).get("email")
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return None


def iso(epoch):
    return dt.datetime.fromtimestamp(float(epoch)).astimezone().isoformat(timespec="seconds")


# ---------------------------------------------------------------- readings

def _reset(value):
    try:
        return dt.datetime.fromisoformat(value) if value else None
    except ValueError:
        return None


def merge_window(known, used, reset, source, at=None):
    """Fold one reading of a window into what is known for that account.

    Returns (reading or None, why). Every session reports the numbers from
    its own last API call, so an idle session keeps saying 39% after the
    account reached 59%, and a session still holding the previous login says
    98% under the new one. Inside one reset window usage only rises, so a
    lower number is stale; a status line whose weekly reset differs from the
    account's known, still-future reset belongs to another account. The
    /usage probe and manual records are first-hand and may move a reset."""
    at = at or now()
    new = {"used_percentage": used, "resets_at": reset}
    if source == "usage-probe":
        # The account's own /usage screen: it outranks whatever a status line
        # left behind, including a higher number in the same window.
        return new, "probe"
    known = known or {}
    k_reset, n_reset = _reset(known.get("resets_at")), _reset(reset)
    if known.get("used_percentage") is None or k_reset is None or k_reset <= at or n_reset is None:
        return new, "new_window"
    if abs((k_reset - n_reset).total_seconds()) > 3600:
        if source == "statusline":
            return None, "other_account_window"
        return new, "reset_moved"
    if float(used) < float(known["used_percentage"]):
        return dict(known), "stale_lower"
    return new, "rose"


PROBE_TRUST_S = int(os.environ.get("CLAUDE_ROTATE_PROBE_TRUST_S", "900"))
PROBE_JUMP = float(os.environ.get("CLAUDE_ROTATE_PROBE_JUMP", "10"))


def contradicts_probe(acct, key, used, at_s=None):
    """A status-line reading far above a recent /usage probe of the same window.
    A worker's status line repeats its own last API response, so a process that
    hit the limit earlier, or one still on another login, keeps saying 100%.
    2026-10-17: the probe read ethan@runmixpeek.com at 13% weekly while status
    lines said 100%; the status lines won, the account was ranked spent, and
    four browser switches away from it failed over 19 minutes. The probe reads
    the account's own /usage screen, so inside PROBE_TRUST_S it wins."""
    at_s = at_s or time.time()
    p = (acct.get("probed") or {}).get(key) or {}
    if p.get("used") is None or at_s - float(p.get("at") or 0) > PROBE_TRUST_S:
        return False
    return float(used) - float(p["used"]) > PROBE_JUMP


def record(email, five=None, five_reset=None, week=None, week_reset=None, source="statusline"):
    """Store one reading. Returns False when it was rejected as another
    account's numbers or held nothing newer than what is known."""
    with locked(ACCOUNTS + ".lock"):
        data = load(ACCOUNTS, {"accounts": {}})
        acct = data["accounts"].setdefault(email, {})
        changed = source != "statusline"
        if source == "statusline":
            # Weekly only: the five-hour window can climb 15 points between
            # probes (2026-10-17 22:17: 85-86% status lines were rejected
            # against a 73% probe, the live trigger never fired, and the
            # account reached 98% before the next tick).
            for key, used in (("seven_day", week),):
                if used is not None and contradicts_probe(acct, key, used):

                    log("claude_rotate_reading_rejected", account=email, source=source, reason="contradicts_probe",
                        window=key, used=used, probed=acct["probed"][key]["used"])
                    return False
        elif source == "usage-probe":
            probed = acct.setdefault("probed", {})
            for key, used in (("seven_day", week), ("five_hour", five)):
                if used is not None:
                    probed[key] = {"used": used, "at": time.time()}
        if week is not None:
            merged, why = merge_window(acct.get("seven_day"), week, week_reset, source)
            if merged is None:
                log("claude_rotate_reading_rejected", account=email, source=source, reason=why,
                    weekly=week, resets_at=week_reset, known_resets_at=acct["seven_day"].get("resets_at"))
                return False
            changed = changed or why != "stale_lower"
            acct["seven_day"] = merged
        if five is not None:
            merged, why = merge_window(acct.get("five_hour"), five, five_reset, source)
            changed = changed or why != "stale_lower"
            acct["five_hour"] = merged
        if not changed:
            return False
        acct["observed_at"] = now().isoformat(timespec="seconds")
        acct["source"] = source
        save(ACCOUNTS, data)
        snapshot = dict(acct)
    # Status lines render constantly; never make one wait on the server.
    usage_log_post(email, snapshot, wait=source != "statusline")
    return True


USAGE_LOG_PATH = "/api/usage/accounts"
# "off" stops writes to the amux DB history (the tests set it).
USAGE_LOG = os.environ.get("CLAUDE_ROTATE_USAGE_LOG", "on")


def _observed_epoch(acct):
    try:
        return dt.datetime.fromisoformat(acct["observed_at"]).timestamp()
    except (KeyError, TypeError, ValueError):
        return None


def usage_log_body(email, acct):
    """One accounts.json entry as a reading for the amux DB history
    (POST /api/usage/accounts/readings, table claude_account_usage)."""
    five, week = acct.get("five_hour") or {}, acct.get("seven_day") or {}
    return {
        "account": email,
        "source": acct.get("source") or "unknown",
        "at": _observed_epoch(acct),
        "five_hour_pct": five.get("used_percentage"),
        "five_hour_resets_at": five.get("resets_at"),
        "seven_day_pct": week.get("used_percentage"),
        "seven_day_resets_at": week.get("resets_at"),
        "plan": acct.get("plan"),
    }


def _amux_base():
    return os.environ.get("CLAUDE_ROTATE_AMUX_URL") or os.environ.get("AMUX_URL") or subprocess.run(
        ["amux", "url"], capture_output=True, text=True, timeout=10).stdout.strip()


def usage_log_post(email, acct, wait=True):
    """Add a reading to the amux DB history. Returns the server's reply when
    waited for, else None. A missed post is caught by usage_log_sync on the
    next tick, so this never raises."""
    body = usage_log_body(email, acct)
    if USAGE_LOG == "off" or body["five_hour_pct"] is None and body["seven_day_pct"] is None:
        return None
    try:
        cmd = ["curl", "-sk", "--max-time", "5", "-X", "POST", "-H", "Content-Type: application/json",
               "-d", json.dumps(body), _amux_base() + USAGE_LOG_PATH + "/readings"]
        if not wait:
            subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
            return None
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=10).stdout
        return json.loads(out) if out.strip() else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def usage_log_sync():
    """Make the amux DB history hold every account's latest reading. Posts the
    ones it lacks and logs what it did, so a server that was down or a dropped
    status-line post is repaired and visible in the log."""
    if USAGE_LOG == "off":
        return None
    try:
        out = subprocess.run(["curl", "-sk", "--max-time", "10", _amux_base() + USAGE_LOG_PATH + "?since_h=2160"],
                             capture_output=True, text=True, timeout=15).stdout
        have = {a["account"]: a["latest"]["at"] for a in json.loads(out)["accounts"]}
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as e:
        log("claude_rotate_usage_log_unreachable", measured=False, why_unmeasured=str(e)[:200] or "empty reply")
        return None
    accounts = load(ACCOUNTS, {"accounts": {}})["accounts"]
    added, failed = [], []
    for email, acct in sorted(accounts.items()):
        at = _observed_epoch(acct)
        if at is None or (email in have and have[email] >= at - 1):
            continue
        reply = usage_log_post(email, acct)
        (added if reply and reply.get("ok") else failed).append(email)
    if added or failed:
        log("claude_rotate_usage_log_backfilled", measured=True, n_considered=len(accounts),
            added=added, failed=failed)
    return {"added": added, "failed": failed, "n_considered": len(accounts)}


REPORTERS = os.path.join(DIR, "reporters")


def note_reporter():
    """Touch one file per calling CLI process, so `status` can say how many
    sessions are feeding readings (a lone reporter makes readings lag)."""
    try:
        os.makedirs(REPORTERS, exist_ok=True)
        path = os.path.join(REPORTERS, str(os.getppid()))
        if not os.path.exists(path) or time.time() - os.path.getmtime(path) > 60:
            open(path, "w").close()
    except OSError:
        pass


def reporters(window_s=600):
    try:
        names = os.listdir(REPORTERS)
    except OSError:
        return 0
    live = 0
    for n in names:
        p = os.path.join(REPORTERS, n)
        try:
            if time.time() - os.path.getmtime(p) <= window_s:
                live += 1
            elif time.time() - os.path.getmtime(p) > 86400:
                os.remove(p)
        except OSError:
            pass
    return live


def trigger_if_exhausted(email, five, week):
    """Start a detached tick as soon as a live reading crosses the threshold.

    The fleet can spend 1% of a week in a few minutes, so waiting for the
    5-minute schedule overshoots. At most one trigger per 90 s; the switch
    lock stops two switches running at once."""
    used = max(float(five.get("used_percentage") or 0), float(week.get("used_percentage") or 0))
    if SWITCH_AT - used > MIN_HEADROOM:
        return False
    stamp = os.path.join(DIR, "last-trigger")
    try:
        if time.time() - os.path.getmtime(stamp) < 90:
            return False
    except OSError:
        pass
    open(stamp, "w").close()
    log("claude_rotate_live_trigger", current=email, used=used)
    subprocess.Popen([sys.executable, os.path.abspath(__file__), "tick"], stdin=subprocess.DEVNULL,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    return True


def login_seen_since(email):
    """When this login was first seen. The CLI changes account before the
    switch records its time, and a worker's next status line still carries the
    previous account's numbers, so the settle window starts at the first
    sighting of a new email as well as at the recorded switch."""
    path = os.path.join(DIR, "seen-login.json")
    seen = load(path, {})
    if seen.get("email") != email:
        seen = {"email": email, "since": time.time()}
        try:
            save(path, seen)
        except OSError:
            pass
    return float(seen.get("since") or 0)


def cmd_statusline():
    try:
        payload = json.load(sys.stdin)
    except ValueError:
        payload = {}
    email = current_email()
    note_reporter()
    limits = payload.get("rate_limits") or {}
    five, week = limits.get("five_hour") or {}, limits.get("seven_day") or {}
    state = load(STATE, {})
    settled = time.time() - max(float(state.get("switched_at") or 0), login_seen_since(email)) >= SETTLE_S
    if email and (five or week) and settled:
        # At most one write per account per 30 s across the whole fleet.
        stamp = os.path.join(DIR, "last-record")
        try:
            fresh = time.time() - os.path.getmtime(stamp) < 30
        except OSError:
            fresh = False
        if not fresh:
            try:
                accepted = record(email,
                       five.get("used_percentage"), iso(five["resets_at"]) if five.get("resets_at") else None,
                       week.get("used_percentage"), iso(week["resets_at"]) if week.get("resets_at") else None)
                open(stamp, "w").close()
                if accepted:
                    trigger_if_exhausted(email, five, week)
            except Exception as e:  # a status line must never fail the CLI
                log("claude_rotate_record_failed", error=str(e)[:200])
    name = (email or "?").split("@")[-1].split(".")[0]
    parts = [name]
    if five.get("used_percentage") is not None:
        parts.append("5h %d%%" % round(five["used_percentage"]))
    if week.get("used_percentage") is not None:
        parts.append("wk %d%%" % round(week["used_percentage"]))
    print(" · ".join(parts))


# ---------------------------------------------------------------- policy

def window(w, at):
    w = w or {}
    reset = w.get("resets_at")
    reset = dt.datetime.fromisoformat(reset) if reset else None
    used = w.get("used_percentage")
    if used is None:
        return None, reset
    return (0.0 if reset and reset <= at else float(used)), reset


def rank(at=None):
    at = at or now()
    rows = []
    for email, a in load(ACCOUNTS, {"accounts": {}})["accounts"].items():
        if a.get("disabled"):
            continue
        u5, r5 = window(a.get("five_hour"), at)
        u7, r7 = window(a.get("seven_day"), at)
        known = u5 is not None or u7 is not None
        head = min(SWITCH_AT - (u5 or 0), SWITCH_AT - (u7 or 0))
        usable = head > MIN_HEADROOM
        blockers = [r for u, r in ((u5, r5), (u7, r7)) if u is not None and SWITCH_AT - u <= MIN_HEADROOM and r]
        rows.append({"email": email, "known": known, "usable": usable, "headroom": round(head, 1),
                     "five_hour": u5, "weekly": u7,
                     "weekly_resets": r7.isoformat() if r7 else None,
                     "available_at": "now" if usable else (max(blockers).isoformat() if blockers else None),
                     "observed_at": a.get("observed_at")})
    far = "9999"
    rows.sort(key=lambda r: (not r["usable"], not r["known"],
                             (r["weekly_resets"] or far) if r["usable"] else (r["available_at"] or far)))
    return rows


FAIL_BACKOFF_N = int(os.environ.get("CLAUDE_ROTATE_FAIL_BACKOFF_N", "3"))
FAIL_BACKOFF_S = int(os.environ.get("CLAUDE_ROTATE_FAIL_BACKOFF_S", str(6 * 3600)))


def note_failure(times, steps, at_s=None):
    """A target's recent failure times plus this one. An unreadable sign-in
    link is not transient, so it skips the account for the whole window."""
    at_s = at_s or time.time()
    times = [t for t in times if at_s - float(t) < FAIL_BACKOFF_S] + [at_s]
    if any("unreadable" in str(x) for x in steps):
        times += [at_s] * max(0, FAIL_BACKOFF_N - len(times))
    return times


def forgive_browser_failures(state, target):
    """After a browser sign-in works, drop the backoff of every account whose
    failures were the browser's, not the account's. 2026-10-17: the sign-in
    page looped for every account from 19:03 to 19:22, three failures put
    hello@amux.io (76 points free) in a 6-hour backoff, and two later sign-ins
    to other accounts worked while the rotator kept picking accounts at 95%.
    An unreadable sign-in link is the account's own fault and keeps its backoff."""
    fails = state.get("switch_failures") or {}
    own = set(state.get("account_failures") or [])
    cleared = sorted(e for e in fails if e == target or e not in own)
    for e in cleared:
        fails.pop(e, None)
    state["account_failures"] = sorted(own - {target})
    return [e for e in cleared if e != target]


def backed_off(state, at_s=None):
    """Accounts with FAIL_BACKOFF_N failed switches inside FAIL_BACKOFF_S. Each
    failed switch costs ~4 minutes of a 5-minute tick and holds every deploy
    behind it, so a target that keeps failing is skipped until the window passes
    or a switch to it succeeds."""
    at_s = at_s or time.time()
    fails = state.get("switch_failures") or {}
    return {e for e, ts in fails.items()
            if sum(1 for t in ts if at_s - float(t) < FAIL_BACKOFF_S) >= FAIL_BACKOFF_N}


def cool(r):
    """Fit for a switch nobody is forced into: the 5-hour window well under the
    preempt line and the weekly one under it."""
    return (r["five_hour"] or 0) < PREEMPT_AT - PREEMPT_MARGIN and (r["weekly"] or 0) < PREEMPT_AT


def dead_mailboxes():
    """Connected mailboxes whose Gmail token no longer works (amux reports them
    as needs_reauth). The sign-in link for that account lands where nothing can
    read it, so a switch to it waits out the whole login page. 2026-10-17: the
    hello@amux.io grant was ended by the amux.io Workspace session policy, the
    link search answered empty rather than failing, and the policy picked
    hello@amux.io (the only roomy account) on every tick for 4 minutes each
    while ethan@trymixpeek.com climbed past 92%.
    None means the read failed: unknown, and never evidence that a mailbox recovered."""
    try:
        d = amux_get("/api/gmail/accounts") or {}
    except Exception:
        return None
    if "needs_reauth" not in d:
        return None
    return {e.lower() for e in (d.get("needs_reauth") or []) if isinstance(e, str)}


def forgive_recovered_mailboxes(state, dead, at_s=None):
    """Drop the switch failures an account collected while its mailbox was dead,
    once the mailbox reads healthy again. 2026-10-17: hello@amux.io was
    reconnected at ~20:40, its token worked, and the rotator still held it in a
    6-hour backoff for the two switches that failed at 18:07 and 18:12 while
    nothing could read its sign-in link. Returns the accounts cleared.
    `dead` must be a measured set; pass None and nothing is forgiven."""
    at_s = at_s or time.time()
    seen = state.setdefault("mailbox_dead_seen", {})
    if dead is None:
        return []
    for e in dead:
        seen[e] = at_s
    fails = state.get("switch_failures") or {}
    cleared = []
    for e in sorted(set(seen) - set(dead)):
        last_dead = float(seen.pop(e))
        kept = [t for t in fails.get(e, []) if float(t) > last_dead]
        if len(kept) != len(fails.get(e, [])):
            cleared.append(e)
            if kept:
                fails[e] = kept
            else:
                fails.pop(e, None)
    return cleared


def decide(current, rows, state, at=None, unreachable=()):
    """Return (target or None, reason). `unreachable` accounts are never targets."""
    at = at or now()
    dead = {e.lower() for e in unreachable} - {current}
    skip = (backed_off(state) | dead) - {current}
    marked = [dict(r, usable=False) if r["email"] in skip else r for r in rows]
    # A backoff saves sign-in time; it must never leave the fleet with no
    # account. 2026-10-17 22:28: ethan@trymixpeek.com at 98% held with
    # "no_usable_account" while hello@amux.io (76 points free) sat in a backoff
    # earned during a browser outage. With nothing else usable, a backed-off
    # account is tried again; a dead mailbox still is not.
    if not any(r["usable"] and r["email"] != current for r in marked):
        marked = [dict(r, usable=False) if r["email"] in dead else r for r in rows]
    rows = marked
    # A target already over the preempt line would be preempted on the next
    # tick, so prefer accounts under it (2026-10-16: the soonest-expiring account
    # sat at 93% weekly and was picked first, which would flap straight back).
    under = [r for r in rows if r["usable"] and max(r["five_hour"] or 0, r["weekly"] or 0) < PREEMPT_AT]
    # With nothing under it, take the roomiest: an account with 4 points left
    # is spent within minutes and costs another sign-in (2026-10-17: 95% weekly
    # joinmixpeek was picked for its early reset and hit 99% three minutes later).
    best = next(iter(under), None) or max((r for r in rows if r["usable"]), key=lambda r: r["headroom"], default=None)
    cur = next((r for r in rows if r["email"] == current), None)
    if best is None:
        return None, "no_usable_account"
    # Preempt first: the account nearest its limit is often also the one whose
    # weekly window resets soonest, which the policy below calls "best".
    if cur is not None and cur["usable"]:
        peak = max(cur["five_hour"] or 0, cur["weekly"] or 0)
        roomier = next((r for r in rows if r["usable"] and r["email"] != current
                        and r["headroom"] > cur["headroom"] and cool(r)), None)
        if peak >= PREEMPT_AT and roomier is not None:
            if time.time() - float(state.get("switched_at") or 0) < MIN_SWITCH_GAP_S:
                return None, "recent_switch"
            return roomier["email"], "preempt_limit"
        # Roomiest first; a target over HOT_AT itself would be preempted straight back.
        hot = max((r for r in rows if r["usable"] and r["email"] != current and r["headroom"] > cur["headroom"]
                   and max(r["five_hour"] or 0, r["weekly"] or 0) < HOT_AT),
                  key=lambda r: r["headroom"], default=None)
        if peak >= HOT_AT and hot is not None:
            return hot["email"], "preempt_hot"
    if best["email"] == current:
        return None, "current_is_best"
    if cur is None or not cur["usable"]:
        return best["email"], "current_exhausted"
    since = time.time() - float(state.get("switched_at") or 0)
    if since < MIN_SWITCH_GAP_S:
        return None, "recent_switch"
    if best["known"] and cool(best) and cur["weekly_resets"] and best["weekly_resets"] and best["weekly_resets"] < cur["weekly_resets"]:
        return best["email"], "expiring_capacity_first"
    return None, "current_is_fine"


def cmd_pick(argv):
    rows = rank()
    pick = rows[0]["email"] if rows and rows[0]["usable"] else None
    if "--json" in argv:
        print(json.dumps({"measured": True, "n_considered": len(rows), "pick": pick, "ranked": rows}, indent=2))
        return
    print("pick:", pick or "NONE usable (wait for the earliest available_at)")
    for r in rows:
        print("  %s %-28s 5h=%-5s wk=%-5s headroom=%-5s weekly_resets=%s available=%s" % (
            "OK" if r["usable"] else "--", r["email"], r["five_hour"], r["weekly"], r["headroom"],
            r["weekly_resets"], r["available_at"]))


MONTHS = {m: i for i, m in enumerate(
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"], 1)}


def parse_reset(text, at=None):
    """'Resets 4:20pm (America/New_York)' or 'Resets Oct 16 at 3am (...)'."""
    at = at or now()
    m = re.search(r"Resets\s+(?:([A-Z][a-z]{2})\s+(\d{1,2})\s+at\s+)?(\d{1,2})(?::(\d{2}))?\s*(am|pm)", text)
    if not m:
        return None
    mon, day, hour, minute, ampm = m.groups()
    hour = int(hour) % 12 + (12 if ampm == "pm" else 0)
    when = at.replace(hour=hour, minute=int(minute or 0), second=0, microsecond=0)
    if mon:
        when = when.replace(month=MONTHS[mon], day=int(day))
        if when < at - dt.timedelta(days=1):
            when = when.replace(year=when.year + 1)
    elif when <= at:
        when += dt.timedelta(days=1)
    return when.isoformat()


def parse_usage(screen, at=None):
    """Read the five-hour and weekly windows from the CLI's /usage screen."""
    lines = [l.strip() for l in screen.splitlines() if l.strip()]
    out = {}
    for key, label in (("five_hour", "Current session"), ("seven_day", "Current week (all models)")):
        idx = next((i for i, l in enumerate(lines) if l.startswith(label)), None)
        if idx is None:
            continue
        used = reset = None
        for l in lines[idx + 1: idx + 4]:
            m = re.search(r"(\d{1,3})% used", l)
            if m and used is None:
                used = int(m.group(1))
            if l.startswith("Resets") and reset is None:
                reset = parse_reset(l, at)
        if used is not None:
            out[key] = {"used_percentage": used, "resets_at": reset}
    return out


def tmux(*args):
    return subprocess.run(["tmux", *args], capture_output=True, text=True, timeout=20)


def probe():
    """Read the logged-in account's usage from /usage in a temporary CLI
    session. It sends no prompt, so it spends no quota, and it works whether
    or not any worker is active."""
    email = current_email()
    name = "claude-rotate-probe-%d" % os.getpid()
    cwd = PROBE_CWD
    started = time.time()
    tmux("new-session", "-d", "-s", name, "-x", "160", "-y", "60", "-c", cwd, CLAUDE)
    try:
        sent, screen = False, ""
        while time.time() - started < 60:
            time.sleep(1.5)
            screen = tmux("capture-pane", "-p", "-t", name).stdout
            if "trust this folder" in screen:
                # Never answered on the owner's behalf: the probe needs a
                # folder already trusted (CLAUDE_ROTATE_PROBE_CWD).
                break
            if not sent and "❯" in screen:
                tmux("send-keys", "-t", name, "/usage", "Enter")
                sent = True
                continue
            if sent and "Current week (all models)" in screen and re.search(
                    r"Current week \(all models\)[\s\S]*?\d+% used", screen):
                break
        windows = parse_usage(screen)
    finally:
        tmux("kill-session", "-t", name)
    elapsed = round(time.time() - started, 1)
    if not email or "seven_day" not in windows:
        log("claude_rotate_probe_failed", measured=False, why_unmeasured="no /usage screen parsed",
            current=email, elapsed_s=elapsed, screen_tail=re.sub(r"\s+", " ", screen)[-200:])
        return None
    if current_email() != email:  # the login changed mid-probe; the reading is ambiguous
        log("claude_rotate_probe_failed", measured=False, why_unmeasured="login changed during probe")
        return None
    five, week = windows.get("five_hour") or {}, windows["seven_day"]
    record(email, five.get("used_percentage"), five.get("resets_at"),
           week.get("used_percentage"), week.get("resets_at"), source="usage-probe")
    log("claude_rotate_probe", measured=True, n_considered=1, current=email, elapsed_s=elapsed,
        five_hour=five.get("used_percentage"), weekly=week.get("used_percentage"))
    return windows


REPO = os.environ.get("CLAUDE_ROTATE_REPO", os.path.join(HOME, "Dev", "amux"))


def install_drift():
    """Log when this installed copy is not origin/main's scripts/claude-rotate.py.
    2026-10-17: a lane wrote a mailbox-recovery change straight into the
    installed copy at 20:57 and never committed it, and the next install from
    main would have deleted it with nothing saying so. Returns the verdict
    fields, or None when they agree or the repo cannot be read."""
    try:
        main = subprocess.run(["git", "-C", REPO, "show", "origin/main:scripts/claude-rotate.py"],
                              capture_output=True, text=True, timeout=20)
        mine = open(os.path.abspath(__file__)).read()
    except (OSError, subprocess.TimeoutExpired):
        return None
    if main.returncode != 0 or main.stdout == mine:
        return None
    a, b = main.stdout.splitlines(), mine.splitlines()
    import difflib
    changed = sum(1 for l in difflib.unified_diff(a, b, lineterm="", n=0)
                  if l[:1] in "+-" and not l.startswith(("+++", "---")))
    fields = {"installed": os.path.abspath(__file__), "lines_differing": changed,
              "fix": "commit the installed change to scripts/claude-rotate.py, or reinstall from main"}
    log("claude_rotate_install_drift", measured=True, n_considered=1, **fields)
    return fields


ENABLED_FILE = os.path.join(DIR, "enabled")


def enabled():
    """Opt-in, default off (Ethan, 2026-10-18): the env var or the marker file."""
    env = os.environ.get("CLAUDE_ROTATE_ENABLED")
    if env is not None:
        return env == "1"
    return os.path.exists(ENABLED_FILE)


def cmd_tick():
    try:
        install_drift()
    except Exception as e:
        log("claude_rotate_install_drift", measured=False, why_unmeasured=str(e)[:200])
    usage_log_sync()
    if not enabled():
        print(log("claude_rotate_disabled", measured=False,
                  why_unmeasured="automatic switching is opt-in; run `claude-rotate.py on` to enable"))
        return 0
    if os.environ.get("CLAUDE_ROTATE_ENABLED") == "0" or os.path.exists(os.path.join(DIR, "paused")):
        print(log("claude_rotate_paused", measured=False, why_unmeasured="paused"))
        return 0
    if not os.path.exists(LOCK) or not _switch_running():
        try:
            probe()
        except Exception as e:
            log("claude_rotate_probe_failed", measured=False, why_unmeasured=str(e)[:200])
        try:
            capture_login()
        except Exception as e:
            log("claude_rotate_capture_failed", measured=False, why_unmeasured=str(e)[:200])
    current = current_email()
    rows = rank()
    state = load(STATE, {})
    try:
        cov = login_coverage([r["email"] for r in rows])
        if cov != state.get("login_coverage"):
            log("claude_rotate_login_coverage", measured=True, n_considered=len(cov), coverage=cov,
                browser_only=sorted(e for e, v in cov.items() if v == "browser"))
            if not os.path.exists(LOCK) or not _switch_running():
                state["login_coverage"] = cov
                save(STATE, state)
    except Exception as e:
        log("claude_rotate_login_coverage", measured=False, why_unmeasured=str(e)[:200])
    health = dead_mailboxes()
    if health is None:
        log("claude_rotate_mailbox_health_unmeasured", measured=False, why_unmeasured="GET /api/gmail/accounts failed")
    dead = (health or set()) & {r["email"].lower() for r in rows}
    # Only this tick writes state while no switch runs; a running switch owns it.
    if not os.path.exists(LOCK) or not _switch_running():
        before = json.dumps(state.get("mailbox_dead_seen"), sort_keys=True)
        recovered = forgive_recovered_mailboxes(state, None if health is None else dead)
        if recovered:
            log("claude_rotate_mailbox_recovered", measured=True, n_considered=len(rows), accounts=recovered,
                why="the mailbox reads healthy again, so failures from while it was dead no longer count")
        if recovered or json.dumps(state.get("mailbox_dead_seen"), sort_keys=True) != before:
            save(STATE, state)
    if dead:
        log("claude_rotate_mailbox_dead", measured=True, n_considered=len(rows), accounts=sorted(dead),
            fix="GET /api/gmail/auth?account=<email>, open the url, approve")
    target, reason = decide(current, rows, state, unreachable=dead)
    skipped = sorted((backed_off(state) | dead) - {current})
    if not target:
        print(log("claude_rotate_hold", measured=True, n_considered=len(rows), current=current, reason=reason,
                  backed_off=skipped))
        cur = next((r for r in rows if r["email"] == current), None)
        if cur and cur["usable"]:
            resume_stranded()
        # A worker that hit the login gap late is caught on the next ticks.
        if time.time() - float(state.get("switched_at") or 0) < 900:
            resume_auth_lost()
        return 0
    print(log("claude_rotate_switch_planned", measured=True, n_considered=len(rows), current=current,
              target=target, reason=reason, backed_off=skipped))
    rc = switch(target, reason)
    if rc == 0 or reason != "current_exhausted" or cli_email() != current:
        return rc
    # The current account is spent and that target failed: try the next one now
    # rather than leaving the fleet stopped until the next tick.
    rows = [dict(r, usable=False) if r["email"] == target else r for r in rows]
    target2, reason2 = decide(current, rows, load(STATE, {}), unreachable=dead)
    if not target2 or target2 == target:
        return rc
    print(log("claude_rotate_switch_planned", measured=True, n_considered=len(rows), current=current,
              target=target2, reason=reason2, fallback_after=target))
    return switch(target2, reason2)


# ---------------------------------------------------------------- resuming

LIMIT_PROMPT = "Stop and wait for limit to reset"


def stranded(sessions):
    """Running, non-isolated workers parked on the rate limit. Isolated workers
    take no amux input; they are counted, not touched."""
    out, isolated = [], []
    for s in sessions:
        if s.get("running") and s.get("status") == "rate_limited":
            (isolated if s.get("isolated") else out).append(s["name"])
    return out, isolated


# Newer Claude Code parks a limited worker on a timer instead of a menu:
# "Usage limit reached · limit resets 1:40am / Continuing automatically at
# 1:40am · esc to cancel". 2026-10-17 22:32: three workers sat on it after the
# switch to hello@amux.io, logged not_on_prompt_or_recent, because only the
# menu was recognised. Escape cancels the timer and the "continue" that follows
# runs on the new login.
# Claude Code 2026-10-20 also words it "Usage limit reached · limit resets
# 1pm / Continuing shortly · esc to cancel"; the rotator logged eight workers
# not_on_prompt_or_recent at 13:00 because only the "at <time>" form matched.
AUTO_WAIT = re.compile(r"Continuing (?:automatically at|shortly)[^\n]{0,40}esc to cancel")
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")


def resume_action(screen):
    """The keys that leave Claude's limit prompt, or None when it is not on screen
    (a worker in any other state must not get a stray keypress)."""
    plain = ANSI.sub("", screen or "")
    if LIMIT_PROMPT in plain:
        return "1"
    if AUTO_WAIT.search(plain):
        return "Escape"
    return None


def _amux(path, body=None):
    base = os.environ.get("AMUX_URL") or subprocess.run(["amux", "url"], capture_output=True, text=True).stdout.strip()
    cmd = ["curl", "-sk", "--max-time", "10", base + path]
    if body is not None:
        cmd[1:1] = ["-X", "POST", "-H", "Content-Type: application/json", "-d", json.dumps(body)]
    out = subprocess.run(cmd, capture_output=True, text=True).stdout
    return json.loads(out) if out.strip() else None


def resume_stranded(force=False):
    """After a switch, or while the current login has room, take workers off
    Claude's limit prompt and continue them on the current login. amux-app
    resumed this way on 2026-10-15 after sitting at the prompt for 20 min."""
    try:
        sessions = _amux("/api/sessions") or []
    except Exception as e:
        log("claude_rotate_resume_failed", measured=False, why_unmeasured=str(e)[:200])
        return
    names, isolated = stranded(sessions)
    if not names and not isolated:
        return
    state = load(STATE, {})
    last = state.get("resumed", {})
    done, skipped = [], []
    for name in names:
        if not force and time.time() - float(last.get(name, 0)) < RESUME_GAP_S:
            skipped.append(name)
            continue
        peek = _amux(f"/api/sessions/{urllib.parse.quote(name)}/peek?lines=40") or {}
        keys = resume_action(peek.get("output") or "")
        if not keys:
            skipped.append(name)
            continue
        _amux(f"/api/sessions/{urllib.parse.quote(name)}/keys", {"keys": keys})
        time.sleep(2)
        _amux(f"/api/sessions/{urllib.parse.quote(name)}/send", {"text": "continue"})
        last[name] = time.time()
        done.append(name)
    state["resumed"] = last
    save(STATE, state)
    print(log("claude_rotate_resumed", measured=True, n_considered=len(names) + len(isolated),
              current=current_email(), resumed=done, not_on_prompt_or_recent=skipped, isolated_left_to_owner=isolated))


# A switch ends with Claude Code writing the new login. On 2026-10-16 a
# subagent running during the 16:10 switch failed with "Not logged in · Please
# run /login": something read the login while it was being replaced. That write
# is Claude Code's, so the gap is measured here and the workers it hits are
# resumed, the same way limit-parked workers are.
AUTH_LOST = re.compile(r"Not logged in.{0,40}/login")


class LoginGapProbe:
    """Poll `claude auth status` from the last sign-in step until the switch
    returns, and report how long the CLI saw no login."""

    def __init__(self):
        self.samples, self._stop, self._t = [], threading.Event(), None

    def start(self):
        if self._t is None:
            self._t = threading.Thread(target=self._run, daemon=True)
            self._t.start()

    def _run(self):
        while not self._stop.is_set():
            try:
                out = subprocess.run([CLAUDE, "auth", "status"], capture_output=True, text=True, timeout=10).stdout
                logged = bool(json.loads(out).get("loggedIn"))
            except Exception:
                logged = None
            self.samples.append((time.time(), logged))
            self._stop.wait(0.2)

    def stop(self):
        if self._t is None:
            return None
        self._stop.set()
        self._t.join(timeout=15)
        return gap_stats(self.samples)


def gap_stats(samples):
    """Longest run of loggedIn=False readings, in seconds from the first false
    sample to the next true one (or the last sample)."""
    longest, n_false, start = 0.0, 0, None
    for i, (t, logged) in enumerate(samples):
        if logged is False:
            n_false += 1
            start = t if start is None else start
        elif start is not None:
            longest, start = max(longest, t - start), None
    if start is not None:
        longest = max(longest, samples[-1][0] - start)
    return {"n_samples": len(samples), "n_logged_out": n_false, "longest_gap_s": round(longest, 2)}


def auth_lost(screen):
    """True when Claude's not-logged-in error is among the last lines on screen,
    which is where a turn that died on it leaves it."""
    lines = [l for l in (screen or "").splitlines() if l.strip()][-8:]
    return any(AUTH_LOST.search(l) for l in lines)


def resume_auth_lost():
    """Continue running, non-isolated workers whose last turn died on the
    login gap. Peeks only the tail, so a session that merely discusses the
    message is left alone."""
    try:
        sessions = _amux("/api/sessions") or []
    except Exception as e:
        log("claude_rotate_auth_resume_failed", measured=False, why_unmeasured=str(e)[:200])
        return []
    state = load(STATE, {})
    last = state.get("auth_resumed", {})
    done, considered = [], 0
    for s in sessions:
        if not s.get("running") or s.get("isolated"):
            continue
        considered += 1
        name = s["name"]
        if time.time() - float(last.get(name, 0)) < RESUME_GAP_S:
            continue
        peek = _amux(f"/api/sessions/{urllib.parse.quote(name)}/peek?lines=12") or {}
        if not auth_lost(peek.get("output") or ""):
            continue
        _amux(f"/api/sessions/{urllib.parse.quote(name)}/send", {"text": "continue"})
        last[name] = time.time()
        done.append(name)
    state["auth_resumed"] = last
    save(STATE, state)
    if done:
        print(log("claude_rotate_auth_resumed", measured=True, n_considered=considered,
                  current=current_email(), resumed=done))
    return done


# ---------------------------------------------------------------- switching

def cdp(*args, check=True):
    env = dict(os.environ, CDP_PROFILE_SYNC="0")
    p = subprocess.run(["amux", "cdp", *args], capture_output=True, text=True, timeout=60, env=env)
    out = "\n".join(l for l in p.stdout.splitlines() if not l.startswith("[amux] saved"))
    if check and p.returncode != 0:
        raise RuntimeError("amux cdp %s failed: %s" % (args[0], (p.stderr or out)[-300:]))
    return out.strip()


def find_tab(marker):
    for line in cdp("list").splitlines():
        parts = line.split(None, 1)
        if len(parts) == 2 and marker in line:
            return parts[0]
    return None


def page_text(tab):
    return cdp("eval", tab, 'document.body ? document.body.innerText.slice(0, 2000) : ""', check=False)


def click_text(tab, pattern):
    js = ('(() => { const re = new RegExp(%s); const el = [...document.querySelectorAll("button,a,[role=button]")]'
          '.find(e => re.test(e.innerText.trim())); if (!el) return "none"; el.scrollIntoView({block: "center"});'
          ' const r = el.getBoundingClientRect(); return Math.round(r.x + r.width / 2) + " " + Math.round(r.y + r.height / 2) })()'
          % json.dumps(pattern))
    pos = cdp("eval", tab, js, check=False).splitlines()[-1:] or ["none"]
    if pos[0] == "none" or not re.match(r"^\d+ \d+$", pos[0]):
        return False
    x, y = pos[0].split()
    cdp("clickxy", tab, x, y)
    return True


_last_trace = {}


def trace(target, tab, text):
    """One line per distinct page the switch sees, for diagnosing a failed run."""
    url = cdp("eval", tab, "location.href", check=False).splitlines()[-1:] or [""]
    key = (url[0].split("?")[0], text[:120])
    if _last_trace.get(tab) == key:
        return
    _last_trace[tab] = key
    with open(LOG.replace(".log", "-trace.log"), "a") as f:
        f.write(json.dumps({"ts": now().isoformat(timespec="seconds"), "target": target,
                            "url": url[0][:160], "text": re.sub(r"\s+", " ", text)[:300]}) + "\n")


def profile_tab():
    """A live tab in the Chrome profile that holds the accounts, reused across
    switches. Tabs that `open -na` creates there can stay on about:blank and
    refuse navigation, so one is opened once and its id kept in state.json."""
    state = load(STATE, {})
    listed = cdp("list", check=False)
    tab = state.get("chrome_tab")
    if tab and any(line.startswith(tab) for line in listed.splitlines()):
        probe = cdp("eval", tab, "document.readyState", check=False).splitlines()[-1:]
        if probe and probe[0] in ("complete", "interactive"):
            return tab
    before = {l.split()[0] for l in listed.splitlines() if l.strip()}
    marker = "https://claude.ai/?amux-rotate=%d" % int(time.time())
    subprocess.run(["open", "-na", "Google Chrome", "--args", "--profile-directory=" + CHROME_PROFILE, marker])
    for _ in range(20):
        time.sleep(1.5)
        new = [l.split()[0] for l in cdp("list", check=False).splitlines()
               if l.strip() and l.split()[0] not in before]
        if new:
            tab = new[0]
            cdp("nav", tab, "https://claude.ai/", check=False)
            state = load(STATE, {})
            state["chrome_tab"] = tab
            save(STATE, state)
            log("claude_rotate_profile_tab", tab=tab)
            return tab
    raise RuntimeError("no tab appeared in Chrome profile " + CHROME_PROFILE)


def code_from_link(link):
    """Open the sign-in link in a throwaway tab and read its 6-digit code."""
    opened = cdp("open", link, check=False)
    m = re.search(r"Opened new tab: (\S+)", opened)
    if not m:
        return None
    helper = m.group(1)
    try:
        for _ in range(10):
            time.sleep(1.5)
            found = cdp("eval", helper, '(document.body.innerText.match(/\\b\\d{6}\\b/) || [""])[0]',
                        check=False).splitlines()[-1:]
            if found and re.fullmatch(r"\d{6}", found[0]):
                return found[0]
        return None
    finally:
        cdp("close", helper, check=False)


def enter_code(tab, code):
    js = ('(() => { let i = document.querySelector("input[autocomplete=one-time-code]");'
          ' if (!i) return "none"; i.focus(); i.select(); return "ok" })()')
    if cdp("eval", tab, js, check=False).splitlines()[-1:] != ["ok"]:
        if not click_text(tab, r"^Enter verification code$"):
            return False
        time.sleep(1.5)
        if cdp("eval", tab, js, check=False).splitlines()[-1:] != ["ok"]:
            return False
    cdp("type", tab, code)
    return click_text(tab, r"Verify")


def fill_email(tab, email):
    """Clear and type the address into the email field. Always retypes to
    ensure React state matches (a login_hint prefill doesn't trigger the
    synthetic change event the form needs)."""
    js = ('(() => { const i = document.querySelector("input[type=email]");'
          ' if (!i) return "none"; i.focus(); i.select();'
          ' document.execCommand("selectAll"); document.execCommand("delete");'
          ' return "ok" })()')
    if cdp("eval", tab, js, check=False).splitlines()[-1:] != ["ok"]:
        return False
    cdp("type", tab, email)
    return True


def amux_get(path):
    url = subprocess.run(["amux", "url"], capture_output=True, text=True).stdout.strip()
    p = subprocess.run(["curl", "-sk", "--max-time", "20", url + path], capture_output=True, text=True)
    return json.loads(p.stdout or "null")


_LINK_UNREADABLE_LOGGED = set()


def magic_link(email, since_epoch):
    unreadable = 0
    # in:anywhere: Gmail files some of these as spam, and a plain search skips spam.
    # 2026-10-16: every link to ethan@mixpeekvectors.com sat in spam, so 98 of 101
    # switches to it waited out the login page and failed.
    # to: as well, because spam piles up: on 2026-10-16 98 of the newest 100
    # links were to mixpeekvectors, so a fresh link to another account could
    # fall outside a newest-20 window across all accounts.
    msgs = amux_get("/api/email/search?q=%s&days=1&limit=20" % urllib.parse.quote(
        "in:anywhere to:%s secure link to Claude" % email)) or []
    for m in msgs:
        if (m.get("to") or "").lower() != email.lower():
            continue
        try:
            sent = dt.datetime.fromisoformat(m.get("date_iso") or m.get("received") or "").timestamp()
        except ValueError:
            sent = None
        if sent is None:
            try:
                from email.utils import parsedate_to_datetime
                sent = parsedate_to_datetime(m.get("date")).timestamp()
            except Exception:
                continue
        if sent < since_epoch - 5:
            continue
        msg = amux_get("/api/email/message/" + urllib.parse.quote(m["message_id"], safe=""))
        if not msg or (isinstance(msg, dict) and msg.get("error")):
            unreadable += 1
            continue
        found = re.findall(r"https://claude\.ai/magic-link#[A-Za-z0-9_\-:.]+", json.dumps(msg))
        if found:
            return found[0]
    if unreadable and email not in _LINK_UNREADABLE_LOGGED:
        # Search sees the email and the message endpoint cannot open it. That is
        # what stalled the fleet on 2026-10-15/16 (spam-filed ids), so say so.
        _LINK_UNREADABLE_LOGGED.add(email)
        log("claude_rotate_link_unreadable", account=email, unreadable=unreadable)
    return None


# ---------------------------------------------------------------- stored logins
#
# SWITCH WITHOUT A BROWSER (Ethan, 2026-10-17: "it needs to understand when to
# switch and only switch to accounts to optimize our usage across them all
# totally using cli"). The browser sign-in failed four times in a row that
# evening (~4.5 min each, its Chrome tab blank or gone) while the fleet sat on
# an account at 100% weekly. The CLI keeps its login in the Keychain item
# `Claude Code-credentials`: `claudeAiOauth` is the account's token pair and
# `mcpOAuth` holds MCP logins shared by every account. Every tick snapshots the
# logged-in account's `claudeAiOauth` (and its `oauthAccount` from
# ~/.claude.json) into a private Keychain item per email, and again right
# before switching away, so a rotated refresh token is never lost. A switch to
# an account with a snapshot swaps it in, keeps `mcpOAuth`, and proves it with
# a /usage probe; on any failure the previous login is put back and the browser
# flow runs, which captures that account for next time. Values only travel on
# stdin to `security -i`, never in argv.
CRED_SVC = os.environ.get("CLAUDE_ROTATE_CRED_SERVICE", "Claude Code-credentials")
CRED_ACCT = os.environ.get("CLAUDE_ROTATE_CRED_ACCOUNT", os.environ.get("USER", ""))
STORE_SVC = os.environ.get("CLAUDE_ROTATE_STORE_SERVICE", "amux-claude-rotate")


# LINUX HAS NO KEYCHAIN (mixpeek-dev.amux.io, 2026-10-31: "make sure it does the
# claude account rotation thing too like i have here"). Claude Code on Linux
# keeps its login in <CLAUDE_CONFIG_DIR or ~/.claude>/.credentials.json, so the
# CLI's own item maps to that file and every other item (the rotator's
# snapshots) to a 0600 file under DIR/keyring. macOS keeps the Keychain.
USE_KEYCHAIN = sys.platform == "darwin" and os.environ.get("CLAUDE_ROTATE_KEYSTORE") != "file"
KEYRING = os.path.join(DIR, "keyring")


def _cli_cred_file(config_dir=None):
    base = config_dir or os.environ.get("CLAUDE_CONFIG_DIR") or os.path.join(HOME, ".claude")
    return os.path.join(base, ".credentials.json")


def _file_for(service, account):
    if service == "Claude Code-credentials" and account == CRED_ACCT:
        return _cli_cred_file()
    safe = lambda v: re.sub(r"[^A-Za-z0-9@._+-]", "_", v) or "_"
    return os.path.join(KEYRING, safe(service), safe(account) + ".json")


def _file_read(path):
    try:
        with open(path) as f:
            v = f.read().strip()
        return v or None
    except OSError:
        return None


def _file_write(path, value):
    try:
        os.makedirs(os.path.dirname(path), mode=0o700, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".new.")
        with os.fdopen(fd, "w") as f:
            f.write(value)
        os.chmod(tmp, 0o600)
        os.replace(tmp, path)
        return True
    except OSError:
        return False


def _kc_read(service, account):
    if not USE_KEYCHAIN:
        return _file_read(_file_for(service, account))
    p = subprocess.run(["security", "find-generic-password", "-s", service, "-a", account, "-w"],
                       capture_output=True, text=True)
    return p.stdout.strip() if p.returncode == 0 and p.stdout.strip() else None


def _kc_write(service, account, value):
    if not USE_KEYCHAIN:
        return _file_write(_file_for(service, account), value)
    # -X takes hex, so quotes in the JSON cannot break the command line, and
    # `security -i` reads it from stdin, so the value is never an argv element.
    cmd = 'add-generic-password -U -s "%s" -a "%s" -X %s\n' % (service, account, value.encode().hex())
    p = subprocess.run(["security", "-i"], input=cmd, capture_output=True, text=True)
    return p.returncode == 0 and "error" not in (p.stderr or "").lower()


# Per-account config folders, each signed in on its own with CLAUDE_CONFIG_DIR
# set. The CLI keeps such a login in the keychain under CRED_SVC plus "-" and
# the first 8 hex digits of sha256(folder path). 2026-10-20: all seven of
# ~/.amux/claude-accounts held logins valid to 11-11, and the rotator, reading
# only STORE_SVC, sent ethan@usemixpeek.com (the one account with weekly
# capacity left) to the browser 47 times over 4.5 hours while the browser
# could not be reached.
ACCOUNTS_DIR = os.environ.get("CLAUDE_ROTATE_ACCOUNTS_DIR", os.path.join(HOME, ".amux", "claude-accounts"))


# The CLI's own name, fixed: CRED_SVC is overridable (a sandbox points it at a
# scratch item), and an override must not move where folder logins are read.
CLI_CRED_SVC = "Claude Code-credentials"


def _folder_service(folder):
    import hashlib
    return CLI_CRED_SVC + "-" + hashlib.sha256(folder.encode()).hexdigest()[:8]


def account_folders():
    """email -> the config folder signed in to it."""
    out = {}
    try:
        names = sorted(os.listdir(ACCOUNTS_DIR))
    except OSError:
        return out
    for n in names:
        folder = os.path.join(ACCOUNTS_DIR, n)
        acct = (load(os.path.join(folder, ".claude.json"), {}) or {}).get("oauthAccount") or {}
        email = (acct.get("emailAddress") or "").lower()
        if email:
            out[email] = folder
    return out


def folder_login(email, folders=None):
    """The login held by `email`'s own config folder, shaped like a STORE_SVC
    entry, or None."""
    folder = (account_folders() if folders is None else folders).get((email or "").lower())
    if not folder:
        return None
    try:
        raw = _kc_read(_folder_service(folder), CRED_ACCT) if USE_KEYCHAIN else _file_read(_cli_cred_file(folder))
        oauth = json.loads(raw or "{}").get("claudeAiOauth")
    except ValueError:
        oauth = None
    if not oauth or not oauth.get("refreshToken"):
        return None
    acct = (load(os.path.join(folder, ".claude.json"), {}) or {}).get("oauthAccount") or {}
    return {"claudeAiOauth": oauth, "oauthAccount": acct, "folder": folder}


def _expired(oauth):
    exp = oauth.get("refreshTokenExpiresAt") or 0
    exp = exp / 1000 if exp > 1e12 else exp
    return bool(exp) and exp < time.time()


def login_candidates(target):
    """[(via, saved)] to try in order: the rotator's own snapshot, then the
    account's config folder when it holds a different token."""
    out = []
    raw = _kc_read(STORE_SVC, target)
    if raw:
        try:
            out.append(("stored_login", json.loads(raw)))
        except ValueError:
            pass
    side = folder_login(target)
    if side:
        mine = (out[0][1].get("claudeAiOauth") or {}).get("refreshToken") if out else None
        if side["claudeAiOauth"].get("refreshToken") != mine:
            out.append(("account_folder", side))
    return out


def login_coverage(emails):
    """email -> how a switch to it would sign in: stored_login, account_folder,
    or browser (nothing saved, so it needs the Chrome sign-in)."""
    folders = account_folders()
    cov = {}
    for e in emails:
        raw = _kc_read(STORE_SVC, e)
        ok = False
        if raw:
            try:
                ok = not _expired(json.loads(raw).get("claudeAiOauth") or {})
            except ValueError:
                ok = False
        if ok:
            cov[e] = "stored_login"
        else:
            side = folder_login(e, folders)
            cov[e] = "account_folder" if side and not _expired(side["claudeAiOauth"]) else "browser"
    return cov


def _write_oauth_account(acct):
    data = load(CLAUDE_JSON, {}) or {}
    data["oauthAccount"] = acct
    save(CLAUDE_JSON, data)


def capture_login():
    """Snapshot the logged-in account's token pair under its email. Only when
    the CLI and ~/.claude.json agree on who is logged in."""
    email = current_email()
    if not email or cli_email() != email:
        return None
    raw = _kc_read(CRED_SVC, CRED_ACCT)
    try:
        oauth = json.loads(raw or "{}").get("claudeAiOauth")
    except ValueError:
        oauth = None
    if not oauth or not oauth.get("refreshToken"):
        log("claude_rotate_capture_failed", measured=False, account=email, why_unmeasured="no claudeAiOauth in the CLI credential")
        return None
    acct = (load(CLAUDE_JSON, {}) or {}).get("oauthAccount") or {}
    prev = _kc_read(STORE_SVC, email)
    blob = json.dumps({"claudeAiOauth": oauth, "oauthAccount": acct, "captured": time.time()})
    if prev and json.loads(prev).get("claudeAiOauth") == oauth:
        return email
    if _kc_write(STORE_SVC, email, blob):
        log("claude_rotate_captured", measured=True, account=email, new=prev is None)
        return email
    log("claude_rotate_capture_failed", measured=False, account=email, why_unmeasured="keychain write failed")
    return None


def stored_switch(target, before, reason, started):
    """Swap in a saved login for `target`: the rotator's snapshot, then the
    account's own config folder. Returns 0 on a proven switch, None to fall
    back to the browser flow."""
    candidates = login_candidates(target)
    if not candidates:
        log("claude_rotate_no_stored_login", target=target, account_folder_checked=ACCOUNTS_DIR)
        return None
    capture_login()  # the account being left keeps its newest refresh token
    for via, saved in candidates:
        oauth = saved.get("claudeAiOauth") or {}
        if _expired(oauth):
            log("claude_rotate_stored_login_expired", target=target, via=via)
            continue
        prev_raw = _kc_read(CRED_SVC, CRED_ACCT)
        prev_acct = (load(CLAUDE_JSON, {}) or {}).get("oauthAccount")
        try:
            cred = json.loads(prev_raw or "{}")
        except ValueError:
            cred = {}
        cred["claudeAiOauth"] = oauth
        if not _kc_write(CRED_SVC, CRED_ACCT, json.dumps(cred)):
            log("claude_rotate_stored_switch_failed", target=target, via=via, why="keychain write failed")
            return None
        _write_oauth_account(saved.get("oauthAccount") or {})
        after = cli_email()
        windows = probe() if after == target else None
        if after == target and windows is not None:
            elapsed = round(time.time() - started, 1)
            state = load(STATE, {})
            state.update({"switched_at": time.time(), "from": before, "to": target, "reason": reason})
            (state.get("switch_failures") or {}).pop(target, None)
            save(STATE, state)
            print(log("claude_rotate_switched", measured=True, from_=before, to=target, reason=reason,
                      elapsed_s=elapsed, via=via, folder=saved.get("folder")))
            capture_login()
            resume_stranded(force=True)
            resume_auth_lost()
            return 0
        # Put the previous login back exactly, then try the next candidate.
        if prev_raw:
            _kc_write(CRED_SVC, CRED_ACCT, prev_raw)
        if prev_acct:
            _write_oauth_account(prev_acct)
        log("claude_rotate_stored_switch_failed", target=target, via=via, still=cli_email(), cli_said=after,
            probed=windows is not None, restored=bool(prev_raw))
    return None


# The browser sign-in drives Ethan's Chrome through the CDP relay, which needs
# one "Allow remote debugging?" click per Chrome launch. 2026-10-19 15:42 to
# 2026-10-20 08:06 the relay had no Chrome connection; every switch still waited
# out a 6-minute sign-in that ended "no tab appeared", and nobody was told.
OWNER_ALERT_GAP_S = int(os.environ.get("CLAUDE_ROTATE_OWNER_ALERT_GAP_S", str(3 * 3600)))


def relay_upstream():
    """The relay's Chrome connection: connected, awaiting_approval or
    disconnected. None when the relay status cannot be read (unknown, so the
    browser flow runs as before)."""
    try:
        p = subprocess.run(["amux", "cdp", "relay"], capture_output=True, text=True, timeout=15)
        line = next((l for l in reversed(p.stdout.splitlines()) if l.strip().startswith("{")), "")
        return json.loads(line).get("upstream") if line else None
    except Exception:
        return None


def browser_unreachable(target, reason, upstream):
    """Skip a sign-in that cannot work, ask Chrome for approval, and tell the
    owner once per OWNER_ALERT_GAP_S when the fleet is on a spent account."""
    try:  # a relay client makes Chrome show the approval prompt
        subprocess.Popen(["amux", "cdp", "list"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True)
    except Exception:
        pass
    fix = "click Allow on Chrome's \"Allow remote debugging?\" prompt"
    print(log("claude_rotate_browser_unreachable", measured=True, n_considered=1, target=target,
              reason=reason, upstream=upstream, fix=fix))
    if reason != "current_exhausted":
        return 1
    state = load(STATE, {})
    if time.time() - float(state.get("owner_alerted_at") or 0) < OWNER_ALERT_GAP_S:
        return 1
    current = current_email()
    msg = ("Claude account switching is stuck: %s is spent and %s has no saved login, so it needs a "
           "browser sign-in, but Chrome is not connected to amux (relay: %s). To fix it, %s." % (current, target, upstream, fix))
    try:
        p = subprocess.run(["amux", "alert", msg, "workers are rate-limited until the switch lands"],
                           capture_output=True, text=True, timeout=30)
        sent = p.returncode == 0
    except Exception:
        sent = False
    state = load(STATE, {})
    state["owner_alerted_at"] = time.time()
    save(STATE, state)
    log("claude_rotate_owner_alerted", measured=True, target=target, sent=sent)
    return 1


def _switch_running():
    with locked(LOCK, nowait=True) as got:
        return not got


def switch(target, reason="manual"):
    with locked(LOCK, nowait=True) as got:
        if not got:
            print(log("claude_rotate_switch_busy", target=target))
            return 1
        return _switch(target, reason)


def _switch(target, reason):
    before = cli_email()
    if before == target:
        print(log("claude_rotate_switch_noop", target=target))
        return 0
    started = time.time()
    rc = stored_switch(target, before, reason, started)
    if rc is not None:
        return rc
    upstream = relay_upstream()
    if upstream is not None and upstream != "connected":
        return browser_unreachable(target, reason, upstream)
    with tempfile.TemporaryDirectory(prefix="claude-rotate-") as tmp:
        url_file = os.path.join(tmp, "url")
        opener = os.path.join(tmp, "open.sh")
        with open(opener, "w") as f:
            # Only record the URL; the switch drives its own long-lived tab.
            f.write('#!/bin/sh\nprintf %%s "$1" > %s\n' % json.dumps(url_file))
        os.chmod(opener, 0o700)
        out_path = os.path.join(tmp, "cli.log")
        # `script` gives the CLI the terminal it expects; the old login stays
        # valid until this process completes, so killing it is a clean abort.
        proc = subprocess.Popen(["script", "-q", out_path, CLAUDE, "auth", "login", "--email", target],
                                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                env=dict(os.environ, BROWSER=opener))
        steps, tab, state, link_used, blank_since, waiting_since = [], None, None, False, None, None
        gap = LoginGapProbe()
        try:
            deadline = started + SWITCH_TIMEOUT_S
            while time.time() < deadline:
                if proc.poll() is not None:
                    break
                time.sleep(2)
                if tab is None:
                    if not os.path.exists(url_file):
                        continue
                    tab = profile_tab()
                    steps.append("tab")
                    cdp("nav", tab, open(url_file).read(), check=False)
                text = page_text(tab)
                trace(target, tab, text)
                if not text.strip():
                    blank_since = blank_since or time.time()
                    if time.time() - blank_since > 25 and steps.count("reload") < 3:
                        cdp("nav", tab, open(url_file).read(), check=False)
                        steps.append("reload")
                        blank_since = None
                    continue
                blank_since = None
                if re.search(r"Authorize", text) and "Logged in as" in text:
                    if ("Logged in as " + target) in text:
                        if click_text(tab, r"^Authorize$"):
                            gap.start()
                            steps.append("authorize")
                            time.sleep(4)
                    elif steps.count("switch-account") >= 3 and steps[-1] == "switch-account":
                        # Clicking "Switch account" keeps bouncing back to the
                        # authorize page (session cookies auto-log the old
                        # account). Clear claude.ai session cookies via CDP
                        # so the next navigation hits the real login form.
                        cdp("evalraw", tab, "Storage.clearDataForOrigin",
                            json.dumps({"origin": "https://claude.ai", "storageTypes": "cookies"}),
                            check=False)
                        auth_url = open(url_file).read()
                        cdp("nav", tab, auth_url, check=False)
                        steps.append("force-login")
                        time.sleep(4)
                    elif click_text(tab, r"^Switch account$"):
                        steps.append("switch-account")
                elif "Sign in here instead" in text:
                    if click_text(tab, r"^Sign in here instead$"):
                        steps.append("sign-in-here")
                elif "click the link sent to" in text or "verification code" in text.lower():
                    # Opening the link in this tab drops the pending Claude Code
                    # authorization ("Sign in here instead" lands on a bare
                    # /login). Read the code it shows in a helper tab and type
                    # it here instead.
                    if not link_used:
                        waiting_since = waiting_since or time.time()
                        link = magic_link(target, started)
                        if not link and target in _LINK_UNREADABLE_LOGGED:
                            # The email is there and cannot be opened; waiting
                            # out the page only keeps the fleet on a spent account.
                            raise RuntimeError("sign-in link unreadable")
                        if not link and time.time() - waiting_since > 75 and "resend" not in steps:
                            # Forwarded mail sometimes never arrives; one more send.
                            if click_text(tab, r"^(Resend email|Try sending again)$"):
                                steps.append("resend")
                        if link:
                            code = code_from_link(link)
                            link_used = True
                            steps.append("code" if code else "no-code")
                            if code and enter_code(tab, code):
                                steps.append("verify")
                                time.sleep(4)
                            elif not code:
                                # Visiting the magic link often completes
                                # the auth without a visible code (the
                                # browser redirect finishes the PKCE flow).
                                # Give the CLI a moment before giving up.
                                steps.append("link-wait")
                                time.sleep(8)
                                if proc.poll() is not None:
                                    break
                elif "Continue with email" in text:
                    if steps.count("continue-email") >= 3:
                        raise RuntimeError("login page did not advance after 3 email submissions")
                    # "Switch account" clears the prefilled address.
                    if fill_email(tab, target):
                        steps.append("fill-email")
                    if click_text(tab, r"^Continue with email$"):
                        steps.append("continue-email")
                        time.sleep(3)
            try:
                proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                pass
        except Exception as e:
            steps.append("error: " + str(e)[:160])
        finally:
            if proc.poll() is None:
                proc.kill()
            if tab:  # park the reused tab so it holds no half-finished login
                cdp("nav", tab, "https://claude.ai/", check=False)
            cli_out = open(out_path, errors="replace").read() if os.path.exists(out_path) else ""
    after = cli_email()
    time.sleep(1)
    gap_info = gap.stop()
    if gap_info:
        log("claude_rotate_login_gap", measured=True, target=target, **gap_info)
    ok = after == target
    elapsed = round(time.time() - started, 1)
    if ok:
        state = load(STATE, {})
        state.update({"switched_at": time.time(), "from": before, "to": target, "reason": reason})
        cleared = forgive_browser_failures(state, target)
        save(STATE, state)
        print(log("claude_rotate_switched", measured=True, from_=before, to=target, reason=reason,
                  elapsed_s=elapsed, steps=steps, backoff_cleared=cleared))
        capture_login()
        resume_stranded(force=True)
        time.sleep(5)
        resume_auth_lost()
        return 0
    tail = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\]8;;[^\x07\x1b]*(\x07|\x1b\\)?", "", cli_out)[-300:]
    state = load(STATE, {})
    fails = state.setdefault("switch_failures", {})
    fails[target] = note_failure(fails.get(target, []), steps)
    marked = set(state.get("account_failures") or [])
    if any("unreadable" in str(x) for x in steps):
        marked.add(target)
    else:
        marked.discard(target)
    state["account_failures"] = sorted(marked)
    save(STATE, state)
    print(log("claude_rotate_switch_failed", measured=True, target=target, still=after, reason=reason,
              elapsed_s=elapsed, steps=steps, cli_tail=tail, recent_failures=len(fails[target]),
              backed_off=len(fails[target]) >= FAIL_BACKOFF_N))
    return 1


# ---------------------------------------------------------------- main

def main(argv):
    cmd = argv[1] if len(argv) > 1 else "status"
    if cmd == "statusline":
        cmd_statusline()
        return 0
    if cmd == "pick":
        cmd_pick(argv[2:])
        return 0
    if cmd == "tick":
        return cmd_tick()
    if cmd in ("on", "off"):
        os.makedirs(DIR, exist_ok=True)
        if cmd == "on":
            open(ENABLED_FILE, "a").close()
        elif os.path.exists(ENABLED_FILE):
            os.remove(ENABLED_FILE)
        print(log("claude_rotate_" + ("enabled" if cmd == "on" else "disabled"), measured=True, n_considered=1, file=ENABLED_FILE))
        return 0
    if cmd == "probe":
        return 0 if probe() else 1
    if cmd == "switch" and len(argv) == 3:
        return switch(argv[2])
    if cmd == "record" and len(argv) == 7:
        _, _, email, f, fr, w, wr = argv
        record(email, float(f), fr or None, float(w), wr or None, source="manual")
        return 0
    if cmd == "history":
        hours = argv[2] if len(argv) > 2 else "168"
        out = subprocess.run(["curl", "-sk", "--max-time", "10",
                              _amux_base() + USAGE_LOG_PATH + "?since_h=" + hours],
                             capture_output=True, text=True).stdout
        print(out)
        return 0 if out.strip() else 1
    if cmd == "status":
        state = load(STATE, {})
        email = current_email()
        cur = load(ACCOUNTS, {"accounts": {}})["accounts"].get(email, {})
        print(json.dumps({"current": email, "reading": cur, "last_switch": state,
                          "reporters_last_10m": reporters()}, indent=2))
        return 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
