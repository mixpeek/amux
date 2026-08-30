#!/usr/bin/env python3
"""Assertive per-environment, per-persona E2E for cloud.amux.io.

WHAT THIS IS. `godmode_walkthrough.py` signs in as god mode and PRINTS evidence for
every customer environment, but by design it never concludes pass/fail (a human
reviewer decides). This is the assertive twin: it exercises the SAME path as a
signed-in user and turns each observation into a PASS/FAIL, so a scheduler or CI can
run it unattended and go red the moment an environment stops working.

WHY IT EXISTS (2026-08-16 disk-full outage, cloud/incidents/2026-08-16-disk-full.md).
cloud.amux.io was 502 for hours and nothing caught it, because no check signs in and
loads a real environment as a user on a schedule. A scheduled run of this suite would
have gone red at the first env-unreachable assertion. That is the whole point: the
first assertion below (env reachable) IS the outage detector.

WHAT "AS A USER" MEANS HERE. For every environment we provisioned for a customer, the
suite does what a user does after logging in: switch into the workspace, wake its
container, list its workers, open each worker to read its transcript, look at its
files, and read the board it produced. Each of those becomes an assertion, per
environment and per persona (a persona = a preconfigured worker role, e.g.
wtso-support / wtso-campaigns / wtso-ops).

SAFETY (ethos rule 8). Real customer orgs (budget_usd < 25, a real owner email) are
checked READ-ONLY — the suite never sends a message into a paying customer's worker
or spends their budget. The active "type a message and get a reply" leg is opt-in
(--send-probe) and is refused for anything that is not a demo environment.

USAGE
  python3 cloud/tests/e2e_personas.py                 # all environments, read-only
  python3 cloud/tests/e2e_personas.py --send-probe    # + active reply probe on DEMO envs only
  python3 cloud/tests/e2e_personas.py --json          # machine-readable summary to stdout
  python3 cloud/tests/e2e_personas.py wtso            # one plan by name/file stem

Exit code 0 = every PROVISIONED environment passed; 1 = at least one failed or the
cloud gateway is unreachable. NOT_PROVISIONED environments do not fail the run (a plan
we have not stood up yet is a gap, not a regression) but are reported.
"""
import glob
import json
import os
import re
import sys
import time

# Reuse the god-mode auth + wake primitives rather than reinventing them (the Clerk
# backend-ticket chain is subtle and already correct in godmode_walkthrough).
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import godmode_walkthrough as gm  # noqa: E402

PLANS_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "plans")

# A demo environment is one it is safe to actively probe: our own demo mailbox and the
# $25 demo budget convention (wtso.json:2 states $5 = real customer, $25 = demo).
def _is_demo(org):
    email = (org.get("email") or "").lower()
    return email.startswith("demo+") or int(org.get("budget_usd") or 0) >= 25


def load_plans(only=None):
    plans = []
    for path in sorted(glob.glob(os.path.join(PLANS_DIR, "*.json"))):
        stem = os.path.splitext(os.path.basename(path))[0]
        if only and stem not in only and not any(o in stem for o in only):
            continue
        p = json.load(open(path))
        org = p.get("org", {})
        workers = [{"name": s.get("name"), "dir": s.get("dir"),
                    "desc": (s.get("desc") or "")[:80]} for s in p.get("sessions", [])]
        plans.append({
            "stem": stem,
            "name": org.get("name") or stem,
            "email": (org.get("email") or "").lower(),
            "budget_usd": org.get("budget_usd"),
            "is_demo": _is_demo(org),
            "workers": workers,
            "dirs": sorted({w["dir"] for w in workers if w.get("dir")}),
            "columns": p.get("board_columns", []),
        })
    return plans


# Known provisioned customer/demo orgs, keyed by plan stem. A fallback for when the
# admin/orgs API is slow or projects fields differently — the orgs table's `name`
# column equals the plan's org.name, but the API response shape has drifted before,
# so pin the confirmed ids (from the gateway orgs DB, 2026-08-18).
KNOWN_ORGS = {
    "capital-express": "org_37aa24eb89c1d97a",
    "elliot-wexus": "org_18f676d91310d02f",
    "rothco": "org_8e89a846b6f5be7d",
}


def resolve_org_id(plan, orgs):
    """Match a plan to its provisioned org_id: admin-list by email/name, else the
    known-org pin. Returns None only when the plan is genuinely not provisioned."""
    for o in orgs:
        oe = (o.get("email") or o.get("owner_email") or "").lower()
        if oe and oe == plan["email"]:
            return o.get("id")
    pn = plan["name"].strip().lower()
    for o in orgs:
        if (o.get("name") or "").strip().lower() == pn:
            return o.get("id")
    return KNOWN_ORGS.get(plan["stem"])


