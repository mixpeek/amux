#!/usr/bin/env python3
"""Daily cloud.amux.io health check + deterministic AUTOFIX.

Checks cloud is online and every customer environment works, AUTO-REPAIRS the failure
modes that are deterministic and safe, and ESCALATES everything else. Every action
leaves a trace (stdout + a host JSONL ledger + a board escalation when it cannot fix).

WHY THIS EXISTS (the 2026-08-16..18 outage). cloud.amux.io was 502 for days. Two root
causes, both fixed BY HAND: (1) the host root disk was 100% full; (2) a failed deploy
ran out of disk mid-write and TRUNCATED /etc/amux/gateway.env (missing
CLERK_PUBLISHABLE_KEY), so gateway.py crash-looped (KeyError) and nginx returned 502.
Nothing detected either automatically. This script encodes the exact hand-repairs so the
next occurrence self-heals within a day, or escalates loudly, instead of staying dark.

SAFE deterministic repairs (each leaves a trace):
  - gateway crash-looping AND gateway.env missing critical keys -> restore gateway.env
    from the newest good backup (merged with current), restart gateway.
  - gateway down AND disk full of LOGS -> truncate container json-logs + journald, restart.
  - gateway crash-looping for another reason -> restart gateway once.
ESCALATE, never auto-act (ethos rule 8 — customer data is the owner's):
  - disk full of DATA (volumes), not logs -> cannot delete; alert + board.
  - any repair that did not restore service -> alert + board.

USAGE
  python3 cloud/cloud_autofix.py           # check + autofix + report
  python3 cloud/cloud_autofix.py --no-fix  # check + report only (dry, no repairs)
  python3 cloud/cloud_autofix.py --json     # machine-readable summary
Exit 0 = healthy (or repaired); 1 = still broken / escalated.
"""
import json
import os
import subprocess
import sys
import time

CLOUD = "https://cloud.amux.io"
HOST = os.environ.get("AMUX_CLOUD_HOST", "34.121.177.76")
SSH_KEY = os.path.expanduser("~/.ssh/amux_cloud")
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRITICAL_ENV_KEYS = ["CLERK_PUBLISHABLE_KEY", "CLERK_SECRET_KEY", "COOKIE_SECRET"]

TRACE = []


def trace(action, detail, ok=None):
    row = {"ts": int(time.time()), "action": action, "detail": detail, "ok": ok}
    TRACE.append(row)
    print("  [autofix] %s: %s%s" % (action, detail, "" if ok is None else (" -> %s" % ("ok" if ok else "FAILED"))),
          file=sys.stderr)


def ssh(script, timeout=90):
    """Run a python3 script on the cloud host via stdin. Returns stdout or ''."""
    try:
        r = subprocess.run(
            ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=8",
             "-i", SSH_KEY, "root@%s" % HOST, "python3 -"],
            input=script, capture_output=True, text=True, timeout=timeout)
        return r.stdout.strip()
    except Exception as e:
        return "SSH_ERROR: %s" % str(e)[:80]


def probe_cloud():
    """HTTP status at CLOUD/. 302 = healthy (auth redirect); 5xx/000 = down."""
    try:
        r = subprocess.run(["curl", "-sk", "-o", "/dev/null", "-w", "%{http_code}",
                            "--max-time", "12", CLOUD + "/"], capture_output=True, text=True, timeout=20)
        return int(r.stdout.strip() or 0)
    except Exception:
        return 0


# Host-side diagnostic: returns one JSON line the local logic acts on.
_DIAG = r'''
import json, os, subprocess
def run(*a):
    try: return subprocess.run(a, capture_output=True, text=True, timeout=30).stdout.strip()
    except Exception: return ""
out = {}
out["gateway_active"] = run("systemctl","is-active","amux-gateway")
out["gateway_nrestarts"] = run("systemctl","show","amux-gateway","-p","NRestarts","--value")
df = run("df","-B1M","--output=pcent,avail","/").splitlines()
if len(df) >= 2:
    pcent, avail = df[-1].split()
    out["disk_pct"] = int(pcent.strip().rstrip("%")); out["disk_free_mb"] = int(avail)
env = "/etc/amux/gateway.env"
present = set()
try:
    for line in open(env):
        if "=" in line and not line.startswith("#"): present.add(line.split("=",1)[0])
except Exception: pass
out["env_missing"] = [k for k in ["CLERK_PUBLISHABLE_KEY","CLERK_SECRET_KEY","COOKIE_SECRET","CONTAINER_SCHEME"] if k not in present]
# logs vs data: container json-logs + journald size (MB) that we CAN safely reclaim
logs = 0
try:
    import glob
    for f in glob.glob("/var/lib/docker/containers/*/*-json.log"):
        try: logs += os.path.getsize(f)
        except Exception: pass
except Exception: pass
out["reclaimable_log_mb"] = logs // (1024*1024)
# newest good gateway.env backup that carries the critical keys
best = None
try:
    import glob
    for b in sorted(glob.glob("/etc/amux/gateway.env.*"), key=lambda p: os.path.getmtime(p), reverse=True):
        ks = set()
        for line in open(b):
            if "=" in line: ks.add(line.split("=",1)[0])
        if {"CLERK_PUBLISHABLE_KEY","CLERK_SECRET_KEY","COOKIE_SECRET"} <= ks:
            best = b; break
except Exception: pass
out["env_backup"] = best
print(json.dumps(out))
'''


