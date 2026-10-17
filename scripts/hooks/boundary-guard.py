#!/usr/bin/env python3
"""PreToolUse guard: enforce the amux worker boundaries in boundaries.toml.

One hook, one registry. The rules, their wording and their examples live in
boundaries.toml (repo root; installed beside this file as
~/.amux/hooks/boundaries.toml), with optional owner overrides in
~/.amux/boundaries.toml. This script holds no rule of its own: it loads the
registry, refuses the tool calls a `deny` rule describes, and refuses heavy
commands while the machine is critical (the `resources` boundary).

Who it applies to: amux workers (AMUX_SESSION set), isolated ones included
while the registry's top-level `apply_to_isolated` is true (the default; it
is the named exception in CLAUDE.md "Isolated workers").

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

FILE_TOOLS = {"Read", "Edit", "Write", "MultiEdit", "NotebookEdit"}
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


def log(event: dict) -> None:
    try:
        d = os.path.join(amux_home(), "logs")
        os.makedirs(d, exist_ok=True)
        event = {"ts": round(time.time(), 3), "session": os.environ.get("AMUX_SESSION", ""), **event}
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
        return 0, ""
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
    return 0, ""


def main() -> None:
    try:
        payload = json.load(sys.stdin)
    except Exception:
        sys.exit(0)
    session = os.environ.get("AMUX_SESSION", "")
    if not session and os.environ.get("AMUX_BOUNDARIES_FORCE") != "1":
        sys.exit(0)  # not an amux worker
    tool = payload.get("tool_name") or ""
    ti = payload.get("tool_input") or {}
    command = (ti.get("command") or "") if tool == "Bash" else ""
    path = ti.get("file_path") or ti.get("notebook_path") or ti.get("path") or ""
    content = "\n".join(str(ti.get(k) or "") for k in ("content", "new_string", "new_source"))
    if tool == "MultiEdit":
        content += "\n".join(str(e.get("new_string") or "") for e in ti.get("edits") or [])
    try:
        boundaries, settings = load_registry()
    except Exception as e:
        log({"verdict": "boundary_registry_unloadable", "error": str(e)[:300], "measured": False, "n_considered": 0})
        sys.stderr.write(f"amux boundary-guard: registry did not load ({e}); boundaries are NOT enforced.\n")
        sys.exit(0)
    if session and not settings.get("apply_to_isolated", True) and is_isolated(session):
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
    sys.exit(0)


if __name__ == "__main__":
    main()
