#!/usr/bin/env python3
"""PreToolUse guard: enforce the amux worker boundaries in boundaries.toml.

One hook, one registry. The rules, their wording and their examples live in
boundaries.toml (repo root; installed beside this file as
~/.amux/hooks/boundaries.toml), with optional owner overrides in
~/.amux/boundaries.toml. This script holds no rule of its own: it loads the
registry, refuses the tool calls a `deny` rule describes, refuses heavy
commands while the machine is critical (the `resources` boundary), and refuses
a new Agent/Task call past the running-subagent cap or under the account's
usage headroom floor (the `subagents` boundary).

Who it applies to: amux workers (AMUX_SESSION set), and isolated ones
(AMUX_ISOLATED_WORKER set) while the registry's top-level `apply_to_isolated`
is true (the default; it is the named exception in CLAUDE.md "Isolated
workers").

Every decision other than a plain allow is appended to
~/.amux/logs/boundaries.jsonl (tokens redacted), which is what
/api/boundaries counts.

Fail-open: a guard must never wedge a tool call. An unreadable payload or
registry lets the call through, and a registry that does not load is logged
as verdict=boundary_registry_unloadable so the sweep sees every boundary
went dark.
"""
from __future__ import annotations

import json
import os
import re
import sys
import time

# subprocess, ssl and urllib are imported where used: they cost ~45ms at
# startup and only the resources check needs them, while this hook runs before
# every Bash, Read and Edit call in the fleet.

try:
    import tomllib
except ModuleNotFoundError:  # python < 3.11
    tomllib = None

# Grep and Glob read the filesystem too: a Grep into the vault prints card
# data (found by the MSG-90256 adversarial pass, 2026-10-17).
FILE_TOOLS = {"Read", "Edit", "Write", "MultiEdit", "NotebookEdit", "Grep", "Glob"}
HERE = os.path.dirname(os.path.abspath(__file__))


def amux_home() -> str:
    return os.environ.get("AMUX_HOME") or os.path.join(os.path.expanduser("~"), ".amux")


def registry_paths() -> list[str]:
    explicit = os.environ.get("AMUX_BOUNDARIES_FILE")
    if explicit:
        return [explicit]
    paths = [os.path.join(HERE, "boundaries.toml")]
    repo_copy = os.path.join(os.path.dirname(os.path.dirname(HERE)), "boundaries.toml")
    if not os.path.exists(paths[0]) and os.path.exists(repo_copy):
        paths = [repo_copy]
    override = os.path.join(amux_home(), "boundaries.toml")
    if os.path.exists(override):
        paths.append(override)
    return paths


def merge(base: list[dict], over: list[dict]) -> list[dict]:
    """Owner overrides replace fields of the boundary with the same id."""
    by_id = {b.get("id"): dict(b) for b in base}
    order = [b.get("id") for b in base]
    for b in over:
        bid = b.get("id")
        if bid in by_id:
            by_id[bid].update(b)
        else:
            by_id[bid] = dict(b)
            order.append(bid)
    return [by_id[i] for i in order]


def load_registry() -> tuple[list[dict], dict]:
    """Enabled boundaries, and the top-level settings (later layers win)."""
    if tomllib is None:
        raise RuntimeError("python has no tomllib (needs 3.11+)")
    boundaries: list[dict] = []
    settings: dict = {}
    for i, p in enumerate(registry_paths()):
        with open(p, "rb") as fh:
            data = tomllib.load(fh)
        layer = data.get("boundary") or []
        boundaries = layer if i == 0 else merge(boundaries, layer)
        settings.update({k: v for k, v in data.items() if k != "boundary"})
    return [b for b in boundaries if b.get("enabled", True)], settings


SECRET_PATTERNS = [
    (re.compile(r"(Bearer|token|Basic)\s+[A-Za-z0-9._~+/=-]{8,}", re.I), r"\1 ***"),
    (re.compile(r"\b(sk|rk|pk)_(live|test)_[A-Za-z0-9]+"), "***"),
    (re.compile(r"(-u|--user)\s+\S+"), r"\1 ***"),
    (re.compile(r"(password|passwd|secret|api[_-]?key)=\S+", re.I), r"\1=***"),
]


def redact(text: str) -> str:
    for pat, rep in SECRET_PATTERNS:
        text = pat.sub(rep, text)
    return text[:200]


# The worker this call belongs to, resolved once in main(): AMUX_SESSION, or
# AMUX_ISOLATED_WORKER for an isolated worker (whose launch unsets the former).
WORKER = ""
WORKER_ISOLATED = False


