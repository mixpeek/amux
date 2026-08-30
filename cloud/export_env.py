#!/usr/bin/env python3
"""Export a LIVE cloud env to an EnvSpec YAML (amux env_config / AMUX-2977 shape).

The other half of Ethan's "save the env as YAML, rapidly redeploy for similar
verticals": seed.py + /api/env/apply WRITE an env; this READS a running one back
into the SAME schema, so you can capture a good env — produced docs and all — as
a reusable vertical template instead of hand-authoring it.

Output shape is identical field-for-field to what /api/env/apply consumes
(EnvSpec: groups[], workers[], schedules[], columns[], files[]), so an exported
YAML round-trips: export org A -> edit the org specifics -> apply to org B.

Usage:
    COOKIE_SECRET=... ADMIN_USER_ID=... \
      python3 cloud/export_env.py --org org_8e89a846b6f5be7d > cloud/verticals/foo.yaml
    # add --files-dir /root/rothco/docs to capture the seeded docs' content

Env: same as seed.py (COOKIE_SECRET, ADMIN_USER_ID, E2E_GATEWAY). Reuses seed.py's
gateway client so there is ONE authenticated path, not a second one.
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import seed  # noqa: E402 — reuse gw()/gw_json()/_cookie() rather than re-implement auth

try:
    import yaml
except ImportError:
    sys.exit("pyyaml required: pip3 install pyyaml")


def plan_to_envspec(plan):
    """Convert a seed-plan (org/sessions/docs/board/board_columns) to an EnvSpec
    (groups/workers/files/schedules/columns) — the convergence toward ONE format.

    Returns (envspec, retained) where `retained` is the seed-plan content EnvSpec
    has NO stanza for and seed.py must still apply itself: `org` (provisioning),
    `board` (initial issue cards), and each worker's opening `prompt`. Returning
    them explicitly is the point — a converter that silently dropped the board
    cards or the first-run prompts would look lossless and quietly gut the demo.
    """
    if "workers" in plan and "sessions" not in plan:
        # already an EnvSpec — pass through, nothing retained
        return plan, {}
    sessions = plan.get("sessions", [])
    groups = {}
    workers = []
    schedules = []
    for s in sessions:
        tags = s.get("tags") or []
        if isinstance(tags, str):
            tags = [t.strip() for t in tags.split(",") if t.strip()]
        workers.append({
            "name": s["name"],
            "dir": s.get("dir", ""),
            "groups": list(tags),
            "desc": s.get("desc", ""),
            "model": s.get("model", "sonnet"),
            "provider": s.get("provider", "claude"),
            # AMUX-2977: prompt is a bare string on the worker, steered once on
            # create. Empty when the plan has none.
            "prompt": s.get("prompt", ""),
        })
        for t in tags:
            groups.setdefault(t, {"name": t, "department": "", "goal": ""})
        for sc in s.get("schedules", []):
            schedules.append({
                "worker": s["name"], "title": sc.get("title", ""),
                "expr": sc.get("expr", ""), "enabled": bool(sc.get("enabled", False)),
                "command": sc.get("command", ""),
            })

    # cards[] (AMUX-2977). amux's cards require `worker` (the owning session), but a
    # seed-plan card is COLUMN-attributed (column: Documents/Engagements/...), not
    # worker-attributed. So infer worker: an explicit session/worker on the card
    # wins; else the worker whose group/name/desc best matches the column; else the
    # first worker (surfaced, never silent). On EXPORT from a live env this inference
    # is unused — live cards carry `session` directly.
    def _worker_for(card):
        w = card.get("worker") or card.get("session")
        if w:
            return w
        col = str(card.get("column", "")).lower()
        for wk in workers:
            hay = (wk["name"] + " " + wk["desc"] + " " + " ".join(wk["groups"])).lower()
            if col and any(tok in hay for tok in col.split() if len(tok) > 3):
                return wk["name"]
        # No match — fall back to the first worker but WARN, so a bad mapping is
        # visible (an unowned card is inert; a mis-owned one is worse silent).
        fb = workers[0]["name"] if workers else ""
        sys.stderr.write(f"# WARN: card '{card.get('title','')[:40]}' (column '{card.get('column','')}') "
                         f"could not match a worker — assigned to '{fb}'. Set the card's worker/column.\n")
        return fb
    cards = [{
        "worker": _worker_for(c),
        "title": c.get("title", ""),
        "desc": c.get("desc", ""),
        "status": c.get("status", "backlog"),
        "type": c.get("type", "code"),
        "epic": c.get("epic", ""),
    } for c in plan.get("board", [])]

    envspec = {
        "groups": list(groups.values()),
        "workers": workers,
        "files": [{"path": d["path"], "content": d.get("content", "")} for d in plan.get("docs", [])],
        "schedules": schedules,
        "columns": list(plan.get("board_columns", [])),
        "cards": cards,
    }
    # EnvSpec now covers the full env; only org provisioning is seed.py's.
    retained = {"org": plan.get("org")}
    return envspec, retained


def _list(v, *keys):
    """A gateway list response may be a bare array or {key: [...]}. Normalize."""
    if isinstance(v, list):
        return v
    if isinstance(v, dict):
        for k in keys:
            if isinstance(v.get(k), list):
                return v[k]
    return []


def export(org, files_dir=None):
    # ---- workers (WorkerSpec: name, dir, groups[<-tags>, desc, model, provider]) ----
    _, sess_raw = seed.gw_json("GET", "/api/sessions", org=org)
    sessions = _list(sess_raw, "sessions")
    workers = []
    all_groups = {}
    for s in sessions:
        name = s.get("name")
        if not name or name == "hello-world":  # scaffold is never part of a template
            continue
        tags = s.get("tags") or s.get("tag") or []
        if isinstance(tags, str):
            tags = [t.strip() for t in tags.split(",") if t.strip()]
        # The CONFIGURED model, not the runtime one. `active_model` is what the
        # last turn ran on and can be a status marker like `<synthetic>` (a capped
        # worker that never really ran) — writing that into a template would
        # produce an un-applyable worker. Prefer the configured `model`, and any
        # non-family value (`<synthetic>`, empty) falls back to the sonnet default.
        model = s.get("model") or s.get("active_model") or ""
        model = str(model)
        if "sonnet" in model:
            model = "sonnet"
        elif "haiku" in model:
            model = "haiku"
        elif "opus" in model:
            model = "opus"
        else:
            model = "sonnet"  # <synthetic>, empty, or unknown -> the demo default
        workers.append({
            "name": name,
            "dir": s.get("dir") or s.get("cwd") or "",
            "groups": list(tags),
            "desc": s.get("desc") or "",
            "model": model,
            "provider": s.get("provider") or "claude",
            # A live env does not retain the first-run task, so export empty —
            # authored-only (the applier steers it once on create; empty = no steer).
            "prompt": "",
        })
        for t in tags:
            all_groups.setdefault(t, {"name": t, "department": "", "goal": ""})

    # Enrich groups with department/goal from the group config, so a round-trip is
    # LOSSLESS (amux's catch: emitting bare names drops department+goal, and the
    # vertical's org description degrades on every export->apply cycle). GET
    # /api/groups returns configured groups with department/goal; merge them over
    # the tag-derived names. Groups a worker references but that have no config
    # still export as {name, "", ""} so the membership survives.
    _, groups_raw = seed.gw_json("GET", "/api/groups", org=org)
    for g in _list(groups_raw, "groups"):
        name = g.get("name")
        if not name:
            continue
        entry = all_groups.setdefault(name, {"name": name, "department": "", "goal": ""})
        if g.get("department"):
            entry["department"] = g["department"]
        if g.get("goal"):
            entry["goal"] = g["goal"]

    # ---- schedules (worker, title, expr, enabled, command) ----
    _, sched_raw = seed.gw_json("GET", "/api/schedules", org=org)
    schedules = []
    for sc in _list(sched_raw, "schedules"):
        schedules.append({
            "worker": sc.get("session") or sc.get("worker") or "",
            "title": sc.get("title") or "",
            "expr": sc.get("schedule_expr") or sc.get("expr") or "",
            "enabled": bool(sc.get("enabled", 0)),
            "command": sc.get("command") or "",
        })

    # ---- board columns + cards ----
    # ?full=1 is LOAD-BEARING (AMUX-3496 seam, found 2026-08-23): the default
    # board list went slim, so `desc` stopped shipping — and the cards below
    # read it. Without this every exported card carried desc:"" and seed.py
    # (:562) writes that straight back on apply, so an export/re-seed cycle
    # silently emptied every description. Producer correct, consumer correct,
    # only the seam wrong.
    _, board_raw = seed.gw_json("GET", "/api/board?full=1", org=org)
    cols = []
    seen = set()
    cards = []
    for it in _list(board_raw, "items"):
        if it.get("archived") or it.get("status") == "discarded":
            continue
        c = it.get("column") or it.get("col")
        if c and c not in seen:
            seen.add(c)
            cols.append(c)
        # cards[] (AMUX-2977): live cards carry `session` = worker directly.
        if "desc" not in it:
            # An export that quietly drops prose is worse than one that stops:
            # the EnvSpec looks complete and apply overwrites live cards with
            # empty descriptions.
            raise SystemExit(
                "REFUSING to export: /api/board?full=1 returned cards with no `desc` "
                f"field (card {it.get('id')!r}). The list payload shape changed; "
                "exporting now would write empty descriptions back on apply."
            )
        cards.append({
            "worker": it.get("session") or "",
            "title": it.get("title", ""),
            "desc": it.get("desc", ""),
            "status": it.get("status", "backlog"),
            "type": it.get("type", "code"),
            "epic": it.get("epic", ""),
        })

    # ---- files (docs): {path, content}. Read content off the container path. ----
    files = []
    if files_dir:
        _, ls = seed.gw_json("GET", f"/api/files?path={files_dir}", org=org)
        for entry in _list(ls, "files", "entries"):
            p = entry.get("path") or (files_dir.rstrip("/") + "/" + entry.get("name", ""))
            if entry.get("type") == "dir" or entry.get("is_dir"):
                continue
            # /api/file returns {"content": "<raw>", ...}, NOT the bare bytes.
            # Storing the whole JSON envelope would write literal `{"content":...}`
            # into the redeployed doc — extract the field.
            code, resp = seed.gw_json("GET", f"/api/file?path={p}", org=org)
            if code == 200 and isinstance(resp, dict) and "content" in resp:
                files.append({"path": p, "content": resp["content"]})
            elif code == 200 and isinstance(resp, str):
                files.append({"path": p, "content": resp})

    spec = {
        "_comment": f"Exported from live env {org} by export_env.py — EnvSpec (AMUX-2977). "
                    f"Edit org specifics and POST to /api/env/apply to redeploy for a similar vertical.",
        "groups": list(all_groups.values()),
        "workers": workers,
        "columns": cols,
        "schedules": schedules,
        "files": files,
        "cards": cards,
    }
    return spec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--org", required=True, help="org id to export (amux-user-<org>)")
    ap.add_argument("--files-dir", help="container dir to capture as files[] (e.g. /root/rothco/docs)")
    a = ap.parse_args()
    if not seed.COOKIE_SECRET:
        sys.exit("COOKIE_SECRET is required (same as seed.py)")
    spec = export(a.org, a.files_dir)
    sys.stdout.write(yaml.safe_dump(spec, sort_keys=False, width=100, allow_unicode=True))
    sys.stderr.write(
        f"# exported {len(spec['workers'])} workers, {len(spec['groups'])} groups, "
        f"{len(spec['columns'])} columns, {len(spec['schedules'])} schedules, "
        f"{len(spec['files'])} files\n")


if __name__ == "__main__":
    main()
