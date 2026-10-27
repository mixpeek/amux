#!/usr/bin/env bash
# Read what scales a live tenant namespace, print it, and record a gate receipt.
#
#   scripts/tenant-gate.sh <kube-context> <namespace> "<why you are changing it>"
#
# Read-only: one `kubectl get` of ScaledObjects (KEDA), HPAs, PDBs,
# Deployments and StatefulSets. It prints WHAT SCALES WHAT: each ScaledObject's
# target, its triggers, any trigger that watches ANOTHER workload's pods (a
# kubernetes-workload trigger with a podSelector), and paused annotations; each
# HPA's target; each PDB's selector. Then it writes a receipt to
#   $AMUX_HOME/tenant-gates/<worker>/<context>__<namespace>.json
# which the boundary guard (boundaries.toml, tenant-workload-gate) requires
# before this worker may scale, patch, restart, delete or load-test in that
# namespace. Context `default` means kubectl's current context (no --context),
# which is how the guard keys a command that names no --context.
#
# Why (2026-10-27): deploy/api in ephemeral-r40255478404 was scaled to zero
# without reading the namespace's ScaledObjects. A separate ScaledObject,
# mvs-primary-scale-to-zero, deactivates mvs-primary when no app=api pods
# exist, so the shard was killed mid index rebuild.
#
# A failed read, or a namespace with no workloads at all, writes NO receipt and
# exits nonzero. A cluster without the KEDA CRD is read without ScaledObjects,
# and the summary says so.
#
# Env: AMUX_HOME (default ~/.amux); the worker is AMUX_SESSION, else
# AMUX_ISOLATED_WORKER. KUBECTL overrides the kubectl binary (tests).
set -euo pipefail

