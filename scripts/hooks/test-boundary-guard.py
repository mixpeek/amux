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


GATE = os.path.join(REPO, "scripts", "tenant-gate.sh")
INCIDENT_NS = "ephemeral-r40255478404"
FAKE_KUBECTL = r"""#!/usr/bin/env bash
# A kubectl that answers `get` from fixture files and records its argv.
echo "$*" >> "$FAKE_KUBE_DIR/argv.log"
case " $* " in *" get "*) ;; *) echo "fake kubectl: refusing non-get: $*" >&2; exit 9 ;; esac
[[ -n "${FAKE_KUBE_FAIL:-}" ]] && { echo "The connection to the server was refused" >&2; exit 1; }
if [[ "$*" == *scaledobjects.keda.sh* ]]; then
  [[ -n "${FAKE_KUBE_NO_KEDA:-}" ]] && { echo 'error: the server doesn'"'"'t have a resource type "scaledobjects"' >&2; exit 1; }
  cat "$FAKE_KUBE_DIR/${FAKE_KUBE_SET:-incident}.so.json"
else
  cat "$FAKE_KUBE_DIR/${FAKE_KUBE_SET:-incident}.rest.json"
fi
"""


def incident_fixture() -> tuple[dict, dict]:
    """ephemeral-r40255478404 as it stood on 2026-10-27: deploy/api, the
    mvs-primary shard, api's own ScaledObject (paused), and the second
    ScaledObject that scales mvs-primary to zero when no app=api pods exist."""
    def workload(kind, name, labels, replicas):
        return {"kind": kind, "metadata": {"name": name}, "spec": {"replicas": replicas, "template": {"metadata": {"labels": labels}}},
                "status": {"readyReplicas": replicas}}
    so = {"items": [
        {"kind": "ScaledObject", "metadata": {"name": "api", "annotations": {"autoscaling.keda.sh/paused": "true"}},
         "spec": {"scaleTargetRef": {"name": "api"}, "minReplicaCount": 0, "maxReplicaCount": 4,
                  "triggers": [{"type": "prometheus", "metadata": {"query": "sum(rate(http_requests_total[1m]))", "threshold": "5"}}]}},
        {"kind": "ScaledObject", "metadata": {"name": "mvs-primary-scale-to-zero"},
         "spec": {"scaleTargetRef": {"kind": "StatefulSet", "name": "mvs-primary"}, "minReplicaCount": 0, "maxReplicaCount": 1,
                  "triggers": [{"type": "kubernetes-workload", "metadata": {"podSelector": "app=api", "value": "1"}}]}}]}
    rest = {"items": [
        workload("Deployment", "api", {"app": "api"}, 1),
        workload("StatefulSet", "mvs-primary", {"app": "mvs-primary"}, 1),
        {"kind": "PodDisruptionBudget", "metadata": {"name": "mvs-primary"},
         "spec": {"minAvailable": 1, "selector": {"matchLabels": {"app": "mvs-primary"}}}}]}
    return so, rest