def log(event: dict) -> None:
    try:
        d = os.path.join(amux_home(), "logs")
        os.makedirs(d, exist_ok=True)
        event = {"ts": round(time.time(), 3), "session": WORKER or os.environ.get("AMUX_SESSION", ""), **({"isolated": True} if WORKER_ISOLATED else {}), **event}
        with open(os.path.join(d, "boundaries.jsonl"), "a") as fh:
            fh.write(json.dumps(event) + "\n")
    except Exception:
        pass


def is_isolated(session: str) -> bool:
    try:
        with open(os.path.join(amux_home(), "sessions", session + ".env")) as fh:
            for line in fh:
                k, _, v = line.strip().partition("=")
                if k.strip() == "CC_ISOLATED":
                    return v.strip().strip("'\"").lower() in ("1", "true", "yes", "on")
    except OSError:
        pass
    return False


def search(pattern: str, text: str) -> bool:
    return re.search(pattern, text, re.IGNORECASE | re.MULTILINE) is not None


def rule_applies(rule: dict, tool: str, command: str, path: str, content: str) -> bool:
    if tool not in (rule.get("tools") or ["Bash"]):
        return False
    if tool == "Bash":
        alls = rule.get("match_all") or []
        if not alls or not all(search(p, command) for p in alls):
            return False
        return not any(search(p, command) for p in rule.get("match_none") or [])
    if tool in FILE_TOOLS:
        pat = rule.get("path")
        if not pat or not path or not search(pat, path):
            return False
        anys = rule.get("content_any") or []
        return not anys or any(search(p, content) for p in anys)
    return False


# ---- resources -----------------------------------------------------------------

def _sysctl(name: str) -> str | None:
    import subprocess

    for exe in ("/usr/sbin/sysctl", "sysctl"):
        try:
            out = subprocess.run([exe, "-n", name], capture_output=True, text=True, timeout=2)
            if out.returncode == 0:
                return out.stdout.strip()
        except (OSError, subprocess.SubprocessError):
            continue
    return None


def host_sample() -> dict:
    """Load per core, swap used fraction and memory level, read directly so the
    check does not depend on the amux server being up."""
    fixed = os.environ.get("AMUX_BOUNDARIES_HOST_SAMPLE")  # tests: a fixed reading
    if fixed:
        s = json.loads(fixed)
        s.setdefault("ncpu", 1)
        if "load5" in s:
            s["load_per_core"] = s["load5"] / s["ncpu"]
        return s
    s: dict = {"ncpu": os.cpu_count() or 1}
    try:
        s["load5"] = os.getloadavg()[1]
    except OSError:
        pass
    lvl = _sysctl("kern.memorystatus_vm_pressure_level")
    if lvl and lvl.strip().isdigit():
        s["mem_level"] = int(lvl)
    swap = _sysctl("vm.swapusage")  # "total = 9216.00M  used = 8134.50M  free = ..."
    if swap:
        m_t = re.search(r"total\s*=\s*([\d.]+)M", swap)
        m_u = re.search(r"used\s*=\s*([\d.]+)M", swap)
        if m_t and m_u and float(m_t.group(1)) > 0:
            s["swap_used"] = float(m_u.group(1)) / float(m_t.group(1))
            s["swap_total_mb"] = float(m_t.group(1))
    elif os.path.exists("/proc/meminfo"):
        try:
            info = dict(l.split(":", 1) for l in open("/proc/meminfo") if ":" in l)
            tot = float(info["SwapTotal"].split()[0]); free = float(info["SwapFree"].split()[0])
            if tot > 0:
                s["swap_used"] = (tot - free) / tot
                s["swap_total_mb"] = tot / 1024
            avail = float(info["MemAvailable"].split()[0]) / float(info["MemTotal"].split()[0])
            s["mem_level"] = 4 if avail < 0.05 else 2 if avail < 0.10 else 1
        except (KeyError, ValueError, OSError):
            pass
    if "load5" in s:
        s["load_per_core"] = s["load5"] / s["ncpu"]
    return s


