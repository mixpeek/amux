#!/usr/bin/env bash
# host-analysis.sh — full host-machine analysis, cross-platform (macOS + Linux).
#
# Re-runnable: emits ONE JSON object to stdout describing the host the amux
# server runs on — OS, CPU/load, memory, swap, disk (measured on the DATA
# volume, not the sealed system snapshot), uptime, the top processes by CPU and
# RSS, and fleet-relevant process counts. Fast by design (<1s): no du/find
# scans, so it is safe to call on every dashboard refresh.
#
# Two consumers, one source of truth:
#   - GET /api/metrics/host embeds these exact bytes (include_str!) and runs
#     them, so the Metrics tab renders precisely this output.
#   - Standalone: a quick host read from any checkout.
#
#   scripts/host-analysis.sh            # JSON (what the server serves)
#   scripts/host-analysis.sh --pretty   # human-readable summary
#
# Best-effort by nature: any single probe that a platform lacks degrades to a
# null/empty field rather than aborting the run (ethos rule 4 — report what did
# not measure, do not lie or crash). Hence NOT `set -e`.
set -uo pipefail

# launchd (macOS) and systemd (Linux) hand the server a minimal PATH; resolve
# the tools we need regardless of who invokes us. Same reason metrics.rs pins
# absolute paths in resolve_bin().
export PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"

STARTED_MS="$(python3 -c 'import time;print(int(time.time()*1000))' 2>/dev/null || echo 0)"
PRETTY=""
[ "${1:-}" = "--pretty" ] || [ "${1:-}" = "--human" ] && PRETTY=1

OS="$(uname -s 2>/dev/null || echo unknown)"
ARCH="$(uname -m 2>/dev/null || echo unknown)"
HOST="$(hostname 2>/dev/null || echo unknown)"

# Defaults (empty = unmeasured; python coerces).
OS_VERSION="" CPU_COUNT="" CPU_PHYSICAL="" LOAD1="" LOAD5="" LOAD15=""
RAM_TOTAL_MB="" RAM_USED_MB="" RAM_FREE_MB="" RAM_PERCENT=""
SWAP_TOTAL_MB="" SWAP_USED_MB="" UPTIME_SECONDS="" MEM_PRESSURE=""
DISK_PATH="${HOME:-/}" DISK_TOTAL_KB="" DISK_USED_KB="" DISK_AVAIL_KB=""
TOP_CPU="" TOP_MEM=""

if [ "$OS" = "Darwin" ]; then
  OS_VERSION="$(sw_vers -productVersion 2>/dev/null || uname -r 2>/dev/null || echo '')"
  CPU_COUNT="$(sysctl -n hw.logicalcpu 2>/dev/null || echo '')"
  CPU_PHYSICAL="$(sysctl -n hw.physicalcpu 2>/dev/null || echo '')"
  # vm.loadavg → "{ 1.23 4.56 7.89 }"
  read -r LOAD1 LOAD5 LOAD15 <<<"$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | awk '{print $1, $2, $3}')"
  # RAM: total from hw.memsize; free = (free + speculative) pages * pagesize.
  memsize="$(sysctl -n hw.memsize 2>/dev/null || echo 0)"
  pagesize="$(sysctl -n hw.pagesize 2>/dev/null || echo 16384)"
  if [ "$memsize" -gt 0 ] 2>/dev/null; then
    free_pages="$(vm_stat 2>/dev/null | awk -F: '/Pages free/{gsub(/[ .]/,"",$2);f=$2} /Pages speculative/{gsub(/[ .]/,"",$2);s=$2} END{print f+s+0}')"
    free_bytes=$(( free_pages * pagesize ))
    RAM_TOTAL_MB="$(awk -v b="$memsize" 'BEGIN{printf "%.1f", b/1048576}')"
    RAM_FREE_MB="$(awk -v b="$free_bytes" 'BEGIN{printf "%.1f", b/1048576}')"
    RAM_USED_MB="$(awk -v t="$memsize" -v f="$free_bytes" 'BEGIN{printf "%.1f", (t-f)/1048576}')"
    RAM_PERCENT="$(awk -v t="$memsize" -v f="$free_bytes" 'BEGIN{if(t>0)printf "%.1f",(t-f)/t*100; else print 0}')"
  fi
  # Swap: "total = 2048.00M  used = 512.00M  free = ..."
  swap="$(sysctl -n vm.swapusage 2>/dev/null || echo '')"
  SWAP_TOTAL_MB="$(echo "$swap" | sed -n 's/.*total = \([0-9.]*\)M.*/\1/p')"
  SWAP_USED_MB="$(echo "$swap" | sed -n 's/.*used = \([0-9.]*\)M.*/\1/p')"
  # Uptime from kern.boottime "{ sec = 1699..., usec = ... }"
  # kern.boottime → "{ sec = 1699..., usec = 0 } <date>". A greedy /sec = / also
  # matches "uSEC =", so take the field positionally: $4 is "<sec>,".
  boot_sec="$(sysctl -n kern.boottime 2>/dev/null | awk '{print $4}' | tr -d ',')"
  # BSD awk has no systime(); use date(1), which is portable.
  [ -n "$boot_sec" ] && UPTIME_SECONDS=$(( $(date +%s) - boot_sec ))
  # 1 normal, 2 warn, 4 critical
  MEM_PRESSURE="$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null || echo '')"
  TOP_CPU="$(ps -Ao pid,pcpu,rss,comm -r 2>/dev/null | sed -n '2,9p')"
  TOP_MEM="$(ps -Ao pid,pcpu,rss,comm -m 2>/dev/null | sed -n '2,9p')"
