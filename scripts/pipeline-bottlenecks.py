#!/usr/bin/env python3
"""pipeline-bottlenecks: find the shared-pipeline bottleneck before a human does.

Ethan, 2026-10-01 14:16 ET: "ensure the harness proactively identifies these
bottlenecks" (AH-291). The first case is the one that held goal spec 12 for a
day: the `amux land` queue on Mixpeek main and the pre-push gate inside it
(25 deep; nothing landed for two hours after 340d477914d; median hold 23.7 min;
a stale GH_TOKEN made every gate run its slow legs in full).

Measures, per repository with an `amux land` queue (all from files on disk, no
model call):
  depth          tickets waiting
  oldest_wait    minutes the oldest ticket has waited
  holder_attempt minutes the current holder's attempt has run
  median_hold    median minutes from "acquired" to an outcome, last 6 h
  landed_6h      landings in the last 6 h
  stale_code     waiters whose land process started before the installed CLI
                 changed, so they run code without the latest queue fixes
  drain_h        hours to drain the queue at the measured rate

A repository is a BOTTLENECK when any threshold trips (all overridable):
  depth >= 8, oldest_wait >= 120, holder_attempt >= 45,
  or median_hold >= 20 with depth >= 4.

On a bottleneck it:
  1. logs one JSON verdict line to ~/.amux/logs/bottlenecks.jsonl
     (verdict=pipeline_bottleneck, measured, n_considered), and prints it;
  2. routes ONE message naming the bottleneck, its numbers and the levers to
     --route (e.g. the orchestrator whose lanes are queued), at most once per
     --cooldown-min per repository unless the drain estimate is 50% worse;
  3. never kills or reorders anything: recovery that is safe to automate
     already lives in `amux land` (stale-lock takeover, dead-ticket pruning,
     aging); this names what is left for a lane to act on.
With no queue anywhere it prints verdict=no_queues, measured=true, so a quiet
run is distinguishable from a probe that never ran.

HOST (AH-294): the same run measures the machine every lane shares. On
2026-10-01 load reached ~80 on 28 cores with 22.7 of 23.5 GB swap used and the
data disk at 98 percent, and the land holder starved inside a git archive
export; the orchestrator found it, not the harness. Measured: 1-minute load per
core, swap used, data-volume use, and the top processes by CPU and by memory,
each attributed to a lane by its cwd (a .worktrees/<lane> path, or a session
scratch dir mapped to the amux session that records it). Thresholds: load per
core >= 2.0, swap >= 90 percent, disk >= 95 percent. A trip logs
verdict=host_bottleneck and routes one message to --host-route (default
mac-ops, the Mac resource lane) under the same cooldown.

TOP CONSTRAINT (AH-398, --constraints): ranks GS-12's candidate constraints by
an estimate of the finish-date hours each costs (proof stalled, review backlog,
owner asks blocking proof, idle lanes with ready work, blocked or cardless
lanes, land queue wait, production stuck on one deploy), logs
verdict=top_constraint, and acts: proof stalled 3 runs running alerts Ethan once
a day (the former SCHED-620 tripwire); owner asks blocking proof alert Ethan;
a stuck production deploy messages the ops deputy; orchestrator-owned
constraints append one line to ~/.amux/state/orchestrator-asks.txt for
amux-helper's hourly batched message (never a direct send).
"""
import argparse, glob, json, os, re, ssl, statistics, subprocess, sys, time, urllib.request

HOME = os.path.expanduser("~")
LOCKS = os.path.join(HOME, ".amux", "locks")
LAND_LOG = os.path.join(HOME, ".amux", "logs", "land.log")
OUT = os.environ.get("AMUX_BOTTLENECKS_OUT") or os.path.join(HOME, ".amux", "logs", "bottlenecks.jsonl")
STATE = os.environ.get("AMUX_BOTTLENECKS_STATE") or os.path.join(HOME, ".amux", "bottlenecks-state.json")
TS = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ) (\S+) (.*?)(?: \[([0-9a-f]{16})\])?$")


def epoch(ts):
    return time.mktime(time.strptime(ts, "%Y-%m-%dT%H:%M:%SZ")) - time.timezone


def ps_start(pid):
    """Process start time (epoch) or None."""
    try:
        out = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], capture_output=True, text=True, timeout=5).stdout.strip()
        return time.mktime(time.strptime(out, "%a %b %d %H:%M:%S %Y")) if out else None
    except Exception:
        return None


def sessions_by_conversation():
    """{conversation id: amux session name} from session metas."""
    out = {}
    for f in glob.glob(os.path.join(HOME, ".amux", "sessions", "*.meta.json")):
        try:
            cid = json.load(open(f)).get("cc_conversation_id") or ""
        except Exception:
            continue
        if cid:
            out[cid] = os.path.basename(f)[: -len(".meta.json")]
    return out


def lane_of(cwd, conv):
    """Best-effort lane for a cwd: a worktree name, or a scratch dir's session."""
    m = re.search(r"/\.worktrees/([^/]+)", cwd)
    if m:
        return m.group(1)
    m = re.search(r"/claude-\d+/[^/]+/([0-9a-f-]{36})", cwd)
    if m:
        return conv.get(m.group(1), "session " + m.group(1)[:8])
    return cwd.replace(HOME, "~")[:60]