def classify(s: dict, cfg: dict) -> tuple[str, list[str]]:
    reasons_c, reasons_s = [], []
    lpc = s.get("load_per_core")
    if lpc is not None:
        if lpc >= cfg.get("load_per_core_critical", 2.5):
            reasons_c.append(f"load {s['load5']:.1f} on {s['ncpu']} cores ({lpc:.2f}/core)")
        elif lpc >= cfg.get("load_per_core_strained", 1.5):
            reasons_s.append(f"load {s['load5']:.1f} on {s['ncpu']} cores ({lpc:.2f}/core)")
    sw = s.get("swap_used")
    if sw is not None and s.get("swap_total_mb", 0) >= 1024:
        if sw >= cfg.get("swap_used_critical", 0.97):
            reasons_c.append(f"swap {sw:.0%} used")
        elif sw >= cfg.get("swap_used_strained", 0.85):
            reasons_s.append(f"swap {sw:.0%} used")
    ml = s.get("mem_level")
    if ml is not None:
        if ml >= cfg.get("mem_level_critical", 4):
            reasons_c.append(f"memory pressure level {ml}")
        elif ml >= cfg.get("mem_level_strained", 2):
            reasons_s.append(f"memory pressure level {ml}")
    if reasons_c:
        return "critical", reasons_c
    if reasons_s:
        return "strained", reasons_s
    measured = any(k in s for k in ("load_per_core", "swap_used", "mem_level"))
    return ("ok" if measured else "unmeasured"), []


def my_usage(session: str) -> str:
    """This worker's footprint from the amux server, best effort (1.5s)."""
    url = os.environ.get("AMUX_URL")
    if not url or not session:
        return ""
    import ssl
    import urllib.request

    try:
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        with urllib.request.urlopen(f"{url}/api/resources?worker={session}", timeout=1.5, context=ctx) as r:
            d = json.load(r)
        w = d.get("worker") or {}
        if w.get("measured"):
            return f"You are using {w.get('cpu_percent', 0):.0f}% CPU and {w.get('rss_mb', 0):.0f} MB across {w.get('n_processes', 0)} processes."
    except Exception:
        pass
    return ""


ACK_RE = re.compile(r"""(?:^|[;&|]\s*|\benv\s+)(AMUX_RESOURCES_ACK)=(?:"([^"]*)"|'([^']*)'|(\S+))""")
DISK_ACK_RE = re.compile(r"""(?:^|[;&|]\s*|\benv\s+)(AMUX_DISK_ACK)=(?:"([^"]*)"|'([^']*)'|(\S+))""")


# ---- per-worker disk budget (mxp-gs12 review, 2026-10-18) -----------------------
#
# The Mac's data volume filled on 2026-10-09 and 2026-10-14 from one worker's
# subagent Rust targets (34 GB and 41 GB), and its scratch reached 266 GB. A
# worker declared AMUX_WORKER_DISK_GB in its resource profile, but nothing
# measured it. Now the worker's scratch (~/.amux/tmp/<worker>, plus any paths
# in AMUX_WORKER_DISK_PATHS, colon-separated) is measured with `du` in a
# DETACHED process (a 69 GB tree takes tens of seconds, and this hook runs
# before every Bash call), cached in ~/.amux/state/disk-usage/<worker>.json for
# AMUX_WORKER_DISK_TTL_S (default 600), and checked against the budget before
# heavy work: past disk_warn_fraction of it the command runs with a warning,
# past the budget heavy work is refused until the worker frees space, raises
# its budget in the Scope tab, or reruns with AMUX_DISK_ACK="<why>" (recorded).
# The budget is the scoped AMUX_WORKER_DISK_GB, else the registry's
# disk_budget_gb_default; 0 turns the check off. /api/resources reads the same
# cache file, so the number a refusal quotes is the number the worker sees.

def disk_paths(session: str) -> list[str]:
    paths = [os.path.join(amux_home(), "tmp", session)]
    for p in (os.environ.get("AMUX_WORKER_DISK_PATHS") or "").split(":"):
        p = os.path.expanduser(p.strip())
        if p and p not in paths:
            paths.append(p)
    return [p for p in paths if os.path.exists(p)]


def disk_cache_path(session: str) -> str:
    return os.path.join(amux_home(), "state", "disk-usage", session + ".json")


def measure_disk(session: str, timeout_s: float) -> dict:
    """du -sk every path, bounded. Writes and returns the cache record."""
    import subprocess

    paths = disk_paths(session)
    rec: dict = {"worker": session, "ts": round(time.time(), 3), "paths": {}, "n_considered": len(paths)}
    total, measured, why = 0, True, None
    deadline = time.time() + timeout_s
    for p in paths:
        left = deadline - time.time()
        if left <= 0:
            measured, why = False, f"du budget {timeout_s:.0f}s spent before {p}"
            break
        try:
            out = subprocess.run(["du", "-sk", p], capture_output=True, text=True, timeout=left)
            kb = int((out.stdout.split() or ["0"])[0])
        except subprocess.TimeoutExpired:
            measured, why = False, f"du timed out on {p} after {timeout_s:.0f}s"
            break
        except (OSError, ValueError) as e:
            measured, why = False, f"du failed on {p}: {e}"
            break
        rec["paths"][p] = kb * 1024
        total += kb * 1024
    rec.update({"bytes": total if measured else None, "measured": measured, "why_unmeasured": why})
    try:
        cp = disk_cache_path(session)
        os.makedirs(os.path.dirname(cp), exist_ok=True)
        tmp = cp + f".{os.getpid()}"
        with open(tmp, "w") as fh:
            json.dump(rec, fh)
        os.replace(tmp, cp)
    except OSError:
        pass
    return rec


