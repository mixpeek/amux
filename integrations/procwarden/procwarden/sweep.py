"""Rule evaluation for scheduled cleanup sweeps.

A sweep:
  1. collects all processes,
  2. evaluates each enabled rule's match + when conditions,
  3. tracks consecutive "strikes" so a rule only fires after `for_sweeps`
     consecutive matches (kills runaways, not momentary spikes),
  4. applies the safety gate, then either logs (dry run) or kills.
"""
from __future__ import annotations

import json
import re
import signal
import time

from . import collect
from .actions import gate, kill_pid
from .config import load_state, save_state
from .collect import CURRENT_USER


def _state_key(p):
    return f"{p.pid}:{int(p.create_time)}"


def _owner_ok(spec, username):
    spec = (spec or "any").lower()
    if spec in ("any", "*", ""):
        return True
    if spec == "me":
        return username == CURRENT_USER
    if spec == "root":
        return username == "root"
    return username == spec


def _matches(rule_match, p):
    if not _owner_ok(rule_match.get("owner"), p.username):
        return False
    st = rule_match.get("status")
    if st and p.status != st:
        return False
    nre = rule_match.get("name_regex")
    if nre and not re.search(nre, p.name):
        return False
    cre = rule_match.get("cmdline_regex")
    if cre and not re.search(cre, p.cmdline):
        return False
    return True


def _conditions_hold(when, p):
    """All present conditions must hold. Empty `when` -> always true (used by
    match-only rules like zombies)."""
    if "cpu_above" in when and not (p.cpu > when["cpu_above"]):
        return False
    if "cpu_below" in when and not (p.cpu < when["cpu_below"]):
        return False
    if "mem_above_mb" in when and not (p.mem_rss > when["mem_above_mb"] * 1024 * 1024):
        return False
    if "uptime_above_hours" in when and not (p.uptime > when["uptime_above_hours"] * 3600):
        return False
    if "net_above_mb" in when and not (
            (p.net_in + p.net_out) > when["net_above_mb"] * 1024 * 1024):
        return False
    return True


def _excluded(cfg, p):
    for pat in cfg.get("exclude_names", []):
        if re.search(pat, p.name):
            return True
    for u in cfg.get("exclude_users", []):
        if p.username == u:
            return True
    return False


def _rule_needs_net(cfg):
    for r in cfg.get("rules", []):
        if "net_above_mb" in (r.get("when") or {}):
            return True
    return False


def evaluate(cfg, procs, state):
    """Return (fired, new_state).

    `fired` is a list of dicts describing each rule that reached its strike
    threshold this sweep, with the gate decision attached.
    """
    new_state = {}
    fired = []
    rules = [r for r in cfg.get("rules", []) if r.get("enabled", True)]

    for p in procs:
        if _excluded(cfg, p):
            continue
        key = _state_key(p)
        prev = state.get(key, {})
        cur = {"name": p.name, "last_seen": int(time.time())}

        for rule in rules:
            rname = rule["name"]
            hit = _matches(rule.get("match", {}), p) and \
                _conditions_hold(rule.get("when", {}), p)
            need = max(1, int(rule.get("for_sweeps", 1)))
            strikes = (prev.get(rname, 0) + 1) if hit else 0
            if strikes:
                cur[rname] = strikes
            if hit and strikes >= need:
                decision = gate(p.safety, rule.get("force", False))
                fired.append({
                    "proc": p, "rule": rule, "strikes": strikes,
                    "needed": need, "decision": decision,
                })
        new_state[key] = cur
    return fired, new_state


def run_sweep(cfg, dry_run_override=None, logger=None):
    """Execute one sweep. Returns a summary dict.

    dry_run_override: True forces dry-run; None uses cfg['enforce'].
    logger: optional callable(str) for human progress lines.
    """
    def log(msg):
        if logger:
            logger(msg)

    enforce = bool(cfg.get("enforce", False))
    if dry_run_override is True:
        enforce = False
    dry = not enforce

    procs = collect.collect(sample_interval=0.6, want_net=_rule_needs_net(cfg))
    state = load_state()
    fired, new_state = evaluate(cfg, procs, state)
    save_state(new_state)

    actions = []
    for f in fired:
        p, rule, decision = f["proc"], f["rule"], f["decision"]
        action = rule.get("action", "report")
        entry = {
            "ts": int(time.time()),
            "rule": rule["name"],
            "pid": p.pid,
            "name": p.name,
            "user": p.username,
            "cpu": round(p.cpu, 1),
            "mem_mb": round(p.mem_rss / 1024 / 1024, 1),
            "uptime_s": int(p.uptime),
            "risk": p.safety.level if p.safety else "?",
            "action": action,
            "strikes": f"{f['strikes']}/{f['needed']}",
        }
        # Per-rule enforce: a rule with "enforce": true acts even while the
        # global flag keeps every other rule dry-run (the orphaned
        # browser_use.skill_cli.server case, 2026-07-17: 5 ppid=1 servers up to
        # 8 days old that no dry-run sweep would ever reap). An explicit CLI
        # --dry-run still forces dry-run for EVERYTHING, per-rule flag included.
        rule_dry = dry
        if rule.get("enforce") is True and dry_run_override is not True:
            rule_dry = False
        if action == "report":
            entry["result"] = "report-only"
        elif not decision.allowed:
            entry["result"] = f"BLOCKED-by-gate: {decision.note}"
        elif rule_dry:
            entry["result"] = "WOULD-KILL (dry-run)"
        else:
            sig = getattr(signal, "SIG" + rule.get("signal", "TERM").upper(),
                          signal.SIGTERM)
            ok, msg = kill_pid(p.pid, sig=sig, escalate=True)
            entry["result"] = ("KILLED: " + msg) if ok else ("FAILED: " + msg)
        actions.append(entry)
        log(f"  [{entry['rule']}] {p.name} (pid {p.pid}, {entry['risk']}) "
            f"cpu={entry['cpu']}% mem={entry['mem_mb']}MB -> {entry['result']}")

    _enforced_rules = [r["name"] for r in cfg.get("rules", [])
                       if r.get("enabled", True) and r.get("enforce") is True]                       if (dry and dry_run_override is not True) else []
    summary = {
        "ts": int(time.time()),
        "mode": ("dry-run" if not _enforced_rules else
                 f"dry-run ({len(_enforced_rules)} rule(s) enforced)") if dry else "enforce",
        "scanned": len(procs),
        "fired": len(actions),
        "actions": actions,
    }
    _append_log(cfg.get("log_file"), summary)
    return summary


def _append_log(path, summary):
    if not path:
        return
    import os
    os.makedirs(os.path.dirname(path), exist_ok=True)
    try:
        with open(path, "a") as f:
            f.write(json.dumps(summary) + "\n")
    except OSError:
        pass