elif [ "$OS" = "Linux" ]; then
  OS_VERSION="$( ( . /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-}" ) || uname -r 2>/dev/null || echo '')"
  CPU_COUNT="$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo '')"
  CPU_PHYSICAL="$(awk -F: '/^physical id/{p[$2]=1} END{n=0;for(k in p)n++;print (n?n:"")}' /proc/cpuinfo 2>/dev/null || echo '')"
  read -r LOAD1 LOAD5 LOAD15 <<<"$(awk '{print $1, $2, $3}' /proc/loadavg 2>/dev/null)"
  # /proc/meminfo values are in KiB.
  mt="$(awk '/^MemTotal:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)"
  ma="$(awk '/^MemAvailable:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)"
  if [ "${mt:-0}" -gt 0 ] 2>/dev/null; then
    RAM_TOTAL_MB="$(awk -v k="$mt" 'BEGIN{printf "%.1f", k/1024}')"
    RAM_FREE_MB="$(awk -v k="$ma" 'BEGIN{printf "%.1f", k/1024}')"
    RAM_USED_MB="$(awk -v t="$mt" -v a="$ma" 'BEGIN{printf "%.1f", (t-a)/1024}')"
    RAM_PERCENT="$(awk -v t="$mt" -v a="$ma" 'BEGIN{if(t>0)printf "%.1f",(t-a)/t*100; else print 0}')"
  fi
  st="$(awk '/^SwapTotal:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)"
  sf="$(awk '/^SwapFree:/{print $2}' /proc/meminfo 2>/dev/null || echo 0)"
  SWAP_TOTAL_MB="$(awk -v k="$st" 'BEGIN{printf "%.1f", k/1024}')"
  SWAP_USED_MB="$(awk -v t="$st" -v f="$sf" 'BEGIN{printf "%.1f", (t-f)/1024}')"
  UPTIME_SECONDS="$(awk '{printf "%d",$1}' /proc/uptime 2>/dev/null || echo '')"
  TOP_CPU="$(ps -eo pid,pcpu,rss,comm --sort=-pcpu 2>/dev/null | sed -n '2,9p')"
  TOP_MEM="$(ps -eo pid,pcpu,rss,comm --sort=-rss 2>/dev/null | sed -n '2,9p')"
fi

