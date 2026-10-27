#!/usr/bin/env python3
"""wake-watch: wake a lane once when a file arrives or a CI run concludes.

Why (Ethan 2026-10-27, MSG-98201/98207/98259). Lanes hand-rolled watcher loops
(ci_watch.sh, rb_boot_watch.sh, chain_done_notify*.sh, ...) under their scratch
dirs: untested, known to one lane, and several died or polled `gh` per lane on
the day this was written. This replaces the pattern with two primitives amux
already has, configured:

- the FILESYSTEM holds the registrations: one JSON file per watch under
  $AMUX_HOME/wake-watch/<lane>/<id>.json;
- a SCHEDULE runs the check: one shell schedule per lane, "wake-watch: <lane>",
  every 1m, `exit_actions {"10": "wake"}`. When a watch fires, `check` prints
  the message and exits 10, and the scheduler delivers that stdout to the
  schedule's own lane (scheduler.rs `shell_wake`), the same path a tmux
  schedule's command takes. So it reaches an isolated lane as that lane's own
  schedule (CLAUDE.md "Isolated workers": schedules are the owner-configured
  exception) and is refused for a paused or archived one. The scheduler retries
  a failed delivery a bounded number of times and records the outcome on the
  run row; a failure also reaches the schedule-failure notice.

Bounds: at most AMUX_WAKE_WATCH_MAX (8) live watches per lane; every watch
expires (default 6h, at most AMUX_WAKE_WATCH_MAX_TTL_S, 7 days) and the lane is
told when one expires unfired; a per-watch poll interval with a floor of
AMUX_WAKE_WATCH_FILE_FLOOR_S (30) for files and AMUX_WAKE_WATCH_CI_FLOOR_S (60)
for CI. CI is read through ONE shared poller per target: the state file under
$AMUX_HOME/ci-watch/<repo>-<key>/state.json is the same one `amux ci wait`
uses, refreshed by whichever reader finds it stale and takes the `poller` lock,
with the GitHub App token. Every other reader only reads the file.

A probe that could not read its input is UNMEASURED and never fires and never
reads as "no event": `status` shows measured=false with why_unmeasured.

Every registration, fire, expiry, prune and failure is a line in
$AMUX_HOME/logs/wake-watch.log with verdict=<word>; failures start with WARN.

Usage:
  wake-watch.py add file --path P [--when exists|changed|line] [--regex R]
                         [--expires 6h] [--every 30] [--note TEXT] [--lane L]
  wake-watch.py add ci --repo O/R (--run ID | --sha SHA [--workflow W] | --workflow W)
                       [--expires 6h] [--every 60] [--note TEXT] [--lane L]
  wake-watch.py status [--lane L] [--json]       read-only; also `ls`
  wake-watch.py rm <ID> [--lane L]
  wake-watch.py check --lane L                   the schedule's command: exit 10 = deliver stdout

Test hooks: WAKE_WATCH_NOW (epoch), WAKE_WATCH_SCHEDULE=0 (do not ensure the
schedule), AMUX_GITHUB_API (API base), WAKE_WATCH_GH_TOKEN (skip get-token.sh).
"""
import argparse
import datetime as dt
import errno
import fcntl
import glob
import json
import os
import re
import secrets
import ssl
import subprocess
import sys
import time
import urllib.error
import urllib.request

EXIT_DELIVER = 10
SCHEDULE_TITLE = "wake-watch: {lane}"
LIVE = "watching"


def env_int(key, default):
    try:
        return int(os.environ.get(key, "").strip() or default)
    except ValueError:
        return default


def home():
    return os.environ.get("AMUX_HOME") or os.path.join(os.path.expanduser("~"), ".amux")


def now():
    v = os.environ.get("WAKE_WATCH_NOW")
    return float(v) if v else time.time()


