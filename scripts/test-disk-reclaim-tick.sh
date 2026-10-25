#!/usr/bin/env bash
# Tests for scripts/disk-reclaim-tick.sh against a fake /api/reclaim.
# The fake records every quarantine POST and purge DELETE, so each cell asserts
# what the tick ASKED the server to move, which is the only thing it decides.
set -euo pipefail
cd "$(dirname "$0")/.."
TICK=scripts/disk-reclaim-tick.sh
T=$(mktemp -d "${TMPDIR:-/tmp}/test-reclaim.XXXXXX")
trap 'kill "${SRV:-0}" 2>/dev/null; rm -rf -- "${T:?}"' EXIT
pass=0; fail=0
ok()  { pass=$((pass+1)); echo "PASS $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; }

now=$(date +%s); old=$((now - 5*86400)); recent=$((now - 3600))
day30h=$((now - 30*3600))   # idle under the normal 48h, over the tight 24h
mkdir -p "$T/dev/a/target" "$T/dev/b/target" "$T/dev/c/target" "$T/dev/d/node_modules" \
         "$T/dev/e/target" "$T/dev/f/.venv" "$T/global/node_modules" \
         "$T/dev/g/.venv" "$T/dev/h/sub/target" "$T/dev/i/.venv" "$T/agents"
oldstamp=$(python3 -c 'import sys,time; print(time.strftime("%Y%m%d%H%M.%S", time.localtime(int(sys.argv[1]))))' "$old")  # BSD and GNU date disagree on -r
# A dir's own mtime moves when an entry is created in it, so age the file AND
# its dir: the recheck reads both, which is what makes a fresh target stay.
for d in "$T"/dev/{a,b,c,e}/target "$T/dev/d/node_modules" "$T/dev/f/.venv" "$T/global/node_modules" \
         "$T/dev/g/.venv" "$T/dev/h/sub/target" "$T/dev/i/.venv"; do echo x > "$d/f"; touch -t "$oldstamp" "$d/f" "$d"; done
echo fresh > "$T/dev/e/target/new.o"   # scan says idle, disk says written now
mkdir -p "$T/dev/t30/target"; echo x > "$T/dev/t30/target/f"
d30=$(python3 -c 'import sys,time; print(time.strftime("%Y%m%d%H%M.%S", time.localtime(int(sys.argv[1]))))' "$day30h")
touch -t "$d30" "$T/dev/t30/target/f" "$T/dev/t30/target"
# In use without being written (momentbench-oss, 2026-10-21): a schedule runs
# from g's folder, h's project carries a keep marker, a LaunchAgent runs i's venv.
touch "$T/dev/h/.amux-keep"
printf '{"schedules":[{"enabled":1,"command":"cd %s && .venv/bin/python job.py"},{"enabled":0,"command":"%s"}]}' "$T/dev/g" "$T/dev/a/target" > "$T/schedules.json"
printf '<string>%s/bin/python</string>\n' "$T/dev/i/.venv" > "$T/agents/x.plist"

# Stale-data fixtures: a fake $HOME with big files in every shape the class must judge.
H="$T/home"; ancient=$((now - 200*86400)); young=$((now - 10*86400))
ancstamp=$(python3 -c 'import sys,time; print(time.strftime("%Y%m%d%H%M.%S", time.localtime(int(sys.argv[1]))))' "$ancient")
mkdir -p "$H/Downloads" "$H/Movies" "$H/Documents" "$H/Library/Caches" "$H/.ollama/models" "$H/Pictures/Photos Library.photoslibrary/originals" "$H/Music/iTunes/iTunes Media/Music"
for f in "Downloads/old installer.dmg" Downloads/opened.iso Downloads/atime.zip Library/Caches/big.bin .ollama/models/blob \
         "Pictures/Photos Library.photoslibrary/originals/x.heic" Movies/open.mov "Music/iTunes/iTunes Media/Music/track.mp3"; do echo x > "$H/$f"; touch -t "$ancstamp" "$H/$f"; done
touch -a "$H/Downloads/atime.zip"            # read today: atime is newer than mtime
echo x > "$H/Documents/young.mov"
cat > "$T/mdls" <<'SH'
#!/bin/sh
# Fake Spotlight: opened.iso was opened two days ago, nothing else ever.
case "$4" in *opened.iso) date -u -v-2d '+%Y-%m-%d %H:%M:%S +0000' 2>/dev/null || date -u -d '-2 days' '+%Y-%m-%d %H:%M:%S +0000' ;; *) echo '(null)' ;; esac
SH
cat > "$T/lsof" <<'SH'
#!/bin/sh
case "$3" in *open.mov) echo 4242 ;; esac
SH
chmod +x "$T/mdls" "$T/lsof"
echo x > "$H/Downloads/cron-input.tar"; touch -t "$ancstamp" "$H/Downloads/cron-input.tar"
printf '0 * * * * cat "%s"\n' "$H/Downloads/cron-input.tar" > "$T/crontab"