# Disk on the DATA volume (statfs on $HOME) — POSIX -P forces a single-line
# record so the fields never wrap. -k forces 1024-byte blocks.
read -r DISK_TOTAL_KB DISK_USED_KB DISK_AVAIL_KB <<<"$(df -Pk "$DISK_PATH" 2>/dev/null | awk 'END{print $2, $3, $4}')"

# Fleet-relevant process counts. pgrep -x (exact name) works on both platforms;
# pgrep -c does not exist on macOS, so count lines.
count_proc() { pgrep -x "$1" 2>/dev/null | wc -l | tr -d ' '; }
# Total process population the top-N lists are ranked from (drives n_considered).
PROC_TOTAL="$(ps -A 2>/dev/null | wc -l | tr -d ' ')"
N_CLAUDE="$(count_proc claude)"
N_RUSTC="$(count_proc rustc)"
N_CARGO="$(count_proc cargo)"
N_NODE="$(count_proc node)"
N_PYTHON="$(count_proc python3)"

export OS ARCH HOST OS_VERSION CPU_COUNT CPU_PHYSICAL LOAD1 LOAD5 LOAD15 \
  RAM_TOTAL_MB RAM_USED_MB RAM_FREE_MB RAM_PERCENT SWAP_TOTAL_MB SWAP_USED_MB \
  UPTIME_SECONDS MEM_PRESSURE DISK_PATH DISK_TOTAL_KB DISK_USED_KB DISK_AVAIL_KB \
  TOP_CPU TOP_MEM PROC_TOTAL N_CLAUDE N_RUSTC N_CARGO N_NODE N_PYTHON STARTED_MS PRETTY

python3 - <<'PY'
import os, json, time

def num(k):
    v = os.environ.get(k, "").strip()
    if v == "":
        return None
    try:
        f = float(v)
        return int(f) if f.is_integer() else round(f, 2)
    except ValueError:
        return None

def procs(k):
    out = []
    for line in os.environ.get(k, "").splitlines():
        parts = line.split(None, 3)
        if len(parts) < 4:
            continue
        pid, cpu, rss, comm = parts
        try:
            out.append({
                "pid": int(pid),
                "cpu_percent": round(float(cpu), 1),
                "rss_mb": round(int(rss) / 1024, 1),
                "command": os.path.basename(comm),
            })
        except ValueError:
            continue
    return out

KIB = 1024
GIB = 1073741824
def gb(k):
    v = num(k)
    return round(v * KIB / GIB, 1) if v is not None else None

cpu_count = num("CPU_COUNT")
load = [num("LOAD1"), num("LOAD5"), num("LOAD15")]
load1 = load[0]
load_per_core = round(load1 / cpu_count, 2) if (load1 is not None and cpu_count) else None

disk_total = gb("DISK_TOTAL_KB")
disk_used = gb("DISK_USED_KB")
disk_free = gb("DISK_AVAIL_KB")
disk_pct = round(disk_used / disk_total * 100, 1) if (disk_used is not None and disk_total) else None

pressure_map = {1: "normal", 2: "warn", 4: "critical"}
mem_pressure = pressure_map.get(num("MEM_PRESSURE")) if num("MEM_PRESSURE") is not None else None

# Verdicts. Disk mirrors health::disk_state — ABSOLUTE free GB, not a percentage
# of a big volume (one cargo target tree is 10-15 GB). CPU is load-per-core.
def disk_state(free):
    if free is None: return "unknown"
    return "critical" if free < 10 else "warn" if free < 50 else "ok"
def cpu_state(lpc):
    if lpc is None: return "unknown"
    return "critical" if lpc >= 4 else "warn" if lpc >= 1.5 else "ok"
def mem_state(pct, pressure):
    # macOS runs ~full by design (inactive pages are reclaimable cache), so the
    # free-page percentage reads "critical" on a healthy box — a detector that
    # reports the RAM exists (ethos rule 7). The authoritative macOS signal is
    # memory PRESSURE. On Linux there is no pressure level here, but the percent
    # is computed from MemAvailable, which already discounts reclaimable cache,
    # so the percentage IS meaningful there.
    if pressure is not None:
        return {"normal": "ok", "warn": "warn", "critical": "critical"}.get(pressure, "unknown")
    if pct is None: return "unknown"
    return "critical" if pct >= 95 else "warn" if pct >= 85 else "ok"