def iso(ts):
    return dt.datetime.fromtimestamp(ts, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


MAX_PER_LANE = env_int("AMUX_WAKE_WATCH_MAX", 8)
MAX_TTL_S = env_int("AMUX_WAKE_WATCH_MAX_TTL_S", 7 * 86400)
FILE_FLOOR_S = env_int("AMUX_WAKE_WATCH_FILE_FLOOR_S", 30)
CI_FLOOR_S = env_int("AMUX_WAKE_WATCH_CI_FLOOR_S", 60)
KEEP_CLOSED_S = env_int("AMUX_WAKE_WATCH_KEEP_CLOSED_S", 86400)
LINE_READ_CAP = 8 * 1024 * 1024


def lane_dir(lane):
    return os.path.join(home(), "wake-watch", lane)


def log(verdict, warn=False, **kv):
    path = os.path.join(home(), "logs", "wake-watch.log")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    parts = [iso(now())] + (["WARN"] if warn else []) + ["verdict=" + verdict]
    for k, v in kv.items():
        s = str(v).replace("\n", " ")
        parts.append("%s=%s" % (k, json.dumps(s) if (" " in s or not s) else s))
    with open(path, "a") as f:
        f.write(" ".join(parts) + "\n")


def write_json(path, obj):
    tmp = "%s.%d.tmp" % (path, os.getpid())
    with open(tmp, "w") as f:
        json.dump(obj, f, indent=1, sort_keys=True)
    os.replace(tmp, path)


def load_watches(lane):
    d = lane_dir(lane)
    out = []
    try:
        names = sorted(os.listdir(d))
    except FileNotFoundError:
        return out
    for n in names:
        if n.endswith(".json"):
            try:
                with open(os.path.join(d, n)) as f:
                    out.append(json.load(f))
            except (OSError, ValueError) as e:
                log("watch_unreadable_registration", warn=True, lane=lane, file=n, error=e)
    return out


def save(w):
    write_json(os.path.join(lane_dir(w["lane"]), w["id"] + ".json"), w)


def parse_duration(s):
    m = re.fullmatch(r"\s*(\d+)\s*([smhd]?)\s*", str(s))
    if not m:
        raise SystemExit("wake-watch: bad duration %r (use 90s, 30m, 6h, 2d)" % s)
    return int(m.group(1)) * {"": 1, "s": 1, "m": 60, "h": 3600, "d": 86400}[m.group(2)]


# ---- file probes ------------------------------------------------------------

def _has_magic(s):
    return any(c in s for c in "*?[")


def _base_dir(pattern):
    """The deepest directory in `pattern` with no glob characters."""
    parts = pattern.split(os.sep)
    keep = []
    for p in parts[:-1]:
        if _has_magic(p):
            break
        keep.append(p)
    return os.sep.join(keep) or (os.sep if pattern.startswith(os.sep) else ".")


def file_probe(w):
    """Read the watched path(s). Returns (matches, measured, why, n_considered).

    `matches` maps path -> (mtime_ns, size). An unlistable base directory or an
    unreadable matched file makes the probe unmeasured: glob() returns [] for a
    directory it cannot read, which would otherwise read as "nothing there".
    """
    pattern = w["path"]
    base = _base_dir(pattern)
    probe = base
    while not os.path.isdir(probe):
        parent = os.path.dirname(probe.rstrip(os.sep)) or os.sep
        if parent == probe:
            break
        probe = parent
    try:
        os.listdir(probe)
    except OSError as e:
        return {}, False, "cannot list %s: %s" % (probe, e.strerror or e), 0
    paths = sorted(glob.glob(pattern)) if _has_magic(pattern) else ([pattern] if os.path.lexists(pattern) else [])
    matches, bad = {}, []
    for p in paths:
        try:
            st = os.stat(p)
            if w["when"] == "line" and not os.access(p, os.R_OK):
                raise PermissionError(errno.EACCES, "Permission denied")
            matches[p] = (st.st_mtime_ns, st.st_size)
        except OSError as e:
            bad.append("%s: %s" % (p, e.strerror or e))
    if bad:
        return matches, False, "cannot read " + "; ".join(bad[:3]), len(paths)
    return matches, True, "", len(paths)


def eval_file(w):
    """Returns an event dict, or None. Updates w's probe fields in place."""
    matches, measured, why, n = file_probe(w)
    w.update(measured=measured, why_unmeasured=why, n_considered=n)
    when = w["when"]
    if when == "exists":
        if matches:
            p = sorted(matches)[0]
            return {"path": p, "detail": "exists (%d match%s)" % (len(matches), "" if len(matches) == 1 else "es")}
        return None
    if when == "changed":
        base = w.get("baseline") or {}
        cur = {p: list(v) for p, v in matches.items()}
        if not measured:
            return None  # a partial listing cannot prove a change
        for p in sorted(set(base) | set(cur)):
            if p not in cur:
                return {"path": p, "detail": "removed"}
            if p not in base:
                return {"path": p, "detail": "created"}
            if list(base[p]) != cur[p]:
                return {"path": p, "detail": "modified"}
        return None
    # when == "line": scan bytes appended since the stored offset.
    offsets = w.setdefault("offsets", {})
    rx = re.compile(w["regex"])
    hit = None
    for p in sorted(matches):
        size = matches[p][1]
        off = int(offsets.get(p, 0))
        if size < off:
            log("watch_file_truncated", lane=w["lane"], watch=w["id"], path=p, offset=off, size=size)
            off = 0
        if size == off:
            offsets[p] = off
            continue
        try:
            with open(p, "rb") as f:
                f.seek(off)
                chunk = f.read(min(size - off, LINE_READ_CAP))
        except OSError as e:
            w.update(measured=False, why_unmeasured="cannot read %s: %s" % (p, e.strerror or e))
            continue
        end = chunk.rfind(b"\n")
        if end < 0:
            continue  # no complete line yet
        complete = chunk[: end + 1]
        for line in complete.split(b"\n")[:-1]:
            text = line.decode("utf-8", "replace")
            if hit is None and rx.search(text):
                hit = {"path": p, "line": text[:500], "detail": "line matched /%s/" % w["regex"]}
        offsets[p] = off + len(complete)
        if hit:
            break
    return hit


# ---- CI probes: one shared poller per target ---------------------------------

def ci_key(w):
    if w.get("run"):
        return str(w["run"])
    if w.get("sha"):
        return "sha-%s%s" % (w["sha"], ("-" + re.sub(r"[^A-Za-z0-9._-]", "_", w["workflow"])) if w.get("workflow") else "")
    return "wf-" + re.sub(r"[^A-Za-z0-9._-]", "_", w["workflow"])


def ci_state_dir(w):
    return os.path.join(home(), "ci-watch", "%s-%s" % (w["repo"].replace("/", "_"), ci_key(w)))


def gh_token():
    tok = os.environ.get("WAKE_WATCH_GH_TOKEN")
    if tok is not None:
        return tok
    script = os.path.join(os.path.expanduser("~"), ".amux", "github-app", "get-token.sh")
    if os.access(script, os.X_OK):
        try:
            return subprocess.run([script, "--min-remaining", "600", "--raw"], capture_output=True, text=True,
                                  timeout=30).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return ""
    return os.environ.get("GH_TOKEN", "")


def gh_get(path):
    base = os.environ.get("AMUX_GITHUB_API", "https://api.github.com").rstrip("/")
    req = urllib.request.Request(base + path, headers={"Accept": "application/vnd.github+json",
                                                       "User-Agent": "amux-wake-watch"})
    tok = gh_token()
    if tok:
        req.add_header("Authorization", "Bearer " + tok)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read().decode() or "null")
    except urllib.error.HTTPError as e:
        return e.code, {"message": e.read().decode(errors="replace")[:300]}
    except (urllib.error.URLError, OSError, ValueError) as e:
        return 0, {"message": str(e)[:300]}