def disk_usage(session: str, ttl_s: float) -> dict:
    """The cached reading, refreshed in the background when stale."""
    fixed = os.environ.get("AMUX_DISK_USAGE_SAMPLE")  # tests: a fixed reading
    if fixed:
        return json.loads(fixed)
    cp = disk_cache_path(session)
    rec: dict = {}
    try:
        with open(cp) as fh:
            rec = json.load(fh)
    except (OSError, ValueError):
        rec = {}
    age = time.time() - float(rec.get("ts") or 0)
    if age > ttl_s and not disk_paths(session):
        # Nothing to measure (no scratch yet): a zero reading costs nothing.
        return {"measured": True, "bytes": 0, "paths": {}, "n_considered": 0}
    if age > ttl_s:
        lock = cp + ".measuring"
        try:
            fresh_lock = os.path.exists(lock) and time.time() - os.stat(lock).st_mtime < 300
        except OSError:
            fresh_lock = False
        if not fresh_lock:
            import subprocess

            try:
                os.makedirs(os.path.dirname(cp), exist_ok=True)
                open(lock, "w").close()
                subprocess.Popen([sys.executable, os.path.abspath(__file__), "--measure-disk", session],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True, env=dict(os.environ))
            except OSError:
                pass
        if not rec:
            return {"measured": False, "why_unmeasured": "first measurement started in the background", "n_considered": 0}
        rec = dict(rec, stale_s=round(age))
    return rec


def disk_budget_bytes(cfg: dict) -> float | None:
    raw = os.environ.get("AMUX_WORKER_DISK_GB")
    try:
        gb = float(raw) if raw not in (None, "") else float(cfg.get("disk_budget_gb_default", 0) or 0)
    except ValueError:
        return None
    return gb * 1024 ** 3 if gb > 0 else None


def check_disk(b: dict, command: str, session: str) -> tuple[int, str]:
    cfg = b.get("resources") or {}
    budget = disk_budget_bytes(cfg)
    if budget is None or not session:
        return 0, ""
    ttl = float(os.environ.get("AMUX_WORKER_DISK_TTL_S") or cfg.get("disk_ttl_s", 600))
    rec = disk_usage(session, ttl)
    base = {"boundary": b.get("id"), "tool": "Bash", "command": redact(command), "budget_gb": round(budget / 1024 ** 3, 2),
            "measured": bool(rec.get("measured")), "n_considered": rec.get("n_considered", 0)}
    if not rec.get("measured"):
        log({**base, "verdict": "disk_budget_unmeasured", "why_unmeasured": rec.get("why_unmeasured")})
        return 0, ""
    used = float(rec.get("bytes") or 0)
    gb = used / 1024 ** 3
    frac = used / budget
    base.update({"used_gb": round(gb, 2), "used_bytes": int(used), "budget_bytes": int(budget), "fraction": round(frac, 3), "stale_s": rec.get("stale_s", 0)})
    paths = ", ".join(f"{p} {v / 1024 ** 3:.1f} GB" for p, v in sorted((rec.get("paths") or {}).items(), key=lambda x: -x[1])[:4])
    if frac >= 1.0:
        ack = DISK_ACK_RE.search(command)
        if ack:
            why = next(g for g in ack.groups()[1:] if g is not None)
            log({**base, "verdict": "disk_budget_acknowledged", "ack": redact(why)})
            return 0, ""
        log({**base, "verdict": "disk_budget_refused"})
        return 2, (
            f"BLOCKED by amux boundary '{b.get('id')}': your scratch uses {gb:.1f} GB, over your disk budget of "
            f"{budget / 1024 ** 3:.0f} GB (AMUX_WORKER_DISK_GB), so new heavy work waits until you free space.\n"
            f"Measured: {paths or 'no paths'} ({rec.get('stale_s', 0)}s ago).\n"
            "Delete build targets and scratch you no longer need (finished subagents' cargo targets are the usual cause; "
            "share one CARGO_TARGET_DIR across your subagents), or raise AMUX_WORKER_DISK_GB in the Scope tab. "
            "If the work truly cannot wait, rerun as\n"
            f"  AMUX_DISK_ACK=\"<why>\" {command[:120]}\n"
            "and the reason is recorded.\n(boundary-guard)\n"
        )
    if frac >= float(cfg.get("disk_warn_fraction", 0.8)):
        log({**base, "verdict": "disk_budget_warn"})
        return 0, (f"amux boundary '{b.get('id')}': WARNING your scratch uses {gb:.1f} GB, {frac:.0%} of your "
                   f"{budget / 1024 ** 3:.0f} GB disk budget ({paths}). Heavy work is refused past 100%; free space now.\n")
    return 0, ""