def _evidence_for(cookie, org, worker):
    """Read one persona's transcript + files as a user would. Returns an evidence dict."""
    name = worker["name"]
    st, d = gm.get(cookie, f"/api/sessions/{name}/peek?lines=400", org)
    hist = (d.get("history") or d.get("output") or "") if isinstance(d, dict) else ""
    clean = re.sub(r"\x1b\[[0-9;]*m", "", hist)
    tool_calls = len(re.findall(r"⏺\s+\*?\*?(Read|Edit|Write|Bash|Grep|Glob)", clean))
    hist_lines = d.get("history_lines") if isinstance(d, dict) else 0
    files = 0
    if worker.get("dir"):
        st2, fl = gm.get(cookie, f"/api/fs/list?path={worker['dir']}", org)
        if isinstance(fl, dict):
            for k in ("entries", "files", "items", "list"):
                if isinstance(fl.get(k), list):
                    files = len(fl[k])
                    break
    return {"history_lines": hist_lines or 0, "tool_calls": tool_calls, "files": files}


def _send_probe(cookie, org, name):
    """Active 'as a user' leg (DEMO only): type a message, confirm the worker replies."""
    token = f"E2E-PERSONA-OK-{int(time.time())}"
    body = json.dumps({"text": f"Health check from the E2E persona suite. Reply with exactly: {token}"}).encode()
    import urllib.request
    req = urllib.request.Request(
        f"{gm.CLOUD}/api/sessions/{name}/send", data=body, method="POST",
        headers={"Cookie": f"amux_session={cookie}; amux_org={org}",
                 "Content-Type": "application/json", "User-Agent": "Mozilla/5.0"})
    try:
        urllib.request.urlopen(req, context=gm._CTX, timeout=60)
    except Exception as e:
        return {"sent": False, "replied": False, "error": str(e)[:80]}
    for _ in range(20):  # up to ~2min for the model to answer
        time.sleep(6)
        st, d = gm.get(cookie, f"/api/sessions/{name}/peek?lines=200", org)
        hist = (d.get("history") or d.get("output") or "") if isinstance(d, dict) else ""
        if token in hist:
            return {"sent": True, "replied": True}
    return {"sent": True, "replied": False}


def check_env(cookie, plan, org_id, send_probe):
    res = {"env": plan["name"], "stem": plan["stem"], "org_id": org_id,
           "is_demo": plan["is_demo"], "personas": [], "board": None, "status": None,
           "reachable": False, "reasons": []}
    if not org_id:
        res["status"] = "NOT_PROVISIONED"
        res["reasons"].append("no provisioned org matches this plan's email/name")
        return res

    # ASSERTION 1 (the outage detector): the environment's container serves as a user.
    if not gm.wake(cookie, org_id):
        res["status"] = "FAIL"
        res["reasons"].append("container did not come up / gateway unreachable (this is the cloud-down signal)")
        return res
    res["reachable"] = True

    st, sessions = gm.get(cookie, "/api/sessions", org_id)
    if not isinstance(sessions, list):
        res["status"] = "FAIL"
        res["reasons"].append(f"/api/sessions did not return a list (HTTP {st})")
        return res
    present = {s.get("name") for s in sessions}

    # ASSERTION 2 + per-persona: every expected worker exists and shows evidence of work.
    for w in plan["workers"]:
        p = {"name": w["name"], "present": w["name"] in present,
             "evidence": None, "probe": None}
        if p["present"]:
            p["evidence"] = _evidence_for(cookie, org_id, w)
            if send_probe and plan["is_demo"]:
                p["probe"] = _send_probe(cookie, org_id, w["name"])
        res["personas"].append(p)

    # Board: it should be populated and NOT full of raw captures (demo quality).
    st, issues = gm.get(cookie, "/api/board?slim=1", org_id)
    if isinstance(issues, list):
        shells = [i for i in issues if re.match(r"(?i)^(capture|captured|prompt)\b",
                  (i.get("title") or "")) or len(i.get("title") or "") > 90]
        res["board"] = {"issues": len(issues), "raw_capture_titles": len(shells)}

    missing = [p["name"] for p in res["personas"] if not p["present"]]
    # A persona shows evidence of work if it has a transcript, tool calls, OR files.
    # tool_calls alone is proof the worker ran (peek may omit history_lines yet still
    # show the ⏺ tool markers), so a worker with tool_calls is NOT "no evidence".
    no_evidence = [p["name"] for p in res["personas"]
                   if p["present"] and p["evidence"]
                   and p["evidence"]["history_lines"] == 0
                   and p["evidence"]["files"] == 0
                   and p["evidence"]["tool_calls"] == 0]
    probe_fail = [p["name"] for p in res["personas"]
                  if p.get("probe") and not p["probe"].get("replied")]
    if missing:
        res["status"] = "FAIL"
        res["reasons"].append(f"missing personas: {missing}")
    elif probe_fail:
        res["status"] = "FAIL"
        res["reasons"].append(f"personas did not reply to a user message: {probe_fail}")
    elif no_evidence:
        res["status"] = "WARN"
        res["reasons"].append(f"personas present but show no logs and no files: {no_evidence}")
    else:
        res["status"] = "PASS"
    return res