def _run_summary(r):
    return {"id": r.get("id"), "name": r.get("name"), "status": r.get("status"), "conclusion": r.get("conclusion"),
            "url": r.get("html_url"), "head_sha": r.get("head_sha"), "created_at": r.get("created_at"),
            "path": r.get("path")}


def ci_poll(w):
    """One GitHub read for this watch's target. Returns the state record."""
    repo = w["repo"]
    if w.get("run"):
        code, d = gh_get("/repos/%s/actions/runs/%s" % (repo, w["run"]))
    elif w.get("sha"):
        code, d = gh_get("/repos/%s/actions/runs?head_sha=%s&per_page=100" % (repo, w["sha"]))
    else:
        code, d = gh_get("/repos/%s/actions/workflows/%s/runs?per_page=10" % (repo, w["workflow"]))
    rec = {"polled_at": now(), "rc": 0 if code == 200 else 1, "http": code}
    if code == 200 and isinstance(d, dict) and w.get("run") and d.get("status"):
        rec.update(measured=True, status=d.get("status"), conclusion=d.get("conclusion"), url=d.get("html_url"),
                   head_sha=d.get("head_sha"), name=d.get("name"))
    elif code == 200 and isinstance(d, dict) and isinstance(d.get("workflow_runs"), list):
        rec.update(measured=True, runs=[_run_summary(r) for r in d["workflow_runs"]])
    else:
        msg = (d or {}).get("message", "") if isinstance(d, dict) else ""
        rec.update(measured=False, why_unmeasured=("http %s: %s" % (code, msg)).strip()[:300])
    log("ci_polled", warn=not rec["measured"], repo=repo, key=ci_key(w), http=code, measured=rec["measured"])
    return rec