def check_resources(b: dict, command: str, session: str) -> tuple[int, str]:
    cfg = b.get("resources") or {}
    if not any(search(p, command) for p in cfg.get("heavy") or []):
        return 0, ""
    ack = ACK_RE.search(command)
    sample = host_sample()
    level, reasons = classify(sample, cfg)
    base = {"boundary": b.get("id"), "tool": "Bash", "level": level, "reasons": reasons,
            "command": redact(command), "measured": level != "unmeasured", "n_considered": 1}
    if level == "critical" and ack:
        why = next(g for g in ack.groups()[1:] if g is not None)
        log({**base, "verdict": "resources_acknowledged", "ack": redact(why)})
        return _disk_after(b, command, session)
    if level == "critical":
        log({**base, "verdict": "resources_refused"})
        usage = my_usage(session)
        return 2, (
            f"BLOCKED by amux boundary '{b.get('id')}': the machine is critical ({'; '.join(reasons)}), "
            "so heavy commands wait until it recovers.\n"
            + (usage + "\n" if usage else "")
            + "Run `amux resources` to see who is using what. Wait and retry, lower parallelism, "
            "or move the work to remote hardware. If it truly cannot wait, rerun as\n"
            f"  AMUX_RESOURCES_ACK=\"<why>\" {command[:120]}\n"
            "and the reason is recorded.\n(boundary-guard)\n"
        )
    if level == "strained":
        log({**base, "verdict": "resources_strained_allowed"})
    if level == "unmeasured":
        log({**base, "verdict": "resources_unmeasured", "why_unmeasured": "no load, swap or memory reading"})
    return _disk_after(b, command, session)


def _disk_after(b: dict, command: str, session: str) -> tuple[int, str]:
    """The disk budget, checked for every heavy command the host check let through."""
    code, msg = check_disk(b, command, session)
    if code:
        return code, msg
    if msg:
        sys.stderr.write(msg)
    return 0, ""


# ---- subagent cap (mxp-gs12 review, 2026-10-18) ---------------------------------
#
# Usage limits stopped every subagent of one large isolated worker five times in
# four days: it fanned out up to 21 Claude Code agents on one shared plan, and
# nothing compared the fan-out with the account's remaining capacity. This
# refuses a NEW Agent/Task tool call when the worker already has
# `max_running` subagents running, or when the account's usage headroom is
# below `min_headroom_pct`. Refusal only: it injects no prompt, routes nothing,
# and the model decides what to do with the slot it has.
#
# The running count is read from Claude Code's own files, so it needs no amux
# reporting (an isolated worker sends none): every subagent has
# <conversation>/subagents/agent-<id>.jsonl beside agent-<id>.meta.json, and
# the meta gains "finished": true when the agent stops. Running = no finished
# flag and a transcript written within `stale_s` (a killed agent never gets the
# flag). A transcript with no meta file counts as running only while it is
# warm (`no_meta_warm_s`). No transcript path in the payload means the count
# is unmeasured, and an unmeasured count ALLOWS the call and says so in the log.
#
# Headroom is 100 minus the highest account-wide window percent in amux's
# GET /api/usage (the session and weekly windows). Unreachable or unmeasured
# also allows. The max is AMUX_MAX_SUBAGENTS in the worker's env when set
# (0 turns the cap off), else the registry's `max_running_default`.

def endpoint_url() -> str:
    url = os.environ.get("AMUX_URL") or ""
    if url:
        return url.rstrip("/")
    try:
        with open(os.path.join(amux_home(), "endpoint.json")) as fh:
            return str(json.load(fh).get("canonical_url") or "").rstrip("/")
    except (OSError, ValueError):
        return ""