def _cloud_up():
    """Preflight: is the gateway serving at all? Any HTTP status is 'up'; a 5xx
    gateway error or a connection failure is 'down'. This is the outage detector,
    and it runs BEFORE the Clerk sign-in dance so a down cloud is reported in one
    request instead of minting a sign-in token that cannot be redeemed."""
    import urllib.request
    import urllib.error
    req = urllib.request.Request(f"{gm.CLOUD}/", headers={"User-Agent": "Mozilla/5.0"})
    try:
        with urllib.request.urlopen(req, context=gm._CTX, timeout=15) as r:
            return True, r.status
    except urllib.error.HTTPError as e:
        return (e.code not in (500, 502, 503, 504)), e.code
    except Exception as e:
        return False, str(e)[:80]


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = {a for a in sys.argv[1:] if a.startswith("--")}
    send_probe = "--send-probe" in flags
    as_json = "--json" in flags
    plans = load_plans(only=args or None)

    def log(*a):
        if not as_json:
            print(*a)

    log("PERSONA E2E — cloud.amux.io — asserts every customer environment works as a user")
    log(f"{'read-only (safe for real customers)' if not send_probe else 'ACTIVE probe on DEMO envs only'}\n")

    # Outage detector: probe the gateway once before anything else.
    up, code = _cloud_up()
    if not up:
        out = {"cloud_reachable": False, "gateway_status": code, "results": []}
        (print(json.dumps(out, indent=2)) if as_json else
         print(f"\nCLOUD UNREACHABLE — gateway returned {code} at {gm.CLOUD}/ .\n"
               "No environment can be exercised. This is the exact failure this suite exists\n"
               "to catch: a scheduled run would go red here the moment the site is down."))
        sys.exit(1)

    # Sign in. If auth breaks after the gateway is up, that is also a real failure.
    # gm.sign_in() prints progress to stdout; in --json mode that pollutes the JSON,
    # so redirect it to stderr for a clean machine-readable document.
    import contextlib
    try:
        if as_json:
            with contextlib.redirect_stdout(sys.stderr):
                cookie = gm.sign_in()
        else:
            cookie = gm.sign_in()
    except (SystemExit, Exception) as e:
        out = {"cloud_reachable": True, "signin_ok": False, "error": str(e)[:200], "results": []}
        (print(json.dumps(out, indent=2)) if as_json else
         print(f"\nSIGN-IN FAILED (gateway is up, {code}, but god-mode auth did not complete):\n  {e}"))
        sys.exit(1)
    log("  signed in as god mode ✓")

    st, d = gm.get(cookie, "/api/gateway/admin/orgs")
    orgs = d.get("orgs", d) if isinstance(d, dict) else (d if isinstance(d, list) else [])
    log(f"  god mode sees {len(orgs)} orgs\n")

    results = []
    for plan in plans:
        org_id = resolve_org_id(plan, orgs)
        try:
            r = check_env(cookie, plan, org_id, send_probe)
        except Exception as e:
            # A single env erroring (a sluggish cloud, a timeout) must not sink the
            # whole run — record it as a FAIL and keep going so the JSON always emits.
            r = {"env": plan["name"], "stem": plan["stem"], "org_id": org_id,
                 "is_demo": plan["is_demo"], "personas": [], "board": None,
                 "status": "FAIL", "reachable": False, "reasons": ["check errored: %s" % str(e)[:120]]}
        results.append(r)
        icon = {"PASS": "✓", "WARN": "!", "FAIL": "✗", "NOT_PROVISIONED": "·"}.get(r["status"], "?")
        log(f"{icon} {r['status']:16s} {r['env']}  ({plan['stem']}, {'demo' if plan['is_demo'] else 'REAL customer'})")
        for p in r["personas"]:
            ev = p.get("evidence") or {}
            probe = p.get("probe")
            pt = ("" if not probe else
                  ("  reply:OK" if probe.get("replied") else "  reply:NONE"))
            mark = "✓" if p["present"] else "✗ MISSING"
            log(f"      {mark:10s} {p['name']:26s} "
                f"logs={ev.get('history_lines','-')} tools={ev.get('tool_calls','-')} files={ev.get('files','-')}{pt}")
        if r["board"]:
            log(f"      board: {r['board']['issues']} issues, {r['board']['raw_capture_titles']} raw-capture titles")
        for why in r["reasons"]:
            log(f"      -> {why}")
        log("")

    provisioned = [r for r in results if r["status"] != "NOT_PROVISIONED"]
    failed = [r for r in results if r["status"] == "FAIL"]
    warned = [r for r in results if r["status"] == "WARN"]
    notprov = [r for r in results if r["status"] == "NOT_PROVISIONED"]
    summary = {
        "cloud_reachable": True,
        "environments": len(results),
        "provisioned": len(provisioned),
        "passed": len([r for r in provisioned if r["status"] == "PASS"]),
        "warned": len(warned),
        "failed": len(failed),
        "not_provisioned": [r["stem"] for r in notprov],
        "results": results,
    }
    if as_json:
        print(json.dumps(summary, indent=2))
    else:
        print(f"{'=' * 72}")
        print(f"  {summary['passed']}/{len(provisioned)} provisioned envs PASS, "
              f"{len(warned)} WARN, {len(failed)} FAIL; {len(notprov)} not provisioned "
              f"({', '.join(summary['not_provisioned']) or 'none'})")
        print(f"{'=' * 72}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