def diagnose():
    raw = ssh(_DIAG)
    try:
        return json.loads(raw.splitlines()[-1])
    except Exception:
        return {"error": raw[:120]}


def fix_gateway_env(backup):
    """Restore gateway.env from a known-good backup merged with current (atomic)."""
    script = r'''
import os, tempfile
BAK = %r
def load(p):
    d = {}
    try:
        for line in open(p):
            line = line.rstrip("\n")
            if line and not line.startswith("#") and "=" in line:
                k, v = line.split("=", 1); d[k] = v
    except FileNotFoundError: pass
    return d
bak = load(BAK); cur = load("/etc/amux/gateway.env")
if not {"CLERK_PUBLISHABLE_KEY","CLERK_SECRET_KEY","COOKIE_SECRET"} <= set(bak):
    print("ABORT: backup missing critical keys"); raise SystemExit(1)
merged = dict(bak); merged.update(cur); merged.setdefault("CONTAINER_SCHEME", "https")
fd, tmp = tempfile.mkstemp(dir="/etc/amux")
with os.fdopen(fd, "w") as f:
    for k, v in merged.items(): f.write("%%s=%%s\n" %% (k, v))
os.chmod(tmp, 0o600); os.replace(tmp, "/etc/amux/gateway.env")
print("restored %%d keys" %% len(merged))
''' % backup
    out = ssh(script)
    ok = "restored" in out
    trace("restore_gateway_env", "from %s: %s" % (os.path.basename(backup or "?"), out[:60]), ok)
    return ok


def fix_logs():
    # `truncate -s 0`, NOT open(f,"w").close(). Replacing the file contents out
    # from under the docker daemon WEDGES `docker logs` for that container until
    # it is restarted — measured 2026-08-27: an open()-truncate of 7 live json
    # logs left all 7 `docker logs` hanging, which crashed the backup-freshness
    # sweep. `truncate` keeps the same inode/offset the daemon is tracking, so the
    # log reader stays healthy. journald vacuum is unaffected.
    # Also truncate /var/log/*.log — the biggest single reclaimable log on this
    # host is /var/log/amux-gateway.log (~52MB), which the container-json glob
    # missed, so the automatic path freed less than a hand-truncation did and the
    # disk kept climbing (AC-414). truncate -s 0 is inode-safe here too: a process
    # holding the fd open for append keeps writing at its old offset (sparse
    # regrow), and df is relieved immediately.
    out = ssh(r'''
import subprocess, glob, os
n = 0
for f in glob.glob("/var/lib/docker/containers/*/*-json.log") + glob.glob("/var/log/*.log"):
    try:
        if os.path.getsize(f) > 20*1024*1024:
            subprocess.run(["truncate", "-s", "0", f], timeout=10); n += 1
    except Exception: pass
subprocess.run(["journalctl", "--vacuum-size=100M"], capture_output=True)
print("truncated %d logs" % n)
''')
    trace("truncate_logs", out[:60], "truncated" in out)
    return "truncated" in out


def restart_gateway():
    out = ssh("import subprocess; subprocess.run(['systemctl','restart','amux-gateway']); "
              "import time; time.sleep(5); "
              "print(subprocess.run(['systemctl','is-active','amux-gateway'],capture_output=True,text=True).stdout.strip())")
    ok = out.strip().endswith("active")
    trace("restart_gateway", "state=%s" % out.strip()[:20], ok)
    return ok