def host_process_start() -> float | None:
    """Start time of the Claude Code process this hook runs under, or None.

    Subagents run inside that process, so a transcript last written before it
    started belongs to an agent the restart ended. Without this, a fleet
    restart (2026-10-19 16:02Z) left mxp-gs12 refused at "9 subagents running"
    with none alive until each transcript aged past `stale_s`.
    """
    fixed = os.environ.get("AMUX_SUBAGENT_HOST_START")  # tests
    if fixed:
        try:
            return float(fixed)
        except ValueError:
            return None
    import subprocess

    pid = os.getppid()
    for _ in range(8):
        if pid <= 1:
            return None
        try:
            out = subprocess.run(["ps", "-o", "ppid=,lstart=,command=", "-p", str(pid)],
                                 capture_output=True, text=True, timeout=2).stdout.strip()
        except Exception:  # noqa: BLE001 - unmeasured, the caller keeps the old rule
            return None
        if not out:
            return None
        parts = out.split(None, 6)
        if len(parts) < 7:
            return None
        ppid, lstart, cmd = parts[0], " ".join(parts[1:6]), parts[6]
        if os.path.basename(cmd.split()[0]) == "claude" or re.match(r"^\d+\.\d+\.\d+$", os.path.basename(cmd.split()[0])):
            try:
                return time.mktime(time.strptime(lstart, "%a %b %d %H:%M:%S %Y"))
            except ValueError:
                return None
        try:
            pid = int(ppid)
        except ValueError:
            return None
    return None


def ended_by_stop(path: str) -> bool:
    """True when the transcript's last entry is the interrupt a stop writes.

    TaskStop never sets "finished" in the meta, so a stopped agent read as
    running until its transcript aged past `stale_s` (30 min). Measured
    2026-10-22: mxp-gs12 stopped a subagent at 23:25:59Z to free a slot for
    critical-path work and the cap still refused the new one. The stop does
    write a final user entry "[Request interrupted by user]"; a resumed agent
    appends after it, so only the last entry is read.
    """
    try:
        with open(path, "rb") as fh:
            fh.seek(0, os.SEEK_END)
            size = fh.tell()
            fh.seek(max(0, size - 8192))
            tail = fh.read().decode("utf-8", "replace")
    except OSError:
        return False
    lines = [ln for ln in tail.splitlines() if ln.strip()]
    if not lines:
        return False
    try:
        last = json.loads(lines[-1])
    except ValueError:
        return False
    msg = last.get("message") or {}
    if last.get("type") != "user" or msg.get("role") != "user":
        return False
    content = msg.get("content")
    texts = [content] if isinstance(content, str) else [
        c.get("text", "") for c in (content or []) if isinstance(c, dict) and c.get("type") == "text"]
    return any(t.startswith("[Request interrupted by user") for t in texts)


def count_running_subagents(transcript_path: str, cfg: dict, now: float | None = None,
                            host_start: float | None = None) -> dict:
    """Running subagents of the conversation behind `transcript_path`."""
    now = time.time() if now is None else now
    if not transcript_path:
        return {"measured": False, "why_unmeasured": "hook payload carried no transcript_path", "n_considered": 0}
    tp = os.path.expanduser(transcript_path)
    parent = os.path.dirname(tp)
    if os.path.basename(parent) == "subagents":
        subs = parent  # a subagent's own call: its siblings are the worker's subagents
    else:
        if not os.path.exists(tp):
            return {"measured": False, "why_unmeasured": f"transcript {tp} does not exist", "n_considered": 0}
        subs = tp[:-len(".jsonl")] + "/subagents" if tp.endswith(".jsonl") else tp + "/subagents"
    stale_s = float(cfg.get("stale_s", 1800))
    warm_s = float(cfg.get("no_meta_warm_s", 240))
    try:
        names = [n for n in os.listdir(subs) if n.endswith(".jsonl")]
    except FileNotFoundError:
        return {"measured": True, "running": 0, "n_considered": 0, "running_ids": []}
    except OSError as e:
        return {"measured": False, "why_unmeasured": f"cannot list {subs}: {e}", "n_considered": 0}
    running: list[str] = []
    ended_by_restart = 0
    ended_by_stop_n = 0
    for n in names:
        stem = n[:-len(".jsonl")]
        try:
            mtime = os.stat(os.path.join(subs, n)).st_mtime
        except OSError:
            continue
        age = now - mtime
        try:
            with open(os.path.join(subs, stem + ".meta.json")) as fh:
                meta = json.load(fh)
        except (OSError, ValueError):
            meta = None
        if meta is None:
            live = age < warm_s
        else:
            live = not meta.get("finished") and age < stale_s
        if live and host_start is not None and mtime < host_start - 2:
            ended_by_restart += 1  # last written before this process started: the restart ended it
            continue
        if live and ended_by_stop(os.path.join(subs, n)):
            ended_by_stop_n += 1  # stopped: TaskStop writes the interrupt and never sets "finished"
            continue
        if live:
            running.append(stem)
    return {"measured": True, "running": len(running), "n_considered": len(names), "running_ids": sorted(running)[:12],
            "ended_by_restart": ended_by_restart, "ended_by_stop": ended_by_stop_n,
            "host_start_measured": host_start is not None}


