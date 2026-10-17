#!/usr/bin/env python3
"""Run every example in boundaries.toml through the real boundary-guard.py.

- every deny rule's `block_examples` must be refused BY THAT BOUNDARY
- every `allow_examples` entry must pass the whole guard
- the resources boundary's `heavy_block_examples` must be refused when the
  host is critical and pass when it is ok; `heavy_allow_examples` must pass
  when critical; an acknowledged heavy command passes and is logged
- non-amux sessions are never refused; isolated workers are refused unless
  the registry sets apply_to_isolated = false
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


def run(example: str, home: str, env_extra: dict | None = None, registry: str = REGISTRY) -> tuple[int, list[dict]]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("AMUX_")}
    env.update({"AMUX_HOME": home, "AMUX_SESSION": "boundary-test", "AMUX_BOUNDARIES_FILE": registry,
                "AMUX_BOUNDARIES_HOST_SAMPLE": OK})
    env.update(env_extra or {})
    log = os.path.join(home, "logs", "boundaries.jsonl")
    if os.path.exists(log):
        os.remove(log)
    p = subprocess.run([sys.executable, GUARD], input=json.dumps(payload(example)), text=True,
                       capture_output=True, env=env, timeout=20)
    events = [json.loads(l) for l in open(log)] if os.path.exists(log) else []
    return p.returncode, events


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

        sample = boundaries[0]["deny"][0]["block_examples"][0]
        code, _ = run(sample, home, {"AMUX_SESSION": ""})
        expect("a non-amux session is not refused", code == 0, f"exit {code}")
        with open(os.path.join(home, "sessions", "iso.env"), "w") as fh:
            fh.write("CC_ISOLATED=1\n")
        code, _ = run(sample, home, {"AMUX_SESSION": "iso"})
        expect("an isolated worker is refused (apply_to_isolated defaults on)", code == 2, f"exit {code}")
        exempt = os.path.join(home, "exempt.toml")
        with open(REGISTRY) as src, open(exempt, "w") as fh:
            fh.write(src.read().replace("apply_to_isolated = true", "apply_to_isolated = false"))
        code, _ = run(sample, home, {"AMUX_SESSION": "iso"}, registry=exempt)
        expect("apply_to_isolated = false exempts an isolated worker", code == 0, f"exit {code}")
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
