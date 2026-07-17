"""Nightly maintenance pass — keep a 24/7 Mac snappy without rebooting.

`procwarden sweep` only kills processes. Keeping a long-uptime server fast needs
a few more, non-process levers that a sweep can't express:

  1. purge      — reclaim inactive/cached memory (relieves swap thrashing).
  2. spotlight  — de-index high-churn dirs so fseventsd/mds stop burning cores.
                  (fseventsd is SIP-protected and unkillable; the only no-reboot
                  fix is to remove the filesystem-event churn that feeds it.)
  3. health     — watch memory %, swap, and indexing daemons; alert on *new*
                  concerns via the amux board (deduped so it never spams).
  4. reap       — run the normal config sweep (report-only until you enforce).

Separation of concern by risk:
  * purge / spotlight / health are NON-destructive and run whenever their config
    flag is on — they can't lose work.
  * the process reap obeys `enforce` (default false = dry-run), exactly like a
    manual sweep, so it never kills your own live sessions until you opt in.

Privilege: purge and mdutil need root. This module shells out via `sudo -n`
(non-interactive) so a narrow /etc/sudoers.d drop-in can grant *just* those two
commands — the pass itself runs unprivileged as you. See `privilege_dropin()`.
"""
from __future__ import annotations

import json
import os
import subprocess
import time

from . import collect
from .util import c, human_bytes

PURGE_BIN = "/usr/sbin/purge"
MDUTIL_BIN = "/usr/bin/mdutil"

_CFG_DIR = os.path.expanduser("~/.procwarden")
MAINT_LOG = os.path.join(_CFG_DIR, "maintain.log")
MAINT_STATE = os.path.join(_CFG_DIR, "maintain_state.json")
DROPIN_PATH = "/etc/sudoers.d/procwarden"


# --------------------------------------------------------------------------- #
# privilege drop-in (purge + mdutil, passwordless, nothing else)
# --------------------------------------------------------------------------- #
def privilege_dropin(user=None):
    """The exact /etc/sudoers.d/procwarden content granting passwordless purge +
    mdutil to `user` (default: the current user). Least privilege — no other
    command is elevated."""
    user = user or collect.CURRENT_USER
    return (f"# procwarden nightly maintenance — passwordless purge + mdutil only\n"
            f"{user} ALL=(root) NOPASSWD: {PURGE_BIN}, {MDUTIL_BIN}\n")


def _sudo(argv):
    """Run a command as root non-interactively. Returns (ok, output).

    If we're already root, run directly; otherwise `sudo -n` (never prompts —
    fails cleanly if the drop-in isn't installed, so the pass degrades instead
    of hanging a headless launchd job on a password prompt)."""
    if os.geteuid() == 0:
        cmd = argv
    else:
        cmd = ["sudo", "-n"] + argv
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=180)
    except Exception as e:  # noqa: BLE001 — best-effort maintenance
        return False, str(e)
    out = (r.stdout or "") + (r.stderr or "")
    return r.returncode == 0, out.strip()


def _needs_password(out):
    return "password is required" in out or "a terminal is required" in out


# --------------------------------------------------------------------------- #
# system snapshot
# --------------------------------------------------------------------------- #
def _compressor_bytes():
    """Bytes held by the VM compressor, parsed from vm_stat. None on failure."""
    try:
        r = subprocess.run(["vm_stat"], capture_output=True, text=True, timeout=8)
    except Exception:
        return None
    page = 4096
    occupied = None
    for line in r.stdout.splitlines():
        if "page size of" in line:
            for tok in line.split():
                if tok.isdigit():
                    page = int(tok)
                    break
        if "occupied by compressor" in line:
            occupied = int(line.rsplit(":", 1)[1].strip().rstrip("."))
    return occupied * page if occupied is not None else None


def _mem_swap():
    import psutil
    vm = psutil.virtual_memory()
    sw = psutil.swap_memory()
    return {
        "mem_used_pct": vm.percent,
        "mem_used": vm.used,
        "mem_available": vm.available,
        "mem_total": vm.total,
        "swap_used_mb": sw.used / 1024 / 1024,
        "swap_total_mb": sw.total / 1024 / 1024,
        "swap_free_mb": sw.free / 1024 / 1024,
        "compressor": _compressor_bytes(),
    }


def _name_match(nl, wl):
    """Prefix-tolerant name match (macOS truncates process comm to ~16 chars)."""
    if not nl or not wl:
        return False
    a, b = nl[:14], wl[:14]
    return a.startswith(b) or b.startswith(a)


def _daemon_cpu(procs, watch):
    """{daemon_name: summed_cpu_percent} for each watched daemon that's running."""
    result = {}
    for w in watch:
        wl = w.lower()
        total, hit = 0.0, False
        for p in procs:
            if _name_match((p.name or "").lower(), wl):
                total += p.cpu
                hit = True
        if hit:
            result[w] = round(total, 1)
    return result