def usage_headroom() -> dict:
    """Account headroom from amux's /api/usage, best effort (2s)."""
    fixed = os.environ.get("AMUX_USAGE_SAMPLE")  # tests: a fixed /api/usage body
    if fixed:
        body = json.loads(fixed)
    else:
        url = endpoint_url()
        if not url:
            return {"measured": False, "why_unmeasured": "no AMUX_URL and no endpoint.json"}
        import ssl
        import urllib.request

        try:
            ctx = ssl.create_default_context()
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
            with urllib.request.urlopen(f"{url}/api/usage", timeout=2, context=ctx) as r:
                body = json.load(r)
        except Exception as e:  # noqa: BLE001 - fail open, say why
            return {"measured": False, "why_unmeasured": f"GET /api/usage failed: {str(e)[:120]}"}
    if not body.get("measured") or body.get("available") is False:
        return {"measured": False, "why_unmeasured": "amux /api/usage has no measured reading"}
    windows = [w for w in body.get("limits") or [] if not w.get("scope") and isinstance(w.get("percent"), (int, float))]
    if not windows:
        return {"measured": False, "why_unmeasured": "amux /api/usage listed no account-wide window"}
    top = max(windows, key=lambda w: w["percent"])
    return {"measured": True, "headroom_pct": round(100 - float(top["percent"]), 1),
            "window": top.get("kind") or top.get("group") or "", "resets_at": top.get("resets_at") or ""}


def subagent_max(cfg: dict) -> int | None:
    raw = os.environ.get(cfg.get("env_max") or "AMUX_MAX_SUBAGENTS")
    try:
        n = int(raw) if raw not in (None, "") else int(cfg.get("max_running_default", 4))
    except ValueError:
        n = int(cfg.get("max_running_default", 4))
    return n if n > 0 else None


def check_subagents(b: dict, tool: str, transcript_path: str) -> tuple[int, str]:
    cfg = b.get("subagents") or {}
    if tool not in (cfg.get("tools") or ["Agent", "Task"]):
        return 0, ""
    limit = subagent_max(cfg)
    floor = float(cfg.get("min_headroom_pct", 0) or 0)
    if limit is None and floor <= 0:
        return 0, ""
    count = (count_running_subagents(transcript_path, cfg, host_start=host_process_start()) if limit is not None
             else {"measured": False, "why_unmeasured": "cap off"})
    head = usage_headroom() if floor > 0 else {"measured": False, "why_unmeasured": "headroom floor off"}
    base = {"boundary": b.get("id"), "tool": tool, "limit": limit, "min_headroom_pct": floor,
            "running": count.get("running"), "running_ids": count.get("running_ids"),
            "ended_by_restart": count.get("ended_by_restart"), "ended_by_stop": count.get("ended_by_stop"),
            "host_start_measured": count.get("host_start_measured"),
            "measured": bool(count.get("measured")), "n_considered": count.get("n_considered", 0),
            "headroom_pct": head.get("headroom_pct"), "headroom_measured": bool(head.get("measured")),
            "window": head.get("window"), "resets_at": head.get("resets_at")}
    if limit is not None and count.get("measured") and count["running"] >= limit:
        log({**base, "level": "WARN", "verdict": "subagent_cap_refused", "cause": "running_cap"})
        return 2, (
            f"BLOCKED by amux boundary '{b.get('id')}': you already have {count['running']} subagents running "
            f"and the limit is {limit} (AMUX_MAX_SUBAGENTS), so this new agent was not started.\n"
            "Retry when one of them finishes (a background agent notifies you when it stops), or do this step "
            "yourself. Fanning out past the account's capacity is how a usage limit stops every agent at once.\n"
            "(boundary-guard; the registry is boundaries.toml)\n"
        )
    if floor > 0 and head.get("measured") and head["headroom_pct"] < floor:
        log({**base, "level": "WARN", "verdict": "subagent_cap_refused", "cause": "usage_headroom"})
        running = f"{count['running']} running, limit {limit}" if count.get("measured") else "running count unmeasured"
        return 2, (
            f"BLOCKED by amux boundary '{b.get('id')}': the account has {head['headroom_pct']:.0f}% usage left in its "
            f"{head.get('window') or 'usage'} window, under the {floor:.0f}% floor for starting new subagents "
            f"({running}).\n"
            f"Retry after the window resets ({head.get('resets_at') or 'see `amux usage`'}), finish the work with the "
            "agents you have, or do this step yourself.\n(boundary-guard; the registry is boundaries.toml)\n"
        )
    unmeasured = [k for k, r, on in (("count", count, limit is not None), ("headroom", head, floor > 0))
                  if on and not r.get("measured")]
    if unmeasured:
        log({**base, "level": "INFO", "verdict": "subagent_cap_allowed_unmeasured", "unmeasured": unmeasured,
             "why_unmeasured": "; ".join(str(r.get("why_unmeasured")) for k, r in (("count", count), ("headroom", head)) if k in unmeasured)})
    return 0, ""