finding() { # path category regenerable mtime protected
  printf '{"path":"%s","category":"%s","regenerable":%s,"bytes":1073741824,"mtime":%s,"protected":%s}' "$@"; }
mkscan() { # <fleet_measured> -> writes the fake's scan file
  cat > "$T/scan.json" <<EOF
{"scan":{"id":"S1","status":"done"},"ownership":{"fleet_measured":$1},"findings":[
$(finding "$T/dev/t30/target" build true $day30h null),
$(finding "$T/dev/a/target" build true $old null),
$(finding "$T/dev/b/target" build true $recent null),
$(finding "$T/dev/c/target" build true $old '{"class":"worker","reason":"running"}'),
$(finding "$T/global/node_modules" build true $old null),
$(finding "$T/dev/d/node_modules" large false $old null),
$(finding "$T/dev/e/target" build true $old null),
$(finding "$T/dev/f/.venv" build true $old null),
$(finding "$T/dev/g/.venv" build true $old null),
$(finding "$T/dev/h/sub/target" build true $old null),
$(finding "$T/dev/i/.venv" build true $old null),
$(finding "$H/Downloads/old installer.dmg" stale false $ancient null),
$(finding "$H/Downloads/opened.iso" stale false $ancient null),
$(finding "$H/Downloads/atime.zip" stale false $ancient null),
$(finding "$H/Library/Caches/big.bin" stale false $ancient null),
$(finding "$H/.ollama/models/blob" stale false $ancient null),
$(finding "$H/Pictures/Photos Library.photoslibrary/originals/x.heic" stale false $ancient null),
$(finding "$H/Movies/open.mov" stale false $ancient null),
$(finding "$H/Music/iTunes/iTunes Media/Music/track.mp3" stale false $ancient null),
$(finding "$H/Documents/young.mov" large false $young null),
$(finding "$H/Downloads/cron-input.tar" stale false $ancient null)]}
EOF
}
cat > "$T/batches.json" <<EOF
{"batches":[
 {"id":"OWNOLD","session":"disk-reclaim-tick","status":"staged","created_at":$((now - 4*86400)),"bytes":1},
 {"id":"OWNNEW","session":"disk-reclaim-tick","status":"staged","created_at":$recent,"bytes":1},
 {"id":"OWN8H","session":"disk-reclaim-tick","status":"staged","created_at":$((now - 8*3600)),"bytes":1},
 {"id":"PEEROLD","session":"amux-admin","status":"staged","created_at":$((now - 9*86400)),"bytes":1},
 {"id":"OWNGONE","session":"disk-reclaim-tick","status":"purged","created_at":$((now - 9*86400)),"bytes":1},
 {"id":"STALEOLD","session":"disk-reclaim-stale","status":"staged","created_at":$((now - 31*86400)),"bytes":1},
 {"id":"STALENEW","session":"disk-reclaim-stale","status":"staged","created_at":$((now - 4*86400)),"bytes":1}]}
EOF