if [[ $# -lt 3 || -z "${3// /}" ]]; then
  echo "usage: tenant-gate.sh <kube-context|default> <namespace> \"<why>\"" >&2
  exit 64
fi
CTX="$1" NS="$2" REASON="$3"
WORKER="${AMUX_SESSION:-${AMUX_ISOLATED_WORKER:-}}"
if [[ -z "$WORKER" ]]; then
  echo "tenant-gate: no AMUX_SESSION or AMUX_ISOLATED_WORKER; a receipt is per worker, so none was written." >&2
  exit 65
fi
KUBECTL="${KUBECTL:-kubectl}"
HOME_DIR="${AMUX_HOME:-$HOME/.amux}"
ctx_args=()
[[ "$CTX" != "default" ]] && ctx_args=(--context "$CTX")

work="$(mktemp -d "${TMPDIR:-/tmp}/tenant-gate.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# ScaledObjects on their own, so a cluster without KEDA does not fail the read.
keda_state="read"
if ! "$KUBECTL" ${ctx_args[@]+"${ctx_args[@]}"} -n "$NS" get scaledobjects.keda.sh -o json >"$work/so.json" 2>"$work/so.err"; then
  if grep -qiE "doesn't have a resource type|the server could not find the requested resource|no matches for kind" "$work/so.err"; then
    keda_state="no KEDA CRD on this cluster"
    echo '{"items":[]}' >"$work/so.json"
  else
    echo "tenant-gate: reading ScaledObjects in $NS failed; no receipt written:" >&2
    cat "$work/so.err" >&2
    exit 1
  fi
fi
if ! "$KUBECTL" ${ctx_args[@]+"${ctx_args[@]}"} -n "$NS" get hpa,pdb,deploy,statefulset -o json >"$work/rest.json" 2>"$work/rest.err"; then
  echo "tenant-gate: reading $NS failed; no receipt written:" >&2
  cat "$work/rest.err" >&2
  exit 1
fi

python3 -I - "$work/so.json" "$work/rest.json" "$HOME_DIR" "$WORKER" "$CTX" "$NS" "$REASON" "$keda_state" <<'PY'
import hashlib, json, os, re, sys, time

so_path, rest_path, home, worker, ctx, ns, reason, keda_state = sys.argv[1:9]
raw = open(so_path, "rb").read() + b"\n" + open(rest_path, "rb").read()
try:
    so = json.loads(open(so_path).read() or "{}").get("items") or []
    rest = json.loads(open(rest_path).read() or "{}").get("items") or []
except ValueError as e:
    sys.stderr.write(f"tenant-gate: kubectl output did not parse ({e}); no receipt written.\n")
    sys.exit(1)

by_kind = {}
for it in rest:
    by_kind.setdefault(it.get("kind", "?"), []).append(it)
workloads = by_kind.get("Deployment", []) + by_kind.get("StatefulSet", [])
if not workloads and not so and not by_kind.get("HorizontalPodAutoscaler"):
    sys.stderr.write(f"tenant-gate: {ns} has no Deployments, StatefulSets, ScaledObjects or HPAs (empty read, "
                     "wrong namespace or context?); no receipt written.\n")
    sys.exit(2)


def name(it):
    return (it.get("metadata") or {}).get("name", "?")


def pod_labels(w):
    return (((w.get("spec") or {}).get("template") or {}).get("metadata") or {}).get("labels") or {}


def selector_pairs(sel: str) -> list[tuple[str, str, str]]:
    """'app=api,tier!=x' -> [(k, op, v)]; set-based terms are kept as-is."""
    out = []
    for term in [t.strip() for t in (sel or "").split(",") if t.strip()]:
        m = re.match(r"^([^!=\s]+)\s*(==|=|!=)\s*(.*)$", term)
        out.append((m.group(1), m.group(2), m.group(3)) if m else (term, "?", ""))
    return out


def selects(sel: str, labels: dict) -> bool | None:
    pairs = selector_pairs(sel)
    if not pairs or any(op == "?" for _, op, _ in pairs):
        return None  # cannot evaluate here
    for k, op, v in pairs:
        if op in ("=", "==") and labels.get(k) != v:
            return False
        if op == "!=" and labels.get(k) == v:
            return False
    return True


def label_selector(ls: dict) -> str:
    parts = [f"{k}={v}" for k, v in sorted((ls or {}).get("matchLabels", {}).items())]
    for e in (ls or {}).get("matchExpressions", []) or []:
        parts.append(f"{e.get('key')} {e.get('operator')} {','.join(e.get('values') or [])}".strip())
    return ",".join(parts)


lines = []
summary = {"scaledobjects": [], "hpas": [], "pdbs": [], "workloads": [], "watches": [], "keda": keda_state}
lines.append(f"Namespace {ns} (context {ctx}), read {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}")
lines.append(f"Workloads ({len(workloads)}):")
for w in workloads:
    sp, st = w.get("spec") or {}, w.get("status") or {}
    rec = {"kind": w.get("kind"), "name": name(w), "replicas": sp.get("replicas"), "ready": st.get("readyReplicas", 0),
           "pod_labels": pod_labels(w)}
    summary["workloads"].append(rec)
    lab = ",".join(f"{k}={v}" for k, v in sorted(rec["pod_labels"].items()))
    lines.append(f"  {rec['kind']}/{rec['name']}: replicas {rec['replicas']}, ready {rec['ready']}, pods [{lab}]")

lines.append(f"ScaledObjects ({len(so)}; {keda_state}):")
for s in so:
    md, sp = s.get("metadata") or {}, s.get("spec") or {}
    ann = md.get("annotations") or {}
    tgt = sp.get("scaleTargetRef") or {}
    target = f"{tgt.get('kind', 'Deployment')}/{tgt.get('name', '?')}"
    paused = {k: v for k, v in ann.items() if k.startswith("autoscaling.keda.sh/paused")}
    rec = {"name": name(s), "target": target, "min": sp.get("minReplicaCount"), "max": sp.get("maxReplicaCount"),
           "paused": paused, "triggers": []}
    lines.append(f"  {rec['name']} scales {target} (min {rec['min']}, max {rec['max']})"
                 + (f" PAUSED {paused}" if paused else ""))
    for t in sp.get("triggers") or []:
        meta = t.get("metadata") or {}
        trec = {"type": t.get("type"), "metadata": meta}
        rec["triggers"].append(trec)
        lines.append(f"    trigger {t.get('type')}: " + ", ".join(f"{k}={v}" for k, v in sorted(meta.items()))[:300])
        sel = meta.get("podSelector")
        if t.get("type") == "kubernetes-workload" and sel:
            hits = [f"{w['kind']}/{w['name']}" for w in summary["workloads"] if selects(sel, w["pod_labels"])]
            unknown = any(selects(sel, w["pod_labels"]) is None for w in summary["workloads"])
            summary["watches"].append({"scaledobject": rec["name"], "target": target, "pod_selector": sel, "watched": hits})
            who = ", ".join(hits) if hits else ("(selector not evaluated here)" if unknown else "(no workload here matches)")
            lines.append(f"    WATCHES pods {sel}: {who}. Scaling those changes {target}.")
    summary["scaledobjects"].append(rec)

hpas = by_kind.get("HorizontalPodAutoscaler", [])
lines.append(f"HPAs ({len(hpas)}):")
for h in hpas:
    sp = h.get("spec") or {}
    tgt = sp.get("scaleTargetRef") or {}
    rec = {"name": name(h), "target": f"{tgt.get('kind')}/{tgt.get('name')}", "min": sp.get("minReplicas"), "max": sp.get("maxReplicas"),
           "owner": [o.get("kind") + "/" + o.get("name") for o in (h.get("metadata") or {}).get("ownerReferences") or []]}
    summary["hpas"].append(rec)
    lines.append(f"  {rec['name']} scales {rec['target']} ({rec['min']}..{rec['max']})"
                 + (f", owned by {', '.join(rec['owner'])}" if rec["owner"] else ""))

pdbs = by_kind.get("PodDisruptionBudget", [])
lines.append(f"PDBs ({len(pdbs)}):")
for p in pdbs:
    sp = p.get("spec") or {}
    sel = label_selector(sp.get("selector") or {})
    rec = {"name": name(p), "selector": sel, "minAvailable": sp.get("minAvailable"), "maxUnavailable": sp.get("maxUnavailable"),
           "covers": [f"{w['kind']}/{w['name']}" for w in summary["workloads"] if selects(sel, w["pod_labels"])]}
    summary["pdbs"].append(rec)
    lines.append(f"  {rec['name']} [{sel}] minAvailable {rec['minAvailable']} maxUnavailable {rec['maxUnavailable']}: covers {', '.join(rec['covers']) or '-'}")

if summary["watches"]:
    lines.append("DEPENDENCIES: a change to a watched workload above moves the ScaledObject's target too.")
print("\n".join(lines))

key = lambda v: re.sub(r"[^A-Za-z0-9._-]", "_", v) or "_"  # noqa: E731 - same as boundary-guard receipt_key
d = os.path.join(home, "tenant-gates", key(worker))
os.makedirs(d, exist_ok=True)
path = os.path.join(d, f"{key(ctx)}__{key(ns)}.json")
receipt = {"ts": round(time.time(), 3), "session": worker, "context": ctx, "namespace": ns, "reason": reason,
           "sha256": hashlib.sha256(raw).hexdigest(), "summary": summary, "text": "\n".join(lines)}
tmp = f"{path}.{os.getpid()}"
with open(tmp, "w") as fh:
    json.dump(receipt, fh, indent=1)
os.replace(tmp, path)
print(f"Receipt: {path} (sha256 {receipt['sha256'][:12]}, valid for this worker for the boundary's window)")
PY