def escalate_board(summary, detail):
    """Board-only escalation: for problems worth a human's queue but not the owner
    fire-alarm (e.g. cloud serves traffic fine but the env-check instrument is down).
    escalate() layers the alert on top of this for real outages.

    DEDUPED by title-prefix: a persistent condition (a multi-day CI freeze, a
    standing warn) must UPDATE its existing open card, not file a new one every
    daily run — otherwise the sweep accumulates instead of discriminating (ethos
    rule 5; the AC-344 freeze escalation would have spawned one card per day)."""
    trace("escalate_board", summary, None)
    title = "cloud-autofix: %s" % summary
    # A stable key: the summary up to its first em-dash/number so "FROZEN — 4
    # commits" and "FROZEN — 5 commits" collapse to one running card.
    key = title.split("—")[0].strip().rstrip("0123456789 ")
    try:
        base = subprocess.run(["amux", "url"], capture_output=True, text=True, timeout=10).stdout.strip()
        existing = None
        rows = subprocess.run(["curl", "-sk", "%s/api/board" % base],
                              capture_output=True, text=True, timeout=15).stdout
        for it in json.loads(rows):
            t = it.get("title") or ""
            if t.split("—")[0].strip().rstrip("0123456789 ") == key and it.get("status") in ("needsyou", "todo", "doing"):
                existing = it.get("id"); break
        if existing:
            # Refresh title + append a dated line; keep the human's context intact.
            out = subprocess.run(["curl", "-sk", "-X", "PATCH", "-H", "Content-Type: application/json",
                                  "-H", "X-Amux-Session:%s" % os.environ.get("AMUX_SESSION", "cloud-autofix"),
                                  "-d", json.dumps({"title": title,
                                                    "desc_append": "\n\n[autofix re-observed %d] %s" % (int(time.time()), detail)}),
                                  "%s/api/board/%s" % (base, existing)], capture_output=True, text=True, timeout=15).stdout
        else:
            # status "todo", not "needsyou": the board refuses an untyped needsyou
            # (needsyou_requires_ask_type) and this caller has no typed human ask —
            # the condition is work for whoever owns it, not a question for Ethan.
            # The 09-01 FROZEN escalation was silently refused on exactly this.
            out = subprocess.run(["curl", "-sk", "-X", "POST", "-H", "Content-Type: application/json",
                                  "-H", "X-Amux-Session:%s" % os.environ.get("AMUX_SESSION", "cloud-autofix"),
                                  "-d", json.dumps({"title": title, "desc": detail,
                                                    "status": "todo", "session": "amux-cloud"}),
                                  "%s/api/board" % base], capture_output=True, text=True, timeout=15).stdout
        # Read the answer, never assume it (ethos rule 4: a card id beside
        # "escalated"). A gate refusal is a 200-shaped JSON with ok:false/error.
        resp = json.loads(out) if out.strip() else {}
        if resp.get("ok") is False or resp.get("error"):
            trace("escalate_board", "BOARD WRITE REFUSED: %s" % (resp.get("code") or resp.get("error") or "?")[:80], False)
        else:
            trace("escalate_board", "card %s" % (resp.get("id") or existing or "?"), True)
    except Exception as e:
        trace("escalate_board", "BOARD WRITE FAILED: %s" % str(e)[:80], False)


def escalate(summary, detail):
    trace("escalate", summary, None)
    # board card (attributed) so the fleet sees it even if paging is down
    escalate_board(summary, detail)
    # fire-alarm (email channel is repaired; push/sms are owner setup)
    try:
        subprocess.run(["amux", "alert", "cloud-autofix could not self-heal: %s. %s" % (summary, detail),
                        "Cloud health autofix escalation"], capture_output=True, text=True, timeout=20)
    except Exception:
        pass


def check_envs(retries=1):
    """Run the per-environment/persona suite for a green/red matrix (read-only).
    One retry on error: a single sample cannot tell a transient 401 (token-mint or
    container-wake race, seen 2026-08-22 — the direct re-run passed seconds later)
    from a real auth outage, and a once-daily check must not cry wolf on a blip."""
    last = {"error": "no attempt"}
    for attempt in range(retries + 1):
        try:
            r = subprocess.run([sys.executable, os.path.join(REPO, "cloud/tests/e2e_personas.py"), "--json"],
                               capture_output=True, text=True, timeout=600, cwd=REPO)
            out = (r.stdout or "").strip()
            if not out:
                last = {"error": "no output"}
            else:
                # e2e_personas.py --json emits a SINGLE pretty-printed JSON object across ~191
                # lines (progress goes to stderr). Parse the WHOLE stdout — splitlines()[-1]
                # grabbed the closing "}" and errored every run, so this env check was silently
                # dead and the daily health rode the 302 probe alone (ethos rule 7, fixed here).
                try:
                    return json.loads(out)
                except json.JSONDecodeError:
                    try:
                        return json.loads(out.splitlines()[-1])  # fallback: trailing-JSON-line format
                    except Exception:
                        last = {"error": "unparseable output: %s" % out[:80]}
        except Exception as e:
            last = {"error": str(e)[:100]}
        if attempt < retries:
            time.sleep(10)
    last["retried"] = retries
    return last