cat > "$T/fake.py" <<'PY'
import http.server, json, sys, time, urllib.parse
T = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def send(self, obj, code=200):
        b = json.dumps(obj).encode(); self.send_response(code)
        self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(b)))
        self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        pending = __import__("os").path.exists(T + "/scan-pending")
        if u.path == "/api/reclaim/scan":
            return self.send([] if pending else [{"id": "S1", "status": "done", "finished_at": time.time() - 60}])
        if u.path == "/api/reclaim/scan/S2":
            return self.send({"scan": {"id": "S2", "status": "running"}})
        if u.path == "/api/reclaim/scan/S1":
            return self.send(json.load(open(T + "/scan.json")))
        if u.path == "/api/reclaim/quarantine":
            return self.send(json.load(open(T + "/batches.json")))
        if u.path == "/api/schedules":
            try: return self.send(json.load(open(T + "/schedules.json")))
            except FileNotFoundError: return self.send({"error": "down"}, 503)
        self.send({"error": "nf"}, 404)
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0); body = self.rfile.read(n).decode()
        if self.path.startswith("/api/reclaim/scan"):
            return self.send({"scan_id": "S2"})
        open(T + "/posts.log", "a").write(self.headers.get("X-Amux-Session", "") + "\t" + body + "\n")
        paths = json.loads(body).get("paths", [])
        # The real reply's shape (reclaim.rs create_quarantine): moved is a COUNT.
        self.send({"batch_id": "B1", "moved": len(paths[:-1]), "refused": [{"path": paths[-1], "class": "owned", "reason": "test"}] if paths else []})
    def do_DELETE(self):
        open(T + "/deletes.log", "a").write(self.path + "\n"); self.send({"ok": True})
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open(T + "/port", "w").write(str(s.server_port)); s.serve_forever()
PY
python3 "$T/fake.py" "$T" & SRV=$!
for _ in $(seq 50); do [ -s "$T/port" ] && break; sleep 0.1; done
API="http://127.0.0.1:$(cat "$T/port")"

run() { : > "$T/posts.log"; : > "$T/deletes.log"; rm -f -- "$T/status.json"
  # The tier comes from free space. Pin it: a CI runner has well under 60G free,
  # so the real df put every unpinned cell in the critical tier (8ac60690e red).
  RECLAIM_FREE_KB="${FREE_KB:-$((500 * 1048576))}" \
  HOME="$H" RECLAIM_API=$API RECLAIM_ROOTS="$T/dev" RECLAIM_TOOLS="${RECLAIM_TOOLS:-0}" RECLAIM_LOG="$T/tick.log" \
    RECLAIM_STATUS_FILE="$T/status.json" RECLAIM_SCOPE_FILE="${SCOPE:-$T/no-scope}" \
    RECLAIM_MDLS="$T/mdls" RECLAIM_LSOF="$T/lsof" \
    RECLAIM_LAUNCH_AGENTS="$T/agents" RECLAIM_CRONTAB="cat $T/crontab" \
    bash "$TICK" "$@" > "$T/out.txt" 2>&1 && echo 0 > "$T/rc" || echo $? > "$T/rc"; }
stale_posted() { grep '^disk-reclaim-stale' "$T/posts.log" | grep -c -F "$1"; }
status() { python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); c={x["key"]:x for x in d["classes"]}; print(eval(sys.argv[2]))' "$T/status.json" "$1" 2>/dev/null; }
posted() { grep -c -F "\"$1\"" "$T/posts.log"; }