def host_measure():
    ncpu = os.cpu_count() or 1
    load1 = os.getloadavg()[0]
    swap_pct = None
    try:
        sw = subprocess.run(["sysctl", "-n", "vm.swapusage"], capture_output=True, text=True, timeout=5).stdout
        tot = float(re.search(r"total = ([\d.]+)M", sw).group(1)); used = float(re.search(r"used = ([\d.]+)M", sw).group(1))
        swap_pct = round(100 * used / tot, 1) if tot else 0.0
    except Exception:
        pass
    disk_pct = None
    try:
        st = os.statvfs("/System/Volumes/Data" if os.path.isdir("/System/Volumes/Data") else "/")
        disk_pct = round(100 * (1 - st.f_bavail / st.f_blocks), 1)
    except Exception:
        pass
    procs = []
    try:
        out = subprocess.run(["ps", "-Ao", "pid=,%cpu=,rss=,comm="], capture_output=True, text=True, timeout=10).stdout
        for line in out.splitlines():
            parts = line.split(None, 3)
            if len(parts) == 4:
                procs.append((int(parts[0]), float(parts[1]), int(parts[2]) // 1024, os.path.basename(parts[3])))
    except Exception:
        pass
    conv = sessions_by_conversation()
    # Apple Virtualization VMs are launchd children with cwd "/": attribute
    # each to the VM manager that started nearest it, within 60 s (a colima/lima
    # hostagent names its profile, e.g. colima-gs12-compute; Docker Desktop has
    # com.docker.virtualization). On 2026-10-01 six gs12 VMs held about 41 GB
    # while the orchestrator believed it ran one.
    def start(pid):
        try:
            out = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], capture_output=True, text=True, timeout=5).stdout.strip()
            return time.mktime(time.strptime(out, "%a %b %d %H:%M:%S %Y"))
        except Exception:
            return None
    managers = []
    for line in subprocess.run(["ps", "-Ao", "pid=,command="], capture_output=True, text=True).stdout.splitlines():
        pid, _, cmd = line.strip().partition(" ")
        m = re.search(r"_lima/([^/\s]+)/", cmd) if "hostagent" in cmd else None
        name = m.group(1) if m else ("docker-desktop" if "com.docker.virtualization" in cmd else None)
        if name:
            t = start(pid)
            if t:
                managers.append((t, name))
    def vm_owner(pid):
        t = start(pid)
        best = min(managers, key=lambda m: abs(m[0] - t), default=None) if t else None
        return best[1] if best and abs(best[0] - t) <= 60 else "unattributed VM"
    def attribute(rows):
        res = []
        for pid, cpu, mb, comm in rows:
            cwd = subprocess.run(["lsof", "-a", "-p", str(pid), "-d", "cwd", "-Fn"], capture_output=True, text=True).stdout
            cwd = next((l[1:] for l in cwd.splitlines() if l.startswith("n")), "")
            lane = vm_owner(pid) if "Virtualization.VirtualMachine" in comm else (lane_of(cwd, conv) if cwd else "?")
            res.append({"pid": pid, "cpu": cpu, "mb": mb, "comm": comm, "lane": lane})
        return res
    top_cpu = attribute(sorted(procs, key=lambda r: -r[1])[:6])
    top_mem = attribute(sorted(procs, key=lambda r: -r[2])[:6])
    return {"ncpu": ncpu, "load1": round(load1, 1), "load_per_core": round(load1 / ncpu, 2),
            "swap_pct": swap_pct, "disk_pct": disk_pct, "top_cpu": top_cpu, "top_mem": top_mem}


# THROUGHPUT (2026-10-05, Ethan: "figure out why these optimizations weren't
# discovered via one of the schedulers"). On 2026-10-05 the landing queue was
# healthy all afternoon (this script said pipeline_ok) while the real
# constraints sat downstream of it: production deploys succeeded about once in
# 20 dispatches, lanes sat idle with no eligible card, and decisions aged in the
# owner's column. Each check below is computed, optional (off unless its flag is
# given) and fixture-driven for tests.
def _get_json(url_path, fixture):
    if fixture:
        return json.load(open(fixture))
    base = subprocess.run(["amux", "url"], capture_output=True, text=True).stdout.strip()
    ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
    with urllib.request.urlopen(base + url_path, context=ctx, timeout=60) as r:
        return json.load(r)


def deploy_measure(repo, workflow, since, runs_fixture=None):
    """Dispatched runs of the deploy workflow since `since`: conclusions and the
    failing job/step names. None when it cannot be read (unmeasured)."""
    try:
        if runs_fixture:
            runs = json.load(open(runs_fixture))
        else:
            env = dict(os.environ)
            tok = subprocess.run(["bash", "-c", os.path.join(HOME, ".amux/github-app/get-token.sh") + " | sed -n 's/^export GH_TOKEN=//p'"],
                                 capture_output=True, text=True).stdout.strip().strip("'\"")
            if tok:
                env["GH_TOKEN"] = tok
            iso = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(since))
            r = subprocess.run(["gh", "api", f"repos/{repo}/actions/workflows/{workflow}/runs?per_page=50&event=workflow_dispatch&created=>{iso}",
                                "--jq", "[.workflow_runs[] | {id, status, conclusion}]"], capture_output=True, text=True, env=env, timeout=60)
            if r.returncode:
                return None
            runs = json.loads(r.stdout or "[]")
            for run in [x for x in runs if x.get("conclusion") == "failure"][:6]:
                j = subprocess.run(["gh", "api", f"repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100", "--jq",
                                    '[.jobs[] | select(.conclusion=="failure") | .name + ": " + ([.steps[] | select(.conclusion=="failure") | .name] | join(", "))]'],
                                   capture_output=True, text=True, env=env, timeout=60)
                run["failed"] = json.loads(j.stdout or "[]") if j.returncode == 0 else []
    except Exception:
        return None
    done = [x for x in runs if x.get("status") == "completed"]
    ok = sum(1 for x in done if x.get("conclusion") == "success")
    steps = {}
    for x in done:
        for f in x.get("failed") or []:
            steps[f] = steps.get(f, 0) + 1
    return {"dispatched": len(runs), "concluded": len(done), "succeeded": ok,
            "failed": sum(1 for x in done if x.get("conclusion") == "failure"),
            "cancelled": sum(1 for x in done if x.get("conclusion") == "cancelled"),
            "top_failing_steps": sorted(steps.items(), key=lambda kv: -kv[1])[:4]}