def tenant_gate_cases(home: str, boundaries: list[dict]) -> None:
    """tenant-workload-gate (2026-10-27): the four incidents refused without a
    receipt and allowed with one from the REAL scripts/tenant-gate.sh, and the
    negatives (expired, other namespace, other session, other context, read-only,
    non-tenant, all namespaces, an innocent first segment)."""
    b = next((x for x in boundaries if x.get("id") == "tenant-workload-gate"), None)
    expect("the registry carries the tenant-workload-gate boundary", b is not None)
    if b is None:
        return
    rr = next(r["requires_receipt"] for r in b["deny"] if r.get("requires_receipt"))
    refused_v, allowed_v = rr["verdict_refused"], rr["verdict_allowed"]
    kdir = os.path.join(home, "fake-kube")
    os.makedirs(kdir, exist_ok=True)
    kubectl = os.path.join(kdir, "kubectl")
    with open(kubectl, "w") as fh:
        fh.write(FAKE_KUBECTL)
    os.chmod(kubectl, 0o755)
    so, rest = incident_fixture()
    for name, body in (("incident.so", so), ("incident.rest", rest), ("empty.so", {"items": []}), ("empty.rest", {"items": []})):
        with open(os.path.join(kdir, name + ".json"), "w") as fh:
            json.dump(body, fh)
    gates = os.path.join(home, "tenant-gates")
    receipt = os.path.join(gates, "boundary-test", f"default__{INCIDENT_NS}.json")

    def gate(ctx: str, ns: str, extra: dict | None = None, reason: str = "incident replay test") -> subprocess.CompletedProcess:
        env = {k: v for k, v in os.environ.items() if not k.startswith("AMUX_")}
        env.update({"AMUX_HOME": home, "AMUX_SESSION": "boundary-test", "KUBECTL": kubectl, "FAKE_KUBE_DIR": kdir})
        for k, v in (extra or {}).items():
            if v is None:
                env.pop(k, None)
            else:
                env[k] = v
        return subprocess.run(["bash", GATE, ctx, ns, reason], text=True, capture_output=True, env=env, timeout=30)

    def verdicts(ev: list[dict]) -> list[dict]:
        return [e for e in ev if e.get("boundary") == "tenant-workload-gate"]

    incidents = [
        ("1 scale api to zero", f"kubectl -n {INCIDENT_NS} scale deploy/api --replicas=0"),
        ("1 pause its ScaledObject", f"kubectl -n {INCIDENT_NS} annotate scaledobject api autoscaling.keda.sh/paused=true --overwrite"),
        ("2 teardown deletes a booting pod", f"kubectl delete pod mvs-primary-0 -n {INCIDENT_NS} --grace-period=0"),
        ("3 mass rollout restart", f"for d in api worker mvs-primary; do kubectl -n {INCIDENT_NS} rollout restart deploy/$d; done"),
        ("4 replay burst at the shard", f"hey -z 60s -c 200 -m POST http://api.{INCIDENT_NS}.svc.cluster.local/v1/retrievers/r/execute"),
    ]
    for label, cmd in incidents:
        code, ev = run(cmd, home)
        v = verdicts(ev)
        expect(f"tenant gate: incident {label} is refused without a receipt",
               code == 2 and [e.get("verdict") for e in v] == [refused_v] and v[0]["targets"][0]["cause"] == "no_receipt"
               and v[0]["targets"][0]["namespace"] == INCIDENT_NS, f"exit {code} {ev}")
        expect(f"tenant gate: refusal for incident {label} names the gate command with context and namespace",
               f"tenant-gate.sh default {INCIDENT_NS} " in last_stderr, last_stderr[:400])

    # The gate itself, against the incident namespace.
    p = gate("default", INCIDENT_NS)
    rec = json.load(open(receipt)) if os.path.exists(receipt) else {}
    expect("tenant gate script: reads the incident namespace and writes a receipt", p.returncode == 0 and rec.get("session") == "boundary-test"
           and rec.get("namespace") == INCIDENT_NS and rec.get("context") == "default" and len(rec.get("sha256", "")) == 64
           and rec.get("reason") == "incident replay test", f"exit {p.returncode} {p.stderr} {rec}")
    expect("tenant gate script: names the ScaledObject that watches app=api pods and what it scales",
           "mvs-primary-scale-to-zero scales StatefulSet/mvs-primary" in p.stdout
           and "WATCHES pods app=api: Deployment/api. Scaling those changes StatefulSet/mvs-primary" in p.stdout
           and "PAUSED" in p.stdout and (rec.get("summary") or {}).get("watches", [{}])[0].get("watched") == ["Deployment/api"],
           p.stdout)
    argv = open(os.path.join(kdir, "argv.log")).read().split("\n")
    expect("tenant gate script: only ever calls kubectl get, with no --context for `default`",
           all(" get " in f" {a} " for a in argv if a) and not any("--context" in a for a in argv), str(argv))

    for label, cmd in incidents:
        code, ev = run(cmd, home)
        v = verdicts(ev)
        expect(f"tenant gate: incident {label} is allowed with a fresh receipt and logged",
               code == 0 and [e.get("verdict") for e in v] == [allowed_v]
               and v[0]["targets"][0].get("receipt_sha256") == rec.get("sha256"), f"exit {code} {ev}")

    scale = incidents[0][1]
    # Expired receipt; then the window override.
    stale = dict(rec, ts=time.time() - 3600)
    with open(receipt, "w") as fh:
        json.dump(stale, fh)
    code, ev = run(scale, home)
    v = verdicts(ev)
    expect("tenant gate: a receipt older than 30 minutes is refused", code == 2 and v and v[0]["targets"][0]["cause"] == "receipt_expired", f"exit {code} {ev}")
    code, ev = run(scale, home, {"AMUX_TENANT_GATE_MAX_AGE_S": "7200"})
    expect("tenant gate: AMUX_TENANT_GATE_MAX_AGE_S widens the window", code == 0 and verdicts(ev)[0]["verdict"] == allowed_v, f"exit {code} {ev}")
    with open(receipt, "w") as fh:
        json.dump(rec, fh)

    # Another namespace, another session, another context.
    code, ev = run(f"kubectl -n ephemeral-r999 scale deploy/api --replicas=0", home)
    expect("tenant gate: a receipt for another namespace does not cover this one",
           code == 2 and verdicts(ev)[0]["targets"][0]["cause"] == "no_receipt", f"exit {code} {ev}")
    code, ev = run(scale, home, {"AMUX_SESSION": "other-worker"})
    expect("tenant gate: another worker's receipt does not cover this worker",
           code == 2 and verdicts(ev)[0]["targets"][0]["cause"] == "no_receipt" and verdicts(ev)[0]["session"] == "other-worker", f"exit {code} {ev}")
    with open(receipt, "w") as fh:
        json.dump(dict(rec, session="other-worker"), fh)
    code, ev = run(scale, home)
    expect("tenant gate: a receipt naming another worker, in this worker's directory, is refused",
           code == 2 and verdicts(ev)[0]["targets"][0]["cause"] == "receipt_other_session", f"exit {code} {ev}")
    with open(receipt, "w") as fh:
        json.dump(dict(rec, namespace="ephemeral-r999"), fh)
    code, ev = run(scale, home)
    expect("tenant gate: a receipt at this path whose fields name another namespace is refused",
           code == 2 and verdicts(ev)[0]["targets"][0]["cause"] == "receipt_other_target", f"exit {code} {ev}")
    with open(receipt, "w") as fh:
        fh.write("{not json")
    code, ev = run(scale, home)
    expect("tenant gate: an unreadable receipt is refused", code == 2 and verdicts(ev)[0]["targets"][0]["cause"] == "receipt_unreadable", f"exit {code} {ev}")
    with open(receipt, "w") as fh:
        json.dump(rec, fh)
    ctx = "gke_mixpeek-inference-463103_us-central1_mixpeek-engine"
    code, ev = run(f"kubectl --context {ctx} -n {INCIDENT_NS} scale deploy/api --replicas=0", home)
    expect("tenant gate: a receipt for context `default` does not cover a named --context, and the refusal says so",
           code == 2 and verdicts(ev)[0]["targets"][0]["context"] == ctx and f"tenant-gate.sh {ctx} {INCIDENT_NS}" in last_stderr
           and "under other contexts" in last_stderr, f"exit {code} {last_stderr[:500]}")
    p = gate(ctx, INCIDENT_NS)
    argv = open(os.path.join(kdir, "argv.log")).read()
    code, ev = run(f"kubectl --context={ctx} -n {INCIDENT_NS} scale deploy/api --replicas=0", home)
    expect("tenant gate: the gate for a named context passes --context and its receipt covers --context=<ctx>",
           p.returncode == 0 and f"--context {ctx}" in argv and code == 0, f"gate {p.returncode} {p.stderr} guard {code} {ev}")
    arn = "arn:aws:eks:us-east-1:1:cluster/x"
    p = gate(arn, INCIDENT_NS)
    code, ev = run(f"kubectl --context {arn} -n {INCIDENT_NS} scale deploy/api --replicas=0", home)
    expect("tenant gate: a context with / and : keys the same file in the gate and the guard",
           p.returncode == 0 and code == 0, f"gate {p.returncode} {p.stderr} guard {code} {ev}")

    # Out of scope or read-only: no receipt needed, nothing logged.
    for cmd in (f"kubectl -n ephemeral-r999 get pods -o wide", f"kubectl -n ephemeral-r999 describe scaledobject x",
                f"kubectl -n ephemeral-r999 logs deploy/api", f"kubectl -n ephemeral-r999 top pods",
                f"kubectl -n ephemeral-r999 get events", "kubectl auth can-i scale deploy -n ephemeral-r999",
                f"kubectl -n ephemeral-r999 port-forward svc/api 8080:80",
                "kubectl -n kube-system scale deploy/coredns --replicas=2", "kubectl -n default delete pod x",
                "helm upgrade grafana charts/grafana -n monitoring"):
        code, ev = run(cmd, home)
        expect(f"tenant gate: passes without a receipt: {cmd[:60]}", code == 0 and not verdicts(ev), f"exit {code} {ev}")

    code, ev = run("kubectl delete pods --all -A", home)
    expect("tenant gate: a mutation across all namespaces is refused (no one receipt covers it)",
           code == 2 and verdicts(ev)[0]["targets"][0]["cause"] == "all_namespaces", f"exit {code} {ev}")
    code, ev = run(f"echo starting && kubectl -n ephemeral-r999 scale deploy/api --replicas=0", home)
    expect("tenant gate: an innocent first segment (echo) does not exempt the mutation after it", code == 2, f"exit {code} {ev}")
    code, ev = run(f"kubectl -n {INCIDENT_NS} get pods && kubectl -n ephemeral-r999 rollout restart deploy/api", home)
    v = verdicts(ev)
    expect("tenant gate: each mutating segment needs its own namespace's receipt",
           code == 2 and [t["namespace"] for t in v[0]["targets"]] == ["ephemeral-r999"], f"exit {code} {ev}")
    code, ev = run(f"sh -c \"kubectl -n ephemeral-r999 delete pod api-0\"", home)
    expect("tenant gate: a mutation inside sh -c is seen", code == 2, f"exit {code} {ev}")

    # The gate refuses to write a receipt for a failed or empty read.
    os.remove(receipt)
    p = gate("default", INCIDENT_NS, {"FAKE_KUBE_FAIL": "1"})
    expect("tenant gate script: a failed read exits nonzero and writes no receipt", p.returncode != 0 and not os.path.exists(receipt), f"exit {p.returncode}")
    p = gate("default", INCIDENT_NS, {"FAKE_KUBE_SET": "empty"})
    expect("tenant gate script: an empty namespace exits nonzero and writes no receipt",
           p.returncode != 0 and not os.path.exists(receipt) and "empty read" in p.stderr, f"exit {p.returncode} {p.stderr}")
    p = gate("default", INCIDENT_NS, {"AMUX_SESSION": None})
    expect("tenant gate script: no worker identity writes no receipt", p.returncode != 0 and not os.path.exists(receipt), f"exit {p.returncode}")
    p = gate("default", INCIDENT_NS, {"FAKE_KUBE_NO_KEDA": "1"})
    expect("tenant gate script: a cluster without the KEDA CRD is still read, and says so",
           p.returncode == 0 and os.path.exists(receipt) and "no KEDA CRD" in p.stdout, f"exit {p.returncode} {p.stderr}")
    # An isolated worker as launched: identity in AMUX_ISOLATED_WORKER only.
    iso = {"AMUX_SESSION": None, "AMUX_ISOLATED_WORKER": "iso-tenant"}
    code, ev = run(scale, home, iso)
    expect("tenant gate: an isolated worker is refused without its own receipt", code == 2, f"exit {code} {ev}")
    p = gate("default", INCIDENT_NS, iso)
    code, ev = run(scale, home, iso)
    expect("tenant gate: an isolated worker's own receipt lets it through", p.returncode == 0 and code == 0, f"gate {p.returncode} {p.stderr} guard {code} {ev}")


