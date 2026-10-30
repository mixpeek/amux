#!/usr/bin/env bash
# disk-reclaim-tick.sh — the scheduled disk cleanup, driven through /api/reclaim.
#
# Run daily by the amux scheduler as a `shell` schedule (zero model cost):
#   scripts/disk-reclaim-tick.sh             # act
#   scripts/disk-reclaim-tick.sh --dry-run   # say what it would do, act on nothing
#
# WHY IT GOES THROUGH THE SERVER. On 2026-10-20 two hand-rolled cleanup scripts
# rm -rf'd a running worker's ~/.amux/tmp, git-tracked customer data and live
# worktrees (crates/amux-server/src/api/reclaim_owner.rs has the incidents). The
# server now refuses those paths at the moment of the move: guard_path (system,
# amux state, git metadata, live session scratch), the ownership gate (running
# workers, tracked files, dirty or unpushed worktrees) and the builder's cargo
# lease. A script that deletes with rm skips all three. This one never calls rm:
#
#   1. Use the newest finished Disk scan (or start one and wait for it).
#   2. Pick regenerable findings (build output, orphaned tmp snapshots) that are
#      unowned, big enough to matter, and untouched for RECLAIM_IDLE_H on disk.
#   3. POST them to /api/reclaim/quarantine. The server moves them aside and
#      refuses anything owned; refusals are printed with their class.
#   4. Purge THIS schedule's own batches once they are RECLAIM_RETAIN_H old,
#      so a wrong move can still be restored in the dashboard until then.
#      Batches anyone else staged are never purged here.
#   5. Run the owning tool's own prune for caches (uv, npm, pnpm, brew,
#      simulators, docker build cache and dangling images). Each tool knows
#      what is still referenced; rm does not.
#
# STALE DATA (Ethan 2026-10-21: "regular cleanups of data that has not been
# accessed in a while across the MacBook"). A second class, separate knobs:
# big files under $HOME that nobody has opened in RECLAIM_STALE_DAYS. That is
# user data, so it gets a long restore window (RECLAIM_STALE_RETAIN_D) under
# its own session, disk-reclaim-stale, and every batch shows in Metrics > Disk
# Cleanup with a Restore button until then. "Not accessed" is the newest of
# mtime, atime and Spotlight's kMDItemLastUsedDate, because APFS only advances
# atime when it is older than mtime: atime alone cannot prove a file unread.
# A file some process has open stays. ~/Library, dot-dirs and app bundles
# (Photos/Music libraries, VMs, .app) are never entered: moving one file out of
# a bundle breaks the bundle.
#
# Knobs come from amux's GLOBAL scope (Scope tab > Environment, ~/.amux/amux.env),
# the same way mac-cleanup-tick reads AMUX_CLEANUP_*: set RECLAIM_STALE_DAYS=180
# there and the next tick uses it. An explicit environment value still wins.
#
# Reclaim is reported as a df delta, never a sum of sizes (APFS snapshots can
# hold freed blocks; reclaim.rs explains why the two disagree).
set -uo pipefail

DRY=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "disk-reclaim: unknown argument $a" >&2; exit 2 ;;
  esac
done

