"""Gather per-process metrics.

Sources:
  * psutil      -> cpu, memory, threads, status, uptime, owner, cmdline
  * nettop      -> per-process network bytes in/out (opt-in; ~1-4s)
  * codesign    -> author / signing authority (opt-in; cached by exe path)

Per-process *disk* I/O is intentionally omitted: macOS does not expose it
without sudo + dtrace (fs_usage/iotop). We surface system-wide disk stats
instead and say so, rather than print a fake column.
"""
from __future__ import annotations

import functools
import getpass
import os
import subprocess
import time
from dataclasses import dataclass, field
from typing import Optional

import psutil

from .safety import Safety, classify

CURRENT_USER = getpass.getuser()


@dataclass
class Proc:
    pid: int
    ppid: int
    name: str
    username: str
    status: str
    cpu: float
    mem_rss: int
    mem_percent: float
    threads: int
    create_time: float
    uptime: float
    exe: str
    cmdline: str
    net_in: int = 0
    net_out: int = 0
    author: str = ""
    safety: Optional[Safety] = None


def _parse_cputime(s):
    """Parse ps `time` field ([DD-][HH:]MM:SS[.ss]) into seconds."""
    s = s.strip()
    days = 0
    if "-" in s:
        d, s = s.split("-", 1)
        days = int(d)
    try:
        parts = [float(x) for x in s.split(":")]
    except ValueError:
        return 0.0
    while len(parts) < 3:
        parts.insert(0, 0.0)
    h, m, sec = parts[-3], parts[-2], parts[-1]
    return days * 86400 + h * 3600 + m * 60 + sec


def _ps_snapshot():
    """{pid: (rss_bytes, cpu_seconds)} straight from the kernel via ps.

    psutil's per-process cpu_times (and memory for root procs) is unreliable on
    recent macOS, so we read both from ps: CPU as cumulative-time deltas (what
    Activity Monitor does) and RSS which is available for every process.
    """
    out = {}
    try:
        r = subprocess.run(["ps", "-A", "-o", "pid=,rss=,time="],
                           capture_output=True, text=True, timeout=8)
    except Exception:
        return out
    for line in r.stdout.split("\n"):
        a = line.split(None, 2)
        if len(a) != 3:
            continue
        try:
            out[int(a[0])] = (int(a[1]) * 1024, _parse_cputime(a[2]))
        except ValueError:
            continue
    return out


def _cputimes(snap):
    return {pid: v[1] for pid, v in snap.items()}


def _cpu_percent_map(c1, c2, wall):
    if wall <= 0:
        return {}
    return {pid: max(0.0, (c2[pid] - c1.get(pid, c2[pid])) / wall * 100.0)
            for pid in c2}


def net_by_pid(timeout=6.0):
    """Map pid -> (bytes_in, bytes_out) via nettop. Empty dict on failure."""
    out = {}
    try:
        r = subprocess.run(
            ["nettop", "-P", "-L", "1", "-x", "-J", "bytes_in,bytes_out", "-n"],
            capture_output=True, text=True, timeout=timeout,
        )
    except Exception:
        return out
    for line in r.stdout.splitlines():
        parts = line.split(",")
        if len(parts) < 3 or "." not in parts[0]:
            continue
        _name, _, pid = parts[0].rpartition(".")
        if not pid.isdigit():
            continue
        try:
            out[int(pid)] = (int(parts[1]), int(parts[2]))
        except ValueError:
            continue
    return out


@functools.lru_cache(maxsize=4096)
def author_for(exe):
    """Best-effort code-sign author for an executable path. Cached per path."""
    if not exe:
        return "unknown"
    try:
        r = subprocess.run(["codesign", "-dvvv", exe],
                           capture_output=True, text=True, timeout=4)
        txt = (r.stderr or "") + (r.stdout or "")
    except Exception:
        return "unknown"
    authority = ""
    for line in txt.splitlines():
        if line.startswith("Authority="):
            authority = line.split("=", 1)[1].strip()
            break
    if not authority:
        return "unsigned"
    if authority in ("Software Signing", "Apple Mac OS Application Signing",
                     "Apple iPhone OS Application Signing"):
        return "Apple"
    # e.g. "Developer ID Application: Google LLC (EQHXZ8M8AV)"
    if ":" in authority:
        return authority.split(":", 1)[1].strip()
    return authority