def ci_state(w, floor):
    """The shared state for this target, refreshed only if stale and only by
    the one reader holding the lock."""
    d = ci_state_dir(w)
    os.makedirs(d, exist_ok=True)
    path, lock = os.path.join(d, "state.json"), os.path.join(d, "poller")
    try:
        with open(path) as f:
            st = json.load(f)
    except (OSError, ValueError):
        st = None
    if st and now() - float(st.get("polled_at", 0)) < floor:
        return st, "reader"
    if st and st.get("status") == "completed" and w.get("run"):
        return st, "reader"  # a concluded run never changes
    try:
        os.mkdir(lock)
    except FileExistsError:
        try:
            pid = int(open(os.path.join(lock, "pid")).read().strip())
            os.kill(pid, 0)
            return st, "reader"  # a live poller holds it; read what it wrote
        except (OSError, ValueError):
            for n in os.listdir(lock):
                os.unlink(os.path.join(lock, n))
            os.rmdir(lock)
            os.mkdir(lock)
    try:
        with open(os.path.join(lock, "pid"), "w") as f:
            f.write(str(os.getpid()))
        st = ci_poll(w)
        write_json(path, st)
    finally:
        for n in os.listdir(lock):
            os.unlink(os.path.join(lock, n))
        os.rmdir(lock)
    return st, "poller"


def eval_ci(w):
    st, _role = ci_state(w, max(CI_FLOOR_S, int(w.get("every", CI_FLOOR_S))))
    if not st:
        w.update(measured=False, why_unmeasured="no CI state yet (another poller holds the lock)", n_considered=0)
        return None
    if not st.get("measured"):
        w.update(measured=False, why_unmeasured=st.get("why_unmeasured", "unmeasured"), n_considered=0)
        return None
    if w.get("run"):
        w.update(measured=True, why_unmeasured="", n_considered=1)
        if st.get("status") == "completed":
            return {"conclusion": st.get("conclusion"), "url": st.get("url"),
                    "detail": "run %s (%s) completed %s" % (w["run"], st.get("name") or "?", st.get("conclusion"))}
        return None
    runs = st.get("runs") or []
    if w.get("workflow"):
        wf = w["workflow"]
        runs = [r for r in runs if r.get("name") == wf or (r.get("path") or "").endswith("/" + wf) or str(r.get("id")) == wf
                or not w.get("sha")]
    if not w.get("sha"):
        reg = iso(w["created_at"] - 60)
        runs = [r for r in runs if (r.get("created_at") or "") >= reg][:1]  # newest run since registration
    w.update(measured=True, why_unmeasured="", n_considered=len(runs))
    if not runs or any(r.get("status") != "completed" for r in runs):
        return None  # zero runs is pending, never success
    concl = sorted({r.get("conclusion") or "?" for r in runs})
    return {"conclusion": ",".join(concl), "url": runs[0].get("url"),
            "detail": "; ".join("%s %s %s" % (r.get("name"), r.get("conclusion"), r.get("url")) for r in runs[:6])}


# ---- registration -------------------------------------------------------------

def live(w):
    return w.get("state") == LIVE