def idle_lanes(prefix, drive_fixture=None):
    """Lanes board-drive found with no card and nothing eligible: working
    capacity with no work. None when unmeasured."""
    try:
        d = _get_json("/api/debug/board-drive", drive_fixture)
    except Exception:
        return None
    out = []
    for l in ((d.get("last") or {}).get("lanes") or []):
        name = l.get("session") or ""
        if not name.startswith(prefix) or l.get("card"):
            continue
        reason = l.get("reason") or ""
        if reason in ("mid-turn", "opted-out") or reason.startswith("active"):
            continue
        if int(l.get("eligible_todos") or 0) == 0:
            out.append({"lane": name, "reason": reason})
    return out


def aged_owner_asks(actor, min_age_h, now, needsyou_fixture=None):
    """needsyou cards addressed to the owner older than min_age_h. None when
    unmeasured."""
    try:
        rows = _get_json("/api/board?status=needsyou&slim=0", needsyou_fixture)
    except Exception:
        return None
    out = []
    for i in rows:
        if i.get("archived") or i.get("status") != "needsyou":
            continue
        if (i.get("ask_actor") or "").strip().lower() != actor.lower():
            continue
        try:
            age = (now - float(i.get("entered_state_at") or 0)) / 3600
        except (TypeError, ValueError):
            continue
        if age >= min_age_h:
            out.append({"id": i.get("id"), "session": i.get("session"), "age_h": round(age, 1)})
    return sorted(out, key=lambda x: -x["age_h"])


def throughput(a, now):
    """Run the enabled throughput checks; return (record, levers)."""
    rec = {"ts": int(now), "measured": True}
    why, levers, n = [], [], 0
    if a.deploy_repo and a.deploy_workflow:
        dm = deploy_measure(a.deploy_repo, a.deploy_workflow, now - 6 * 3600, a.runs_file)
        rec["deploy"] = dm if dm is not None else "unmeasured"
        if dm:
            n += dm["concluded"]
            if dm["concluded"] >= a.deploy_min_runs and dm["succeeded"] / dm["concluded"] < a.deploy_success:
                why.append(f"deploys: {dm['succeeded']} of {dm['concluded']} {a.deploy_workflow} dispatches succeeded in 6 h")
                steps = "; ".join(f"{k} (x{v})" for k, v in dm["top_failing_steps"])
                levers.append("make promotes green before anything else: one owner on the failing steps"
                              + (f" ({steps})" if steps else "") + "; finished cards wait on a deploy to verify")
    if a.lane_prefix:
        il = idle_lanes(a.lane_prefix, a.drive_file)
        rec["idle_lanes"] = il if il is not None else "unmeasured"
        if il is not None:
            n += len(il)
            if len(il) >= a.idle_min:
                why.append(f"idle lanes: {len(il)} {a.lane_prefix}* lanes have no card and nothing eligible")
                levers.append("route work to idle lanes now: " + ", ".join(x["lane"] for x in il[:8]))
    if a.owner:
        oa = aged_owner_asks(a.owner, a.owner_age_h, now, a.needsyou_file)
        rec["owner_asks_aged"] = oa if oa is not None else "unmeasured"
        if oa is not None:
            n += len(oa)
            if len(oa) >= a.owner_min:
                why.append(f"owner asks: {len(oa)} needsyou cards to {a.owner} older than {a.owner_age_h} h")
    rec.update({"verdict": "throughput_bottleneck" if why else "throughput_ok", "why": why, "n_considered": n})
    return rec, levers


# TOP CONSTRAINT (AH-398, Ethan 2026-10-07: "each hour, name the top
# constraint with numbers and trigger the recovery or route it, so it doesn't
# depend on anyone noticing"). GS-12 proof sat at 1-5 verified a day for a week
# while every check logged BEHIND and nobody escalated. This ranks candidate
# constraints by an estimate of the finish-date hours each costs, names the top
# one, and acts through a fixed mapping. The estimates are heuristics, printed
# beside the numbers they come from, so a wrong rank is visible.
PROOF_RE = re.compile(r"^GS12 (proof|requirement)")
DONE_STATES = ("done", "verified", "discarded")


def _deps(i):
    d = i.get("depends_on") or []
    if isinstance(d, str):
        try:
            d = json.loads(d)
        except ValueError:
            d = []
    return d


def _at(i):
    try:
        return float(i.get("closed_at") or i.get("entered_state_at") or 0)
    except (TypeError, ValueError):
        return 0.0