def snapshot(procs=None, watch=None):
    """Point-in-time system snapshot used for before/after + health."""
    snap = _mem_swap()
    snap["load"] = list(os.getloadavg())
    if watch:
        if procs is None:
            procs = collect.collect(sample_interval=0.5)
        snap["daemons"] = _daemon_cpu(procs, watch)
    else:
        snap["daemons"] = {}
    return snap


# --------------------------------------------------------------------------- #
# actions
# --------------------------------------------------------------------------- #
def run_purge(dry):
    """Reclaim inactive memory. Returns a result dict (never raises)."""
    import psutil
    before = psutil.virtual_memory().available
    if dry:
        return {"action": "purge", "result": "skipped (dry-run)",
                "freed_bytes": 0}
    ok, out = _sudo([PURGE_BIN])
    after = psutil.virtual_memory().available
    freed = max(0, after - before)
    if ok:
        return {"action": "purge", "result": "ok", "freed_bytes": freed}
    res = ("needs-privilege (install the sudoers drop-in — see "
           "`procwarden maintain --print-sudoers`)"
           if _needs_password(out) else f"failed: {out[:120]}")
    return {"action": "purge", "result": res, "freed_bytes": 0}


SENTINEL = ".metadata_never_index"


def spotlight_exclude(paths, dry):
    """Exclude each directory subtree from Spotlight by dropping a
    `.metadata_never_index` sentinel file. This is per-directory and needs no
    root — unlike `mdutil -i off`, which only operates at *volume* granularity
    (verified: `mdutil -s` on a subdir returns "unknown indexing state"). Starves
    mds/mdworker of index churn on hot dirs."""
    results = []
    for raw in paths:
        path = os.path.expanduser(raw)
        if not os.path.isdir(path):
            results.append({"path": path, "result": "skipped (not a dir)"})
            continue
        sentinel = os.path.join(path, SENTINEL)
        if os.path.exists(sentinel):
            results.append({"path": path, "result": "already excluded"})
            continue
        if dry:
            results.append({"path": path, "result": f"would drop {SENTINEL} (dry-run)"})
            continue
        try:
            open(sentinel, "a").close()
            results.append({"path": path, "result": f"excluded ({SENTINEL})"})
        except OSError as e:
            results.append({"path": path, "result": f"failed: {e}"})
    return results


# --------------------------------------------------------------------------- #
# health + alerting (deduped so persistent issues don't spam nightly)
# --------------------------------------------------------------------------- #
def evaluate_health(cfg, snap):
    """Return a list of {key, msg} concerns from thresholds in the config."""
    h = cfg["maintenance"]["health"]
    out = []
    if snap["mem_used_pct"] > h["mem_used_pct_above"]:
        out.append({"key": "mem",
                    "msg": f"memory {snap['mem_used_pct']:.0f}% used "
                           f"(threshold {h['mem_used_pct_above']}%)"})
    if snap["swap_used_mb"] > h["swap_used_mb_above"]:
        out.append({"key": "swap",
                    "msg": f"swap {snap['swap_used_mb']:.0f}MB used of "
                           f"{snap['swap_total_mb']:.0f}MB "
                           f"(threshold {h['swap_used_mb_above']}MB)"})
    for name, cpu in snap["daemons"].items():
        if cpu > h["daemon_cpu_above"]:
            out.append({"key": f"daemon:{name}",
                        "msg": f"{name} at {cpu:.0f}% CPU — SIP-protected "
                               f"indexing daemon; add its churn source to "
                               f"maintenance.spotlight_exclude"})
    return out


def _amux_post(base, path, payload):
    try:
        r = subprocess.run(
            ["curl", "-sk", "-m", "10", "-X", "POST",
             "-H", "Content-Type: application/json",
             "-d", json.dumps(payload), f"{base}{path}"],
            capture_output=True, text=True, timeout=15)
        return r.returncode == 0
    except Exception:
        return False


def alert(cfg, concerns, fresh):
    """Notify on *fresh* concerns only (deduped against last run). Returns the
    list of channels used."""
    al = cfg["maintenance"]["alert"]
    used = []
    if not fresh:
        return used
    lines = "; ".join(x["msg"] for x in fresh)
    if al.get("post_board"):
        ok = _amux_post(al["amux_base"], "/api/board", {
            "title": f"procwarden: {len(fresh)} new host-health concern(s)",
            "desc": lines,
            "session": al.get("session", "procwarden-maintain"),
            "status": "todo",
        })
        if ok:
            used.append("board")
    if al.get("urgent_owner_alert") and any(x["key"] in ("mem", "swap")
                                            for x in fresh):
        ok = _amux_post(al["amux_base"], "/api/alert/owner", {
            "message": f"procwarden host health: {lines}",
            "reason": "memory/swap pressure on the 24/7 server",
            "session": al.get("session", "procwarden-maintain"),
        })
        if ok:
            used.append("owner-alert")
    return used