def restart_stopped_workers(env_result):
    """Restart plan-declared workers a recreate left stopped (AC-407).

    A container recreate — deploy `recreate=yes`, a per-workspace admin recreate,
    or a host reboot — stops every tmux session, and the rust server does not
    restore them (it has no AMUX_AUTOSTART_SESSIONS). Until the server grows that,
    the persona suite is the only thing that notices, and it only WARNs. This
    restarts exactly the DECLARED workers that are present-but-stopped, which
    restores the env's OWN configured set rather than imposing one (ethos rule 8:
    a declared persona a recreate knocked down is continuity, not a new decision),
    over the container's internal API via SSH. Returns [(worker, http_code)].

    Verified by hand 2026-09-01 on capital-express (org_37aa…, recreated 17:14 by a
    non-deploy op): POST /api/sessions/<name>/start -> 202, workers returned to idle."""
    org = env_result.get("org_id")
    stopped = [p["name"] for p in env_result.get("personas", [])
               if p.get("present") and not p.get("running")]
    if not org or not stopped:
        return []
    c = "amux-user-%s" % org
    done = []
    for name in stopped:
        # Internal rust port is 8822 in every container (compose maps <ext>:8822).
        out = ssh(
            "import subprocess;"
            "print(subprocess.run(['docker','exec',%r,'bash','-lc',"
            "'curl -sk -o /dev/null -w \"%%{http_code}\" -X POST "
            "https://localhost:8822/api/sessions/%s/start'],"
            "capture_output=True,text=True,timeout=30).stdout.strip())" % (c, name),
            timeout=45)
        done.append((name, (out or "").strip()[:12]))
    return done


def check_deploy_freshness():
    """Is the cloud image behind origin/main, and WHY (AC-344). The auto-deploy
    (deploy-cloud.yml) is gated on green rust CI via workflow_run, so when main CI
    is RED the deploy shows 'skipped' — byte-identical to 'nothing to deploy'. The
    image then freezes and falls behind, invisibly, until a human notices; the card
    records this happening 3x, each caught by hand. This joins the three signals no
    single view joins — last successful deploy sha, origin/main tip, and rust CI
    status — and names the cause: FROZEN (behind + CI red) vs normal lag (behind +
    CI green, auto-deploy will catch up) vs current. Runs locally (gh + git)."""
    def sh(*a):
        try:
            return subprocess.run(a, capture_output=True, text=True, timeout=30, cwd=REPO).stdout.strip()
        except Exception:
            return ""
    sh("git", "fetch", "origin", "-q")
    deployed = sh("gh", "run", "list", "--workflow=deploy-cloud.yml", "-L", "20",
                  "--json", "headSha,conclusion", "-q",
                  'map(select(.conclusion=="success"))[0].headSha')
    if not deployed:
        return {"error": "no successful deploy-cloud run found (gh failed?)"}
    behind = sh("git", "rev-list", "--count", "%s..origin/main" % deployed)
    behind = int(behind) if behind.isdigit() else -1
    res = {"deployed": deployed[:12], "behind": behind}
    if behind <= 0:
        res["state"] = "current"
        return res
    # Behind — is main CI red (frozen) or green (normal lag)?
    ci = sh("gh", "run", "list", "--workflow=rust.yml", "--branch=main", "-L", "1",
            "--json", "conclusion,headSha,status", "-q", ".[0]")
    try:
        ci = json.loads(ci) if ci else {}
    except Exception:
        ci = {}
    concl = ci.get("conclusion")
    res["ci_conclusion"] = concl
    if concl == "failure":
        res["state"] = "FROZEN"  # behind AND CI red -> auto-deploy is silently skipping
    elif ci.get("status") in ("in_progress", "queued"):
        res["state"] = "deploying"  # CI running, catch-up in flight
    else:
        res["state"] = "lag"  # behind but CI green -> normal, will catch up
    return res


def check_disk():
    """Root-disk usage AND a breakdown of the top consumers when it is high.
    AC-348: the disk-full alarm fires but names no cause, so every incident meant
    SSHing to run du/docker-system-df by hand (2026-08-27: 94% full, and the 6.7G
    the tools called 'reclaimable' was actually pinned to running containers, so
    the honest remedy was not a prune). This makes the NEXT climb self-explain:
    when disk >= 85% it reports docker image/volume reclaimable, oversized logs,
    and the same-host backup dir, so a human sees WHERE before deciding what is
    safe to touch (volumes may be customer data — never auto-pruned, ethos 8)."""
    out = ssh(r'''
import json, subprocess, os
def run(*a):
    try: return subprocess.run(a, capture_output=True, text=True, timeout=40).stdout.strip()
    except Exception: return ""
st = os.statvfs("/")
pct = round(100.0 * (st.f_blocks - st.f_bfree) / st.f_blocks, 1)
free_gb = round(st.f_bavail * st.f_frsize / 1e9, 1)
res = {"pct": pct, "free_gb": free_gb}
if pct >= 85:
    top = {}
    for line in run("docker","system","df","--format","{{.Type}}\t{{.Reclaimable}}").splitlines():
        p = line.split("\t")
        if len(p) == 2: top[p[0]] = p[1]
    res["docker_reclaimable"] = top
    bk = run("du","-sh","/var/amux/backups")
    res["backups_dir"] = bk.split()[0] if bk else "0"
    big = [l for l in run("bash","-c",
        "find /var/lib/docker/containers -name '*-json.log' -size +20M -exec du -h {} + 2>/dev/null | sort -rh | head -3").splitlines()]
    res["oversized_logs"] = big
print(json.dumps(res))
''', timeout=60)
    try:
        return json.loads(out)
    except Exception:
        return {"error": (out.strip()[:100] if out and out.strip() else "ssh returned no output (host unreachable or command produced nothing)")}