def constraints(a, now, state):
    """Candidate constraints with numbers, ranked by estimated finish-date
    hours. Returns (ranked, n_considered, unmeasured sources)."""
    unmeasured, out, n = [], [], 0

    def read(name, path, fixture):
        try:
            return _get_json(path, fixture)
        except Exception:
            unmeasured.append(name)
            return None
    board = read("board", "/api/board?all=1&slim=0", a.board_file)
    sessions = read("sessions", "/api/sessions", a.sessions_file)
    land = read("land", "/api/land", a.land_file)
    try:
        if a.prod_file:
            prod = json.load(open(a.prod_file))
        else:
            with urllib.request.urlopen(a.prod_url, timeout=20) as r:
                prod = json.load(r)
    except Exception:
        prod = None
        unmeasured.append("prod")
    pre, hub = a.lane_prefix or "gs12-", a.hub
    ready = []
    if board is not None:
        by = {i.get("id"): i for i in board}
        g = [i for i in board if not i.get("archived")
             and ((i.get("session") or "").startswith(pre) or i.get("session") == hub)]
        n += len(g)
        proof = [i for i in g if PROOF_RE.search(i.get("title") or "")]
        v24 = sum(1 for i in proof if i.get("status") == "verified" and _at(i) > now - 86400)
        ver = sum(1 for i in proof if i.get("status") == "verified")
        remaining = sum(1 for i in proof if i.get("status") not in DONE_STATES)
        active = sum(1 for i in proof if i.get("status") in ("doing", "todo") and i.get("session") != hub)
        ready = [i["id"] for i in proof if i.get("status") in ("backlog", "todo") and i.get("session") == hub
                 and all((by.get(d) or {}).get("status") in ("done", "verified") for d in _deps(i))]
        # Days to finish at the measured rate minus days at the needed rate.
        lost_h = max(0.0, remaining / max(v24, 0.5) - remaining / a.proof_per_day) * 24
        # Too few proof cards in work is tomorrow's low rate: an hour per
        # missing active card, so a good last 24 h does not hide it.
        lost_h += max(0, a.proof_min_active - active) * 1.0
        bad = v24 < a.proof_per_day or active < a.proof_min_active
        out.append({"name": "proof_stalled" if bad else "proof_on_pace", "owner": "orchestrator",
                    "est_hours": round(lost_h if bad else 0, 1),
                    "numbers": {"proof_verified": ver, "proof_total": len(proof), "verified_24h": v24,
                                "need_per_day": a.proof_per_day, "active_on_lanes": active,
                                "need_active": a.proof_min_active, "remaining": remaining,
                                "ready_on_hub": len(ready)}})
        done = [i for i in g if i.get("status") == "done"]
        ages = sorted((now - _at(i)) / 3600 for i in done if _at(i))
        med = round(statistics.median(ages), 1) if ages else 0
        out.append({"name": "review_backlog", "owner": "orchestrator",
                    "est_hours": round(len(done) * 0.25 if med >= 6 else 0, 1),
                    "numbers": {"done_waiting": len(done), "median_age_h": med}})
        blocked_by_owner = []
        for i in g:
            if i.get("status") != "needsyou" or (i.get("ask_actor") or "").lower() != a.owner_name.lower():
                continue
            age = (now - _at(i)) / 3600
            if age < 2:
                continue
            blocks = [p["id"] for p in proof if p.get("status") not in DONE_STATES and i.get("id") in _deps(p)]
            if blocks or PROOF_RE.search(i.get("title") or ""):
                blocked_by_owner.append({"id": i.get("id"), "age_h": round(age, 1), "blocks": blocks[:5]})
        out.append({"name": "owner_ask_blocks_proof", "owner": "ethan",
                    "est_hours": round(24.0 * len(blocked_by_owner), 1), "numbers": {"asks": blocked_by_owner}})
    if sessions is not None:
        lanes = [s for s in sessions if (s.get("name") or "").startswith(pre)]
        n += len(lanes)
        idle = [s["name"] for s in lanes if s.get("status") == "idle"]
        a2_on = "A2" not in [x.strip() for x in (a.rules_off or "").split(",")]
        out.append({"name": "idle_lanes_with_ready_work", "owner": "harness" if a2_on else "orchestrator",
                    "est_hours": round(min(len(idle), len(ready)) * 1.0, 1),
                    "numbers": {"idle": idle, "ready_proof_on_hub": len(ready), "a2_pool_on": a2_on}})
        waiting = [s["name"] for s in lanes if s.get("status") == "waiting"
                   and (s.get("waiting_reason") or s.get("status_reason") or "") != "owner"]
        nocard = [s["name"] for s in lanes if (s.get("runtime_board") or {}).get("violation")]
        out.append({"name": "lanes_blocked_or_cardless", "owner": "orchestrator",
                    "est_hours": round(len(waiting) * 1.0 + len(nocard) * 0.5, 1),
                    "numbers": {"waiting_not_on_owner": waiting, "active_no_valid_card": nocard}})
    if land is not None:
        p95 = land.get("wait_p95_min") or 0
        out.append({"name": "land_wait", "owner": "harness", "est_hours": round(p95 / 60 if p95 >= 30 else 0, 1),
                    "numbers": {"wait_p95_min": round(p95, 1), "items_24h": land.get("n_considered")}})
    if prod is not None:
        sha = prod.get("deploy_sha") or ""
        ps = state.setdefault("_prod", {})
        if ps.get("sha") != sha:
            ps.update({"sha": sha, "since": now})
        stale_h = (now - ps["since"]) / 3600
        out.append({"name": "deploy_stalled", "owner": "ops-deputy",
                    "est_hours": round(stale_h if stale_h >= a.prod_stale_h else 0, 1),
                    "numbers": {"deploy_sha": sha[:12], "hours_on_this_sha": round(stale_h, 1)}})
    out.sort(key=lambda c: -c["est_hours"])
    return out, n, unmeasured