ram_pct = num("RAM_PERCENT")
obj = {
    "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
    "analysis_ms": max(0, int(time.time() * 1000) - (num("STARTED_MS") or 0)),
    "host": os.environ.get("HOST", "unknown"),
    "os": os.environ.get("OS", ""),
    "os_version": os.environ.get("OS_VERSION", ""),
    "arch": os.environ.get("ARCH", ""),
    "cpu": {
        "count": cpu_count,
        "physical": num("CPU_PHYSICAL"),
        "load_avg": load,
        "load_per_core": load_per_core,
    },
    "memory": {
        "total_mb": num("RAM_TOTAL_MB"),
        "used_mb": num("RAM_USED_MB"),
        "free_mb": num("RAM_FREE_MB"),
        "percent": ram_pct,
        "pressure": mem_pressure,
    },
    "swap": {"total_mb": num("SWAP_TOTAL_MB"), "used_mb": num("SWAP_USED_MB")},
    "disk": {
        "path": os.environ.get("DISK_PATH", ""),
        "total_gb": disk_total,
        "used_gb": disk_used,
        "free_gb": disk_free,
        "percent": disk_pct,
    },
    "uptime_seconds": num("UPTIME_SECONDS"),
    "top_cpu": procs("TOP_CPU"),
    "top_mem": procs("TOP_MEM"),
    "process_counts": {
        "total": max(0, (num("PROC_TOTAL") or 1) - 1),  # drop the ps header row
        "claude": num("N_CLAUDE"),
        "rustc": num("N_RUSTC"),
        "cargo": num("N_CARGO"),
        "node": num("N_NODE"),
        "python3": num("N_PYTHON"),
    },
    "verdicts": {
        "cpu": cpu_state(load_per_core),
        "memory": mem_state(ram_pct, mem_pressure),
        "disk": disk_state(disk_free),
    },
}

if os.environ.get("PRETTY"):
    v = obj["verdicts"]
    def line(label, val):
        print(f"  {label:<16} {val}")
    print(f"HOST ANALYSIS — {obj['host']} ({obj['os']} {obj['os_version']} {obj['arch']})")
    print(f"  generated {obj['generated_at']}  in {obj['analysis_ms']}ms")
    print(f"\nCPU   [{v['cpu']}]")
    line("cores", f"{obj['cpu']['count']} logical / {obj['cpu']['physical']} physical")
    line("load avg", f"{obj['cpu']['load_avg']}  ({obj['cpu']['load_per_core']}/core)")
    print(f"\nMEMORY  [{v['memory']}]")
    line("ram", f"{obj['memory']['used_mb']} / {obj['memory']['total_mb']} MB  ({obj['memory']['percent']}%)  pressure={obj['memory']['pressure']}")
    line("swap", f"{obj['swap']['used_mb']} / {obj['swap']['total_mb']} MB")
    print(f"\nDISK  [{v['disk']}]  ({obj['disk']['path']})")
    line("space", f"{obj['disk']['used_gb']} / {obj['disk']['total_gb']} GB used  ({obj['disk']['free_gb']} GB free, {obj['disk']['percent']}%)")
    up = obj["uptime_seconds"] or 0
    print(f"\nUPTIME  {up // 86400}d {(up % 86400) // 3600}h {(up % 3600) // 60}m")
    print(f"\nPROCESS COUNTS")
    line("", "  ".join(f"{k}={val}" for k, val in obj["process_counts"].items()))
    print(f"\nTOP CPU")
    for p in obj["top_cpu"]:
        print(f"  {p['cpu_percent']:>6}%  {p['rss_mb']:>8} MB  {p['command']}")
    print(f"\nTOP MEM")
    for p in obj["top_mem"]:
        print(f"  {p['rss_mb']:>8} MB  {p['cpu_percent']:>6}%  {p['command']}")
else:
    print(json.dumps(obj))
PY