scope_reclaim_knobs() { # <scope file>: RECLAIM_* from the global scope, env wins
  local f=$1 line k v
  [ -f "$f" ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in RECLAIM_[A-Z0-9_]*=*) ;; *) continue ;; esac
    k=${line%%=*}; v=${line#*=}
    v=${v#\"}; v=${v%\"}; v=${v#\'}; v=${v%\'}
    [ -n "${!k+x}" ] && continue
    export "$k=$v"
  done < "$f"
}
scope_reclaim_knobs "${RECLAIM_SCOPE_FILE:-${AMUX_HOME:-$HOME/.amux}/amux.env}"

API=${RECLAIM_API:-$(amux url 2>/dev/null)}
SESSION=${RECLAIM_SESSION:-disk-reclaim-tick}
IDLE_H=${RECLAIM_IDLE_H:-48}             # untouched this long on disk before it moves
RETAIN_H=${RECLAIM_RETAIN_H:-72}         # our batches stay restorable this long
MIN_MB=${RECLAIM_MIN_MB:-256}            # smaller findings are not worth a rebuild
CATS=${RECLAIM_CATS:-build tmp-orphan}   # finding categories eligible for auto-move
# Only these trees are moved unattended. A build-dir NAME is not enough: the first
# dry run picked ~/.nvm/.../lib/node_modules (every global CLI) and an app's own
# runtime under ~/Library/Application Support. The dashboard still lists those
# for a person to judge.
ROOTS=${RECLAIM_ROOTS:-$HOME/Dev $HOME/.amux /private/tmp /private/var/folders}
SCAN_MAX_AGE_H=${RECLAIM_SCAN_MAX_AGE_H:-20}
# The scheduler kills a shell schedule at 600s (SHELL_TIMEOUT_S in
# runtime_jobs/scheduler.rs), and a scan takes 12 to 41 minutes. Waiting 2700s
# for one killed the 2026-10-22 04:00 run at 600s, with its log cut mid-wait.
# So wait 300s at most, which leaves the rest of the budget for the quarantine
# and the tool prunes. An unfinished scan carries on server-side, and the
# schedule's second daily fire (06:00) reuses it, being under SCAN_MAX_AGE_H old.
SCAN_WAIT_S=${RECLAIM_SCAN_WAIT_S:-300}
SCAN_POLL_S=${RECLAIM_SCAN_POLL_S:-30}
RECHECK_S=${RECLAIM_RECHECK_S:-60}       # per-path budget for the on-disk idle recheck
TOOLS=${RECLAIM_TOOLS:-1}                # 0 skips step 5 (the test suite sets it)
# ONE deadline for the whole run, under the scheduler's 600s shell timeout
# (SHELL_TIMEOUT_S). Each step's own cap (a 900s tool prune, an 1800s
# quarantine) used to be bigger than the whole budget, so a slow brew cleanup
# under load ran into the kill at 12:12 on 2026-10-23 and the run ended with no
# summary and a failure notice. Every bounded step now takes the smaller of its
# own cap and what is left, and a step with too little left is skipped and
# counted (verdict=reclaim_deadline_skip), so the run always reaches "done".
DEADLINE_S=${RECLAIM_DEADLINE_S:-540}
RESERVE_S=${RECLAIM_RESERVE_S:-15}       # kept back for the summary and status file
RUN_T0=$SECONDS
deadline_skipped=0
left() { echo $((DEADLINE_S - RESERVE_S - (SECONDS - RUN_T0))); }
cap() { local l; l=$(left); [ "$l" -lt "$1" ] && echo "$l" || echo "$1"; }   # <own cap> -> bounded seconds
LOG=${RECLAIM_LOG:-$HOME/.amux/logs/disk-reclaim-tick.log}
# The tick's last run as JSON. /api/reclaim/quarantine returns it as
# "scheduled", which is what Metrics > Disk Cleanup draws.
STATUS=${RECLAIM_STATUS_FILE:-$HOME/.amux/logs/disk-reclaim-tick.json}
STALE=${RECLAIM_STALE:-1}                          # 0 turns the stale-data class off
STALE_SESSION=${RECLAIM_STALE_SESSION:-disk-reclaim-stale}
STALE_DAYS=${RECLAIM_STALE_DAYS:-90}               # not opened this long
STALE_RETAIN_D=${RECLAIM_STALE_RETAIN_D:-30}       # restorable this long before purge
STALE_MIN_MB=${RECLAIM_STALE_MIN_MB:-500}
STALE_ROOTS=${RECLAIM_STALE_ROOTS:-$HOME}
STALE_EXCLUDE=${RECLAIM_STALE_EXCLUDE:-}           # extra path prefixes to leave alone
MDLS=${RECLAIM_MDLS:-mdls}
LSOF=${RECLAIM_LSOF:-lsof}

mkdir -p "$(dirname "$LOG")" 2>/dev/null
exec > >(tee -a "$LOG") 2>&1
say() { printf '%s disk-reclaim: %s\n' "$(date '+%F %T')" "$*"; }
api() { local path=$1 t=${2:-30}; shift; [ $# -gt 0 ] && shift; curl -sk --max-time "$t" -H "X-Amux-Session: $SESSION" "$@" "$API$path"; }
free_kb() { if [ -n "${RECLAIM_FREE_KB:-}" ]; then echo "$RECLAIM_FREE_KB"; else df -k "$HOME" | awk 'NR==2{print $4}'; fi; }   # override: tests
gb() { awk -v k="${1:-0}" 'BEGIN{printf "%.1fG", k/1048576}'; }

# Pressure tiers. Quarantine is a rename on the same volume, so it frees nothing
# until the purge: on a full disk a 72h restore window is 72h of no relief. The
# hourly health tick starts this run the moment it sees a disk constraint
# (mac-cleanup-tick.sh, verdict=disk_reclaim_triggered); these tiers make that
# run worth starting. An explicit RECLAIM_IDLE_H / RECLAIM_RETAIN_H wins.
TIGHT_GB=${RECLAIM_TIGHT_FREE_GB:-150}
CACHE_H=${RECLAIM_DOCKER_CACHE_H:-24}                # docker build cache unused this long is pruned
DOCKER_CTX_CMD=${RECLAIM_DOCKER_CTX_CMD:-colima list --json}
VM_FULL_PCT=${RECLAIM_VM_FULL_PCT:-85}
vm_docker_pct() { # <context>: percent used of the VM's /var/lib/docker, colima contexts only
  case "$1" in colima-*) timeout 20 colima ssh -p "${1#colima-}" -- df --output=pcent /var/lib/docker 2>/dev/null | tail -1 ;; esac
}
VM_PCT_CMD=${RECLAIM_VM_PCT_CMD:-vm_docker_pct}
CRITICAL_GB=${RECLAIM_CRITICAL_FREE_GB:-60}

if [ -z "$API" ]; then say "verdict=reclaim_tick_unmeasured why=no amux url"; exit 1; fi
WORK=$(mktemp -d "${TMPDIR:-/tmp}/disk-reclaim.XXXXXX")
trap 'rm -rf -- "${WORK:?}"' EXIT
free0=$(free_kb)
tier=normal
if [ $((free0 / 1048576)) -lt "$CRITICAL_GB" ]; then
  tier=critical
  IDLE_H=${RECLAIM_IDLE_H:-12}; RETAIN_H=${RECLAIM_RETAIN_H:-6}; SCAN_MAX_AGE_H=${RECLAIM_SCAN_MAX_AGE_H:-6}; CACHE_H=${RECLAIM_DOCKER_CACHE_H:-6}
elif [ $((free0 / 1048576)) -lt "$TIGHT_GB" ]; then
  tier=tight
  IDLE_H=${RECLAIM_IDLE_H:-24}; RETAIN_H=${RECLAIM_RETAIN_H:-24}; CACHE_H=${RECLAIM_DOCKER_CACHE_H:-12}
fi
floor=$TIGHT_GB; [ "$tier" = critical ] && floor=$CRITICAL_GB
[ "$tier" = normal ] || say "verdict=reclaim_tick_${tier} free $(gb "$free0") is under the ${floor}G floor: idle>=${IDLE_H}h, purge after ${RETAIN_H}h"
say "start dry=$DRY free=$(gb "$free0") free_src=${RECLAIM_FREE_KB:+override}${RECLAIM_FREE_KB:-df} tier=$tier idle>=${IDLE_H}h retain=${RETAIN_H}h min=${MIN_MB}M cats=[$CATS] roots=[$ROOTS]"
say "stale class: on=$STALE unused>=${STALE_DAYS}d retain=${STALE_RETAIN_D}d min=${STALE_MIN_MB}M roots=[$STALE_ROOTS]"

# ── 1. a scan ──────────────────────────────────────────────────────────────
scan_id=$(api /api/reclaim/scan | python3 -c '
import json, sys, time
max_age = float(sys.argv[1]) * 3600
try: rows = json.load(sys.stdin)
except Exception: sys.exit(0)
rows = rows if isinstance(rows, list) else rows.get("scans", [])
done = [r for r in rows if r.get("status") == "done" and time.time() - (r.get("finished_at") or 0) <= max_age]
if done: print(max(done, key=lambda r: r["finished_at"])["id"])' "$SCAN_MAX_AGE_H")
if [ -n "$scan_id" ]; then
  say "scan: reusing $scan_id (finished within ${SCAN_MAX_AGE_H}h)"
elif [ "$DRY" = 1 ]; then
  say "scan: none finished within ${SCAN_MAX_AGE_H}h, and a dry run starts none"
elif [ "$tier" = normal ] && [ "${RECLAIM_SCAN_WHEN_NORMAL:-0}" != 1 ]; then
  # A HOME SCAN IS NOT FREE (hog RCA 20261030-044026). It walks ~1.7M folders
  # for 40-70 minutes and was the server's top CPU share while it ran, and a
  # deploy restart kills most of them, so the next run found no finished scan
  # and started another. With free space above the tight floor there is
  # nothing urgent to reclaim: the tools below still run, and a scan finished
  # within the window (a person's Rescan) is still used.
  say "scan: none finished within ${SCAN_MAX_AGE_H}h; free $(gb "$free0") is above the ${TIGHT_GB}G floor, so no home scan starts (verdict=reclaim_tick_scan_skipped_normal; RECLAIM_SCAN_WHEN_NORMAL=1 starts one)"
else
  scan_id=$(api /api/reclaim/scan 30 -X POST -H 'Content-Type: application/json' -d '{}' \
    | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("scan_id") or "")
except Exception: pass')
  if [ -n "$scan_id" ]; then
    say "scan: started $scan_id, waiting up to ${SCAN_WAIT_S}s"
    t0=$SECONDS; st=running
    SCAN_WAIT_S=$(cap "$SCAN_WAIT_S")
    while [ "$st" = running ] && [ $((SECONDS - t0)) -lt "$SCAN_WAIT_S" ]; do
      sleep "$SCAN_POLL_S"
      st=$(api "/api/reclaim/scan/$scan_id?limit=1" 60 | python3 -c 'import json,sys
try: print(json.load(sys.stdin)["scan"]["status"])
except Exception: print("running")')
    done
    if [ "$st" = running ]; then
      say "scan: $scan_id still running after $((SECONDS - t0))s; it finishes server-side and the next run within ${SCAN_MAX_AGE_H}h uses it (verdict=reclaim_scan_pending)"
      scan_id=
    elif [ "$st" != done ]; then
      say "scan: $scan_id ended as '$st' after $((SECONDS - t0))s, not used"; scan_id=
    fi
  else
    say "scan: could not start one (another scan running, or the API refused)"
  fi
fi

# ── 2. selection ──────────────────────────────────────────────────────────
moved=0; refused=0; picked=0
s_picked=0; s_moved=0; s_refused=0; s_kept_used=0; s_kept_open=0; s_batch=-; n_inuse=0
if [ -z "$scan_id" ]; then
  say "verdict=reclaim_tick_no_scan quarantine step skipped: no usable scan"
else
  api "/api/reclaim/scan/$scan_id?limit=5000" 120 > "$WORK/scan.json"
  python3 - "$WORK/scan.json" "$IDLE_H" "$MIN_MB" "$CATS" "$ROOTS" > "$WORK/candidates.tsv" <<'PY'
import json, os, sys, time
d = json.load(open(sys.argv[1]))
idle_s, min_b, cats = float(sys.argv[2]) * 3600, int(sys.argv[3]) << 20, set(sys.argv[4].split())
roots = [r.rstrip("/") + "/" for r in sys.argv[5].split()]
own = d.get("ownership") or {}
if not own.get("fleet_measured", False):
    print("UNMEASURED\tfleet ownership probe did not run")
    sys.exit(0)
now = time.time()
for f in d.get("findings") or []:
    p = f.get("path") or ""
    if f.get("protected") or not f.get("regenerable") or f.get("category") not in cats:
        continue
    if not any(p.startswith(r) for r in roots):
        continue
    if (f.get("bytes") or 0) < min_b or now - (f.get("mtime") or now) < idle_s:
        continue
    if os.path.lexists(p):
        print(f"{f['bytes']}\t{p}")
PY
  if head -1 "$WORK/candidates.tsv" | grep -q '^UNMEASURED'; then
    say "verdict=reclaim_tick_unmeasured scan ownership was not measured; nothing moved"
    : > "$WORK/candidates.tsv"
  fi

  # IN USE WITHOUT BEING WRITTEN. A venv a schedule runs from every 3 hours is
  # only read, and reading moves no mtime; APFS does not advance atime either
  # once it is past mtime. So on 2026-10-21 the idle test quarantined
  # momentbench's scheduled .venv (momentbench-oss). A path stays when an
  # enabled amux schedule, a LaunchAgent or a crontab line names it or its
  # parent, or when it or an ancestor holds a .amux-keep marker. No readable
  # schedule list means no moves at all: a reference we cannot see is exactly
  # the case this guards.
  : > "$WORK/refs.txt"
  if api /api/schedules 60 > "$WORK/schedules.json" && python3 - "$WORK/schedules.json" >> "$WORK/refs.txt" <<'EOF'
import json, sys
d = json.load(open(sys.argv[1]))
d = d if isinstance(d, list) else d.get("schedules")
if not isinstance(d, list): sys.exit(1)
for s in d:
    if s.get("enabled"): print(s.get("command") or "")
EOF
  then
    cat "${RECLAIM_LAUNCH_AGENTS:-$HOME/Library/LaunchAgents}"/*.plist >> "$WORK/refs.txt" 2>/dev/null || true
    ${RECLAIM_CRONTAB:-crontab -l} >> "$WORK/refs.txt" 2>/dev/null || true
    refs_ok=1
  else
    refs_ok=0
    say "verdict=reclaim_tick_refs_unmeasured could not read /api/schedules; nothing moves this run"
  fi
  cat > "$WORK/inuse.py" <<'EOF'
import os, re, sys
# <refs> <home> <tsv>: rewrite the tsv without paths in use, print why each stayed
refs, home, tsv = open(sys.argv[1], errors="ignore").read(), sys.argv[2].rstrip("/"), sys.argv[3]
for t in ("${HOME}/", "$HOME/", "~/"):
    refs = refs.replace(t, home + "/")
keep, out = [], []
for line in open(tsv):
    b, _, p = line.rstrip("\n").partition("\t")
    if not p: continue
    d, why = os.path.dirname(p), None
    if p in refs: why = "named by a schedule, LaunchAgent or crontab"
    # The folder rule is for directories (a venv beside the script a schedule
    # runs). A single file is kept only when named: a crontab reading one file
    # in ~/Downloads says nothing about the rest of ~/Downloads.
    elif os.path.isdir(p) and d != home and re.search(re.escape(d) + r"""(?:[/\s"';&|)]|$)""", refs):
        why = f"its folder {d} is named by a schedule, LaunchAgent or crontab"
    else:
        a = p
        while a not in ("/", ""):
            if os.path.exists(os.path.join(a, ".amux-keep")): why = f".amux-keep in {a}"; break
            a = os.path.dirname(a)
    (keep if why else out).append((p, why, line))
open(tsv, "w").writelines(l for _, _, l in out)
for p, why, _ in keep: print(f"  keep {p} (in use: {why})")
print(f"INUSE {len(keep)}")
EOF
  in_use() { # <tsv>: drop paths in use; sets n_inuse
    local r
    if [ "$refs_ok" != 1 ]; then : > "$1"; return 0; fi
    r=$(python3 "$WORK/inuse.py" "$WORK/refs.txt" "$HOME" "$1") || { say "verdict=reclaim_tick_refs_unmeasured in-use check failed; nothing moves"; : > "$1"; return 0; }
    printf '%s\n' "$r" | grep -v '^INUSE ' | while IFS= read -r l; do say "$l"; done
    n_inuse=$(( ${n_inuse:-0} + $(printf '%s\n' "$r" | sed -n 's/^INUSE //p') ))
  }
  in_use "$WORK/candidates.tsv"
  # The scan is up to SCAN_MAX_AGE_H old. Re-check on disk now: any file written
  # inside the idle window keeps the path. A recheck that runs out of time keeps
  # it too, since the expensive direction to be wrong in is the move.
  touch -t "$(date -v-"${IDLE_H}"H '+%Y%m%d%H%M.%S' 2>/dev/null || date -d "-${IDLE_H} hours" '+%Y%m%d%H%M.%S')" "$WORK/idle-stamp"
  : > "$WORK/paths.txt"
  n_recent=0; n_slow=0
  while IFS=$'\t' read -r bytes p; do
    [ -n "$p" ] || continue
    hit=$(timeout "$RECHECK_S" find "$p" -newer "$WORK/idle-stamp" -print -quit 2>/dev/null); rc=$?
    if [ "$rc" = 124 ]; then n_slow=$((n_slow+1)); say "  keep $p (idle recheck ran out of ${RECHECK_S}s)"; continue; fi
    if [ -n "$hit" ]; then n_recent=$((n_recent+1)); say "  keep $p (written inside ${IDLE_H}h: $hit)"; continue; fi
    printf '%s\n' "$p" >> "$WORK/paths.txt"; picked=$((picked+1))
    say "  pick $(gb $((bytes/1024))) $p"
  done < "$WORK/candidates.tsv"
  say "selection: $picked picked from scan $scan_id; kept $n_recent recently written, $n_slow unrechecked"

  # ── 3. quarantine ───────────────────────────────────────────────────────
  quarantine() { # <session> <paths file> -> "moved refused batch" on stdout
    python3 -c 'import json,sys; print(json.dumps({"paths":[l.rstrip("\n") for l in open(sys.argv[1]) if l.strip()],"scan_id":sys.argv[2]}))' \
      "$2" "$scan_id" > "$WORK/req.json"
    curl -sk --max-time "$(cap 1800)" -H "X-Amux-Session: $1" -X POST -H 'Content-Type: application/json' \
      --data-binary @"$WORK/req.json" "$API/api/reclaim/quarantine" > "$WORK/q.json"
    cat > "$WORK/reply.py" <<'PY'
import json, sys
# The server answers {"batch_id", "moved": <count>, "refused": [{path, reason, class}]}.
try: d = json.load(open(sys.argv[1]))
except Exception as e:
    print(f"verdict=reclaim_tick_bad_reply quarantine reply was not JSON: {e}", file=sys.stderr)
    print("? ? -"); sys.exit(0)
ref = d.get("refused") or []
for r in ref:
    print(f"REFUSED\t{r.get('class','?')}\t{r.get('path')}\t{r.get('reason')}", file=sys.stderr)
m = d.get("moved")
print(m if isinstance(m, int) else len(m or []), len(ref), d.get("batch_id") or "-")
PY
    python3 "$WORK/reply.py" "$WORK/q.json"
  }
  if [ "$picked" -gt 0 ] && [ "$DRY" = 0 ]; then
    read -r moved refused batch < <(quarantine "$SESSION" "$WORK/paths.txt")
    say "quarantine: batch $batch moved $moved, refused $refused (refusals above name their class)"
  elif [ "$picked" -gt 0 ]; then
    say "quarantine: dry run, would move $picked path(s)"
  fi

  # ── 3b. stale data: big files nobody has opened in STALE_DAYS ─────────────
  if [ "$STALE" = 1 ]; then
    cat > "$WORK/stale.py" <<'EOF'
import json, os, sys, time
d = json.load(open(sys.argv[1]))
age_s, min_b = float(sys.argv[2]) * 86400, int(sys.argv[3]) << 20
roots = [r.rstrip("/") + "/" for r in sys.argv[4].split()]
home = sys.argv[5].rstrip("/")
# Media folders an app indexes behave like bundles too: moving a track out of
# iTunes Media leaves a broken entry in the Music library.
excl = [home + d for d in ("/Library/", "/Music/iTunes/", "/Music/Music/", "/Movies/TV/")] \
     + [e.rstrip("/") + "/" for e in sys.argv[6].split()]
# Bundles look like folders and are one document: moving a file out of one
# corrupts it (a Photos library, a VM, an app).
BUNDLES = (".app", ".photoslibrary", ".musiclibrary", ".tvlibrary", ".imovielibrary",
           ".fcpbundle", ".logicx", ".band", ".aplibrary", ".vmwarevm", ".pvm", ".utm",
           ".sparsebundle", ".bundle", ".framework", ".xcarchive", ".lrdata")
if not (d.get("ownership") or {}).get("fleet_measured", False):
    print("UNMEASURED"); sys.exit(0)
now = time.time()
for f in d.get("findings") or []:
    p = f.get("path") or ""
    # The scan files a big file as "stale" past its own 90d mark and "large"
    # before it, so a shorter STALE_DAYS still finds candidates.
    if f.get("protected") or f.get("regenerable") or f.get("category") not in ("stale", "large"):
        continue
    if not any(p.startswith(r) for r in roots) or any(p.startswith(e) for e in excl):
        continue
    dirs = (p[len(home):] if p.startswith(home + "/") else p).split("/")[:-1]
    if any(c.startswith(".") or c.lower().endswith(BUNDLES) for c in dirs):
        continue
    if (f.get("bytes") or 0) < min_b or now - (f.get("mtime") or now) < age_s:
        continue
    if os.path.isfile(p) and not os.path.islink(p):
        print(f"{f['bytes']}\t{p}")
EOF
    python3 "$WORK/stale.py" "$WORK/scan.json" "$STALE_DAYS" "$STALE_MIN_MB" "$STALE_ROOTS" "$HOME" "$STALE_EXCLUDE" > "$WORK/stale.tsv"
    if head -1 "$WORK/stale.tsv" | grep -q '^UNMEASURED'; then
      say "verdict=reclaim_stale_unmeasured scan ownership was not measured; no stale data moved"
      : > "$WORK/stale.tsv"
    fi
    in_use "$WORK/stale.tsv"
    # Last use is the newest of mtime, atime and Spotlight's last-used date,
    # read NOW, since the scan can be hours old. Unreadable means keep.
    cat > "$WORK/lastuse.py" <<'EOF'
import os, subprocess, sys
from datetime import datetime
p, mdls = sys.argv[1], sys.argv[2]
st = os.stat(p)
t = max(st.st_mtime, st.st_atime)
try:
    out = subprocess.run([mdls, "-raw", "-name", "kMDItemLastUsedDate", p],
                         capture_output=True, text=True, timeout=20).stdout.strip()
    if out and out != "(null)":
        t = max(t, datetime.strptime(out, "%Y-%m-%d %H:%M:%S %z").timestamp())
except Exception:
    pass
print(int(t))
EOF
    now_s=$(date +%s); cutoff=$(( now_s - STALE_DAYS * 86400 ))
    : > "$WORK/stale-paths.txt"
    while IFS=$'\t' read -r bytes p; do
      [ -n "$p" ] || continue
      last=$(python3 "$WORK/lastuse.py" "$p" "$MDLS" 2>/dev/null) || last=
      if [ -z "$last" ] || [ "$last" -gt "$cutoff" ]; then
        s_kept_used=$((s_kept_used+1)); say "  keep $p (opened inside ${STALE_DAYS}d, last use ${last:-unreadable})"; continue
      fi
      if [ -n "$("$LSOF" -t -- "$p" 2>/dev/null)" ]; then
        s_kept_open=$((s_kept_open+1)); say "  keep $p (open in a running process)"; continue
      fi
      printf '%s\n' "$p" >> "$WORK/stale-paths.txt"; s_picked=$((s_picked+1))
      say "  stale $(gb $((bytes/1024))) $p (last used $(( (now_s - last) / 86400 ))d ago)"
    done < "$WORK/stale.tsv"
    say "stale: $s_picked picked; kept $s_kept_used opened recently, $s_kept_open open now"
    if [ "$s_picked" -gt 0 ] && [ "$DRY" = 0 ]; then
      read -r s_moved s_refused s_batch < <(quarantine "$STALE_SESSION" "$WORK/stale-paths.txt")
      say "stale: batch $s_batch moved $s_moved, refused $s_refused; restorable for ${STALE_RETAIN_D}d in Metrics > Disk Cleanup"
    elif [ "$s_picked" -gt 0 ]; then
      say "stale: dry run, would move $s_picked file(s)"
    fi
  fi
fi

# ── 4. purge our own expired batches ───────────────────────────────────────
purged=0
api /api/reclaim/quarantine 60 | python3 -c '
import json, sys, time
# Each class purges only batches under its own session, after its own window.
retain = {sys.argv[1]: float(sys.argv[2]) * 3600, sys.argv[3]: float(sys.argv[4]) * 86400}
try: d = json.load(sys.stdin)
except Exception: sys.exit(0)
for b in d.get("batches") or []:
    me = b.get("session")
    if me in retain and b.get("status") == "staged" and time.time() - (b.get("created_at") or time.time()) >= retain[me]:
        print(b["id"], b.get("bytes") or 0, me)' "$SESSION" "$RETAIN_H" "$STALE_SESSION" "$STALE_RETAIN_D" > "$WORK/expired.txt"
while read -r bid bbytes bsess; do
  [ -n "$bid" ] || continue
  if [ "$DRY" = 1 ]; then say "purge: dry run, would purge batch $bid ($(gb $((bbytes/1024))))"; continue; fi
  if [ "$(left)" -lt 10 ]; then deadline_skipped=$((deadline_skipped+1)); say "purge: batch $bid left for the next run, run deadline reached (verdict=reclaim_deadline_skip)"; continue; fi
  code=$(curl -sk --max-time "$(cap 1800)" -o "$WORK/purge.json" -w '%{http_code}' -X DELETE \
    -H "X-Amux-Session: $bsess" "$API/api/reclaim/quarantine/$bid?confirm=$bid")
  case "$code" in
    2??) purged=$((purged+1)); say "purge: batch $bid ($(gb $((bbytes/1024))))" ;;
    *) say "purge: batch $bid refused http=$code $(head -c 200 "$WORK/purge.json")" ;;
  esac
done < "$WORK/expired.txt"

# ── 5. tool-owned caches ───────────────────────────────────────────────────
tool() { # <label> <cmd...>: run if installed, bounded, report the df delta
  local label=$1; shift
  command -v "$1" >/dev/null 2>&1 || { say "tool: $label not installed"; return 0; }
  if [ "$DRY" = 1 ]; then say "tool: dry run, would run: $*"; return 0; fi
  local b a rc=0 t
  t=$(cap 900)
  if [ "$t" -lt 10 ]; then
    deadline_skipped=$((deadline_skipped+1))
    say "tool: $label skipped, ${t}s left of the ${DEADLINE_S}s run deadline (verdict=reclaim_deadline_skip)"
    return 0
  fi
  b=$(free_kb); timeout "$t" "$@" >/dev/null 2>&1 || rc=$?; a=$(free_kb)
  if [ "$rc" = 124 ]; then
    say "tool: $label stopped at its ${t}s share of the run deadline, freed $(gb $((a - b))) (verdict=reclaim_tool_timeout)"
  else
    say "tool: $label rc=$rc freed $(gb $((a - b)))"
  fi
}
if [ "$TOOLS" = 1 ]; then
  tool "uv cache prune" uv cache prune
  tool "npm cache verify" npm cache verify
  tool "pnpm store prune" pnpm store prune
  tool "unavailable simulators" xcrun simctl delete unavailable
  # Every running Docker engine, not only the current context. 2026-10-22 the
  # goal-shared Colima VM was the Mac's biggest writer (datadisk 134G -> 159G in
  # a day, ~29G/h at peak) and held 21.9G of build cache last used 11-14h ago:
  # the old 72h filter on the default context never touched it. Freed blocks
  # reach the Mac through the VM trim schedule (SCHED-617, every 5m).
  ctxs=$($DOCKER_CTX_CMD 2>/dev/null | python3 -c 'import json,sys
for l in sys.stdin:
    try: d = json.loads(l)
    except Exception: continue
    if d.get("status") == "Running" and d.get("runtime", "docker") == "docker": print("colima-" + d["name"])' 2>/dev/null)
  [ -n "$ctxs" ] || ctxs=default
  for ctx in $ctxs; do
    if [ "$(left)" -ge 10 ] && timeout "$(cap 20)" docker --context "$ctx" info >/dev/null 2>&1; then
      # A VM can fill from the inside while the Mac still has room: 2026-10-23
      # gs12-local reached 92% (128G of 147G) on 56G of build cache under a day
      # old, with the Mac over the tight floor, so the host-free tiers kept the
      # 24h window. A VM at VM_FULL_PCT or more gets the 6h window.
      ch=$CACHE_H; pct=$($VM_PCT_CMD "$ctx" 2>/dev/null | tr -dc '0-9')
      if [ -n "$pct" ] && [ "$pct" -ge "$VM_FULL_PCT" ] && [ "$ch" -gt 6 ]; then
        ch=6; say "verdict=reclaim_tick_vm_full docker[$ctx] disk ${pct}% used (>= ${VM_FULL_PCT}%): build cache unused 6h goes"
      fi
      tool "docker[$ctx] build cache >${ch}h" docker --context "$ctx" builder prune -f --filter "until=${ch}h"
      tool "docker[$ctx] dangling images" docker --context "$ctx" image prune -f
    else
      say "tool: docker[$ctx] not answering"
    fi
  done
  # Last: the slowest prune (82s on a calm morning, still running 6 min in under
  # load on 2026-10-23), so the quick ones never wait behind it.
  tool "brew cleanup" brew cleanup -s --prune=30
fi

free1=$(free_kb)
say "done dry=$DRY picked=$picked moved=$moved refused=$refused stale_picked=$s_picked stale_moved=$s_moved kept_in_use=$n_inuse purged=$purged deadline_skipped=$deadline_skipped elapsed=$((SECONDS - RUN_T0))s/${DEADLINE_S}s free $(gb "$free0") -> $(gb "$free1") (df delta $(gb $((free1 - free0))))"

# The run, for Metrics > Disk Cleanup (served as "scheduled" by GET
# /api/reclaim/quarantine). Every value is this run's; written to a temp file
# and renamed so a reader never sees half of it.
cat > "$WORK/status.py" <<'EOF'
import json, os, sys, time
a = dict(x.split("=", 1) for x in sys.argv[2:])
n = lambda k: int(a[k]) if a.get(k, "").lstrip("-").isdigit() else 0
st = {"at": int(time.time()), "dry": a["dry"] == "1", "scan_id": a["scan_id"] or None,
      "free_kb_before": n("free0"), "free_kb_after": n("free1"), "purged": n("purged"),
      "kept_in_use": n("n_inuse"),
      "classes": [
        {"key": "build", "label": "Idle build output and orphaned tmp", "session": a["session"],
         "enabled": True, "retain_s": int(float(a["retain_h"]) * 3600),
         "rule": f"regenerable, untouched {a['idle_h']}h, {a['min_mb']} MB+, under {a['roots']}",
         "picked": n("picked"), "moved": n("moved"), "refused": n("refused")},
        {"key": "stale", "label": f"Files not opened in {a['stale_days']} days", "session": a["stale_session"],
         "enabled": a["stale"] == "1", "retain_s": int(float(a["stale_retain_d"]) * 86400),
         "rule": f"not opened {a['stale_days']}d, {a['stale_min_mb']} MB+, under {a['stale_roots']}",
         "picked": n("s_picked"), "moved": n("s_moved"), "refused": n("s_refused"),
         "kept_recently_used": n("s_kept_used"), "kept_open": n("s_kept_open")}]}
tmp = sys.argv[1] + ".tmp"
os.makedirs(os.path.dirname(sys.argv[1]) or ".", exist_ok=True)
with open(tmp, "w") as fh: json.dump(st, fh)
os.replace(tmp, sys.argv[1])
EOF
python3 "$WORK/status.py" "$STATUS" n_inuse="$n_inuse" dry="$DRY" scan_id="${scan_id:-}" free0="$free0" free1="$free1" purged="$purged" \
  session="$SESSION" retain_h="$RETAIN_H" idle_h="$IDLE_H" min_mb="$MIN_MB" roots="$ROOTS" \
  picked="$picked" moved="$moved" refused="$refused" \
  stale="$STALE" stale_session="$STALE_SESSION" stale_days="$STALE_DAYS" stale_retain_d="$STALE_RETAIN_D" \
  stale_min_mb="$STALE_MIN_MB" stale_roots="$STALE_ROOTS" \
  s_picked="$s_picked" s_moved="$s_moved" s_refused="$s_refused" s_kept_used="$s_kept_used" s_kept_open="$s_kept_open" \
  || say "verdict=reclaim_tick_status_unwritten could not write $STATUS"