def orchestrator_ask(a, now, st, key, text):
    """Orchestrator-owned: one line for amux-helper's hourly batched message.
    Never messages the orchestrator from here."""
    if now - st.get("ask_" + key, 0) < 3600:
        return "deduped: queued for the orchestrator within the hour"
    if a.dry_run:
        return "would queue for the orchestrator"
    os.makedirs(os.path.dirname(a.orchestrator_asks), exist_ok=True)
    with open(a.orchestrator_asks, "a") as f:
        f.write(time.strftime("%Y-%m-%dT%H:%MZ", time.gmtime(now)) + " " + text + "\n")
    st["ask_" + key] = now
    return "queued for the orchestrator"


def _alert(a, msg, why):
    if a.dry_run:
        return "would alert ethan"
    r = subprocess.run(["amux", "alert", msg, why], capture_output=True, text=True)
    return "alerted ethan" if r.returncode == 0 else "alert failed: " + (r.stderr or r.stdout).strip()[:120]


def proof_line(nums):
    return (f"GS-12 proof: {nums['proof_verified']}/{nums['proof_total']} verified, {nums['verified_24h']} in 24 h "
            f"(need {nums['need_per_day']:g}/day), {nums['active_on_lanes']} proof cards active on lanes "
            f"(need {nums['need_active']}), {nums['ready_on_hub']} ready on the orchestrator board")


def proof_tripwire(ranked, a, now, state):
    """The SCHED-620 rule, folded in, evaluated every run whatever ranks
    first: three consecutive stalled runs, then Ethan, at most once a day.
    Returns the action taken, or "" when none."""
    st = state.setdefault("_constraints", {})
    p = next((c for c in ranked if c["name"] in ("proof_stalled", "proof_on_pace")), None)
    if p is None:
        return ""
    if p["name"] != "proof_stalled":
        st["proof_bad_runs"] = 0
        return ""
    bad = st.get("proof_bad_runs", 0) + 1
    st["proof_bad_runs"] = bad
    if bad >= 3 and now - st.get("proof_alert", 0) >= 86400:
        r = _alert(a, proof_line(p["numbers"]) + f", stalled {bad} hourly runs.", "GS-12 proof stalled 3h+ (bottleneck detector)")
        if not a.dry_run:
            st["proof_alert"] = now
        return r
    return ""


def act(top, a, now, state):
    """Recover or route the top constraint, deduped; returns what was done."""
    st = state.setdefault("_constraints", {})
    name, nums = top["name"], top["numbers"]
    if top["est_hours"] <= 0:
        return "none: no constraint costs measurable finish-date hours"
    alert = lambda msg, why: _alert(a, msg, why)
    if name == "proof_stalled":
        return orchestrator_ask(a, now, st, "proof", proof_line(nums) + ": put lanes on the ready proof cards first.")
    if name == "owner_ask_blocks_proof":
        if now - st.get("owner_alert", 0) < 86400:
            return "deduped: Ethan alerted within 24 h"
        ids = ", ".join(f"{x['id']} ({x['age_h']} h)" for x in nums["asks"][:6])
        r = alert(f"GS-12 proof cards wait on your answers: {ids}.", "owner asks block GS-12 proof (bottleneck detector)")
        if not a.dry_run:
            st["owner_alert"] = now
        return r
    if name == "deploy_stalled":
        if now - st.get("deploy_route", 0) < 6 * 3600:
            return "deduped: ops deputy messaged within 6 h"
        if a.dry_run:
            return "would message " + a.ops_lane
        msg = (f"Ask (amux bottleneck detector): production has been on {nums['deploy_sha']} for "
               f"{nums['hours_on_this_sha']} h, the top GS-12 constraint this hour. What blocks the next promote?")
        r = subprocess.run(["amux", "send", a.ops_lane, "--stdin"], input=msg, capture_output=True, text=True)
        if r.returncode == 0:
            st["deploy_route"] = now
        return ("messaged " if r.returncode == 0 else "message failed: ") + a.ops_lane
    if name == "idle_lanes_with_ready_work" and nums.get("a2_pool_on"):
        return "none extra: A2 pool dispatch hands idle lanes ready work"
    if name == "lanes_blocked_or_cardless" and nums.get("waiting_not_on_owner") and not nums.get("active_no_valid_card"):
        return "none extra: prompt_block alerts the lane's hub after 10 min"
    if top["owner"] == "harness":
        return "logged for amux-helper (harness-owned)"
    return orchestrator_ask(a, now, st, name, f"{name}: {json.dumps(nums)[:400]}")