def cmd_add(a):
    lane = a.lane or os.environ.get("AMUX_SESSION") or ""
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}", lane):
        raise SystemExit("wake-watch add: no lane (pass --lane or run inside a lane with AMUX_SESSION)")
    t = now()
    ttl = parse_duration(a.expires)
    if ttl <= 0 or ttl > MAX_TTL_S:
        raise SystemExit("wake-watch add: --expires must be between 1s and %ds" % MAX_TTL_S)
    os.makedirs(lane_dir(lane), exist_ok=True)
    with open(os.path.join(lane_dir(lane), ".lock"), "w") as lk:
        fcntl.flock(lk, fcntl.LOCK_EX)
        current = [w for w in load_watches(lane) if live(w) and w.get("expires_at", 0) > t]
        if len(current) >= MAX_PER_LANE:
            log("watch_cap_refused", warn=True, lane=lane, live=len(current), cap=MAX_PER_LANE)
            print("wake-watch add: refused: %s already has %d live watches (cap %d, AMUX_WAKE_WATCH_MAX). "
                  "Remove one with `amux watch rm <ID>`." % (lane, len(current), MAX_PER_LANE), file=sys.stderr)
            return 3
        floor = FILE_FLOOR_S if a.kind == "file" else CI_FLOOR_S
        every = max(floor, int(a.every or floor))
        w = {"id": "W-" + secrets.token_hex(3), "lane": lane, "kind": a.kind, "state": LIVE, "created_at": t,
             "expires_at": t + ttl, "every": every, "note": a.note or "", "last_checked": t, "measured": None,
             "n_considered": 0, "why_unmeasured": ""}
        if a.kind == "file":
            path = os.path.abspath(os.path.expanduser(a.path))
            when = a.when or ("line" if a.regex else "exists")
            if when == "line":
                if not a.regex:
                    raise SystemExit("wake-watch add: --when line needs --regex")
                re.compile(a.regex)
            w.update(path=path, when=when, regex=a.regex or "")
            matches, measured, why, n = file_probe(w)
            if not measured:
                log("watch_refused_unreadable", warn=True, lane=lane, path=path, why=why)
                print("wake-watch add: refused: %s (a watch that cannot read its path would never fire)" % why,
                      file=sys.stderr)
                return 2
            # Baselines: what exists now is not an event.
            w["baseline"] = {p: list(v) for p, v in matches.items()}
            w["offsets"] = {p: v[1] for p, v in matches.items()}
        else:
            if not re.fullmatch(r"[\w.-]+/[\w.-]+", a.repo or ""):
                raise SystemExit("wake-watch add ci: --repo owner/name is required")
            if not (a.run or a.sha or a.workflow):
                raise SystemExit("wake-watch add ci: pass --run ID, --sha SHA or --workflow W")
            if a.run and not str(a.run).isdigit():
                raise SystemExit("wake-watch add ci: --run must be a number")
            w.update(repo=a.repo, run=a.run or "", sha=a.sha or "", workflow=a.workflow or "")
        save(w)
    log("watch_registered", lane=lane, watch=w["id"], kind=w["kind"], target=describe(w), every=every,
        expires_at=iso(w["expires_at"]))
    sched = ensure_schedule(lane, w["expires_at"])
    print("%s registered for %s: %s (every %ss, expires %s)" % (w["id"], lane, describe(w), every, iso(w["expires_at"])))
    if every != int(a.every or floor):
        print("  poll interval raised to the %ss floor" % floor)
    print("  schedule: " + sched)
    return 0 if not sched.startswith("FAILED") else 4


def describe(w):
    if w["kind"] == "file":
        return "file %s %s%s" % (w["when"], w["path"], (" /%s/" % w["regex"]) if w.get("regex") else "")
    return "ci %s %s" % (w["repo"], ("run " + str(w["run"])) if w.get("run") else
                         ("sha %s%s" % (w["sha"], (" workflow " + w["workflow"]) if w.get("workflow") else ""))
                         if w.get("sha") else ("next run of workflow " + w["workflow"]))