# ── act ──
mkscan true; run
[ "$(cat "$T/rc")" = 0 ] && ok "exits 0" || { bad "exits 0 (rc=$(cat "$T/rc"))"; cat "$T/out.txt"; }
[ "$(posted "$T/dev/a/target")" = 1 ] && ok "idle unowned build output in a root is quarantined" || bad "idle build output quarantined"
[ "$(posted "$T/dev/f/.venv")" = 1 ] && ok "a .venv in a root is quarantined" || bad ".venv quarantined"
[ "$(posted "$T/dev/b/target")" = 0 ] && ok "scan-recent finding is kept" || bad "scan-recent kept"
[ "$(posted "$T/dev/c/target")" = 0 ] && ok "protected finding is kept" || bad "protected kept"
[ "$(posted "$T/global/node_modules")" = 0 ] && ok "build-named dir outside the roots is kept" || bad "outside roots kept"
[ "$(posted "$T/dev/d/node_modules")" = 0 ] && ok "non-regenerable category is kept" || bad "non-regenerable kept"
[ "$(posted "$T/dev/e/target")" = 0 ] && ok "on-disk recheck keeps a dir written inside the idle window" || bad "on-disk recheck"
grep -q "written inside 48h" "$T/out.txt" && ok "recheck says why it kept" || bad "recheck reason printed"
cut -f1 "$T/posts.log" | grep -qx disk-reclaim-tick && ok "POST carries the schedule's session" || bad "session header"
grep -q 'moved 1, refused 1' "$T/out.txt" && ok "server refusals are counted, not hidden" || bad "refusal count"
grep -q 'REFUSED.*owned' "$T/out.txt" && ok "a refusal prints its class" || bad "refusal class"
grep -q 'OWNOLD?confirm=OWNOLD' "$T/deletes.log" && ok "own expired batch purged with confirm" || bad "own expired purge"
grep -q 'OWNNEW' "$T/deletes.log" && bad "own recent batch must stay restorable" || ok "own recent batch stays restorable"
grep -q 'PEEROLD' "$T/deletes.log" && bad "peer batch must never be purged" || ok "peer's batch is never purged"
grep -q 'OWNGONE' "$T/deletes.log" && bad "already purged batch re-purged" || ok "already-purged batch is skipped"
grep -q 'done dry=0 picked=2 moved=1 refused=1 stale_picked=1 stale_moved=0 kept_in_use=4 purged=2' "$T/out.txt" && ok "summary line is computed from the run" || { bad "summary line"; tail -2 "$T/out.txt"; }

# ── in use without being written ──
[ "$(posted "$T/dev/g/.venv")" = 0 ] && grep -q "dev/g/.venv (in use: its folder" "$T/out.txt" && ok "in use: a venv whose folder a schedule cds into stays" || bad "in use: scheduled venv"
[ "$(posted "$T/dev/h/sub/target")" = 0 ] && grep -q '.amux-keep in' "$T/out.txt" && ok "in use: a .amux-keep in an ancestor keeps it" || bad "in use: keep marker"
[ "$(posted "$T/dev/i/.venv")" = 0 ] && ok "in use: a venv a LaunchAgent runs stays" || bad "in use: LaunchAgent"
[ "$(posted "$T/dev/a/target")" = 1 ] && ok "in use: a DISABLED schedule naming a path does not keep it" || bad "in use: disabled schedule"
[ "$(stale_posted cron-input.tar)" = 0 ] && ok "in use: a crontab line naming a stale file keeps it" || bad "in use: crontab"

# ── stale data ──
[ "$(stale_posted "$H/Downloads/old installer.dmg")" = 1 ] && ok "stale: a file unopened 200d is quarantined under disk-reclaim-stale" || bad "stale: unopened file quarantined"
grep '^disk-reclaim-tick' "$T/posts.log" | grep -q -F "$H/" && bad "stale file must not ride the build batch" || ok "stale: files go in their own batch, never the build one"
[ "$(stale_posted opened.iso)" = 0 ] && ok "stale: Spotlight last-used inside the window keeps it" || bad "stale: Spotlight last-used"
[ "$(stale_posted atime.zip)" = 0 ] && ok "stale: atime inside the window keeps it" || bad "stale: atime"
[ "$(stale_posted big.bin)" = 0 ] && ok "stale: ~/Library is never entered" || bad "stale: Library excluded"
[ "$(stale_posted .ollama)" = 0 ] && ok "stale: dot-dirs are never entered" || bad "stale: dot-dir excluded"
[ "$(stale_posted track.mp3)" = 0 ] && ok "stale: an app-indexed media folder (iTunes Media) is never entered" || bad "stale: iTunes Media excluded"
[ "$(stale_posted x.heic)" = 0 ] && ok "stale: nothing is pulled out of a bundle" || bad "stale: bundle excluded"
[ "$(stale_posted open.mov)" = 0 ] && grep -q 'open.mov (open in a running process)' "$T/out.txt" && ok "stale: a file open in a process stays" || bad "stale: open file kept"
[ "$(stale_posted young.mov)" = 0 ] && ok "stale: a big file modified 10d ago stays" || bad "stale: young file kept"
grep -q 'STALEOLD?confirm=STALEOLD' "$T/deletes.log" && ok "stale: a batch past its 30d window is purged" || bad "stale: expired purge"
grep -q 'STALENEW' "$T/deletes.log" && bad "stale batch inside 30d must stay restorable" || ok "stale: a 4d-old batch outlives the build class's 72h"
[ "$(status 'c["stale"]["picked"], c["stale"]["kept_open"], c["stale"]["retain_s"], c["build"]["picked"]')" = "(1, 1, 2592000, 2)" ] \
  && ok "status file carries each class's counts and window" || bad "status file: $(status 'c')"

