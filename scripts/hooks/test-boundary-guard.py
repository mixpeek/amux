#!/usr/bin/env python3
"""Run every example in boundaries.toml through the real boundary-guard.py.

- every deny rule's `block_examples` must be refused BY THAT BOUNDARY
- every `allow_examples` entry must pass the whole guard
- the resources boundary's `heavy_block_examples` must be refused when the
  host is critical and pass when it is ok; `heavy_allow_examples` must pass
  when critical; an acknowledged heavy command passes and is logged
- non-amux sessions are never refused; isolated workers (as launched:
  AMUX_ISOLATED_WORKER set, AMUX_SESSION unset) are refused unless the
  registry sets apply_to_isolated = false
- the subagents boundary refuses a new Agent/Task call at the running cap or
  under the usage headroom floor, and allows an unmeasured count
- an owner override in ~/.amux/boundaries.toml can disable a boundary

Example strings: a Bash command as-is, or "<Tool> <path>" for a file tool,
optionally followed by " :: <content written>".

Prints one line per case and a computed summary; exits 1 on any failure.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
GUARD = os.path.join(HERE, "boundary-guard.py")
REGISTRY = os.environ.get("AMUX_BOUNDARIES_FILE") or os.path.join(REPO, "boundaries.toml")
FILE_TOOLS = ("Read", "Edit", "Write", "MultiEdit", "NotebookEdit", "Grep", "Glob")
CRITICAL = json.dumps({"load5": 100.0, "ncpu": 4})
OK = json.dumps({"load5": 1.0, "ncpu": 4, "mem_level": 1})

failures: list[str] = []
cases = 0


def payload(example: str) -> dict:
    head, _, content = example.partition(" :: ")
    tool, _, rest = head.partition(" ")
    if tool == "Grep" and rest.startswith("/"):
        return {"tool_name": "Grep", "tool_input": {"pattern": "x", "path": rest}}
    if tool == "Glob" and rest.startswith("/"):
        return {"tool_name": "Glob", "tool_input": {"pattern": rest}}
    if tool in FILE_TOOLS and rest.startswith("/"):
        ti = {"file_path": rest}
        if content:
            ti["content" if tool == "Write" else "new_string"] = content
        return {"tool_name": tool, "tool_input": ti}
    return {"tool_name": "Bash", "tool_input": {"command": example}}


USAGE_OK = json.dumps({"measured": True, "available": True, "limits": [
    {"kind": "session", "percent": 40, "resets_at": "2026-10-19T01:40:00+00:00", "scope": None},
    {"kind": "weekly_all", "percent": 60, "resets_at": "2026-10-25T01:00:00+00:00", "scope": None}]})
last_stderr = ""


def run_payload(pl: dict, home: str, env_extra: dict | None = None, registry: str = REGISTRY) -> tuple[int, list[dict]]:
    """env_extra values of None remove that variable."""
    global last_stderr
    env = {k: v for k, v in os.environ.items() if not k.startswith("AMUX_")}
    env.update({"AMUX_HOME": home, "AMUX_SESSION": "boundary-test", "AMUX_BOUNDARIES_FILE": registry,
                "AMUX_BOUNDARIES_HOST_SAMPLE": OK, "AMUX_USAGE_SAMPLE": USAGE_OK})
    env.update(env_extra or {})
    env = {k: v for k, v in env.items() if v is not None}
    log = os.path.join(home, "logs", "boundaries.jsonl")
    if os.path.exists(log):
        os.remove(log)
    p = subprocess.run([sys.executable, GUARD], input=json.dumps(pl), text=True,
                       capture_output=True, env=env, timeout=20)
    last_stderr = p.stderr
    events = [json.loads(l) for l in open(log)] if os.path.exists(log) else []
    return p.returncode, events


def run(example: str, home: str, env_extra: dict | None = None, registry: str = REGISTRY) -> tuple[int, list[dict]]:
    return run_payload(payload(example), home, env_extra, registry)


def conversation(home: str, name: str, running: int, finished: int = 0, stale: int = 0, no_meta_cold: int = 0,
                 stopped: int = 0, resumed: int = 0) -> str:
    """A Claude Code conversation laid out as on disk, with subagent
    transcripts and meta files. Returns the transcript_path a hook gets."""
    root = os.path.join(home, "projects", name)
    subs = os.path.join(root, "conv", "subagents")
    os.makedirs(subs, exist_ok=True)
    tp = os.path.join(root, "conv.jsonl")
    with open(tp, "w") as fh:
        fh.write("{}\n")
    old = time.time() - 7200

    def agent(i: int, kind: str, meta: dict | None) -> str:
        stem = os.path.join(subs, f"agent-{kind}{i}")
        with open(stem + ".jsonl", "w") as fh:
            fh.write("{}\n")
        if meta is not None:
            with open(stem + ".meta.json", "w") as fh:
                json.dump(meta, fh)
        return stem

    for i in range(running):
        agent(i, "run", {"requestShape": "background"})
    for i in range(finished):
        agent(i, "done", {"requestShape": "background", "finished": True})
    for i in range(stale):
        os.utime(agent(i, "stale", {"requestShape": "background"}) + ".jsonl", (old, old))
    interrupt = json.dumps({"type": "user", "message": {"role": "user", "content": [
        {"type": "text", "text": "[Request interrupted by user]"}]}})
    for i in range(stopped):  # TaskStop: the interrupt is the last entry and "finished" is never set
        with open(agent(i, "stopped", {"requestShape": "background"}) + ".jsonl", "a") as fh:
            fh.write(interrupt + "\n")
    for i in range(resumed):  # stopped, then resumed: work appended after the interrupt
        with open(agent(i, "resumed", {"requestShape": "background"}) + ".jsonl", "a") as fh:
            fh.write(interrupt + "\n" + json.dumps({"type": "assistant", "message": {"role": "assistant"}}) + "\n")
    for i in range(no_meta_cold):
        os.utime(agent(i, "cold", None) + ".jsonl", (old, old))
    return tp


def agent_call(tp: str | None) -> dict:
    pl = {"tool_name": "Agent", "tool_input": {"description": "x", "prompt": "y", "subagent_type": "general-purpose"}}
    if tp is not None:
        pl["transcript_path"] = tp
    return pl


def expect(label: str, ok: bool, detail: str = "") -> None:
    global cases
    cases += 1
    print(("PASS " if ok else "FAIL ") + label + (f"  [{detail}]" if detail and not ok else ""))
    if not ok:
        failures.append(label)


def main() -> int:
    reg = tomllib.load(open(REGISTRY, "rb"))
    boundaries = reg["boundary"]
    with tempfile.TemporaryDirectory() as home:
        os.makedirs(os.path.join(home, "sessions"))
        n_rules = 0
        for b in boundaries:
            for i, rule in enumerate(b.get("deny") or []):
                n_rules += 1
                if not rule.get("block_examples"):
                    expect(f"{b['id']}#{i} has block examples", False, "a rule without one cannot be shown to work")
                for ex in rule.get("block_examples") or []:
                    code, ev = run(ex, home)
                    by = [e.get("boundary") for e in ev if e.get("verdict") == "boundary_refused"]
                    expect(f"{b['id']}#{i} blocks: {ex[:70]}", code == 2 and by == [b["id"]], f"exit {code}, refused by {by}")
                for ex in rule.get("allow_examples") or []:
                    code, ev = run(ex, home)
                    expect(f"{b['id']}#{i} allows: {ex[:70]}", code == 0, f"exit {code}, {[e.get('boundary') for e in ev]}")
            res = b.get("resources")
            if res:
                for ex in res.get("heavy_block_examples") or []:
                    code, ev = run(ex, home, {"AMUX_BOUNDARIES_HOST_SAMPLE": CRITICAL})
                    expect(f"{b['id']} refuses at critical: {ex[:60]}", code == 2 and any(e.get("verdict") == "resources_refused" for e in ev), f"exit {code}")
                    code, _ = run(ex, home)
                    expect(f"{b['id']} allows when ok: {ex[:60]}", code == 0, f"exit {code}")
                for ex in res.get("heavy_allow_examples") or []:
                    code, ev = run(ex, home, {"AMUX_BOUNDARIES_HOST_SAMPLE": CRITICAL})
                    expect(f"{b['id']} allows at critical: {ex[:60]}", code == 0, f"exit {code}")
                code, ev = run("AMUX_RESOURCES_ACK=\"hotfix\" cargo build", home, {"AMUX_BOUNDARIES_HOST_SAMPLE": CRITICAL})
                expect(f"{b['id']} records an acknowledged heavy command", code == 0 and any(e.get("verdict") == "resources_acknowledged" and e.get("ack") == "hotfix" for e in ev), f"exit {code} {ev}")
                # Disk budget (mxp-gs12 review, 2026-10-18), each verdict with its twin.
                GB = 1024 ** 3
                def sample(used, measured=True):
                    return json.dumps({"measured": measured, "bytes": used if measured else None,
                                       "paths": {"/scratch": used} if measured else {}, "n_considered": 1,
                                       "why_unmeasured": None if measured else "test"})
                over = {"AMUX_WORKER_DISK_GB": "1", "AMUX_DISK_USAGE_SAMPLE": sample(2 * GB)}
                code, ev = run("cargo build", home, over)
                expect(f"{b['id']} refuses heavy work past the disk budget", code == 2 and any(e.get("verdict") == "disk_budget_refused" and e.get("measured") is True for e in ev), f"exit {code} {ev}")
                code, ev = run("git status", home, over)
                expect(f"{b['id']} lets light work through past the disk budget", code == 0, f"exit {code} {ev}")
                code, ev = run("AMUX_DISK_ACK=\"release build\" cargo build", home, over)
                expect(f"{b['id']} records an acknowledged over-budget heavy command", code == 0 and any(e.get("verdict") == "disk_budget_acknowledged" and e.get("ack") == "release build" for e in ev), f"exit {code} {ev}")
                code, ev = run("cargo build", home, {"AMUX_WORKER_DISK_GB": "1", "AMUX_DISK_USAGE_SAMPLE": sample(int(0.85 * GB))})
                expect(f"{b['id']} warns at 85% of the disk budget and runs the command", code == 0 and any(e.get("verdict") == "disk_budget_warn" for e in ev), f"exit {code} {ev}")
                code, ev = run("cargo build", home, {"AMUX_WORKER_DISK_GB": "1", "AMUX_DISK_USAGE_SAMPLE": sample(int(0.10 * GB))})
                expect(f"{b['id']} is silent well under the disk budget", code == 0 and not any(str(e.get("verdict", "")).startswith("disk_budget") for e in ev), f"exit {code} {ev}")
                code, ev = run("cargo build", home, {"AMUX_WORKER_DISK_GB": "0", "AMUX_DISK_USAGE_SAMPLE": sample(2 * GB)})
                expect(f"{b['id']} AMUX_WORKER_DISK_GB=0 turns the disk check off", code == 0 and not any(str(e.get("verdict", "")).startswith("disk_budget") for e in ev), f"exit {code} {ev}")
                code, ev = run("cargo build", home, {"AMUX_WORKER_DISK_GB": "1", "AMUX_DISK_USAGE_SAMPLE": sample(0, measured=False)})
                expect(f"{b['id']} an unmeasured disk reading lets work through and says so", code == 0 and any(e.get("verdict") == "disk_budget_unmeasured" and e.get("measured") is False for e in ev), f"exit {code} {ev}")
                # The real measurement path: du on a scratch dir, then the refusal quotes it.
                scratch = os.path.join(home, "tmp", "boundary-test")
                os.makedirs(scratch, exist_ok=True)
                with open(os.path.join(scratch, "target.bin"), "wb") as fh:
                    fh.write(os.urandom(3 * 1024 * 1024))
                menv = {k: v for k, v in os.environ.items() if not k.startswith("AMUX_")}
                menv["AMUX_HOME"] = home
                subprocess.run([sys.executable, GUARD, "--measure-disk", "boundary-test"], env=menv, timeout=60)
                code, ev = run("cargo build", home, {"AMUX_WORKER_DISK_GB": "0.001"})
                hit = [e for e in ev if e.get("verdict") == "disk_budget_refused"]
                expect(f"{b['id']} measures real scratch with du and refuses past a 1 MB budget", code == 2 and hit and hit[0].get("used_bytes", 0) >= 3 * 1024 * 1024, f"exit {code} {ev}")
                code, ev = run("cargo build", home, {"AMUX_WORKER_DISK_GB": "1"})
                expect(f"{b['id']} the same real scratch passes a 1 GB budget", code == 0 and not any(e.get("verdict") == "disk_budget_refused" for e in ev), f"exit {code} {ev}")

        # Subagent cap (mxp-gs12 review, 2026-10-18), each verdict with its twin.
        n_sub = sum(1 for b in boundaries if b.get("kind") == "subagents")
        expect("the registry carries the subagents boundary", n_sub == 1, f"{n_sub} subagents boundaries")
        if n_sub:
            def refused(ev: list[dict], cause: str) -> list[dict]:
                return [e for e in ev if e.get("verdict") == "subagent_cap_refused" and e.get("cause") == cause]

            tp = conversation(home, "under", running=3, finished=5, stale=2, no_meta_cold=2)
            code, ev = run_payload(agent_call(tp), home)
            expect("subagents: 3 running under a cap of 4 is allowed (finished, stale and cold ones not counted)",
                   code == 0 and not any(str(e.get("verdict", "")).startswith("subagent_cap") for e in ev), f"exit {code} {ev}")
            tp = conversation(home, "at", running=4, finished=3)
            code, ev = run_payload(agent_call(tp), home)
            hit = refused(ev, "running_cap")
            expect("subagents: 4 running at a cap of 4 is refused, logged WARN with the count",
                   code == 2 and bool(hit) and hit[0].get("running") == 4 and hit[0].get("limit") == 4
                   and hit[0].get("level") == "WARN" and hit[0].get("measured") is True, f"exit {code} {ev}")
            expect("subagents: the refusal tells the model the count, the limit and when to retry",
                   "4 subagents running" in last_stderr and "limit is 4" in last_stderr and "Retry when" in last_stderr, last_stderr)
            # A restart ends every in-process subagent without a finished flag (2026-10-19, mxp-gs12).
            rtp = conversation(home, "restarted", running=9)
            code, ev = run_payload(agent_call(rtp), home, {"AMUX_SUBAGENT_HOST_START": str(time.time() + 60)})
            expect("subagents: agents last written before the host process started are not counted",
                   code == 0 and not refused(ev, "running_cap"), f"exit {code} {ev}")
            code, ev = run_payload(agent_call(rtp), home, {"AMUX_SUBAGENT_HOST_START": str(time.time() - 3600)})
            hit = refused(ev, "running_cap")
            expect("subagents: control: the same agents written after the host started are refused",
                   code == 2 and bool(hit) and hit[0].get("ended_by_restart") == 0 and hit[0].get("host_start_measured") is True,
                   f"exit {code} {ev}")
            stp = conversation(home, "stopped", running=3, stopped=2)
            code, ev = run_payload(agent_call(stp), home)
            expect("subagents: stopped agents (interrupt last, no finished flag) are not counted",
                   code == 0 and not refused(ev, "running_cap"), f"exit {code} {ev}")
            stp = conversation(home, "resumed", running=3, resumed=1)
            code, ev = run_payload(agent_call(stp), home)
            hit = refused(ev, "running_cap")
            expect("subagents: control: an agent resumed after its interrupt counts again",
                   code == 2 and bool(hit) and hit[0].get("running") == 4 and hit[0].get("ended_by_stop") == 0,
                   f"exit {code} {ev}")
            code, ev = run_payload({**agent_call(tp), "tool_name": "Task"}, home)
            expect("subagents: the Task spelling is capped the same", code == 2 and bool(refused(ev, "running_cap")), f"exit {code}")
            code, ev = run_payload({"tool_name": "Bash", "tool_input": {"command": "echo hi"}, "transcript_path": tp}, home)
            expect("subagents: other tools are not capped", code == 0, f"exit {code} {ev}")
            sub_tp = os.path.join(os.path.dirname(tp), "conv", "subagents", "agent-run0.jsonl")
            code, ev = run_payload(agent_call(sub_tp), home)
            expect("subagents: a subagent's own Agent call counts its siblings", code == 2 and bool(refused(ev, "running_cap")), f"exit {code} {ev}")
            code, ev = run_payload(agent_call(tp), home, {"AMUX_MAX_SUBAGENTS": "6"})
            expect("subagents: AMUX_MAX_SUBAGENTS=6 raises the cap", code == 0, f"exit {code} {ev}")
            code, ev = run_payload(agent_call(conversation(home, "two", running=2)), home, {"AMUX_MAX_SUBAGENTS": "2"})
            expect("subagents: AMUX_MAX_SUBAGENTS=2 lowers the cap", code == 2 and bool(refused(ev, "running_cap")), f"exit {code} {ev}")
            code, ev = run_payload(agent_call(tp), home, {"AMUX_MAX_SUBAGENTS": "0"})
            expect("subagents: AMUX_MAX_SUBAGENTS=0 turns the count cap off", code == 0 and not refused(ev, "running_cap"), f"exit {code} {ev}")
            code, ev = run_payload(agent_call(None), home)
            un = [e for e in ev if e.get("verdict") == "subagent_cap_allowed_unmeasured"]
            expect("subagents: an unmeasured count (no transcript_path) is allowed and logged measured=false",
                   code == 0 and bool(un) and un[0].get("measured") is False and "count" in (un[0].get("unmeasured") or []), f"exit {code} {ev}")
            code, ev = run_payload(agent_call(os.path.join(home, "projects", "nope.jsonl")), home)
            expect("subagents: a transcript that does not exist is unmeasured, not zero",
                   code == 0 and any(e.get("verdict") == "subagent_cap_allowed_unmeasured" and e.get("measured") is False for e in ev), f"exit {code} {ev}")
            low = json.dumps({"measured": True, "available": True, "limits": [
                {"kind": "session", "percent": 95, "resets_at": "2026-10-19T01:40:00+00:00", "scope": None},
                {"kind": "weekly_scoped", "percent": 100, "scope": {"model": {"display_name": "Fable"}}}]})
            calm = conversation(home, "calm", running=1)
            code, ev = run_payload(agent_call(calm), home, {"AMUX_USAGE_SAMPLE": low})
            hit = refused(ev, "usage_headroom")
            expect("subagents: 5% account headroom under a 10% floor is refused, naming the reset",
                   code == 2 and bool(hit) and hit[0].get("headroom_pct") == 5 and "2026-10-19T01:40" in last_stderr
                   and "5% usage left" in last_stderr, f"exit {code} {ev} {last_stderr}")
            scoped_only = json.dumps({"measured": True, "available": True, "limits": [
                {"kind": "session", "percent": 50, "scope": None},
                {"kind": "weekly_scoped", "percent": 100, "scope": {"model": {"display_name": "Fable"}}}]})
            code, ev = run_payload(agent_call(calm), home, {"AMUX_USAGE_SAMPLE": scoped_only})
            expect("subagents: a model-scoped window does not set account headroom", code == 0, f"exit {code} {ev}")
            code, ev = run_payload(agent_call(calm), home, {"AMUX_USAGE_SAMPLE": json.dumps({"measured": False})})
            un = [e for e in ev if e.get("verdict") == "subagent_cap_allowed_unmeasured"]
            expect("subagents: unmeasured headroom is allowed and says so",
                   code == 0 and bool(un) and un[0].get("unmeasured") == ["headroom"], f"exit {code} {ev}")
            code, ev = run_payload(agent_call(calm), home, {"AMUX_USAGE_SAMPLE": None, "AMUX_URL": "https://127.0.0.1:9"})
            expect("subagents: an unreachable amux server is unmeasured headroom, allowed",
                   code == 0 and any(e.get("verdict") == "subagent_cap_allowed_unmeasured" for e in ev), f"exit {code} {ev}")
            # Isolated workers as launched: AMUX_SESSION unset, AMUX_ISOLATED_WORKER set.
            iso_env = {"AMUX_SESSION": None, "AMUX_ISOLATED_WORKER": "iso-big"}
            code, ev = run_payload(agent_call(tp), home, iso_env)
            hit = refused(ev, "running_cap")
            expect("subagents: an isolated worker (AMUX_ISOLATED_WORKER, no AMUX_SESSION) is capped",
                   code == 2 and bool(hit) and hit[0].get("session") == "iso-big", f"exit {code} {ev}")
            iso_exempt = os.path.join(home, "iso-exempt.toml")
            with open(REGISTRY) as src, open(iso_exempt, "w") as fh:
                fh.write(src.read().replace("apply_to_isolated = true", "apply_to_isolated = false"))
            code, ev = run_payload(agent_call(tp), home, iso_env, registry=iso_exempt)
            expect("subagents: apply_to_isolated = false lets an isolated worker fan out", code == 0 and not ev, f"exit {code} {ev}")
            code, ev = run_payload(agent_call(tp), home, registry=iso_exempt)
            expect("subagents: apply_to_isolated = false still caps a non-isolated worker", code == 2, f"exit {code} {ev}")

        sample = boundaries[0]["deny"][0]["block_examples"][0]
        code, _ = run(sample, home, {"AMUX_SESSION": ""})
        expect("a non-amux session is not refused", code == 0, f"exit {code}")
        with open(os.path.join(home, "sessions", "iso.env"), "w") as fh:
            fh.write("CC_ISOLATED=1\n")
        code, _ = run(sample, home, {"AMUX_SESSION": "iso"})
        expect("an isolated worker is refused (apply_to_isolated defaults on)", code == 2, f"exit {code}")
        # The shape an isolated launch really has: no AMUX_SESSION, identity in AMUX_ISOLATED_WORKER.
        code, _ = run(sample, home, {"AMUX_SESSION": None, "AMUX_ISOLATED_WORKER": "iso-real"})
        expect("an isolated worker as launched (AMUX_ISOLATED_WORKER only) is refused", code == 2, f"exit {code}")
        exempt = os.path.join(home, "exempt.toml")
        with open(REGISTRY) as src, open(exempt, "w") as fh:
            fh.write(src.read().replace("apply_to_isolated = true", "apply_to_isolated = false"))
        code, _ = run(sample, home, {"AMUX_SESSION": "iso"}, registry=exempt)
        expect("apply_to_isolated = false exempts an isolated worker", code == 0, f"exit {code}")
        code, _ = run(sample, home, {"AMUX_SESSION": None, "AMUX_ISOLATED_WORKER": "iso-real"}, registry=exempt)
        expect("apply_to_isolated = false exempts an isolated worker as launched", code == 0, f"exit {code}")
        code, _ = run(sample, home, {"AMUX_SESSION": "boundary-test"}, registry=exempt)
        expect("apply_to_isolated = false still refuses a non-isolated worker", code == 2, f"exit {code}")

        # Owner override: the same id with enabled = false turns it off.
        merged = os.path.join(home, "boundaries.toml")
        with open(merged, "w") as fh:
            fh.write(f'[[boundary]]\nid = "{boundaries[0]["id"]}"\nenabled = false\n')
        env = {"AMUX_BOUNDARIES_FILE": ""}  # use the default path search
        installed = os.path.join(home, "hooks")
        os.makedirs(installed)
        code, _ = run(sample, home, env, registry="")
        expect("an owner override can disable a boundary", code == 0, f"exit {code}")

        code, ev = run("echo hi", home, {"AMUX_BOUNDARIES_FILE": os.path.join(home, "missing.toml")})
        expect("a missing registry fails open and says so", code == 0 and any(e.get("verdict") == "boundary_registry_unloadable" for e in ev), f"exit {code}")

    print(f"{cases - len(failures)} of {cases} cases passed over {n_rules} deny rules in {len(boundaries)} boundaries")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