def check_backups():
    """Litestream replication freshness for every RUNNING env. Litestream->S3 is
    the real backup of customer DBs (the nightly backup-cloud.yml workflow made
    same-host copies and is disabled_manually since ~08-15); AMUX-2802's lesson
    is that backups stopped for NINE DAYS and the only witness was an unowned
    card. This check makes staleness self-announcing where the daily sweep
    already looks. A running env whose litestream sidecar is missing, or whose
    last 'replica sync' line is older than 15 minutes, is reported stale."""
    out = ssh(r'''
import json, subprocess, datetime
def run(*a):
    try: return subprocess.run(a, capture_output=True, text=True, timeout=30).stdout
    except Exception: return ""
running = [n for n in run("docker","ps","--format","{{.Names}}").split() if n.startswith("amux-user-")]
stale, fresh = [], 0
now = datetime.datetime.now(datetime.timezone.utc)
for n in running:
    env = n[len("amux-user-"):]
    ls = "amux-litestream-" + env
    # litestream logs go to stderr — capture both streams. GUARD the call: a
    # single container whose `docker logs` HANGS (a truncated json-log can wedge
    # docker's log reader) must not crash the whole sweep — before this guard one
    # hung sidecar raised TimeoutExpired here and the entire check returned empty,
    # so 7 healthy envs went unreported (2026-08-27). Short 10s timeout so 8
    # containers stay well inside the ssh budget.
    try:
        tail = subprocess.run(["docker","logs","--tail","40",ls], capture_output=True, text=True, timeout=10)
        text = (tail.stdout or "") + (tail.stderr or "")
    except Exception:
        stale.append(env + " (docker logs timed out — sidecar log reader wedged, restart it)")
        continue
    if "No such container" in text or not text.strip():
        stale.append(env + " (no litestream sidecar)")
        continue
    last = None
    for line in text.splitlines():
        if "replica sync" in line and line.startswith("time="):
            last = line.split("time=",1)[1].split(" ",1)[0]
    if not last:
        stale.append(env + " (no replica-sync line in recent logs)")
        continue
    try:
        ts = datetime.datetime.fromisoformat(last.replace("Z","+00:00"))
        age = (now - ts).total_seconds()
        if age > 900:
            stale.append(env + " (last sync %dm ago)" % (age // 60))
        else:
            fresh += 1
    except Exception:
        stale.append(env + " (unparseable sync time: %s)" % last[:30])
print(json.dumps({"running": len(running), "fresh": fresh, "stale": stale}))
''', timeout=60)
    try:
        return json.loads(out)
    except Exception:
        return {"error": (out.strip()[:100] if out and out.strip() else "ssh returned no output (host unreachable or command produced nothing)")}


def check_orphans():
    """Running amux-user containers with NO gateway.db org/user row. The deploy is
    DIRECTORY-driven (deploy-cloud.yml loops /var/amux/users/*/), so a workspace dir
    left behind by an incomplete deletion gets its container RESURRECTED even after
    the org is gone from the DB — 6 came back on 2026-08-18 (AC-373). Report-only:
    a DB-less container can also be a brief mid-provision race, so surfacing it in
    the trace ledger (which a sweep reads) is the fix, not an unattended delete."""
    # The host has NO sqlite3 CLI — query the gateway DB via the python sqlite3
    # module. A `sqlite3 …` CLI version of this returned '' for every id (command
    # not found), which flags EVERY container as an orphan — the exact ethos-rule-7
    # instrument that reports a confident wrong answer (caught 2026-08-18).
    out = ssh(r'''
import json, subprocess, sqlite3
def run(*a):
    try: return subprocess.run(a, capture_output=True, text=True, timeout=30).stdout.strip()
    except Exception: return ""
names = [n for n in run("docker","ps","--format","{{.Names}}").splitlines() if n.startswith("amux-user-")]
orphans = []
missing = []
try:
    c = sqlite3.connect("/var/amux/gateway.db")
    for n in names:
        oid = n[len("amux-user-"):]
        row = c.execute("SELECT 1 FROM orgs WHERE id=? UNION SELECT 1 FROM users WHERE id=? LIMIT 1", (oid, oid)).fetchone()
        if not row:
            orphans.append(oid)
    # INVERSE direction: an amux-user container that EXISTS but is not running
    # (Exited/Created). On 2026-08-22 a force-recreate name-collision left one env
    # Exited(137) plus a stray Created container while the deploy job reported
    # SUCCESS — orphan-checking alone is blind to this whole direction. The
    # discriminator is deliberately NOT "dir with no container": idle envs are
    # scaled to zero by REMOVING their container (15 dirs, 8 running, 0 stopped
    # on a healthy host), so exists-but-stopped is anomalous and dir-absence is
    # normal — the dir version would cry wolf on every idle user env (verified
    # against the live host before shipping this check).
    for line in run("docker","ps","-a","--filter","name=amux-user","--format","{{.Names}}\t{{.State}}").splitlines():
        parts = line.split("\t")
        if len(parts) == 2 and parts[1] != "running":
            missing.append("%s (%s)" % (parts[0], parts[1]))
    print(json.dumps({"running": len(names), "orphans": orphans, "missing": missing}))
except Exception as e:
    print(json.dumps({"running": len(names), "error": str(e)[:80]}))
''', timeout=45)
    try:
        return json.loads(out)
    except Exception:
        return {"error": (out.strip()[:100] if out and out.strip() else "ssh returned no output (host unreachable or command produced nothing)")}