# ── knobs from the global scope; an explicit env value wins ──
printf 'OTHER=1\nRECLAIM_STALE_DAYS="400"\n' > "$T/scope.env"
SCOPE="$T/scope.env" run
[ "$(stale_posted "old installer.dmg")" = 0 ] && grep -q 'unused>=400d' "$T/out.txt" && ok "scope: RECLAIM_STALE_DAYS from the Scope tab applies" || bad "scope knob applies"
SCOPE="$T/scope.env" RECLAIM_STALE_DAYS=90 run
[ "$(stale_posted "old installer.dmg")" = 1 ] && ok "scope: an explicit env value beats the scope file" || bad "env beats scope"
RECLAIM_STALE=0 run
[ "$(grep -c '^disk-reclaim-stale' "$T/posts.log")" = 0 ] && [ "$(status 'c["stale"]["enabled"]')" = False ] && ok "RECLAIM_STALE=0 turns the class off and says so" || bad "stale off switch"

# ── schedule list unreadable: nothing moves ──
mv "$T/schedules.json" "$T/schedules.off"; run; mv "$T/schedules.off" "$T/schedules.json"
[ ! -s "$T/posts.log" ] && grep -q 'verdict=reclaim_tick_refs_unmeasured' "$T/out.txt" && ok "in use: no readable schedule list moves nothing and says so" || bad "in use: refs unmeasured"

# ── pressure tiers (a quarantine frees nothing until its purge) ──
mkscan true; run
[ "$(posted "$T/dev/t30/target")" = 0 ] && ok "normal tier: a 30h-idle dir waits for 48h" || bad "normal tier keeps 30h-idle"
grep -q 'OWN8H' "$T/deletes.log" && bad "normal tier must keep an 8h-old batch" || ok "normal tier keeps an 8h-old batch restorable"
FREE_KB=$((100 * 1048576)) run
grep -q 'verdict=reclaim_tick_tight' "$T/out.txt" && ok "under 150G the run says it is tight" || bad "tight verdict"
[ "$(posted "$T/dev/t30/target")" = 1 ] && ok "tight tier: a 30h-idle dir is picked (idle>=24h)" || bad "tight tier picks 30h-idle"
grep -q 'OWN8H' "$T/deletes.log" && bad "tight tier must keep an 8h-old batch (24h window)" || ok "tight tier keeps an 8h-old batch (24h window)"
FREE_KB=$((40 * 1048576)) run
grep -q 'verdict=reclaim_tick_critical' "$T/out.txt" && ok "under 60G the run says it is critical" || bad "critical verdict"
grep -q 'OWN8H?confirm=OWN8H' "$T/deletes.log" && ok "critical tier purges an 8h-old batch (6h window)" || bad "critical tier purge"
grep -q 'OWNNEW' "$T/deletes.log" && bad "critical tier must keep a 1h-old batch" || ok "critical tier still keeps a 1h-old batch"
FREE_KB=$((40 * 1048576)) RECLAIM_RETAIN_H=72 run
grep -q 'OWN8H' "$T/deletes.log" && bad "an explicit RECLAIM_RETAIN_H must win" || ok "an explicit RECLAIM_RETAIN_H wins over the tier"