def main() -> None:
    if len(sys.argv) == 3 and sys.argv[1] == "--measure-disk":
        # The detached measurement disk_usage() starts. Bounded so a wedged
        # filesystem cannot leave it running; the lock is removed either way.
        try:
            measure_disk(sys.argv[2], float(os.environ.get("AMUX_DISK_DU_TIMEOUT_S") or 240))
        finally:
            try:
                os.unlink(disk_cache_path(sys.argv[2]) + ".measuring")
            except OSError:
                pass
        sys.exit(0)
    try:
        payload = json.load(sys.stdin)
    except Exception:
        sys.exit(0)
    # An isolated worker's launch unsets AMUX_SESSION (its routing key) and
    # exports AMUX_ISOLATED_WORKER as identity only. The guard reads that name
    # so apply_to_isolated actually covers isolated workers; before 2026-10-18
    # it exited here for every one of them.
    session = os.environ.get("AMUX_SESSION", "")
    isolated_env = os.environ.get("AMUX_ISOLATED_WORKER", "") if not session else ""
    session = session or isolated_env
    global WORKER, WORKER_ISOLATED
    WORKER, WORKER_ISOLATED = session, bool(isolated_env)
    if not session and os.environ.get("AMUX_BOUNDARIES_FORCE") != "1":
        sys.exit(0)  # not an amux worker
    tool = payload.get("tool_name") or ""
    ti = payload.get("tool_input") or {}
    command = (ti.get("command") or "") if tool == "Bash" else ""
    path = ti.get("file_path") or ti.get("notebook_path") or ti.get("path") or ""
    if tool in ("Grep", "Glob"):
        # A directory search: match the directory as a directory, plus the
        # Glob pattern or Grep glob, so ~/.amux/vault and ~/.amux/vault/* both hit.
        base = os.path.expanduser(ti.get("path") or "")
        pat = os.path.expanduser((ti.get("pattern") if tool == "Glob" else ti.get("glob")) or "")
        joined = pat if pat.startswith("/") else (base.rstrip("/") + "/" + pat if base else pat)
        path = (base.rstrip("/") + "/" if base else "") + "\n" + joined
    content = "\n".join(str(ti.get(k) or "") for k in ("content", "new_string", "new_source"))
    if tool == "MultiEdit":
        content += "\n".join(str(e.get("new_string") or "") for e in ti.get("edits") or [])
    try:
        boundaries, settings = load_registry()
    except Exception as e:
        log({"verdict": "boundary_registry_unloadable", "error": str(e)[:300], "measured": False, "n_considered": 0})
        sys.stderr.write(f"amux boundary-guard: registry did not load ({e}); boundaries are NOT enforced.\n")
        sys.exit(0)
    if session and not settings.get("apply_to_isolated", True) and (isolated_env or is_isolated(session)):
        sys.exit(0)  # the owner exempted isolated workers in the registry
    for b in boundaries:
        for i, rule in enumerate(b.get("deny") or []):
            try:
                hit = rule_applies(rule, tool, command, path, content)
            except re.error as e:
                log({"verdict": "boundary_rule_invalid", "boundary": b.get("id"), "rule": i, "error": str(e)})
                continue
            if hit:
                log({"verdict": "boundary_refused", "boundary": b.get("id"), "rule": i, "tool": tool,
                     "subject": redact(command or path), "measured": True, "n_considered": 1})
                sys.stderr.write(
                    f"BLOCKED by amux boundary '{b.get('id')}' ({b.get('title', '')}).\n"
                    f"{rule.get('reason', '').strip()}\n\n{b.get('rule', '').strip()}\n"
                    "(boundary-guard; the registry is boundaries.toml, GET $AMUX_URL/api/boundaries)\n"
                )
                sys.exit(2)
        if b.get("kind") == "resources" and tool == "Bash" and command:
            code, msg = check_resources(b, command, session)
            if code:
                sys.stderr.write(msg)
                sys.exit(code)
        if b.get("kind") == "subagents":
            code, msg = check_subagents(b, tool, str(payload.get("transcript_path") or ""))
            if code:
                sys.stderr.write(msg)
                sys.exit(code)
    sys.exit(0)


if __name__ == "__main__":
    main()