def _safe(fn, default):
    """Call a psutil getter, tolerating denied/partial access.

    Root-owned system processes deny several fields; we still want to LIST and
    CLASSIFY them (so they show up as HIGH/FATAL), not drop them silently.
    """
    try:
        return fn()
    except (psutil.AccessDenied, psutil.NoSuchProcess, psutil.ZombieProcess,
            OSError):
        return default


_TOTAL_MEM = psutil.virtual_memory().total or 1


def _build_proc(p, now, net, cpu_map, rss_map):
    """Construct a Proc from a psutil.Process, field-by-field tolerant.
    Returns None only if the process no longer exists at all."""
    try:
        name = p.name()
    except psutil.NoSuchProcess:
        return None
    except psutil.AccessDenied:
        name = f"pid{p.pid}"
    mem = _safe(p.memory_info, None)
    # psutil memory is denied for root procs; fall back to ps RSS.
    rss = mem.rss if mem else rss_map.get(p.pid, 0)
    mem_pct = _safe(p.memory_percent, 0.0) or (rss / _TOTAL_MEM * 100.0)
    ctime = _safe(p.create_time, now)
    ni, no = net.get(p.pid, (0, 0))
    proc = Proc(
        pid=p.pid,
        ppid=_safe(p.ppid, 0),
        name=name,
        username=_safe(p.username, "?"),
        status=_safe(p.status, "?"),
        cpu=cpu_map.get(p.pid, 0.0),
        mem_rss=rss,
        mem_percent=mem_pct,
        threads=_safe(p.num_threads, 0),
        create_time=ctime,
        uptime=max(0.0, now - ctime),
        exe=_safe(p.exe, ""),
        cmdline=" ".join(_safe(p.cmdline, [])) or name,
        net_in=ni,
        net_out=no,
    )
    proc.safety = classify(proc.pid, proc.ppid, proc.name, proc.exe,
                           proc.username, proc.status, CURRENT_USER)
    return proc


def collect(sample_interval=0.4, want_net=False, want_author=False,
            mine_only=False):
    """Return a list[Proc]. CPU is measured over `sample_interval` seconds."""
    # Measure CPU from ps cumulative-time deltas across the interval; RSS too.
    s1 = _ps_snapshot()
    w1 = time.time()
    time.sleep(sample_interval)
    s2 = _ps_snapshot()
    cpu_map = _cpu_percent_map(_cputimes(s1), _cputimes(s2), time.time() - w1)
    rss_map = {pid: v[0] for pid, v in s2.items()}

    net = net_by_pid() if want_net else {}
    now = time.time()
    procs = []
    for p in psutil.process_iter():
        proc = _build_proc(p, now, net, cpu_map, rss_map)
        if proc is None:
            continue
        if mine_only and proc.username != CURRENT_USER:
            continue
        if want_author:
            proc.author = author_for(proc.exe)
        procs.append(proc)
    return procs


def get_one(pid, want_net=True, want_author=True):
    """Detailed snapshot of a single pid, or None if it's gone."""
    try:
        p = psutil.Process(pid)
    except psutil.NoSuchProcess:
        return None
    # cpu over a short window via ps cumulative-time delta
    s1 = _ps_snapshot()
    w1 = time.time()
    time.sleep(0.4)
    s2 = _ps_snapshot()
    cpu_map = _cpu_percent_map(_cputimes(s1), _cputimes(s2), time.time() - w1)
    rss_map = {pid: v[0] for pid, v in s2.items()}
    net = net_by_pid() if want_net else {}
    proc = _build_proc(p, time.time(), net, cpu_map, rss_map)
    if proc is None:
        return None
    if want_author:
        proc.author = author_for(proc.exe)
    return proc


def system_overview():
    vm = psutil.virtual_memory()
    du = psutil.disk_usage("/")
    return {
        "cpu_percent": psutil.cpu_percent(interval=0.3),
        "cpu_count": psutil.cpu_count(),
        "load": os.getloadavg(),
        "mem_total": vm.total,
        "mem_used": vm.used,
        "mem_available": vm.available,
        "mem_percent": vm.percent,
        "disk_total": du.total,
        "disk_used": du.used,
        "disk_free": du.free,
        "disk_percent": du.percent,
    }


SORT_KEYS = {
    "cpu": lambda p: p.cpu,
    "mem": lambda p: p.mem_rss,
    "net": lambda p: p.net_in + p.net_out,
    "time": lambda p: -p.uptime,   # longest-running first
    "pid": lambda p: -p.pid,
}