# ── docker: every running VM, cache window by tier ──
mkdir -p "$T/fakebin"
printf '#!/bin/sh\necho "$@" >> "%s/docker.log"\nexit 0\n' "$T" > "$T/fakebin/docker"; chmod +x "$T/fakebin/docker"
printf '%s\n' '{"name":"goal-shared","status":"Running","runtime":"docker"}' '{"name":"gs12-local","status":"Running","runtime":"docker"}' \
  '{"name":"idle","status":"Stopped","runtime":"docker"}' '{"name":"k","status":"Running","runtime":"containerd"}' > "$T/colima.json"
printf '#!/bin/sh\ncase "$1" in *gs12-local) echo " 92%%" ;; *) echo " 10%%" ;; esac\n' > "$T/vmpct.sh"; chmod +x "$T/vmpct.sh"
dockrun() { : > "$T/docker.log"; PATH="$T/fakebin:$PATH" RECLAIM_TOOLS=1 RECLAIM_DOCKER_CTX_CMD="cat $T/colima.json" RECLAIM_VM_PCT_CMD="${VMPCT:-echo 10%}" run --dry-run "$@"; }
dockrun
grep -q 'docker --context colima-goal-shared builder prune -f --filter until=24h' "$T/out.txt" && grep -q 'context colima-gs12-local builder prune' "$T/out.txt" \
  && ok "build cache is pruned in every running docker VM" || bad "every running VM"
grep -qE 'colima-idle|colima-k ' "$T/out.txt" && bad "a stopped or non-docker VM must be skipped" || ok "a stopped or non-docker VM is skipped"
grep -q 'builder prune' "$T/docker.log" && bad "a dry run must not prune" || ok "a dry run prunes nothing"
FREE_KB=$((100 * 1048576)) dockrun
grep -q 'until=12h' "$T/out.txt" && ok "tight tier prunes build cache unused 12h" || bad "tight cache window"
FREE_KB=$((40 * 1048576)) dockrun
grep -q 'until=6h' "$T/out.txt" && ok "critical tier prunes build cache unused 6h" || bad "critical cache window"
VMPCT="$T/vmpct.sh" dockrun
grep -q 'context colima-gs12-local builder prune -f --filter until=6h' "$T/out.txt" && ok "a VM 85%+ full gets the 6h cache window" || bad "full VM gets 6h"
grep -q 'context colima-goal-shared builder prune -f --filter until=24h' "$T/out.txt" && ok "a roomy VM beside it keeps 24h" || bad "roomy VM keeps 24h"
grep -q 'verdict=reclaim_tick_vm_full docker\[colima-gs12-local\] disk 92%' "$T/out.txt" && ok "the full VM is named with its percent" || bad "vm_full verdict"

# ── dry run ──
run --dry-run
[ ! -s "$T/posts.log" ] && [ ! -s "$T/deletes.log" ] && ok "dry run posts and deletes nothing" || bad "dry run is inert"
grep -q 'would move 2 path' "$T/out.txt" && grep -q 'would move 1 file' "$T/out.txt" && grep -q 'would purge batch OWNOLD' "$T/out.txt" && ok "dry run says what it would do" || bad "dry run report"

# ── ownership not measured ──
mkscan false; run
[ ! -s "$T/posts.log" ] && grep -q 'verdict=reclaim_tick_unmeasured' "$T/out.txt" && ok "unmeasured ownership moves nothing and says so" || bad "unmeasured ownership"