def main():
    no_fix = "--no-fix" in sys.argv
    as_json = "--json" in sys.argv
    disk_only = "--disk-only" in sys.argv
    result = {"trace": TRACE, "healthy": False}

    # --disk-only: a LIGHTWEIGHT stopgap (AC-414). The full sweep runs once a day
    # (SCHED-356), but a net-negative disk gains ~100MB/h and would hit 100%
    # between daily runs — so this fast path (check_disk + preventive fix_logs,
    # skipping the slow persona/deploy/orphan checks) is scheduled every 2h to
    # hold the disk below 100% until the host disk is resized. It is deliberately
    # SILENT on the board: AC-414 already carries the escalation, so this must not
    # file a card every 2h. Retire the schedule when AC-414 is resolved.
    if disk_only:
        _disk = check_disk()
        result["disk"] = _disk
        if _disk.get("error"):
            trace("disk", "ERROR: %s" % _disk["error"], False)
            result["healthy"] = False
        else:
            trace("disk", "root %.1f%% used, %.1fGB free" % (_disk.get("pct", 0), _disk.get("free_gb", 0)),
                  _disk.get("pct", 0) < 90)
            if _disk.get("pct", 0) >= 95 and not no_fix:
                _fb = _disk.get("free_gb", 0)
                fix_logs()
                _disk = check_disk(); result["disk"] = _disk
                trace("disk_preventive", "after truncate: %.1f%% used, %.1fGB free (was %.1fGB)"
                      % (_disk.get("pct", 0), _disk.get("free_gb", 0), _fb), _disk.get("pct", 100) < 95)
            result["healthy"] = _disk.get("pct", 100) < 98
        ssh("import json; open('/var/log/cloud-autofix.jsonl','a').write(%r+chr(10))"
            % json.dumps({"ts": int(time.time()), "disk_only": True, "trace": TRACE}), timeout=20)
        if as_json:
            print(json.dumps(result, indent=2))
        else:
            print("cloud-autofix --disk-only: disk %.1f%% used (%.1fGB free) -> %s"
                  % (result["disk"].get("pct", 0), result["disk"].get("free_gb", 0),
                     "ok" if result["healthy"] else "CRITICAL"))
        sys.exit(0 if result["healthy"] else 1)

    status = probe_cloud()
    result["cloud_status"] = status
    trace("probe", "cloud.amux.io -> %d" % status, status in (200, 301, 302, 401, 403))

    if status in (200, 301, 302, 401, 403):
        # Cloud is serving. Verify the environments too.
        result["healthy"] = True
        result["envs"] = check_envs()
        # Surface a broken or failing env-check LOUDLY (ok=FAILED), not as a bland None:
        # a None reads as "no data", which is how the parse bug above hid for so long.
        _env = result["envs"]
        env_problem = None
        if _env.get("error"):
            trace("check_envs", "ERROR after retry: %s" % _env["error"], False)
            env_problem = ("env check BROKEN (ran + retried, still erroring)",
                           "e2e_personas could not run or parse on either attempt: %s. Cloud "
                           "serves a %d so this is the INSTRUMENT down, not the gateway — but "
                           "env health is unknowable until it is fixed." % (_env["error"], status))
        else:
            _failed = _env.get("failed")
            trace("check_envs", "reachable=%s provisioned=%s passed=%s warned=%s failed=%s" % (
                _env.get("cloud_reachable"), _env.get("provisioned"), _env.get("passed"),
                _env.get("warned"), _failed), (_failed == 0))
            if _failed:
                _bad = ["%s (%s)" % (r0.get("env"), "; ".join(r0.get("reasons") or []))
                        for r0 in _env.get("results", []) if r0.get("status") == "FAIL"]
                env_problem = ("%d customer env FAILURE(s)" % _failed, " | ".join(_bad)[:600])
            # SELF-HEAL: a recreate left declared workers stopped (AC-407). Restart
            # them here rather than only WARNing, so a per-workspace recreate no
            # longer needs a hand on the box. Runs unless --no-fix; a no-op when
            # every declared worker is already running.
            if not no_fix:
                for r0 in _env.get("results", []):
                    restarted = restart_stopped_workers(r0)
                    if restarted:
                        ok_ct = sum(1 for _, code in restarted if code in ("200", "202"))
                        trace("restart_workers", "%s: %s" % (
                            r0.get("stem"), ", ".join("%s->%s" % (n, c) for n, c in restarted)),
                            ok_ct == len(restarted))
        # Orphaned (DB-less) running containers — a deploy resurrection self-announces here.
        result["orphans"] = check_orphans()
        _orph = result["orphans"].get("orphans") or []
        _miss = result["orphans"].get("missing") or []
        trace("orphans", "running=%s orphaned=%s%s stopped=%s%s" % (
            result["orphans"].get("running"), len(_orph),
            (" -> " + ",".join(_orph)) if _orph else "",
            len(_miss), (" -> " + ",".join(_miss)) if _miss else ""), not (_orph or _miss))
        if _miss and not env_problem:
            # A container that exists but is not running is a customer env DOWN in a
            # way the gateway's wake path cannot repair (it wakes absent containers,
            # not wedged ones) — the 2026-08-22 recreate collision shape.
            env_problem = ("%d container(s) exist but are not running" % len(_miss),
                           "Wedged (Exited/Created) amux-user containers: %s. The deploy job "
                           "likely reported green anyway; remove the collided containers and "
                           "`docker compose up -d` in the env dir." % ", ".join(_miss))
        # Backup freshness (AMUX-2802): litestream->S3 is the real customer-DB
        # backup; nine days of silent backup absence is the incident this check
        # exists to make impossible to repeat.
        # Deploy freshness + WHY-behind (AC-344): a silent freeze self-announces.
        result["deploy"] = check_deploy_freshness()
        _dep = result["deploy"]
        if _dep.get("error"):
            trace("deploy", "freshness probe: %s" % _dep["error"], None)
        else:
            trace("deploy", "deployed=%s behind=%s state=%s%s" % (
                _dep.get("deployed"), _dep.get("behind"), _dep.get("state"),
                (" ci=%s" % _dep.get("ci_conclusion")) if _dep.get("ci_conclusion") else ""),
                _dep.get("state") != "FROZEN")
            if _dep.get("state") == "FROZEN" and not env_problem:
                env_problem = ("cloud image FROZEN — %s commits behind, main CI is RED" % _dep.get("behind"),
                               "deploy-cloud auto-deploy is gated on green rust CI, so a red main freezes the "
                               "image and shows 'skipped' (looks like nothing-to-deploy). deployed=%s, behind=%s, "
                               "rust.yml main conclusion=failure. Fix the red main, or dispatch deploy-cloud manually "
                               "once it is green. This is AC-344's exact failure, now self-announcing."
                               % (_dep.get("deployed"), _dep.get("behind")))

        # Root-disk usage + top-consumer breakdown when high (AC-348).
        result["disk"] = check_disk()
        _disk = result["disk"]
        if _disk.get("error"):
            trace("disk", "ERROR: %s" % _disk["error"], False)
        else:
            extra = ""
            if _disk.get("docker_reclaimable"):
                extra = " | docker=%s backups=%s" % (_disk.get("docker_reclaimable"), _disk.get("backups_dir"))
            trace("disk", "root %.1f%% used, %.1fGB free%s" % (_disk.get("pct", 0), _disk.get("free_gb", 0), extra),
                  _disk.get("pct", 0) < 90)
            # PREVENTIVE self-heal (AC-414): fix_logs previously ran ONLY on the
            # DOWN path, so on the healthy path the disk was allowed to climb to
            # 100% and truncate gateway.env before a single log was freed — a
            # preventable outage. Truncate oversized logs here, while cloud is
            # still UP, whenever the disk is critically full, then re-measure so
            # the escalation below carries the POST-truncation number (naming that
            # self-help was tried, and whether it was enough).
            _truncated_this_tick = False
            if _disk.get("pct", 0) >= 95 and not no_fix:
                _free_before = _disk.get("free_gb", 0)
                _truncated_this_tick = fix_logs()
                _disk = check_disk()
                result["disk"] = _disk
                trace("disk_preventive", "after truncate: %.1f%% used, %.1fGB free (was %.1fGB)"
                      % (_disk.get("pct", 0), _disk.get("free_gb", 0), _free_before),
                      _disk.get("pct", 100) < 95)
            hi = _disk.get("pct", 0) >= 90
            if hi and not env_problem:
                _tried = " (logs already truncated this tick — this is the net-negative disk, only a resize or deprovision fixes it)" \
                    if _truncated_this_tick else ""
                env_problem = ("root disk at %.1f%% (%.1fGB free)%s" % (_disk.get("pct"), _disk.get("free_gb"), _tried),
                               "Top consumers — docker reclaimable: %s; same-host backups: %s; oversized logs: %s. "
                               "NOTE: 'reclaimable' images may be pinned to running containers (freed only by a "
                               "recreate), and unused VOLUMES may be customer data — never auto-prune volumes (ethos 8)."
                               % (_disk.get("docker_reclaimable"), _disk.get("backups_dir"), _disk.get("oversized_logs")))

        result["backups"] = check_backups()
        _stale = result["backups"].get("stale") or []
        if result["backups"].get("error"):
            trace("backups", "ERROR: %s" % result["backups"]["error"], False)
        else:
            trace("backups", "litestream fresh=%s/%s%s" % (
                result["backups"].get("fresh"), result["backups"].get("running"),
                (" STALE -> " + "; ".join(_stale)) if _stale else ""), not _stale)
        if _stale and not env_problem:
            env_problem = ("litestream replication STALE for %d env(s)" % len(_stale),
                           "Customer-DB backup stream not current: %s. Litestream->S3 is the "
                           "only off-host backup (nightly workflow disabled since 08-15); a "
                           "stale stream means those DBs have no fresh backup." % "; ".join(_stale))
        # An env layer that is failing or unknowable must dent the verdict — on
        # 2026-08-22 a 401'd env check rode under "HEALTHY exit 0", which is the
        # green-check-that-cannot-fail shape (ethos rule 7). Board-only escalation:
        # the gateway IS up, so the owner fire-alarm stays quiet; warns do not
        # trip this (the standing idle-worker WARN should not page daily).
        if env_problem:
            escalate_board(env_problem[0], env_problem[1])
            result["healthy"] = False
            result["env_problem"] = env_problem[0]
    else:
        # Cloud is DOWN. Diagnose and apply deterministic repairs.
        d = diagnose()
        result["diagnosis"] = d
        trace("diagnose", "gw=%s restarts=%s disk=%s%% env_missing=%s log_mb=%s"
              % (d.get("gateway_active"), d.get("gateway_nrestarts"), d.get("disk_pct"),
                 d.get("env_missing"), d.get("reclaimable_log_mb")), None)
        if no_fix:
            trace("no_fix", "dry run — skipping repairs", None)
        else:
            fixed_something = False
            # Repair 1: FREE DISK FIRST. fix_gateway_env writes a tempfile under
            # /etc/amux, which FAILS on a 100%-full disk — so restoring before
            # freeing space silently no-ops and leaves prod down. That is exactly
            # what happened 2026-09-03 (AC-414): disk 100% -> gateway.env truncated
            # -> restore ran first, could not write, failed -> 502 stood until a
            # human restored by hand. Truncate logs BEFORE the env restore so the
            # restore has room, and the whole outage self-heals in one pass.
            if (d.get("disk_pct", 0) >= 95) and d.get("reclaimable_log_mb", 0) >= 300:
                fixed_something |= fix_logs()
            # Repair 2: truncated gateway.env (the incident's real blocker) — now
            # with space to write its atomic tempfile.
            if d.get("env_missing") and d.get("env_backup"):
                fixed_something |= fix_gateway_env(d["env_backup"])
            # Repair 3: bring the gateway up (covers crash-loop + post-repair).
            restart_gateway()
            # Re-probe.
            time.sleep(3)
            status2 = probe_cloud()
            result["cloud_status_after"] = status2
            trace("reprobe", "cloud.amux.io -> %d" % status2, status2 in (301, 302, 200, 401, 403))
            result["healthy"] = status2 in (200, 301, 302, 401, 403)
            # Escalate what could not be fixed.
            if not result["healthy"]:
                if d.get("disk_pct", 0) >= 95 and d.get("reclaimable_log_mb", 0) < 300:
                    escalate("disk full of DATA, cannot auto-delete (ethos rule 8)",
                             "disk %s%%, %sMB free; reclaimable logs only %sMB. Needs a resize or an owner-authorised reap."
                             % (d.get("disk_pct"), d.get("disk_free_mb"), d.get("reclaimable_log_mb")))
                else:
                    escalate("cloud still 502 after deterministic repairs",
                             "diagnosis=%s; repairs did not restore service." % json.dumps(d)[:300])

    # Persist the trace ledger on the host (best-effort — disk may be full).
    ssh("import json; open('/var/log/cloud-autofix.jsonl','a').write(%r+chr(10))"
        % json.dumps({"ts": int(time.time()), "healthy": result["healthy"], "trace": TRACE}), timeout=20)

    if as_json:
        print(json.dumps(result, indent=2))
    else:
        print("\ncloud-autofix: %s (status %s%s)%s" % (
            "HEALTHY" if result["healthy"] else "UNHEALTHY / ESCALATED",
            result.get("cloud_status"),
            "->%s" % result["cloud_status_after"] if "cloud_status_after" in result else "",
            " — %s" % result["env_problem"] if result.get("env_problem") else ""))
    sys.exit(0 if result["healthy"] else 1)


if __name__ == "__main__":
    main()