def run_constraints(a, now):
    try:
        state = json.load(open(STATE))
    except Exception:
        state = {}
    if not a.rules_off:
        try:
            for line in open(os.path.join(HOME, ".amux/env/gs12-platform.env")):
                if line.startswith("AMUX_CONTRACT_RULES_OFF="):
                    a.rules_off = line.split("=", 1)[1].strip().strip('"')
        except OSError:
            pass
    ranked, ncons, unmeas = constraints(a, now, state)
    top = ranked[0] if ranked else None
    action = act(top, a, now, state) if top else "none: nothing measured"
    tripped = proof_tripwire(ranked, a, now, state)
    if tripped:
        action += "; proof tripwire: " + tripped
    rec = {"ts": int(now), "verdict": "top_constraint" if top and top["est_hours"] > 0 else "no_constraint",
           "name": top["name"] if top else None, "est_hours": top["est_hours"] if top else 0,
           "numbers": top["numbers"] if top else {}, "action": action,
           "ranked": [{"name": c["name"], "est_hours": c["est_hours"]} for c in ranked],
           "measured": bool(ranked), "n_considered": ncons}
    if unmeas:
        rec["why_unmeasured"] = "could not read: " + ", ".join(unmeas)
    with open(OUT, "a") as f:
        f.write(json.dumps(rec) + "\n")
    print(json.dumps(rec))
    json.dump(state, open(STATE, "w"))
    return rec