# ── a scan longer than the schedule's budget (SCHED-783, 2026-10-22 04:00) ──
mkscan true; touch "$T/scan-pending"
RECLAIM_SCAN_WAIT_S=2 RECLAIM_SCAN_POLL_S=1 run; rm -f -- "$T/scan-pending"
[ "$(cat "$T/rc")" = 0 ] && grep -q 'scan: S2 still running after .*verdict=reclaim_scan_pending' "$T/out.txt" && [ ! -s "$T/posts.log" ] \
  && ok "a scan still running when the wait ends: exit 0, nothing moved, says the next run uses it" || { bad "scan pending"; tail -5 "$T/out.txt"; }
limit=$(sed -nE 's/^pub const SHELL_TIMEOUT_S: u64 = ([0-9]+);.*/\1/p' crates/amux-server/src/runtime_jobs/scheduler.rs)
wait_s=$(sed -nE 's/^SCAN_WAIT_S=\$\{RECLAIM_SCAN_WAIT_S:-([0-9]+)\}.*/\1/p' "$TICK")
[ -n "$limit" ] && [ -n "$wait_s" ] && [ $((wait_s + 240)) -le "$limit" ] \
  && ok "the default scan wait (${wait_s}s) leaves 240s of the scheduler's ${limit}s for the rest" || bad "scan wait ${wait_s:-?}s vs scheduler limit ${limit:-?}s"

# ── one deadline for the whole run (SCHED-783, 2026-10-23 12:12) ──
# A prune that outlasts the run's share is stopped, the steps after it are
# skipped and counted, and the run still reaches its summary inside the budget.
# Every tool here is a fake: a real npm or brew must never run from this suite.
mkdir -p "$T/slowbin"
printf '#!/bin/sh\nsleep 30\n' > "$T/slowbin/uv"
for b in npm pnpm brew xcrun docker; do printf '#!/bin/sh\necho "%s $*" >> "%s/tools.log"\n' "$b" "$T" > "$T/slowbin/$b"; done
chmod +x "$T/slowbin"/*; : > "$T/tools.log"; mkscan true
t0=$SECONDS
PATH="$T/slowbin:$PATH" RECLAIM_TOOLS=1 RECLAIM_DOCKER_CTX_CMD="cat $T/colima.json" RECLAIM_DEADLINE_S=16 RECLAIM_RESERVE_S=2 run
took=$((SECONDS - t0))
grep -q 'tool: uv cache prune stopped at its .*verdict=reclaim_tool_timeout' "$T/out.txt" \
  && ok "a prune past the run deadline is stopped and says so" || { bad "slow prune stopped"; grep 'tool:' "$T/out.txt"; }
grep -q 'tool: brew cleanup skipped.*verdict=reclaim_deadline_skip' "$T/out.txt" && ! grep -q '^brew' "$T/tools.log" \
  && ok "steps with no time left are skipped, not started" || { bad "deadline skip"; grep 'tool:' "$T/out.txt"; }
[ "$(cat "$T/rc")" = 0 ] && grep -qE 'done dry=0 .*deadline_skipped=[1-9]' "$T/out.txt" && [ "$took" -lt 25 ] \
  && ok "the run still ends with its summary inside the deadline (${took}s)" || { bad "summary inside deadline (${took}s rc=$(cat "$T/rc"))"; tail -3 "$T/out.txt"; }
deadline=$(sed -nE 's/^DEADLINE_S=\$\{RECLAIM_DEADLINE_S:-([0-9]+)\}.*/\1/p' "$TICK")
[ -n "$limit" ] && [ -n "$deadline" ] && [ "$deadline" -lt "$limit" ] \
  && ok "the run deadline (${deadline}s) is under the scheduler's ${limit}s" || bad "deadline ${deadline:-?}s vs scheduler ${limit:-?}s"

# ── no rm on a candidate, ever ──
grep -nE '(^|[^a-z_])rm ' "$TICK" | grep -v -- '"${WORK:?}"' | grep -v '^[0-9]*:#' > "$T/rm.txt" || true
[ ! -s "$T/rm.txt" ] && ok "the tick has no rm except its own scratch dir" || { bad "rm found"; cat "$T/rm.txt"; }

echo "disk-reclaim-tick: $pass passed, $fail failed"
[ "$fail" = 0 ]