def amux_api(method, path, body=None):
    url = (os.environ.get("AMUX_URL") or "").rstrip("/")
    if not url:
        try:
            url = json.load(open(os.path.join(home(), "endpoint.json"))).get("url", "").rstrip("/")
        except (OSError, ValueError):
            url = ""
    if not url:
        return 0, {"error": "no AMUX_URL and no endpoint.json"}
    req = urllib.request.Request(url + path, method=method, data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Content-Type": "application/json"})
    lane = os.environ.get("AMUX_SESSION", "")
    if lane:
        req.add_header("X-Amux-Session", lane)
    if os.environ.get("AMUX_WORKER_TOKEN"):
        req.add_header("X-Amux-Worker-Token", os.environ["AMUX_WORKER_TOKEN"])
    ctx = ssl.create_default_context()
    ctx.check_hostname, ctx.verify_mode = False, ssl.CERT_NONE
    try:
        with urllib.request.urlopen(req, timeout=20, context=ctx) as r:
            return r.status, json.loads(r.read().decode() or "null")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except ValueError:
            return e.code, {}
    except (urllib.error.URLError, OSError, ValueError) as e:
        return 0, {"error": str(e)}


def schedule_command(lane):
    repo = os.environ.get("AMUX_REPO_DIR") or os.path.join(os.path.expanduser("~"), "Dev", "amux")
    # Committed bytes, not a shared worktree's draft (CLAUDE.md "Shared checkouts").
    return "git -C %s show origin/main:scripts/wake-watch.py | python3 - check --lane %s" % (repo, lane)


def ensure_schedule(lane, until_ts):
    """One shell schedule per lane runs `check`; its `until` covers the latest
    expiry (plus 3 minutes so the expiry notice is delivered)."""
    if os.environ.get("WAKE_WATCH_SCHEDULE", "1") == "0":
        return "not ensured (WAKE_WATCH_SCHEDULE=0)"
    title = SCHEDULE_TITLE.format(lane=lane)
    latest = max([until_ts] + [float(w["expires_at"]) for w in load_watches(lane) if live(w)])
    until = iso(latest + 180)
    code, d = amux_api("GET", "/api/schedules")
    rows = d if isinstance(d, list) else (d or {}).get("schedules", []) if isinstance(d, dict) else []
    if code != 200:
        log("watch_schedule_failed", warn=True, lane=lane, http=code, step="list")
        return "FAILED to list schedules (http %s); the watch will not fire until %r exists" % (code, title)
    mine = [r for r in rows if r.get("title") == title and r.get("session") == lane and not r.get("deleted")]
    if mine:
        r = mine[0]
        # `until` is recomputed from every live watch, not compared as text:
        # the server stores its own normalised form.
        patch = {"enabled": 1, "until": until}
        code, _ = amux_api("PATCH", "/api/schedules/%s" % r.get("id"), patch)
        ok = 200 <= code < 300
        log("watch_schedule_extended" if ok else "watch_schedule_failed", warn=not ok, lane=lane, schedule=r.get("id"),
            http=code, until=patch.get("until", r.get("until")))
        return ("%s (until %s)" % (r.get("id"), patch.get("until", r.get("until")))) if ok else \
            "FAILED to extend %s (http %s)" % (r.get("id"), code)
    body = {"title": title, "session": lane, "kind": "shell", "command": schedule_command(lane),
            "schedule_expr": "every 1m", "exit_actions": {"10": "wake"}, "until": until}
    code, d = amux_api("POST", "/api/schedules", body)
    ok = 200 <= code < 300
    sid = (d or {}).get("id") if isinstance(d, dict) else None
    log("watch_schedule_created" if ok else "watch_schedule_failed", warn=not ok, lane=lane, schedule=sid, http=code)
    return ("%s created (every 1m, until %s)" % (sid, until)) if ok else \
        "FAILED to create %r (http %s): %s" % (title, code, json.dumps(d)[:200])


# ---- the check (run by the lane's schedule) -------------------------------------

def cmd_check(a):
    lane = a.lane
    t = now()
    events, considered = [], 0
    os.makedirs(lane_dir(lane), exist_ok=True)
    with open(os.path.join(lane_dir(lane), ".lock"), "w") as lk:
        fcntl.flock(lk, fcntl.LOCK_EX)
        for w in load_watches(lane):
            if not live(w):
                if t - float(w.get("closed_at") or t) > KEEP_CLOSED_S:
                    os.unlink(os.path.join(lane_dir(lane), w["id"] + ".json"))
                    log("watch_pruned", lane=lane, watch=w["id"], state=w.get("state"))
                continue
            considered += 1
            if t >= float(w["expires_at"]):
                w.update(state="expired", closed_at=t)
                save(w)
                log("watch_expired", lane=lane, watch=w["id"], target=describe(w))
                events.append("- %s EXPIRED unfired after %s: %s%s" % (
                    w["id"], _ago(t - w["created_at"]), describe(w), _note(w)))
                continue
            if t - float(w.get("last_checked") or 0) < int(w.get("every", 0)):
                continue
            try:
                ev = eval_file(w) if w["kind"] == "file" else eval_ci(w)
            except Exception as e:  # a broken probe is unmeasured, not "no event"
                ev = None
                w.update(measured=False, why_unmeasured="probe error: %s" % e)
            w["last_checked"] = t
            if ev:
                w.update(state="fired", closed_at=t, event=ev)
                save(w)  # closed BEFORE printing: a lost print is a lost wake, never a duplicate
                log("watch_fired", lane=lane, watch=w["id"], target=describe(w), path=ev.get("path", ""),
                    line=ev.get("line", ""), conclusion=ev.get("conclusion", ""), url=ev.get("url", ""))
                where = ev.get("path") or ev.get("url") or ""
                line = (": " + ev["line"]) if ev.get("line") else ""
                events.append("- %s fired: %s, %s %s%s%s" % (w["id"], describe(w), ev["detail"], where, line, _note(w)))
            else:
                if w.get("measured") is False:
                    log("watch_unmeasured", warn=True, lane=lane, watch=w["id"], why=w.get("why_unmeasured", ""))
                save(w)
    log("watch_check", lane=lane, measured=True, n_considered=considered, events=len(events))
    if not events:
        return 0
    print("wake-watch: %d event(s) for %s\n\n%s\n\nEach watch above is now closed. `amux watch ls` lists the rest."
          % (len(events), lane, "\n\n".join(events)))
    return EXIT_DELIVER


def _ago(s):
    s = int(s)
    return "%dh%02dm" % (s // 3600, s % 3600 // 60) if s >= 3600 else "%dm" % (s // 60)


def _note(w):
    return (" (note: %s)" % w["note"]) if w.get("note") else ""


def cmd_status(a):
    root = os.path.join(home(), "wake-watch")
    lanes = [a.lane] if a.lane else (sorted(n for n in os.listdir(root) if not n.startswith(".")) if os.path.isdir(root) else [])
    out = {"measured": True, "n_considered": 0, "lanes": {}}
    for lane in lanes:
        ws = load_watches(lane)
        out["n_considered"] += len(ws)
        out["lanes"][lane] = [{k: w.get(k) for k in ("id", "kind", "state", "measured", "n_considered", "why_unmeasured",
                                                    "every", "note", "event")} |
                              {"target": describe(w), "expires_at": iso(w["expires_at"]),
                               "last_checked": iso(w["last_checked"]) if w.get("last_checked") else None} for w in ws]
    if a.json:
        print(json.dumps(out, indent=1))
        return 0
    print("wake-watch: %d watch(es) across %d lane(s) (measured=true, n_considered=%d)"
          % (out["n_considered"], len(out["lanes"]), out["n_considered"]))
    for lane, ws in out["lanes"].items():
        for w in ws:
            m = {True: "measured", False: "UNMEASURED", None: "not yet checked"}[w["measured"]]
            print("  %s %s %-8s %s [%s, n_considered=%s%s] expires %s" % (
                lane, w["id"], w["state"], w["target"], m, w["n_considered"],
                (", " + w["why_unmeasured"]) if w.get("why_unmeasured") else "", w["expires_at"]))
    return 0


def cmd_rm(a):
    lane = a.lane or os.environ.get("AMUX_SESSION") or ""
    p = os.path.join(lane_dir(lane), a.id + ".json")
    if not os.path.exists(p):
        print("wake-watch rm: no watch %s for lane %r" % (a.id, lane), file=sys.stderr)
        return 1
    os.unlink(p)
    log("watch_removed", lane=lane, watch=a.id)
    print("removed " + a.id)
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(prog="amux watch", description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    add = sub.add_parser("add")
    add.add_argument("kind", choices=["file", "ci"])
    add.add_argument("--lane")
    add.add_argument("--path")
    add.add_argument("--when", choices=["exists", "changed", "line"])
    add.add_argument("--regex")
    add.add_argument("--repo")
    add.add_argument("--run")
    add.add_argument("--sha")
    add.add_argument("--workflow")
    add.add_argument("--expires", default="6h")
    add.add_argument("--every", type=int)
    add.add_argument("--note")
    for name in ("status", "ls"):
        s = sub.add_parser(name)
        s.add_argument("--lane")
        s.add_argument("--json", action="store_true")
    rm = sub.add_parser("rm")
    rm.add_argument("id")
    rm.add_argument("--lane")
    ck = sub.add_parser("check")
    ck.add_argument("--lane", required=True)
    a = ap.parse_args(argv)
    if a.cmd == "add" and a.kind == "file" and not a.path:
        ap.error("add file needs --path")
    return {"add": cmd_add, "status": cmd_status, "ls": cmd_status, "rm": cmd_rm, "check": cmd_check}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