def _load_state():
    try:
        with open(MAINT_STATE) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {}


def _save_state(state):
    os.makedirs(_CFG_DIR, exist_ok=True)
    tmp = MAINT_STATE + ".tmp"
    with open(tmp, "w") as f:
        json.dump(state, f)
    os.replace(tmp, MAINT_STATE)


# --------------------------------------------------------------------------- #
# orchestrator
# --------------------------------------------------------------------------- #
def run_maintain(cfg, dry_run_override=None, logger=None):
    """Execute one nightly maintenance pass. Returns a summary dict.

    dry_run_override=True forces every action (purge/spotlight/reap) to log-only.
    Non-destructive actions otherwise run whenever their config flag is set;
    the process reap additionally obeys cfg['enforce']."""
    def log(msg):
        if logger:
            logger(msg)

    m = cfg.get("maintenance", {})
    watch = m.get("watch_daemons", [])
    force_dry = dry_run_override is True

    procs = collect.collect(sample_interval=0.6)
    before = snapshot(procs=procs, watch=watch)
    log(c("host: ", "bold")
        + f"mem {before['mem_used_pct']:.0f}% used, "
          f"swap {before['swap_used_mb']:.0f}/{before['swap_total_mb']:.0f}MB, "
          f"load {before['load'][0]:.1f}")
    for name, cpu in before["daemons"].items():
        tag = c("HOT", "red") if cpu > m.get("health", {}).get(
            "daemon_cpu_above", 80) else c("ok", "dim")
        log(f"  daemon {name}: {cpu:.0f}% [{tag}]")

    actions = []

    # 1. memory purge (non-destructive) --------------------------------------
    if m.get("enabled", True) and m.get("purge_memory", True):
        pr = run_purge(force_dry)
        actions.append(pr)
        log(c("purge: ", "bold") + pr["result"]
            + (f"  (+{human_bytes(pr['freed_bytes'])} free)"
               if pr["freed_bytes"] else ""))

    # 2. spotlight de-index of churn dirs (non-destructive) ------------------
    if m.get("enabled", True) and m.get("spotlight_exclude"):
        sr = spotlight_exclude(m["spotlight_exclude"], force_dry)
        actions.append({"action": "spotlight", "results": sr})
        for r in sr:
            log(c("spotlight: ", "bold") + f"{r['path']} -> {r['result']}")

    # 3. process reap — reuse the sweep; obeys cfg['enforce'] ----------------
    reap = None
    if m.get("enabled", True) and m.get("run_sweep", True):
        from . import sweep as sweepmod
        reap = sweepmod.run_sweep(
            cfg, dry_run_override=(True if force_dry else None),
            logger=(lambda s: log("  " + s)) if logger else None)
        log(c("reap: ", "bold")
            + f"{reap['fired']} rule hit(s), mode={reap['mode']}")

    # 4. health check + deduped alert ----------------------------------------
    after = snapshot(procs=None, watch=watch) if not force_dry else before
    concerns = evaluate_health(cfg, after)
    st = _load_state()
    last_keys = set(st.get("concern_keys", []))
    fresh = [x for x in concerns if x["key"] not in last_keys]
    channels = alert(cfg, concerns, fresh) if not force_dry else []
    for x in concerns:
        new = " " + c("(new)", "yellow") if x in fresh else ""
        log(c("health: ", "bold") + c("⚠ " + x["msg"], "yellow") + new)
    if not concerns:
        log(c("health: ", "bold") + c("all clear", "green"))
    if channels:
        log(c("alert: ", "bold") + "notified via " + ", ".join(channels))

    summary = {
        "ts": int(time.time()),
        "mode": "dry-run" if force_dry else "live",
        "before": before,
        "after": after,
        "actions": actions,
        "reap": {"fired": reap["fired"], "mode": reap["mode"]} if reap else None,
        "concerns": concerns,
        "fresh_concerns": fresh,
        "alert_channels": channels,
    }
    if not force_dry:
        st["concern_keys"] = [x["key"] for x in concerns]
        st["last_run"] = summary["ts"]
        _save_state(st)
    _append_log(summary)
    return summary


def _append_log(summary):
    os.makedirs(_CFG_DIR, exist_ok=True)
    try:
        with open(MAINT_LOG, "a") as f:
            f.write(json.dumps(summary) + "\n")
    except OSError:
        pass