def holds_by_repo(since):
    """{repo_key: [minutes per completed hold]}, {repo_key: landings} since `since`."""
    holds, landed, open_ = {}, {}, {}
    try:
        lines = open(LAND_LOG, errors="replace").read().splitlines()
    except OSError:
        return holds, landed
    for line in lines:
        m = TS.match(line)
        if not m or not m.group(4):
            continue
        t = epoch(m.group(1))
        if t < since:
            continue
        who, msg, key = m.group(2), m.group(3), m.group(4)
        if msg.startswith("acquired"):
            open_[(key, who)] = t
        elif msg.startswith(("landed ", "push failed", "batch with", "rebase onto", "gave up")):
            if msg.startswith("landed ") and "landed in a batch" not in msg:
                landed[key] = landed.get(key, 0) + 1
            st = open_.pop((key, who), None)
            if st is not None:
                holds.setdefault(key, []).append((t - st) / 60)
    return holds, landed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--route", default="", help="lane to message on a bottleneck (empty: log only)")
    ap.add_argument("--depth", type=int, default=8)
    ap.add_argument("--oldest-wait", type=int, default=120)
    ap.add_argument("--holder-attempt", type=int, default=45)
    ap.add_argument("--median-hold", type=int, default=20)
    ap.add_argument("--cooldown-min", type=int, default=120)
    ap.add_argument("--dry-run", action="store_true", help="never send")
    ap.add_argument("--host-route", default="mac-ops", help="lane to message on a host bottleneck (empty: log only)")
    ap.add_argument("--load-per-core", type=float, default=2.0)
    ap.add_argument("--swap-pct", type=float, default=90)
    ap.add_argument("--disk-pct", type=float, default=95)
    ap.add_argument("--deploy-repo", default="", help="owner/repo whose deploy workflow to measure (empty: skip)")
    ap.add_argument("--deploy-workflow", default="")
    ap.add_argument("--deploy-min-runs", type=int, default=4)
    ap.add_argument("--deploy-success", type=float, default=0.5)
    ap.add_argument("--lane-prefix", default="", help="lanes to check for idleness (empty: skip)")
    ap.add_argument("--idle-min", type=int, default=2)
    ap.add_argument("--owner", default="", help="ask_actor whose aged needsyou cards to count (empty: skip)")
    ap.add_argument("--owner-age-h", type=float, default=2)
    ap.add_argument("--owner-min", type=int, default=3)
    ap.add_argument("--runs-file", help=argparse.SUPPRESS)
    ap.add_argument("--drive-file", help=argparse.SUPPRESS)
    ap.add_argument("--needsyou-file", help=argparse.SUPPRESS)
    ap.add_argument("--throughput-only", action="store_true", help=argparse.SUPPRESS)
    ap.add_argument("--constraints", action="store_true", help="rank the top GS-12 constraint and act on it (AH-398)")
    ap.add_argument("--constraints-only", action="store_true", help=argparse.SUPPRESS)
    ap.add_argument("--hub", default="mixpeek-override")
    ap.add_argument("--owner-name", default="ethan")
    ap.add_argument("--ops-lane", default="gs12-ops-deputy")
    ap.add_argument("--proof-per-day", type=float, default=7)
    ap.add_argument("--proof-min-active", type=int, default=10)
    ap.add_argument("--prod-stale-h", type=float, default=6)
    ap.add_argument("--prod-url", default="https://api.mixpeek.com/version")
    ap.add_argument("--rules-off", default="", help="gs12-platform AMUX_CONTRACT_RULES_OFF (read from its env file when empty)")
    ap.add_argument("--orchestrator-asks", default=os.path.join(HOME, ".amux/state/orchestrator-asks.txt"))
    ap.add_argument("--board-file", help=argparse.SUPPRESS)
    ap.add_argument("--sessions-file", help=argparse.SUPPRESS)
    ap.add_argument("--land-file", help=argparse.SUPPRESS)
    ap.add_argument("--prod-file", help=argparse.SUPPRESS)
    a = ap.parse_args()

    now = time.time()
    if a.constraints or a.constraints_only:
        run_constraints(a, now)
        if a.constraints_only:
            return 0
    if a.deploy_repo or a.lane_prefix or a.owner:
        trec, tlevers = throughput(a, now)
        with open(OUT, "a") as f:
            f.write(json.dumps(trec) + "\n")
        print(json.dumps(trec))
        try:
            tstate = json.load(open(STATE))
        except Exception:
            tstate = {}
        last = tstate.get("_throughput", {})
        if tlevers and a.route and not a.dry_run and now - last.get("sent", 0) >= a.cooldown_min * 60:
            msg = ("Throughput (amux pipeline-bottlenecks): " + "; ".join(trec["why"]) + ".\nLevers: " + " | ".join(tlevers))
            r = subprocess.run(["amux", "send", a.route, "--stdin"], input=msg, capture_output=True, text=True)
            with open(OUT, "a") as f:
                f.write(json.dumps({"ts": int(now), "verdict": "throughput_routed" if r.returncode == 0 else "throughput_route_failed",
                                    "route": a.route, "measured": True, "n_considered": 1,
                                    "detail": (r.stdout or r.stderr).strip()[:200]}) + "\n")
            if r.returncode == 0:
                tstate["_throughput"] = {"sent": now}
                json.dump(tstate, open(STATE, "w"))
        if a.throughput_only:
            return 0
    holds, landed = holds_by_repo(now - 6 * 3600)
    installed_ver = ""
    try:
        m = re.search(r'^LAND_BEHAVIOR_VERSION="([^"]+)"', open(os.path.join(HOME, ".local", "bin", "amux"), errors="replace").read(), re.M)
        installed_ver = m.group(1) if m else ""
    except OSError:
        pass
    try:
        state = json.load(open(STATE))
    except Exception:
        state = {}

    found = []
    queues = sorted(glob.glob(os.path.join(LOCKS, "land-*.q")))
    for qd in queues:
        key = os.path.basename(qd)[len("land-"):-len(".q")]
        lock = qd[:-2]
        tickets = sorted(os.listdir(qd)) if os.path.isdir(qd) else []
        if not tickets and not os.path.isdir(lock):
            continue
        waits, stale, lanes = [], [], []
        for t in tickets:
            # Land's marker files (.resume-<lane>, .tohead-<pid>, .batch-<pid>)
            # live in the queue dir too; they are not tickets.
            if t.startswith("."):
                continue
            pid = t.rsplit("-", 1)[-1]
            who = (open(os.path.join(qd, t), errors="replace").readline().strip() or "?")
            lanes.append(who)
            if t.startswith("000000000000-"):
                # Moved by `amux land --to-head`: its name carries no arrival
                # time (read as 25 years, 2026-10-03), so use the file's mtime.
                arrived = int(os.path.getmtime(os.path.join(qd, t)))
            elif t.startswith("000"):
                arrived = int(now // 1e9 * 1e9 + int(t[3:12]))
            else:
                arrived = int(t[:12])
            waits.append((now - arrived) / 60)
            # Stale = the ticket records an older LAND_BEHAVIOR_VERSION than
            # the installed CLI. Install time moved on every install, lint-only
            # ones included, and flagged eight waiters for a pointless requeue
            # (2026-10-01). A ticket with no version predates versioning and is
            # not flagged: the honest answer there is "unknown".
            try:
                tv = open(os.path.join(qd, t), errors="replace").read().splitlines()
                tver = tv[4].strip() if len(tv) > 4 else ""
            except OSError:
                tver = ""
            if installed_ver and tver and tver < installed_ver:
                stale.append(f"{who} (pid {pid}, land {tver} < {installed_ver})")
        holder, attempt = None, None
        if os.path.isdir(lock):
            holder = (open(os.path.join(lock, "who")).read().strip() if os.path.exists(os.path.join(lock, "who")) else "?")
            try:
                attempt = (now - int(open(os.path.join(lock, "since")).read().strip())) / 60
            except Exception:
                attempt = None
        h = holds.get(key, [])
        med = statistics.median(h) if h else None
        n_land = landed.get(key, 0)
        rate = n_land / 6.0
        depth = len(tickets)
        # Only with at least 3 measured holds: land.log lines carry the repo
        # key since 2026-10-01 17:38Z, and fewer holds undercount the rate.
        drain = (depth / rate) if rate > 0 and len(h) >= 3 else None
        why = []
        if depth >= a.depth: why.append(f"depth {depth} >= {a.depth}")
        if waits and max(waits) >= a.oldest_wait: why.append(f"oldest wait {max(waits):.0f} min >= {a.oldest_wait}")
        if attempt is not None and attempt >= a.holder_attempt: why.append(f"holder attempt {attempt:.0f} min >= {a.holder_attempt}")
        if med is not None and med >= a.median_hold and depth >= 4: why.append(f"median hold {med:.0f} min >= {a.median_hold} with {depth} waiting")
        rec = {"ts": int(now), "repo_key": key, "depth": depth,
               "oldest_wait_min": round(max(waits), 1) if waits else 0,
               "holder": holder, "holder_attempt_min": None if attempt is None else round(attempt, 1),
               "median_hold_min": None if med is None else round(med, 1), "holds_measured_6h": len(h),
               "landed_6h": n_land, "drain_h": None if drain is None else round(drain, 1),
               "stale_code_waiters": stale, "lanes": sorted(set(lanes)),
               "measured": True, "n_considered": depth}
        if why:
            rec.update({"verdict": "pipeline_bottleneck", "why": why})
            found.append(rec)
        else:
            rec.update({"verdict": "pipeline_ok"})
        with open(OUT, "a") as f:
            f.write(json.dumps(rec) + "\n")
        print(json.dumps(rec))

    if not queues:
        rec = {"ts": int(now), "verdict": "no_queues", "measured": True, "n_considered": 0}
        with open(OUT, "a") as f:
            f.write(json.dumps(rec) + "\n")
        print(json.dumps(rec))

    for rec in found:
        key = rec["repo_key"]
        last = state.get(key, {})
        # "Worse" needs a measured baseline. With none (never sent, or the last
        # send had an unmeasured drain) any drain compared against 0 read as 50%
        # worse and skipped the cooldown: 35 sends in 26 h at a 120 min cooldown.
        prev = last.get("drain_h") or 0
        worse = prev > 0 and (rec["drain_h"] or 0) >= 1.5 * prev
        due = now - last.get("sent", 0) >= a.cooldown_min * 60
        if not a.route or a.dry_run or not (due or worse):
            continue
        levers = []
        if rec["stale_code_waiters"]:
            levers.append("re-queue lands running old code (amux land --cancel, then amux land --detach) so they get the current fixes: "
                          + ", ".join(rec["stale_code_waiters"][:8]))
        if rec["median_hold_min"] and rec["median_hold_min"] >= a.median_hold:
            levers.append(f"the gate itself: median hold {rec['median_hold_min']} min is the per-landing cost; shortening it moves every lane")
        if rec["holder_attempt_min"] and rec["holder_attempt_min"] >= a.holder_attempt:
            levers.append(f"the holder {rec['holder']} is {rec['holder_attempt_min']:.0f} min into one attempt: check its push is progressing")
        if not levers:
            # Nothing the recipient can act on: a message here is a status
            # update that interrupts a lane mid-turn. The verdict line above is
            # the record; say so here so a sweep can count the suppressions.
            with open(OUT, "a") as f:
                f.write(json.dumps({"ts": int(now), "verdict": "pipeline_bottleneck_not_routed_no_lever",
                                    "repo_key": key, "route": a.route, "measured": True, "n_considered": 1}) + "\n")
            continue
        msg = (f"Bottleneck (amux pipeline-bottlenecks, AH-291): the land queue {key} is the constraint. "
               f"{'; '.join(rec['why'])}. Depth {rec['depth']}, oldest wait {rec['oldest_wait_min']:.0f} min, "
               f"holder {rec['holder']} ({rec['holder_attempt_min']} min), median hold {rec['median_hold_min']} min over "
               f"{rec['holds_measured_6h']} holds, {rec['landed_6h']} landings in 6 h, "
               + (f"drain about {rec['drain_h']} h.\n" if rec['drain_h'] is not None else "drain unmeasured (under 3 measured holds).\n")
               + "Levers: " + " | ".join(levers))
        r = subprocess.run(["amux", "send", a.route, "--stdin"], input=msg, capture_output=True, text=True)
        sent = r.returncode == 0
        with open(OUT, "a") as f:
            f.write(json.dumps({"ts": int(now), "verdict": "pipeline_bottleneck_routed" if sent else "pipeline_bottleneck_route_failed",
                                "repo_key": key, "route": a.route, "measured": True, "n_considered": 1,
                                "detail": (r.stdout or r.stderr).strip()[:200]}) + "\n")
        if sent:
            state[key] = {"sent": now, "drain_h": rec["drain_h"]}
    # Host (AH-294).
    h = host_measure()
    hwhy = []
    if h["load_per_core"] >= a.load_per_core: hwhy.append(f"load {h['load1']} on {h['ncpu']} cores ({h['load_per_core']}/core >= {a.load_per_core})")
    if h["swap_pct"] is not None and h["swap_pct"] >= a.swap_pct: hwhy.append(f"swap {h['swap_pct']}% >= {a.swap_pct}")
    if h["disk_pct"] is not None and h["disk_pct"] >= a.disk_pct: hwhy.append(f"disk {h['disk_pct']}% >= {a.disk_pct}")
    hrec = {"ts": int(now), "verdict": "host_bottleneck" if hwhy else "host_ok", "why": hwhy,
            **h, "measured": True, "n_considered": len(h["top_cpu"]) + len(h["top_mem"])}
    with open(OUT, "a") as f:
        f.write(json.dumps(hrec) + "\n")
    print(json.dumps({k: hrec[k] for k in ("verdict", "why", "load1", "swap_pct", "disk_pct")}))
    last = state.get("_host", {})
    if hwhy and a.host_route and not a.dry_run and now - last.get("sent", 0) >= a.cooldown_min * 60:
        fmt = lambda rows, key, unit: ", ".join(f"{r['comm']} {r[key]}{unit} ({r['lane']}, pid {r['pid']})" for r in rows[:5])
        msg = (f"Host bottleneck (amux pipeline-bottlenecks, AH-294): {'; '.join(hwhy)}. "
               f"Top CPU: {fmt(h['top_cpu'], 'cpu', '%')}. Top memory: {fmt(h['top_mem'], 'mb', ' MB')}. "
               "Every lane's builds, tests and pushes share this machine; ask the named lanes to defer heavy work, "
               "or free disk (session scratch: scripts/scratch-reaper.py).")
        r = subprocess.run(["amux", "send", a.host_route, "--stdin"], input=msg, capture_output=True, text=True)
        with open(OUT, "a") as f:
            f.write(json.dumps({"ts": int(now), "verdict": "host_bottleneck_routed" if r.returncode == 0 else "host_bottleneck_route_failed",
                                "route": a.host_route, "measured": True, "n_considered": 1,
                                "detail": (r.stdout or r.stderr).strip()[:200]}) + "\n")
        if r.returncode == 0:
            state["_host"] = {"sent": now}
    json.dump(state, open(STATE, "w"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