ASK = {"tool_name": "AskUserQuestion", "tool_input": {"questions": [{
    "question": "Resubmit the failed GS-12 batch now or wait for the morning window?", "header": "Batch",
    "multiSelect": False, "options": [{"label": "Resubmit now", "description": "uses the night window"},
                                      {"label": "Wait", "description": "owner reviews first"}]}]}}


def modal_cases(home: str, boundaries: list[dict]) -> None:
    """orchestrator-modal (2026-10-27): an orchestrator never opens a modal prompt."""
    b = next((b for b in boundaries if b.get("kind") == "modal"), None)
    expect("the registry carries the orchestrator-modal boundary", b is not None)
    if b is None:
        return
    cfg = b["modal"]
    verdict = cfg.get("verdict")
    expect("orchestrator-modal: the verdict is modal_prompt_refused", verdict == "modal_prompt_refused", str(verdict))
    # The guard only sees the tools its settings matcher names.
    import importlib.util
    spec = importlib.util.spec_from_file_location("installer", os.path.join(HERE, "install-claude-status-hooks.py"))
    inst = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(inst)
    import re as _re
    matcher = inst.canonical_boundary_guard("/x/boundary-guard.py")[0]["matcher"]
    sub = next((x for x in boundaries if x.get("kind") == "subagents"), {}).get("subagents") or {}
    wanted = list(cfg.get("tools") or []) + list(sub.get("tools") or [])
    missing = [t for t in wanted if not _re.search(matcher, t)]
    expect("the installed guard matcher names every modal and subagent tool", not missing, f"missing {missing} from {matcher}")
    expect("the guard matcher stays anchored (no AskUserQuestionX)", not _re.search(matcher, "AskUserQuestionX"), matcher)

    def modal(ev: list[dict]) -> list[dict]:
        return [e for e in ev if e.get("verdict") == verdict and e.get("boundary") == b["id"]]

    listed = cfg["orchestrators"][0]
    for tool in cfg.get("tools") or []:
        pl = ASK if tool == "AskUserQuestion" else {"tool_name": tool, "tool_input": {"plan": "x"}}
        code, ev = run_payload(pl, home, {"AMUX_SESSION": None, "AMUX_ISOLATED_WORKER": listed})
        hit = modal(ev)
        expect(f"orchestrator-modal: {tool} is refused for listed orchestrator {listed} (isolated, as launched)",
               code == 2 and bool(hit) and hit[0].get("tool") == tool and hit[0].get("level") == "WARN", f"exit {code} {ev}")
    run_payload(ASK, home, {"AMUX_SESSION": listed})
    expect("orchestrator-modal: the refusal restates the question as numbered options and says keep working",
           "1. Resubmit now" in last_stderr and "2. Wait" in last_stderr and "numbered options" in last_stderr
           and "keep working" in last_stderr, last_stderr)
    code, ev = run_payload(ASK, home, {"AMUX_SESSION": "some-builder"})
    expect("orchestrator-modal: AskUserQuestion is allowed for an unlisted worker", code == 0 and not modal(ev), f"exit {code} {ev}")
    code, ev = run_payload(ASK, home, {"AMUX_SESSION": "some-builder", "AMUX_ORCHESTRATOR": "1"})
    expect("orchestrator-modal: AMUX_ORCHESTRATOR=1 makes an unlisted worker an orchestrator",
           code == 2 and bool(modal(ev)) and "AMUX_ORCHESTRATOR" in modal(ev)[0].get("orchestrator_by", ""), f"exit {code} {ev}")
    code, ev = run_payload(ASK, home, {"AMUX_SESSION": listed, "AMUX_ORCHESTRATOR": "0"})
    expect("orchestrator-modal: AMUX_ORCHESTRATOR=0 releases a listed worker", code == 0 and not modal(ev), f"exit {code} {ev}")
    env_file = os.path.join(home, "sessions", "some-builder.env")
    with open(env_file, "w") as fh:
        fh.write('AMUX_ORCHESTRATOR="1"\n')
    code, ev = run_payload(ASK, home, {"AMUX_SESSION": "some-builder"})
    expect("orchestrator-modal: a scoped AMUX_ORCHESTRATOR=1 in sessions/<worker>.env applies without a restart",
           code == 2 and bool(modal(ev)), f"exit {code} {ev}")
    os.unlink(env_file)
    code, ev = run_payload({"tool_name": "Bash", "tool_input": {"command": "echo hi"}}, home, {"AMUX_SESSION": listed})
    expect("orchestrator-modal: other tools are untouched for an orchestrator", code == 0 and not modal(ev), f"exit {code} {ev}")
    # Nothing configured: an empty list and no env refuse nobody.
    bare = os.path.join(home, "no-orchestrators.toml")
    with open(REGISTRY) as src, open(bare, "w") as fh:
        text = src.read()
        fh.write(text.replace('orchestrators = ["amux-helper", "mxp-gs12", "tubescience-parity"]', "orchestrators = []"))
    expect("orchestrator-modal: test fixture emptied the orchestrators list", "orchestrators = []" in open(bare).read())
    code, ev = run_payload(ASK, home, {"AMUX_SESSION": listed}, registry=bare)
    expect("orchestrator-modal: with nothing configured AskUserQuestion is allowed", code == 0 and not modal(ev), f"exit {code} {ev}")


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
                refused_verdict = (rule.get("requires_receipt") or {}).get("verdict_refused") or "boundary_refused"
                for ex in rule.get("block_examples") or []:
                    code, ev = run(ex, home)
                    by = [e.get("boundary") for e in ev if e.get("verdict") == refused_verdict]
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
                # Mac disk RCA 20261025-190143: du on a 236 GB scratch took 896s, past the
                # old 240s deadline, so the budget never applied. A measurement 10 minutes
                # in must still hold the lock: no second du starts beside it.
                lock = os.path.join(home, "state", "disk-usage", "boundary-test.json.measuring")
                cache = os.path.join(home, "state", "disk-usage", "boundary-test.json")
                with open(cache, "w") as fh:
                    json.dump({"worker": "boundary-test", "ts": time.time() - 7200, "measured": True, "bytes": 1, "paths": {}, "n_considered": 1}, fh)
                open(lock, "w").close()
                ten_min_ago = time.time() - 600
                os.utime(lock, (ten_min_ago, ten_min_ago))
                code, ev = run("cargo build", home, {"AMUX_WORKER_DISK_GB": "1"})
                expect(f"{b['id']} a disk measurement 10 minutes in keeps its lock (no second du)", abs(os.stat(lock).st_mtime - ten_min_ago) < 1, f"lock mtime moved by {os.stat(lock).st_mtime - ten_min_ago:.0f}s")
                os.unlink(lock)

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
            # A scope change writes sessions/<worker>.env; the guard reads it live,
            # and it wins over the launch-time process env.
            sess_dir = os.path.join(home, "sessions"); os.makedirs(sess_dir, exist_ok=True)
            env_file = os.path.join(sess_dir, "boundary-test.env")
            with open(env_file, "w") as f:
                f.write('# updated: test\nAMUX_MAX_SUBAGENTS="6"\n')
            code, ev = run_payload(agent_call(tp), home)
            expect("subagents: a scoped AMUX_MAX_SUBAGENTS in sessions/<worker>.env applies without a restart", code == 0, f"exit {code} {ev}")
            with open(env_file, "w") as f:
                f.write("AMUX_MAX_SUBAGENTS=2\n")
            code, ev = run_payload(agent_call(tp), home, {"AMUX_MAX_SUBAGENTS": "6"})
            expect("subagents: the scoped value wins over the launch env", code == 2 and bool(refused(ev, "running_cap")), f"exit {code} {ev}")
            os.unlink(env_file)
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
            # Priority-aware over-cap allowance and subagent age (2026-10-27, MSG-98201).
            def marked(tp: str, desc: str = "[P1] land the shard fix", prompt: str = "y") -> dict:
                pl = agent_call(tp)
                pl["tool_input"] = {**pl["tool_input"], "description": desc, "prompt": prompt}
                return pl

            def verdict(ev: list[dict], v: str) -> list[dict]:
                return [e for e in ev if e.get("verdict") == v]

            aged = conversation(home, "aged", running=4)
            subs_dir = os.path.join(os.path.dirname(aged), "conv", "subagents")
            three_h = time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(time.time() - 3 * 3600 - 120))
            with open(os.path.join(subs_dir, "agent-run0.jsonl"), "w") as fh:
                fh.write(json.dumps({"type": "user", "message": {"role": "user", "content": "say \"timestamp\":\"2001-01-01T00:00:00Z\""},
                                     "timestamp": three_h}) + "\n")
            with open(os.path.join(subs_dir, "agent-run0.meta.json"), "w") as fh:
                json.dump({"requestShape": "background", "description": "BM25 parity sweep"}, fh)
            code, ev = run_payload(agent_call(aged), home)
            hit = refused(ev, "running_cap")
            ages = {a.get("id"): a.get("age_s") for a in (hit[0].get("running_agents") or [])} if hit else {}
            expect("subagents: an unmarked call at the cap is refused and logs each running agent's age",
                   code == 2 and bool(hit) and 3 * 3600 < (ages.get("run0") or 0) < 4 * 3600
                   and hit[0].get("oldest_age_s") == ages.get("run0") and len(ages) == 4, f"exit {code} {ev}")
            expect("subagents: the refusal names the marker and lists running agents with description and age",
                   "[P1]" in last_stderr and "BM25 parity sweep" in last_stderr and "running 3h0" in last_stderr
                   and "TaskStop" in last_stderr, last_stderr)
            expect("subagents: an agent running for hours logs subagent_long_running for a sweep",
                   bool(verdict(ev, "subagent_long_running")), f"{ev}")
            code, ev = run_payload(marked(aged), home)
            hit = verdict(ev, "subagent_cap_preempt_allowed")
            expect("subagents: a [P1] call at the cap is allowed once, logged with the running count",
                   code == 0 and bool(hit) and hit[0].get("running") == 4 and hit[0].get("limit") == 4
                   and hit[0].get("priority_marker") == "[P1]" and not refused(ev, "running_cap"), f"exit {code} {ev}")
            code, ev = run_payload(marked(aged, desc="parity", prompt="#1 priority: land it"), home)
            expect("subagents: a #1 marker at the start of the prompt counts too",
                   code == 0 and bool(verdict(ev, "subagent_cap_preempt_allowed")), f"exit {code} {ev}")
            code, ev = run_payload(marked(aged, desc="#10 of 12 sweeps"), home)
            expect("subagents: #10 is not the #1 marker", code == 2 and bool(refused(ev, "running_cap")), f"exit {code} {ev}")
            code, ev = run_payload(marked(aged, desc="fix [P1] later"), home)
            expect("subagents: a marker mid-description does not count", code == 2 and bool(refused(ev, "running_cap")), f"exit {code} {ev}")
            over = conversation(home, "over", running=5)
            code, ev = run_payload(marked(over), home)
            hit = refused(ev, "over_cap_bound")
            expect("subagents: a marked call past cap + over_cap_max is refused (cause over_cap_bound)",
                   code == 2 and bool(hit) and hit[0].get("running") == 5 and "at most 5" in last_stderr, f"exit {code} {ev}")
            code, ev = run_payload(marked(conversation(home, "below", running=2)), home)
            expect("subagents: below the cap a marked call logs nothing",
                   code == 0 and not any(str(e.get("verdict", "")).startswith("subagent_cap") for e in ev), f"exit {code} {ev}")
            code, ev = run_payload(marked(aged), home, {"AMUX_USAGE_SAMPLE": low})
            expect("subagents: a marked call at the cap still obeys the usage headroom floor",
                   code == 2 and bool(refused(ev, "usage_headroom")) and not verdict(ev, "subagent_cap_preempt_allowed"), f"exit {code} {ev}")
            no_over = os.path.join(home, "no-over-cap.toml")
            with open(REGISTRY) as src, open(no_over, "w") as fh:
                fh.write(src.read().replace("over_cap_max = 1", "over_cap_max = 0"))
            expect("subagents: test fixture set over_cap_max = 0", "over_cap_max = 0" in open(no_over).read())
            code, ev = run_payload(marked(aged), home, registry=no_over)
            expect("subagents: over_cap_max = 0 turns the allowance off", code == 2 and bool(refused(ev, "over_cap_bound")), f"exit {code} {ev}")
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

        modal_cases(home, boundaries)
        tenant_gate_cases(home, boundaries)

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
