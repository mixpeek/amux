#!/usr/bin/env python3
"""PreToolUse guard: enforce the amux worker boundaries in boundaries.toml.

One hook, one registry. The rules, their wording and their examples live in
boundaries.toml (repo root; installed beside this file as
~/.amux/hooks/boundaries.toml), with optional owner overrides in
~/.amux/boundaries.toml. This script holds no rule of its own: it loads the
registry, refuses the tool calls a `deny` rule describes, refuses heavy
commands while the machine is critical (the `resources` boundary), and refuses
a new Agent/Task call past the running-subagent cap or under the account's
usage headroom floor (the `subagents` boundary), and refuses modal prompt tools
(AskUserQuestion, plan mode) for orchestrator lanes (the `modal` boundary),
and refuses hook-less pushes to a protected branch (the `push-gates` boundary),
and refuses a cluster or cloud delete that this worker has not previewed (the
`delete-gate` boundary).

Who it applies to: amux workers (AMUX_SESSION set), and isolated ones
(AMUX_ISOLATED_WORKER set) while the registry's top-level `apply_to_isolated`
is true (the default; it is the named exception in CLAUDE.md "Isolated
workers").

Every decision other than a plain allow is appended to
~/.amux/logs/boundaries.jsonl (tokens redacted), which is what
/api/boundaries counts.

Fail-open: a guard must never wedge a tool call. An unreadable payload or
registry lets the call through, and a registry that does not load is logged
as verdict=boundary_registry_unloadable so the sweep sees every boundary
went dark. The one exception is a deny rule with `requires_receipt`
(tenant-workload-gate): once its patterns match a command segment, a missing,
stale or foreign receipt refuses the call, and so does a failed check. The
other is the push-gates boundary (kind push_gates): a `git push` segment that
writes main or master is refused when it skips the hooks or runs in a repo
that ships a pre-push hook it has not installed, when the hook state
cannot be read, and in a repo whose `repos` entry sets `land = true` (it lands
main through `amux land`, whose own push runs in its subprocess, unseen here).
The delete gate (kind delete_gate) also fails closed: a delete with no fresh,
unused preview receipt, or a receipt that cannot be read, consumed or written,
is refused.
"""
from __future__ import annotations

import json
import os
import re
import sys
import time

# subprocess, ssl and urllib are imported where used: they cost ~45ms at
# startup and only the resources check needs them, while this hook runs before
# every Bash, Read and Edit call in the fleet.

try:
    import tomllib
except ModuleNotFoundError:  # python < 3.11
    tomllib = None

# Grep and Glob read the filesystem too: a Grep into the vault prints card
# data (found by the MSG-90256 adversarial pass, 2026-10-17).
FILE_TOOLS = {"Read", "Edit", "Write", "MultiEdit", "NotebookEdit", "Grep", "Glob"}
HERE = os.path.dirname(os.path.abspath(__file__))


def amux_home() -> str:
    return os.environ.get("AMUX_HOME") or os.path.join(os.path.expanduser("~"), ".amux")


def registry_paths() -> list[str]:
    explicit = os.environ.get("AMUX_BOUNDARIES_FILE")
    if explicit:
        return [explicit]
    paths = [os.path.join(HERE, "boundaries.toml")]
    repo_copy = os.path.join(os.path.dirname(os.path.dirname(HERE)), "boundaries.toml")
    if not os.path.exists(paths[0]) and os.path.exists(repo_copy):
        paths = [repo_copy]
    override = os.path.join(amux_home(), "boundaries.toml")
    if os.path.exists(override):
        paths.append(override)
    return paths


def merge(base: list[dict], over: list[dict]) -> list[dict]:
    """Owner overrides replace fields of the boundary with the same id."""
    by_id = {b.get("id"): dict(b) for b in base}
    order = [b.get("id") for b in base]
    for b in over:
        bid = b.get("id")
        if bid in by_id:
            by_id[bid].update(b)
        else:
            by_id[bid] = dict(b)
            order.append(bid)
    return [by_id[i] for i in order]


def load_registry() -> tuple[list[dict], dict]:
    """Enabled boundaries, and the top-level settings (later layers win)."""
    if tomllib is None:
        raise RuntimeError("python has no tomllib (needs 3.11+)")
    boundaries: list[dict] = []
    settings: dict = {}
    for i, p in enumerate(registry_paths()):
        with open(p, "rb") as fh:
            data = tomllib.load(fh)
        layer = data.get("boundary") or []
        boundaries = layer if i == 0 else merge(boundaries, layer)
        settings.update({k: v for k, v in data.items() if k != "boundary"})
    return [b for b in boundaries if b.get("enabled", True)], settings


SECRET_PATTERNS = [
    (re.compile(r"(Bearer|token|Basic)\s+[A-Za-z0-9._~+/=-]{8,}", re.I), r"\1 ***"),
    (re.compile(r"\b(sk|rk|pk)_(live|test)_[A-Za-z0-9]+"), "***"),
    (re.compile(r"(-u|--user)\s+\S+"), r"\1 ***"),
    (re.compile(r"(password|passwd|secret|api[_-]?key)=\S+", re.I), r"\1=***"),
]


def redact(text: str) -> str:
    for pat, rep in SECRET_PATTERNS:
        text = pat.sub(rep, text)
    return text[:200]


# The worker this call belongs to, resolved once in main(): AMUX_SESSION, or
# AMUX_ISOLATED_WORKER for an isolated worker (whose launch unsets the former).
WORKER = ""
WORKER_ISOLATED = False


def log(event: dict) -> None:
    try:
        d = os.path.join(amux_home(), "logs")
        os.makedirs(d, exist_ok=True)
        event = {"ts": round(time.time(), 3), "session": WORKER or os.environ.get("AMUX_SESSION", ""), **({"isolated": True} if WORKER_ISOLATED else {}), **event}
        with open(os.path.join(d, "boundaries.jsonl"), "a") as fh:
            fh.write(json.dumps(event) + "\n")
    except Exception:
        pass


def is_isolated(session: str) -> bool:
    try:
        with open(os.path.join(amux_home(), "sessions", session + ".env")) as fh:
            for line in fh:
                k, _, v = line.strip().partition("=")
                if k.strip() == "CC_ISOLATED":
                    return v.strip().strip("'\"").lower() in ("1", "true", "yes", "on")
    except OSError:
        pass
    return False


def search(pattern: str, text: str) -> bool:
    return re.search(pattern, text, re.IGNORECASE | re.MULTILINE) is not None


def rule_applies(rule: dict, tool: str, command: str, path: str, content: str) -> bool:
    if tool not in (rule.get("tools") or ["Bash"]):
        return False
    if tool == "Bash":
        alls = rule.get("match_all") or []
        if rule.get("requires_receipt"):
            # A cheap prefilter only: quotes read as spaces so a command inside
            # bash -c '...' still passes it. match_none and the receipt are
            # decided per segment in check_receipts.
            loose = re.sub(r"[\"'`()]", " ", command)
            return bool(alls) and all(search(p, loose) for p in alls)
        if not alls or not all(search(p, command) for p in alls):
            return False
        return not any(search(p, command) for p in rule.get("match_none") or [])
    if tool in FILE_TOOLS:
        pat = rule.get("path")
        if not pat or not path or not search(pat, path):
            return False
        anys = rule.get("content_any") or []
        return not anys or any(search(p, content) for p in anys)
    return False


# ---- receipts (tenant-workload-gate, 2026-10-27) --------------------------------
#
# A deny rule with `requires_receipt` refuses only the command segments whose
# target has no fresh receipt. On 2026-10-27 a worker scaled
# ephemeral-r40255478404's deploy/api to zero without reading the namespace's
# ScaledObjects, and a second ScaledObject that watches app=api pods
# deactivated the mvs-primary shard mid index rebuild. The receipt is what the
# read-only gate (scripts/tenant-gate.sh) writes after it reads what scales
# what. Everything that decides scope (which segments, which namespaces, how
# the namespace and context are found) is in the registry; this code only
# splits the command, applies those patterns, and checks the file. Fail
# CLOSED: a matching segment with no valid receipt is refused.

SHELL_WRAPPERS = {"bash", "sh", "zsh", "eval", "ssh", "su", "watch"}
OPERATORS = {";", "&&", "||", "|", "&", "\n", ";;", "|&"}


def receipt_key(value: str) -> str:
    """The receipt file name part for a context or namespace. tenant-gate.sh
    applies the same substitution, so the two always agree."""
    return re.sub(r"[^A-Za-z0-9._-]", "_", value) or "_"


def command_token_segments(command: str, depth: int = 0, via: str = "") -> list[tuple[list[str], str]]:
    """Simple commands as token lists, split on shell operators and newlines,
    with the string given to bash -c / sh -c / eval / ssh split again (those
    follow their parent, tagged with the wrapper's name in `via`)."""
    import shlex

    try:
        lex = shlex.shlex(command, posix=True, punctuation_chars=";&|\n")
        lex.whitespace = " \t\r"
        lex.whitespace_split = True
        tokens = list(lex)
    except ValueError:  # unbalanced quotes (a heredoc body, say): split crudely
        tokens = []
        for part in re.split(r"(;|&&|\|\||\||\n)", command):
            tokens.extend(["\n"] if part in (";", "&&", "||", "|", "\n") else part.split())
    segs: list[list[str]] = [[]]
    for t in tokens:
        if t in OPERATORS:
            segs.append([])
        else:
            segs[-1].append(t)
    out: list[tuple[list[str], str]] = []
    for seg in segs:
        if not seg:
            continue
        out.append((seg, via))
        if depth < 2:
            words = [w for w in seg if not re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", w)]
            if words and os.path.basename(words[0]) in SHELL_WRAPPERS:
                wrapper = os.path.basename(words[0])
                for w in words[1:]:
                    if re.search(r"\s", w):
                        out.extend(command_token_segments(w, depth + 1, via if via == "ssh" else wrapper))
    return out


def command_segments(command: str, depth: int = 0) -> list[str]:
    """Simple commands, split on shell operators and newlines, with the string
    given to bash -c / sh -c / eval / ssh split again. A quoted argument with
    spaces in it (a commit message, a grep pattern) is not read as a command."""
    # A quoted argument with spaces in it (a commit message, a JSON patch)
    # is data to this command, so it reads as one placeholder word.
    return [" ".join("_" if re.search(r"\s", t) else t for t in seg) for seg, _ in command_token_segments(command, depth)]


def first_group(patterns: list[str], text: str) -> str | None:
    for p in patterns or []:
        m = re.search(p, text, re.IGNORECASE)
        if m:
            return m.group(1) if m.groups() else m.group(0)
    return None


def segment_matches(rule: dict, seg: str) -> bool:
    alls = rule.get("match_all") or []
    return bool(alls) and all(search(p, seg) for p in alls) and not any(search(p, seg) for p in rule.get("match_none") or [])


def read_receipt(path: str, worker: str, context: str, namespace: str, max_age_s: float) -> tuple[dict | None, str]:
    """(receipt, "") when valid, else (None, cause)."""
    try:
        with open(path) as fh:
            r = json.load(fh)
    except FileNotFoundError:
        return None, "no_receipt"
    except (OSError, ValueError):
        return None, "receipt_unreadable"
    if not isinstance(r, dict):
        return None, "receipt_unreadable"
    if r.get("session") != worker:
        return None, "receipt_other_session"
    if r.get("context") != context or r.get("namespace") != namespace:
        return None, "receipt_other_target"
    try:
        age = time.time() - float(r.get("ts"))
    except (TypeError, ValueError):
        return None, "receipt_unreadable"
    if age > max_age_s or age < -60:
        return None, "receipt_expired"
    return dict(r, age_s=round(age)), ""


def check_receipts(b: dict, i: int, rule: dict, command: str) -> tuple[int, str]:
    rr = rule.get("requires_receipt") or {}
    in_scope = rr.get("namespaces") or []
    targets: list[dict] = []
    for seg in command_segments(command):
        if not segment_matches(rule, seg):
            continue
        if first_group(rr.get("all_namespaces_patterns") or [], seg) is not None:
            targets.append({"segment": seg, "namespace": "*", "context": first_group(rr.get("context_patterns"), seg) or rr.get("default_context", "default")})
            continue
        ns = first_group(rr.get("namespace_patterns"), seg)
        if not ns or not any(re.search(p, ns, re.IGNORECASE) for p in in_scope):
            continue  # no namespace named, or not a tenant namespace: out of scope
        targets.append({"segment": seg, "namespace": ns,
                        "context": first_group(rr.get("context_patterns"), seg) or rr.get("default_context", "default")})
    if not targets:
        return 0, ""
    env_key = rr.get("max_age_env") or ""
    raw = (scoped_env_value(env_key) if env_key else None) or (os.environ.get(env_key) if env_key else None)
    try:
        max_age = float(raw) if raw not in (None, "") else float(rr.get("max_age_s", 1800))
    except ValueError:
        max_age = float(rr.get("max_age_s", 1800))
    rdir = os.path.join(amux_home(), rr.get("dir") or "receipts", receipt_key(WORKER))
    refused: list[dict] = []
    allowed: list[dict] = []
    for t in targets:
        if t["namespace"] == "*":
            refused.append({**t, "cause": "all_namespaces"})
            continue
        path = os.path.join(rdir, f"{receipt_key(t['context'])}__{receipt_key(t['namespace'])}.json")
        rec, cause = read_receipt(path, WORKER, t["context"], t["namespace"], max_age)
        if rec is None:
            refused.append({**t, "cause": cause, "receipt": path})
        else:
            allowed.append({**t, "receipt": path, "receipt_age_s": rec.get("age_s"), "receipt_sha256": rec.get("sha256")})
    base = {"boundary": b.get("id"), "rule": i, "tool": "Bash", "subject": redact(command), "max_age_s": max_age,
            "measured": True, "n_considered": len(targets)}
    strip = lambda ts: [{k: (redact(v) if k == "segment" else v) for k, v in t.items()} for t in ts]  # noqa: E731
    if not refused:
        log({**base, "level": "INFO", "verdict": rr.get("verdict_allowed") or "receipt_allowed", "targets": strip(allowed)})
        return 0, ""
    log({**base, "level": "WARN", "verdict": rr.get("verdict_refused") or "receipt_refused", "targets": strip(refused),
         "with_receipt": strip(allowed)})
    gate = rr.get("gate_command") or ""
    lines = []
    for t in refused:
        what = {"no_receipt": "this session has no gate receipt for it",
                "receipt_expired": f"this session's gate receipt is older than {max_age / 60:.0f} min",
                "receipt_other_session": "the receipt there was written for another worker",
                "receipt_other_target": "the receipt there names another context or namespace",
                "receipt_unreadable": "the receipt there does not parse",
                "all_namespaces": "it spans every namespace, so no one receipt can cover it; name one namespace with -n"}.get(t["cause"], t["cause"])
        lines.append(f"- `{t['segment'][:140]}`: namespace {t['namespace']}, context {t['context']}: {what}.")
        if t["namespace"] != "*" and gate:
            lines.append("  Run, as its own command:\n    " + gate.format(context=t["context"], namespace=t["namespace"]))
            try:
                others = sorted(n for n in os.listdir(rdir) if n.endswith(f"__{receipt_key(t['namespace'])}.json")
                                and n != f"{receipt_key(t['context'])}__{receipt_key(t['namespace'])}.json")
            except OSError:
                others = []
            if others:
                lines.append(f"  (you hold receipts for this namespace under other contexts: {', '.join(others)}; "
                             "name that --context in your command if it is the cluster you mean)")
    return 2, (
        f"BLOCKED by amux boundary '{b.get('id')}' ({b.get('title', '')}).\n"
        f"{rule.get('reason', '').strip()}\n" + "\n".join(lines) + "\n"
        "Read what the gate prints (what scales what, and which ScaledObjects watch this workload's pods), "
        "then retry.\n\n" + b.get("rule", "").strip() + "\n"
        "(boundary-guard; the registry is boundaries.toml, GET $AMUX_URL/api/boundaries)\n"
    )


# ---- push gates (push-gates, 2026-10-27) ----------------------------------------
#
# A worker's subagent pushed to the mixpeek repo's main from a fresh clone that
# had no pre-push hook installed (1212229610, since reverted). Its precheck had
# refused, but the push was chained after it, and the clone ran no hook, so
# mixpeek's .githooks/pre-push gates never ran. This check refuses a `git push`
# segment that targets a protected branch when it skips the hooks
# (--no-verify, or core.hooksPath emptied inline) or when the repository it
# runs in SHIPS a pre-push hook that is not installed. Each command segment is
# judged on its own, in the directory it will run in (the payload's cwd,
# moved by `cd <dir>` segments and `git -C <dir>`). It runs git only for a
# segment that is a push, and fails CLOSED when it cannot tell the hook state.

PUSH_LAUNCHERS = {"env", "command", "exec", "nohup", "time", "nice", "builtin"}
# git global options that take the next word as their value.
GIT_GLOBAL_ARG = {"-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path", "--config-env",
                  "--super-prefix", "--list-cmds", "--attr-source"}
# push options that take the next word as their value.
PUSH_OPT_ARG = {"-o", "--push-option", "--repo", "--receive-pack", "--exec"}


class PushParse:
    def __init__(self) -> None:
        self.dirs: list[str] = []         # -C values, in order
        self.globals: list[str] = []      # global options to replay on the probe
        self.env: dict[str, str] = {}     # GIT_* assignments before the command
        self.inline_hooks: list[str] = []  # core.hooksPath values set inline
        self.no_verify = False
        self.dry_run = False
        self.all_refs = ""                # --all / --mirror / --branches
        self.positional: list[str] = []


def parse_git_push(tokens: list[str]) -> PushParse | None:
    """A PushParse when this simple command is `git ... push ...`, else None."""
    toks = list(tokens)
    if toks:
        toks[0] = toks[0].lstrip("({")
    while toks and toks[-1] in (")", "}"):
        toks.pop()
    toks = [t for t in toks if t]
    pp = PushParse()
    i = 0
    while i < len(toks):
        t = toks[i]
        m = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)=(.*)$", t, re.S)
        if m:
            pp.env[m.group(1)] = m.group(2)
            i += 1
            continue
        if os.path.basename(t) in PUSH_LAUNCHERS or (t.startswith("-") and i and os.path.basename(toks[i - 1]) == "env"):
            i += 1
            continue
        break
    if i >= len(toks) or os.path.basename(toks[i]) != "git":
        return None
    i += 1
    while i < len(toks) and toks[i].startswith("-"):
        t = toks[i]
        key = t.split("=", 1)[0]
        if key in GIT_GLOBAL_ARG and "=" not in t and not (key == "-c" and len(t) > 2):
            val = toks[i + 1] if i + 1 < len(toks) else ""
            i += 2
        elif t.startswith("-c") and len(t) > 2 and not t.startswith("--"):
            key, val = "-c", t[2:]
            i += 1
        else:
            val = t.split("=", 1)[1] if "=" in t else ""
            i += 1
        if key == "-C":
            pp.dirs.append(val)
            continue
        if key == "-c" and re.match(r"^core\.hookspath(=|$)", val, re.I):
            pp.inline_hooks.append(val.split("=", 1)[1] if "=" in val else "")
        if key in GIT_GLOBAL_ARG:
            pp.globals += [key, val] if key not in ("--git-dir", "--work-tree", "--namespace") else [f"{key}={val}"]
        else:
            pp.globals.append(t)
    if i >= len(toks) or toks[i] != "push":
        return None
    i += 1
    for k, v in pp.env.items():  # GIT_CONFIG_KEY_<n>=core.hooksPath GIT_CONFIG_VALUE_<n>=...
        m = re.match(r"^GIT_CONFIG_KEY_(\d+)$", k)
        if m and v.lower() == "core.hookspath":
            pp.inline_hooks.append(pp.env.get(f"GIT_CONFIG_VALUE_{m.group(1)}", ""))
    while i < len(toks):
        t = toks[i]
        i += 1
        if t == "--":
            pp.positional += toks[i:]
            break
        if t.startswith("--"):
            key = t.split("=", 1)[0]
            if key == "--no-verify":
                pp.no_verify = True
            elif key == "--verify":
                pp.no_verify = False
            elif key == "--dry-run":
                pp.dry_run = True
            elif key in ("--all", "--mirror", "--branches"):
                pp.all_refs = key
            elif key in PUSH_OPT_ARG and "=" not in t:
                i += 1
            continue
        if t.startswith("-") and len(t) > 1:
            if t in PUSH_OPT_ARG:
                i += 1
            elif "n" in t[1:] and t[1] != "o":
                pp.dry_run = True  # push's -n is --dry-run (it has no short --no-verify)
            continue
        pp.positional.append(t)
    return pp


def _resolve_dir(base: str | None, d: str) -> str | None:
    if base is None:
        return None
    d = os.path.expanduser(d)
    d = re.sub(r"^\$(HOME\b|\{HOME\})", os.path.expanduser("~"), d)
    if "$" in d or "`" in d:
        return None
    return os.path.normpath(os.path.join(base, d))


def refspec_dst(spec: str, current: str | None) -> str | None:
    """The remote branch a refspec writes, as a short name, or None."""
    spec = spec.lstrip("+")
    if spec.startswith("tag "):
        return None
    src, colon, dst = spec.partition(":")
    name = dst if colon else src
    if not colon and name in ("HEAD", "@"):
        name = current or ""
    name = re.sub(r"^refs/heads/", "", name)
    return name or None


def _git(args: list[str], cwd: str, env: dict, timeout: float) -> tuple[int, str]:
    import subprocess

    full_env = {**os.environ, **env, "GIT_TERMINAL_PROMPT": "0"}
    p = subprocess.run(["git", *args], cwd=cwd, env=full_env, capture_output=True, text=True, timeout=timeout)
    return p.returncode, p.stdout


def check_push_gates(b: dict, command: str, cwd: str) -> tuple[int, str]:
    cfg = b.get("push_gates") or {}
    if not re.search(r"\bgit\b", command) or not re.search(r"\bpush\b", command):
        return 0, ""  # the cheap prefilter: no git, no subprocess
    protected = {x.lower() for x in (cfg.get("protected_branches") or ["main", "master"])}
    timeout = float(cfg.get("timeout_s") or 3)
    cur_dir: str | None = cwd or os.getcwd()
    results: list[dict] = []
    for toks, via in command_token_segments(command):
        words = [t for t in toks if not re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", t)]
        if words and words[0].lstrip("({") in ("cd", "pushd"):
            target = words[1] if len(words) > 1 else "~"
            cur_dir = None if target == "-" else _resolve_dir(cur_dir, target)
            continue
        pp = parse_git_push(toks)
        if pp is None or pp.dry_run:
            continue  # a dry run writes nothing to the remote
        seg = " ".join(toks)
        rec: dict = {"segment": redact(seg)}
        results.append(rec)
        repo_dir = cur_dir if via != "ssh" else None
        for d in pp.dirs:
            repo_dir = _resolve_dir(repo_dir, d)
        explicit = pp.positional[1:]
        # Decided from the text alone: explicit refspecs that write no protected branch.
        if not pp.all_refs and explicit:
            dsts = [refspec_dst(s, None) for s in explicit]
            if all(x is not None and "*" not in x and x.lower() not in protected and x not in ("HEAD", "@") for x in dsts):
                rec["skip"] = "not_protected"
                continue
        if repo_dir is None:
            rec.update(cause="repo_unknown", why="cannot tell which repository this push runs in"
                       + (" (it runs on another host over ssh)" if via == "ssh" else ""))
            continue
        env = {k: v for k, v in pp.env.items() if k.startswith("GIT_")}
        try:
            code, out = _git([*pp.globals, "rev-parse", "--git-path", "hooks", "--git-common-dir"], repo_dir, env, timeout)
            if code != 0:
                raise RuntimeError(f"git rev-parse exited {code} in {repo_dir}")
            hooks_rel, _common = (out.splitlines() + ["", ""])[:2]
            hooks_dir = os.path.normpath(os.path.join(repo_dir, hooks_rel))
            code, top = _git([*pp.globals, "rev-parse", "--show-toplevel"], repo_dir, env, timeout)
            toplevel = top.strip() if code == 0 else ""
            _code, conf = _git([*pp.globals, "config", "--get-regexp",
                                r"^(remote\..*\.(url|push)|branch\..*\.merge|push\.default)$"], repo_dir, env, timeout)
            config: dict[str, list[str]] = {}
            for line in conf.splitlines():
                k, _, v = line.partition(" ")
                config.setdefault(k.lower(), []).append(v)
            current = None
            if not explicit or any(s.lstrip("+") in ("HEAD", "@") for s in explicit):
                code, out = _git([*pp.globals, "symbolic-ref", "-q", "--short", "HEAD"], repo_dir, env, timeout)
                current = out.strip() if code == 0 else None
        except Exception as e:  # noqa: BLE001 - fails closed
            rec.update(cause="probe_failed", why=f"could not read the repository's hook state ({str(e)[:160]})", repo=repo_dir)
            continue
        rec["repo"] = toplevel or repo_dir
        # Which branches the push writes.
        remote = pp.positional[0] if pp.positional else ""
        if pp.all_refs:
            targets = [pp.all_refs]
        elif explicit:
            targets = [refspec_dst(s, current) or "" for s in explicit]
        else:
            specs = config.get(f"remote.{(remote or 'origin').lower()}.push") or []
            if specs:
                targets = [refspec_dst(s, current) or "" for s in specs]
            else:
                mode = ((config.get("push.default") or ["simple"])[-1]).lower()
                targets = [current or ""]
                if current and mode in ("upstream", "tracking", "simple"):
                    up = (config.get(f"branch.{current.lower()}.merge") or [""])[-1]
                    targets.append(re.sub(r"^refs/heads/", "", up))
                if mode == "matching":
                    targets.append("*")
        hit = [t for t in targets if t and (t.startswith("--") or "*" in t or t.lower() in protected)]
        if not hit:
            rec["skip"] = "not_protected"
            continue
        rec["targets"] = hit
        # Does the repository ship a pre-push hook, and how is it installed?
        install = ""
        ships = ""
        for sh in cfg.get("shipped_hooks") or []:
            if toplevel and os.path.isfile(os.path.join(toplevel, sh.get("path", ""))):
                ships, install = sh.get("path", ""), sh.get("install", "")
                break
        urls = [u for k, vs in config.items() if k.startswith("remote.") and k.endswith(".url") for u in vs]
        if remote and ("/" in remote or ":" in remote):
            urls.append(remote)
        for r in cfg.get("repos") or []:
            if any(re.search(r.get("remote", "$^"), u, re.I) for u in urls):
                ships = ships or f"registry: {r.get('remote')}"
                install = install or r.get("install", "")
                break
        install = install or cfg.get("default_install") or "git config core.hooksPath .githooks"
        # Does this repository land through the queue (`amux land`)? Judged on
        # the remote actually pushed to; a remote name with no URL in config
        # falls back to every remote URL (fails closed).
        push_urls = [remote] if remote and ("/" in remote or ":" in remote) else \
            (config.get(f"remote.{(remote or 'origin').lower()}.url") or urls)
        land = next((r.get("remote") for r in cfg.get("repos") or [] if r.get("land")
                     and any(re.search(r.get("remote", "$^"), u, re.I) for u in push_urls)), "")
        rec.update(ships_hook=ships or False, install=install, hooks_dir=hooks_dir, land_required=bool(land))
        if pp.no_verify:
            rec.update(cause="no_verify", why="it passes --no-verify, which skips the pre-push gates")
            continue
        bad = [v for v in pp.inline_hooks if v.strip() in ("", "/dev/null")]
        if bad:
            rec.update(cause="hooks_disabled_inline", why=f"it sets core.hooksPath to {bad[0]!r} inline, which turns the hooks off")
            continue
        hook = os.path.join(hooks_dir, "pre-push")
        installed = os.path.isfile(hook) and os.access(hook, os.X_OK)
        rec["hook_installed"] = installed
        if ships and not installed:
            rec.update(cause="hook_not_installed",
                       why=f"this repository ships a pre-push hook ({ships}) and none is installed at {hook}")
            continue
        if land:
            rec.update(cause="land_required", land_cmd=cfg.get("land_command") or "amux land --detach",
                       why=f"this repository lands main through the queue ({land}), and a direct push races it")
            continue
        rec["allowed"] = True
    checked = [r for r in results if "skip" not in r]
    if not checked:
        return 0, ""
    refused = [r for r in checked if r.get("cause")]
    base = {"boundary": b.get("id"), "tool": "Bash", "subject": redact(command), "cwd": cwd,
            "measured": True, "n_considered": len(checked)}
    if not refused:
        log({**base, "level": "INFO", "verdict": cfg.get("verdict_allowed") or "push_gate_allowed", "targets": checked})
        return 0, ""
    land_cmd = cfg.get("land_command") or "amux land --detach"
    for verdict, group in ((cfg.get("verdict_land_required") or "push_gate_land_required",
                            [r for r in refused if r["cause"] == "land_required"]),
                           (cfg.get("verdict_refused") or "push_gate_refused",
                            [r for r in refused if r["cause"] != "land_required"])):
        if group:
            log({**base, "level": "WARN", "verdict": verdict, "targets": group,
                 "measured": not any(r["cause"] in ("probe_failed", "repo_unknown") for r in group)})
    lines = []
    for r in refused:
        lines.append(f"- `{r['segment'][:140]}`" + (f" in {r['repo']}" if r.get("repo") else "") + f": {r['why']}.")
        # How this repository's main is written once the cause is fixed.
        again = f"run `{land_cmd}` from {r['repo']} instead of git push" if r.get("land_required") else "push again"
        if r["cause"] == "land_required":
            lines.append(f"  Fix: from {r['repo']}, run `{land_cmd}` instead of git push. It queues behind other "
                         "lanes, rebases, runs the pre-push gates and pushes for you (`amux land --help`).")
        elif r["cause"] == "hook_not_installed":
            lines.append(f"  Fix: in {r['repo']}, run `{r['install']}`, then {again}.")
        elif r["cause"] == "no_verify":
            lines.append(f"  Fix: drop --no-verify and {again}. If a gate refuses, fix what it reports.")
        elif r["cause"] == "hooks_disabled_inline":
            lines.append(f"  Fix: drop the core.hooksPath override and {again}.")
        else:
            lines.append("  This check fails closed. Run the push as its own command from inside the repository "
                         "(cd <repo> && git push ...), with the repo's pre-push hook installed.")
    return 2, (
        f"BLOCKED by amux boundary '{b.get('id')}' ({b.get('title', '')}).\n" + "\n".join(lines) + "\n\n"
        + b.get("rule", "").strip() + "\n(boundary-guard; the registry is boundaries.toml, GET $AMUX_URL/api/boundaries)\n"
    )


# ---- delete gate (delete-gate, 2026-10-27, V7-delete) ---------------------------
#
# On 2026-10-24 a production delete-all was replayed and data was lost, and the
# fleet has deleted cloud resources by hand before. Owner rules: deleting
# customer or production data needs the owner, and every delete is previewed
# first so its blast radius is seen before it happens. Two phases:
#
#   1. A preview segment records a receipt for this worker, keyed by the
#      normalised delete it previews, at $AMUX_HOME/<dir>/<worker>/<sha>.json.
#   2. A delete segment is refused unless a receipt for its key from this
#      worker exists and is younger than max_age_s. A receipt is used once.
#
# One preview form per tool, the smallest set that shows the blast radius:
#   kubectl|oc delete ARGS           <- the same command with --dry-run=client|server
#   gcloud GROUP... delete T...      <- gcloud GROUP... describe T (one per target)
#   gsutil rm|rb URL..., gcloud storage rm URL...
#                                    <- gsutil ls URL... or gcloud storage ls URL...
#                                       (-r on both or on neither)
# Every segment is judged on its own (command_token_segments). Deletes are
# checked against receipts that existed BEFORE this command, and previews are
# recorded only once the whole command is allowed, so a preview chained with
# its delete never satisfies it. Fails CLOSED: a receipt that cannot be read,
# consumed or written refuses the command.

LAUNCHER_ARG_OPTS = {
    "env": {"-u", "-C", "-S", "--unset", "--chdir"}, "command": set(), "exec": {"-a"}, "nohup": set(),
    "time": set(), "nice": {"-n"}, "builtin": set(), "sudo": {"-u", "-g", "-h", "-p", "-C", "-U"},
    "xargs": {"-n", "-I", "-L", "-P", "-d", "-s", "-E", "-a"}, "timeout": {"-s", "-k", "--signal", "--kill-after"},
    "watch": {"-n", "--interval"},
}
KUBECTL_ALIASES = {"-n": "--namespace", "-l": "--selector", "-f": "--filename", "-o": "--output", "-k": "--kustomize",
                   "-s": "--server", "-A": "--all-namespaces", "-R": "--recursive", "-v": "--v"}
KUBECTL_VALUE_FLAGS = {
    "--namespace", "--context", "--cluster", "--kubeconfig", "--user", "--server", "--token", "--as", "--as-group",
    "--as-uid", "--request-timeout", "--cache-dir", "--certificate-authority", "--client-certificate", "--client-key",
    "--tls-server-name", "--username", "--password", "--v", "--vmodule", "--log-file", "--log-dir", "--profile",
    "--profile-output", "--selector", "--filename", "--field-selector", "--output", "--grace-period", "--timeout",
    "--kustomize", "--raw", "--template", "--chunk-size"}
# How a delete is shown or paced, not what it removes: left out of the key.
KUBECTL_DROP = {"--dry-run", "--output", "--template", "--wait", "--timeout", "--grace-period", "--now", "--force",
                "--ignore-not-found", "--v", "--vmodule", "--request-timeout", "--chunk-size", "--show-managed-fields"}
GCLOUD_BOOL = {"--quiet", "-q", "--async", "--recursive", "-r", "-R", "--all", "--all-versions", "-a", "--force",
               "--log-http", "--user-output-enabled", "--continue-on-error", "--dry-run", "--read-paths-from-stdin", "-I"}
GCLOUD_DROP = {"--quiet", "--async", "--format", "--verbosity", "--log-http", "--user-output-enabled",
               "--no-user-output-enabled", "--continue-on-error", "--delete-disks", "--keep-disks", "--force"}
GSUTIL_GLOBAL_ARG = {"-o", "-h", "-u", "-i"}
URL_RE = re.compile(r"^[a-z][a-z0-9+.-]*://", re.I)


def strip_launchers(tokens: list[str]) -> list[str]:
    """The simple command's own argv: env assignments, launchers (env, sudo,
    xargs, timeout, ...) and their options removed."""
    toks = list(tokens)
    if toks:
        toks[0] = toks[0].lstrip("({")
    while toks and toks[-1] in (")", "}"):
        toks.pop()
    toks = [t for t in toks if t]
    i = 0
    while i < len(toks):
        t = toks[i]
        if re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", t):
            i += 1
            continue
        base = os.path.basename(t)
        if base not in LAUNCHER_ARG_OPTS:
            break
        i += 1
        while i < len(toks) and toks[i].startswith("-") and toks[i] != "--":
            i += 2 if (toks[i] in LAUNCHER_ARG_OPTS[base] and "=" not in toks[i]) else 1
        if i < len(toks) and toks[i] == "--":
            i += 1
        if base == "timeout" and i < len(toks):
            i += 1  # the duration
    return toks[i:]


def parse_flags(args: list[str], aliases: dict, value_flags: set | None, bool_flags: set) -> tuple[list[str], list[tuple[str, str | None]]]:
    """(positionals, [(flag, value or None)]). With value_flags (kubectl), only
    those take a separate value; without (gcloud), every --flag does unless it
    is a known boolean or a --no- flag."""
    pos: list[str] = []
    flags: list[tuple[str, str | None]] = []
    i = 0
    while i < len(args):
        t = args[i]
        i += 1
        if t == "--":
            pos += args[i:]
            break
        if not (t.startswith("-") and len(t) > 1) or re.match(r"^-\d", t):
            pos.append(t)
            continue
        name, eq, val = t.partition("=")
        if not eq and not t.startswith("--") and len(t) > 2 and value_flags is not None \
                and aliases.get(t[:2], t[:2]) in value_flags:
            name, val, eq = t[:2], t[2:], "="  # -nfoo
        name = aliases.get(name, name)
        if eq:
            flags.append((name, val))
            continue
        if value_flags is not None:
            takes = name in value_flags
        else:
            takes = name.startswith("--") and name not in bool_flags and not name.startswith("--no-")
        if takes and i < len(args) and (args[i] == "-" or not args[i].startswith("-")):
            flags.append((name, args[i]))
            i += 1
        else:
            flags.append((name, None))
    return pos, flags


def _flag_text(flags: list[tuple[str, str | None]], drop: set) -> list[str]:
    return sorted(n if v is None else f"{n}={v}" for n, v in flags if n not in drop)


def _storage_op(argv: list[str], verb_at: int, tool: str) -> dict:
    """gsutil rm|rb|ls, gcloud storage rm|ls: keyed on the URLs and -r."""
    rest = argv[verb_at + 1:]
    urls = sorted({u.rstrip("/") or u for u in rest if URL_RE.match(u)})
    rec = any(t == "--recursive" or re.match(r"^-[A-Za-z]*[rR]", t) for t in rest if not URL_RE.match(t))
    stdin = any(t in ("-I", "--read-paths-from-stdin") for t in rest)
    preview = argv[verb_at] == "ls"
    key = "gcs " + ("-r " if rec else "") + " ".join(urls)
    lister = ["gsutil", "ls"] if tool == "gsutil" else ["gcloud", "storage", "ls"]
    op = {"phase": "preview" if preview else "delete", "keys": [key],
          "suggest": [" ".join(lister + (["-r"] if rec else []) + urls)]}
    if not preview and (stdin or not urls):
        op.update(cause="no_target", keys=[],
                  why="its targets are not on the command line (stdin or none), so no preview can show them")
    return op


def parse_delete_segment(tokens: list[str]) -> dict | None:
    """{"phase": "delete"|"preview", "keys": [...], "suggest": [...]} for a
    segment the delete gate judges, else None."""
    import shlex

    argv = strip_launchers(tokens)
    if not argv:
        return None
    tool = os.path.basename(argv[0])
    args = argv[1:]
    if tool in ("kubectl", "oc"):
        pos, flags = parse_flags(args, KUBECTL_ALIASES, KUBECTL_VALUE_FLAGS, set())
        if not pos or pos[0] != "delete":
            return None
        dry = [v for n, v in flags if n == "--dry-run"]
        preview = any(v is None or v.lower() in ("client", "server", "true") for v in dry)
        key = " ".join(["kubectl delete", *pos[1:], *_flag_text(flags, KUBECTL_DROP)])
        op = {"phase": "preview" if preview else "delete", "keys": [key],
              "suggest": [shlex.join(["kubectl", *args]) + " --dry-run=server -o name"]}
        if not preview and any(n == "--filename" and v == "-" for n, v in flags):
            op.update(cause="no_target", keys=[], why="it reads its manifest from stdin, so no preview can show the same objects")
        return op
    if tool == "gsutil":
        i = 0
        while i < len(args) and args[i].startswith("-"):
            i += 2 if args[i] in GSUTIL_GLOBAL_ARG else 1
        if i < len(args) and args[i] in ("rm", "rb", "ls"):
            return _storage_op(args, i, "gsutil")
        return None
    if tool == "gcloud":
        pos, flags = parse_flags(args, {"-q": "--quiet"}, None, GCLOUD_BOOL)
        verb = next((j for j, p in enumerate(pos) if p in ("delete", "describe")
                     or (p in ("rm", "ls") and j and pos[j - 1] == "storage")), None)
        if verb is None:
            return None
        if pos[verb] in ("rm", "ls"):
            st = args.index("storage")
            return _storage_op(args, args.index(pos[verb], st + 1), "gcloud")
        path, targets = pos[:verb], pos[verb + 1:] or [""]
        ftext = _flag_text(flags, GCLOUD_DROP)
        keys = [" ".join(["gcloud", *path, "::", t, *ftext]) for t in targets]
        suggest = [" ".join(["gcloud", *path, "describe", *([shlex.quote(t)] if t else []), *map(shlex.quote, ftext)])
                   for t in targets]
        return {"phase": "preview" if pos[verb] == "describe" else "delete", "keys": keys, "suggest": suggest}
    return None


def _preview_path(rdir: str, key: str) -> str:
    import hashlib

    return os.path.join(rdir, hashlib.sha256(key.encode()).hexdigest()[:32] + ".json")


def read_preview(path: str, key: str, max_age_s: float) -> tuple[dict | None, str]:
    try:
        with open(path) as fh:
            r = json.load(fh)
    except FileNotFoundError:
        return None, "no_preview"
    except (OSError, ValueError):
        return None, "preview_unreadable"
    if not isinstance(r, dict):
        return None, "preview_unreadable"
    if r.get("session") != WORKER:
        return None, "preview_other_session"
    if r.get("key") != key:
        return None, "preview_other_target"
    try:
        age = time.time() - float(r.get("ts"))
    except (TypeError, ValueError):
        return None, "preview_unreadable"
    if age > max_age_s or age < -60:
        return None, "preview_expired"
    return dict(r, age_s=round(age)), ""


def check_delete_gate(b: dict, command: str) -> tuple[int, str]:
    cfg = b.get("delete_gate") or {}
    if not re.search(r"\b(kubectl|oc|gcloud|gsutil)\b", command):
        return 0, ""  # the cheap prefilter
    ops: list[dict] = []
    for toks, _via in command_token_segments(command):
        op = parse_delete_segment(toks)
        if op is not None:
            op["segment"] = " ".join(toks)
            ops.append(op)
    if not ops:
        return 0, ""
    max_age = float(cfg.get("max_age_s", 900))
    rdir = os.path.join(amux_home(), cfg.get("dir") or "state/delete-previews", receipt_key(WORKER))
    deletes = [o for o in ops if o["phase"] == "delete"]
    previews = [o for o in ops if o["phase"] == "preview"]
    chained = {k for o in previews for k in o["keys"]}
    base = {"boundary": b.get("id"), "tool": "Bash", "subject": redact(command), "max_age_s": max_age,
            "measured": True, "n_considered": len(ops)}
    v_refused = cfg.get("verdict_refused") or "delete_gate_refused"
    refused: list[dict] = []
    allowed: list[dict] = []
    used: set[str] = set()
    for d in deletes:
        if d.get("cause"):
            refused.append({"segment": redact(d["segment"]), "operand": "", "cause": d["cause"], "why": d["why"]})
            continue
        for key, suggest in zip(d["keys"], d["suggest"]):
            path = _preview_path(rdir, key)
            rec, cause = (None, "no_preview") if path in used else read_preview(path, key, max_age)
            t = {"segment": redact(d["segment"]), "operand": redact(key), "receipt": path}
            if rec is None:
                refused.append({**t, "cause": cause, "suggest": suggest, "chained": key in chained})
            else:
                used.add(path)
                allowed.append({**t, "preview_age_s": rec.get("age_s"), "preview": rec.get("preview")})
    if not refused:
        for path in sorted(used):  # a receipt is good for one delete
            try:
                os.remove(path)
            except OSError as e:
                refused.append({"segment": redact(command), "operand": "", "receipt": path, "cause": "state_unwritable",
                                "why": f"its preview receipt could not be consumed ({e})"})
    if not refused and previews:
        try:
            os.makedirs(rdir, exist_ok=True)
            for o in previews:
                for key in o["keys"]:
                    path = _preview_path(rdir, key)
                    tmp = f"{path}.{os.getpid()}.tmp"
                    with open(tmp, "w") as fh:
                        json.dump({"session": WORKER, "key": key, "ts": time.time(), "preview": redact(o["segment"])}, fh)
                    os.replace(tmp, path)
        except OSError as e:
            refused.append({"segment": redact(command), "operand": " | ".join(k for o in previews for k in o["keys"]),
                            "cause": "state_unwritable", "why": f"the preview receipt could not be written ({e})"})
    if refused:
        log({**base, "level": "WARN", "verdict": v_refused, "targets": refused,
             "measured": not any(r["cause"] == "state_unwritable" for r in refused)})
    else:
        if allowed:
            log({**base, "level": "INFO", "verdict": cfg.get("verdict_allowed") or "delete_gate_allowed", "targets": allowed})
        if previews:
            log({**base, "level": "INFO", "verdict": cfg.get("verdict_preview") or "delete_gate_preview_recorded",
                 "targets": [{"segment": redact(o["segment"]), "operand": redact(k)} for o in previews for k in o["keys"]]})
        return 0, ""
    mins = f"{max_age / 60:g} min"
    lines = []
    for r in refused:
        what = {"no_preview": f"this worker has no unused preview of this delete from the last {mins}",
                "preview_expired": f"this worker's preview of it is older than {mins}",
                "preview_other_session": "the preview receipt there was written for another worker",
                "preview_other_target": "the preview receipt there names another target",
                "preview_unreadable": "the preview receipt there does not parse"}.get(r["cause"], r.get("why", r["cause"]))
        lines.append(f"- `{r['segment'][:160]}`: {what}.")
        if r.get("chained"):
            lines.append("  A preview chained in the same command does not count: run it on its own first and read it.")
        if r.get("suggest"):
            lines.append("  Preview it first, as its own command, and read what it lists:\n    " + r["suggest"])
        elif r["cause"] == "no_target":
            lines.append("  Name the targets on the command line, preview them, then delete.")
        elif r["cause"] == "state_unwritable":
            lines.append(f"  This check fails closed. Check that {rdir} is writable.")
    return 2, (
        f"BLOCKED by amux boundary '{b.get('id')}' ({b.get('title', '')}).\n" + "\n".join(lines) + "\n"
        f"Then run the delete again: a preview is good for one delete within {mins}.\n\n"
        + b.get("rule", "").strip() + "\n(boundary-guard; the registry is boundaries.toml, GET $AMUX_URL/api/boundaries)\n"
    )


# ---- resources -----------------------------------------------------------------

def _sysctl(name: str) -> str | None:
    import subprocess

    for exe in ("/usr/sbin/sysctl", "sysctl"):
        try:
            out = subprocess.run([exe, "-n", name], capture_output=True, text=True, timeout=2)
            if out.returncode == 0:
                return out.stdout.strip()
        except (OSError, subprocess.SubprocessError):
            continue
    return None


def host_sample() -> dict:
    """Load per core, swap used fraction and memory level, read directly so the
    check does not depend on the amux server being up."""
    fixed = os.environ.get("AMUX_BOUNDARIES_HOST_SAMPLE")  # tests: a fixed reading
    if fixed:
        s = json.loads(fixed)
        s.setdefault("ncpu", 1)
        if "load5" in s:
            s["load_per_core"] = s["load5"] / s["ncpu"]
        return s
    s: dict = {"ncpu": os.cpu_count() or 1}
    try:
        s["load5"] = os.getloadavg()[1]
    except OSError:
        pass
    lvl = _sysctl("kern.memorystatus_vm_pressure_level")
    if lvl and lvl.strip().isdigit():
        s["mem_level"] = int(lvl)
    swap = _sysctl("vm.swapusage")  # "total = 9216.00M  used = 8134.50M  free = ..."
    if swap:
        m_t = re.search(r"total\s*=\s*([\d.]+)M", swap)
        m_u = re.search(r"used\s*=\s*([\d.]+)M", swap)
        if m_t and m_u and float(m_t.group(1)) > 0:
            s["swap_used"] = float(m_u.group(1)) / float(m_t.group(1))
            s["swap_total_mb"] = float(m_t.group(1))
    elif os.path.exists("/proc/meminfo"):
        try:
            info = dict(l.split(":", 1) for l in open("/proc/meminfo") if ":" in l)
            tot = float(info["SwapTotal"].split()[0]); free = float(info["SwapFree"].split()[0])
            if tot > 0:
                s["swap_used"] = (tot - free) / tot
                s["swap_total_mb"] = tot / 1024
            avail = float(info["MemAvailable"].split()[0]) / float(info["MemTotal"].split()[0])
            s["mem_level"] = 4 if avail < 0.05 else 2 if avail < 0.10 else 1
        except (KeyError, ValueError, OSError):
            pass
    if "load5" in s:
        s["load_per_core"] = s["load5"] / s["ncpu"]
    return s


def classify(s: dict, cfg: dict) -> tuple[str, list[str]]:
    reasons_c, reasons_s = [], []
    lpc = s.get("load_per_core")
    if lpc is not None:
        if lpc >= cfg.get("load_per_core_critical", 2.5):
            reasons_c.append(f"load {s['load5']:.1f} on {s['ncpu']} cores ({lpc:.2f}/core)")
        elif lpc >= cfg.get("load_per_core_strained", 1.5):
            reasons_s.append(f"load {s['load5']:.1f} on {s['ncpu']} cores ({lpc:.2f}/core)")
    sw = s.get("swap_used")
    if sw is not None and s.get("swap_total_mb", 0) >= 1024:
        if sw >= cfg.get("swap_used_critical", 0.97):
            reasons_c.append(f"swap {sw:.0%} used")
        elif sw >= cfg.get("swap_used_strained", 0.85):
            reasons_s.append(f"swap {sw:.0%} used")
    ml = s.get("mem_level")
    if ml is not None:
        if ml >= cfg.get("mem_level_critical", 4):
            reasons_c.append(f"memory pressure level {ml}")
        elif ml >= cfg.get("mem_level_strained", 2):
            reasons_s.append(f"memory pressure level {ml}")
    if reasons_c:
        return "critical", reasons_c
    if reasons_s:
        return "strained", reasons_s
    measured = any(k in s for k in ("load_per_core", "swap_used", "mem_level"))
    return ("ok" if measured else "unmeasured"), []


def my_usage(session: str) -> str:
    """This worker's footprint from the amux server, best effort (1.5s)."""
    url = os.environ.get("AMUX_URL")
    if not url or not session:
        return ""
    import ssl
    import urllib.request

    try:
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        with urllib.request.urlopen(f"{url}/api/resources?worker={session}", timeout=1.5, context=ctx) as r:
            d = json.load(r)
        w = d.get("worker") or {}
        if w.get("measured"):
            return f"You are using {w.get('cpu_percent', 0):.0f}% CPU and {w.get('rss_mb', 0):.0f} MB across {w.get('n_processes', 0)} processes."
    except Exception:
        pass
    return ""


ACK_RE = re.compile(r"""(?:^|[;&|]\s*|\benv\s+)(AMUX_RESOURCES_ACK)=(?:"([^"]*)"|'([^']*)'|(\S+))""")
DISK_ACK_RE = re.compile(r"""(?:^|[;&|]\s*|\benv\s+)(AMUX_DISK_ACK)=(?:"([^"]*)"|'([^']*)'|(\S+))""")


# ---- per-worker disk budget (mxp-gs12 review, 2026-10-18) -----------------------
#
# The Mac's data volume filled on 2026-10-09 and 2026-10-14 from one worker's
# subagent Rust targets (34 GB and 41 GB), and its scratch reached 266 GB. A
# worker declared AMUX_WORKER_DISK_GB in its resource profile, but nothing
# measured it. Now the worker's scratch (~/.amux/tmp/<worker>, plus any paths
# in AMUX_WORKER_DISK_PATHS, colon-separated) is measured with `du` in a
# DETACHED process (a 69 GB tree takes tens of seconds, and this hook runs
# before every Bash call), cached in ~/.amux/state/disk-usage/<worker>.json for
# AMUX_WORKER_DISK_TTL_S (default 600), and checked against the budget before
# heavy work: past disk_warn_fraction of it the command runs with a warning,
# past the budget heavy work is refused until the worker frees space, raises
# its budget in the Scope tab, or reruns with AMUX_DISK_ACK="<why>" (recorded).
# The budget is the scoped AMUX_WORKER_DISK_GB, else the registry's
# disk_budget_gb_default; 0 turns the check off. /api/resources reads the same
# cache file, so the number a refusal quotes is the number the worker sees.

def disk_paths(session: str) -> list[str]:
    paths = [os.path.join(amux_home(), "tmp", session)]
    for p in (os.environ.get("AMUX_WORKER_DISK_PATHS") or "").split(":"):
        p = os.path.expanduser(p.strip())
        if p and p not in paths:
            paths.append(p)
    return [p for p in paths if os.path.exists(p)]


# The detached du's deadline. It was 240s, and du on mxp-gs12's 236 GB scratch
# takes 896s on this machine under load (Mac disk RCA 20261025-190143), so
# every measurement timed out, the reading stayed "unmeasured" and the 100 GB
# budget never applied to the worker furthest past it: the bigger the tree,
# the less the budget could see it. The walk runs detached, so a long deadline
# costs no command anything; the lock above stays fresh for the whole run so
# two measurements never overlap.
DISK_DU_TIMEOUT_S = float(os.environ.get("AMUX_DISK_DU_TIMEOUT_S") or 1800)


def disk_cache_path(session: str) -> str:
    return os.path.join(amux_home(), "state", "disk-usage", session + ".json")


def measure_disk(session: str, timeout_s: float) -> dict:
    """du -sk every path, bounded. Writes and returns the cache record."""
    import subprocess

    paths = disk_paths(session)
    rec: dict = {"worker": session, "ts": round(time.time(), 3), "paths": {}, "n_considered": len(paths)}
    total, measured, why = 0, True, None
    deadline = time.time() + timeout_s
    for p in paths:
        left = deadline - time.time()
        if left <= 0:
            measured, why = False, f"du budget {timeout_s:.0f}s spent before {p}"
            break
        try:
            out = subprocess.run(["du", "-sk", p], capture_output=True, text=True, timeout=left)
            kb = int((out.stdout.split() or ["0"])[0])
        except subprocess.TimeoutExpired:
            measured, why = False, f"du timed out on {p} after {timeout_s:.0f}s"
            break
        except (OSError, ValueError) as e:
            measured, why = False, f"du failed on {p}: {e}"
            break
        rec["paths"][p] = kb * 1024
        total += kb * 1024
    rec.update({"bytes": total if measured else None, "measured": measured, "why_unmeasured": why})
    try:
        cp = disk_cache_path(session)
        os.makedirs(os.path.dirname(cp), exist_ok=True)
        tmp = cp + f".{os.getpid()}"
        with open(tmp, "w") as fh:
            json.dump(rec, fh)
        os.replace(tmp, cp)
    except OSError:
        pass
    return rec


def disk_usage(session: str, ttl_s: float) -> dict:
    """The cached reading, refreshed in the background when stale."""
    fixed = os.environ.get("AMUX_DISK_USAGE_SAMPLE")  # tests: a fixed reading
    if fixed:
        return json.loads(fixed)
    cp = disk_cache_path(session)
    rec: dict = {}
    try:
        with open(cp) as fh:
            rec = json.load(fh)
    except (OSError, ValueError):
        rec = {}
    age = time.time() - float(rec.get("ts") or 0)
    if age > ttl_s and not disk_paths(session):
        # Nothing to measure (no scratch yet): a zero reading costs nothing.
        return {"measured": True, "bytes": 0, "paths": {}, "n_considered": 0}
    if age > ttl_s:
        lock = cp + ".measuring"
        try:
            fresh_lock = os.path.exists(lock) and time.time() - os.stat(lock).st_mtime < DISK_DU_TIMEOUT_S + 300
        except OSError:
            fresh_lock = False
        if not fresh_lock:
            import subprocess

            try:
                os.makedirs(os.path.dirname(cp), exist_ok=True)
                open(lock, "w").close()
                subprocess.Popen([sys.executable, os.path.abspath(__file__), "--measure-disk", session],
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                 start_new_session=True, env=dict(os.environ))
            except OSError:
                pass
        if not rec:
            return {"measured": False, "why_unmeasured": "first measurement started in the background", "n_considered": 0}
        rec = dict(rec, stale_s=round(age))
    return rec


def disk_budget_bytes(cfg: dict) -> float | None:
    raw = os.environ.get("AMUX_WORKER_DISK_GB")
    try:
        gb = float(raw) if raw not in (None, "") else float(cfg.get("disk_budget_gb_default", 0) or 0)
    except ValueError:
        return None
    return gb * 1024 ** 3 if gb > 0 else None


def check_disk(b: dict, command: str, session: str) -> tuple[int, str]:
    cfg = b.get("resources") or {}
    budget = disk_budget_bytes(cfg)
    if budget is None or not session:
        return 0, ""
    ttl = float(os.environ.get("AMUX_WORKER_DISK_TTL_S") or cfg.get("disk_ttl_s", 600))
    rec = disk_usage(session, ttl)
    base = {"boundary": b.get("id"), "tool": "Bash", "command": redact(command), "budget_gb": round(budget / 1024 ** 3, 2),
            "measured": bool(rec.get("measured")), "n_considered": rec.get("n_considered", 0)}
    if not rec.get("measured"):
        log({**base, "verdict": "disk_budget_unmeasured", "why_unmeasured": rec.get("why_unmeasured")})
        return 0, ""
    used = float(rec.get("bytes") or 0)
    gb = used / 1024 ** 3
    frac = used / budget
    base.update({"used_gb": round(gb, 2), "used_bytes": int(used), "budget_bytes": int(budget), "fraction": round(frac, 3), "stale_s": rec.get("stale_s", 0)})
    paths = ", ".join(f"{p} {v / 1024 ** 3:.1f} GB" for p, v in sorted((rec.get("paths") or {}).items(), key=lambda x: -x[1])[:4])
    if frac >= 1.0:
        ack = DISK_ACK_RE.search(command)
        if ack:
            why = next(g for g in ack.groups()[1:] if g is not None)
            log({**base, "verdict": "disk_budget_acknowledged", "ack": redact(why)})
            return 0, ""
        log({**base, "verdict": "disk_budget_refused"})
        return 2, (
            f"BLOCKED by amux boundary '{b.get('id')}': your scratch uses {gb:.1f} GB, over your disk budget of "
            f"{budget / 1024 ** 3:.0f} GB (AMUX_WORKER_DISK_GB), so new heavy work waits until you free space.\n"
            f"Measured: {paths or 'no paths'} ({rec.get('stale_s', 0)}s ago).\n"
            "Delete build targets and scratch you no longer need (finished subagents' cargo targets are the usual cause; "
            "share one CARGO_TARGET_DIR across your subagents), or raise AMUX_WORKER_DISK_GB in the Scope tab. "
            "If the work truly cannot wait, rerun as\n"
            f"  AMUX_DISK_ACK=\"<why>\" {command[:120]}\n"
            "and the reason is recorded.\n(boundary-guard)\n"
        )
    if frac >= float(cfg.get("disk_warn_fraction", 0.8)):
        log({**base, "verdict": "disk_budget_warn"})
        return 0, (f"amux boundary '{b.get('id')}': WARNING your scratch uses {gb:.1f} GB, {frac:.0%} of your "
                   f"{budget / 1024 ** 3:.0f} GB disk budget ({paths}). Heavy work is refused past 100%; free space now.\n")
    return 0, ""


def check_resources(b: dict, command: str, session: str) -> tuple[int, str]:
    cfg = b.get("resources") or {}
    if not any(search(p, command) for p in cfg.get("heavy") or []):
        return 0, ""
    ack = ACK_RE.search(command)
    sample = host_sample()
    level, reasons = classify(sample, cfg)
    base = {"boundary": b.get("id"), "tool": "Bash", "level": level, "reasons": reasons,
            "command": redact(command), "measured": level != "unmeasured", "n_considered": 1}
    if level == "critical" and ack:
        why = next(g for g in ack.groups()[1:] if g is not None)
        log({**base, "verdict": "resources_acknowledged", "ack": redact(why)})
        return _disk_after(b, command, session)
    if level == "critical":
        log({**base, "verdict": "resources_refused"})
        usage = my_usage(session)
        return 2, (
            f"BLOCKED by amux boundary '{b.get('id')}': the machine is critical ({'; '.join(reasons)}), "
            "so heavy commands wait until it recovers.\n"
            + (usage + "\n" if usage else "")
            + "Run `amux resources` to see who is using what. Wait and retry, lower parallelism, "
            "or move the work to remote hardware. If it truly cannot wait, rerun as\n"
            f"  AMUX_RESOURCES_ACK=\"<why>\" {command[:120]}\n"
            "and the reason is recorded.\n(boundary-guard)\n"
        )
    if level == "strained":
        log({**base, "verdict": "resources_strained_allowed"})
    if level == "unmeasured":
        log({**base, "verdict": "resources_unmeasured", "why_unmeasured": "no load, swap or memory reading"})
    return _disk_after(b, command, session)


def _disk_after(b: dict, command: str, session: str) -> tuple[int, str]:
    """The disk budget, checked for every heavy command the host check let through."""
    code, msg = check_disk(b, command, session)
    if code:
        return code, msg
    if msg:
        sys.stderr.write(msg)
    return 0, ""


# ---- subagent cap (mxp-gs12 review, 2026-10-18) ---------------------------------
#
# Usage limits stopped every subagent of one large isolated worker five times in
# four days: it fanned out up to 21 Claude Code agents on one shared plan, and
# nothing compared the fan-out with the account's remaining capacity. This
# refuses a NEW Agent/Task tool call when the worker already has
# `max_running` subagents running, or when the account's usage headroom is
# below `min_headroom_pct`. Refusal only: it injects no prompt, routes nothing,
# and the model decides what to do with the slot it has.
#
# The running count is read from Claude Code's own files, so it needs no amux
# reporting (an isolated worker sends none): every subagent has
# <conversation>/subagents/agent-<id>.jsonl beside agent-<id>.meta.json, and
# the meta gains "finished": true when the agent stops. Running = no finished
# flag and a transcript written within `stale_s` (a killed agent never gets the
# flag). A transcript with no meta file counts as running only while it is
# warm (`no_meta_warm_s`). No transcript path in the payload means the count
# is unmeasured, and an unmeasured count ALLOWS the call and says so in the log.
#
# Headroom is 100 minus the highest account-wide window percent in amux's
# GET /api/usage (the session and weekly windows). Unreachable or unmeasured
# also allows. The max is AMUX_MAX_SUBAGENTS in the worker's env when set
# (0 turns the cap off), else the registry's `max_running_default`.

def endpoint_url() -> str:
    url = os.environ.get("AMUX_URL") or ""
    if url:
        return url.rstrip("/")
    try:
        with open(os.path.join(amux_home(), "endpoint.json")) as fh:
            return str(json.load(fh).get("canonical_url") or "").rstrip("/")
    except (OSError, ValueError):
        return ""


def host_process_start() -> float | None:
    """Start time of the Claude Code process this hook runs under, or None.

    Subagents run inside that process, so a transcript last written before it
    started belongs to an agent the restart ended. Without this, a fleet
    restart (2026-10-19 16:02Z) left mxp-gs12 refused at "9 subagents running"
    with none alive until each transcript aged past `stale_s`.
    """
    fixed = os.environ.get("AMUX_SUBAGENT_HOST_START")  # tests
    if fixed:
        try:
            return float(fixed)
        except ValueError:
            return None
    import subprocess

    pid = os.getppid()
    for _ in range(8):
        if pid <= 1:
            return None
        try:
            out = subprocess.run(["ps", "-o", "ppid=,lstart=,command=", "-p", str(pid)],
                                 capture_output=True, text=True, timeout=2).stdout.strip()
        except Exception:  # noqa: BLE001 - unmeasured, the caller keeps the old rule
            return None
        if not out:
            return None
        parts = out.split(None, 6)
        if len(parts) < 7:
            return None
        ppid, lstart, cmd = parts[0], " ".join(parts[1:6]), parts[6]
        if os.path.basename(cmd.split()[0]) == "claude" or re.match(r"^\d+\.\d+\.\d+$", os.path.basename(cmd.split()[0])):
            try:
                return time.mktime(time.strptime(lstart, "%a %b %d %H:%M:%S %Y"))
            except ValueError:
                return None
        try:
            pid = int(ppid)
        except ValueError:
            return None
    return None


def ended_by_stop(path: str) -> bool:
    """True when the transcript's last entry is the interrupt a stop writes.

    TaskStop never sets "finished" in the meta, so a stopped agent read as
    running until its transcript aged past `stale_s` (30 min). Measured
    2026-10-22: mxp-gs12 stopped a subagent at 23:25:59Z to free a slot for
    critical-path work and the cap still refused the new one. The stop does
    write a final user entry "[Request interrupted by user]"; a resumed agent
    appends after it, so only the last entry is read.
    """
    try:
        with open(path, "rb") as fh:
            fh.seek(0, os.SEEK_END)
            size = fh.tell()
            fh.seek(max(0, size - 8192))
            tail = fh.read().decode("utf-8", "replace")
    except OSError:
        return False
    lines = [ln for ln in tail.splitlines() if ln.strip()]
    if not lines:
        return False
    try:
        last = json.loads(lines[-1])
    except ValueError:
        return False
    msg = last.get("message") or {}
    if last.get("type") != "user" or msg.get("role") != "user":
        return False
    content = msg.get("content")
    texts = [content] if isinstance(content, str) else [
        c.get("text", "") for c in (content or []) if isinstance(c, dict) and c.get("type") == "text"]
    return any(t.startswith("[Request interrupted by user") for t in texts)


def count_running_subagents(transcript_path: str, cfg: dict, now: float | None = None,
                            host_start: float | None = None) -> dict:
    """Running subagents of the conversation behind `transcript_path`."""
    now = time.time() if now is None else now
    if not transcript_path:
        return {"measured": False, "why_unmeasured": "hook payload carried no transcript_path", "n_considered": 0}
    tp = os.path.expanduser(transcript_path)
    parent = os.path.dirname(tp)
    if os.path.basename(parent) == "subagents":
        subs = parent  # a subagent's own call: its siblings are the worker's subagents
    else:
        if not os.path.exists(tp):
            return {"measured": False, "why_unmeasured": f"transcript {tp} does not exist", "n_considered": 0}
        subs = tp[:-len(".jsonl")] + "/subagents" if tp.endswith(".jsonl") else tp + "/subagents"
    stale_s = float(cfg.get("stale_s", 1800))
    warm_s = float(cfg.get("no_meta_warm_s", 240))
    try:
        names = [n for n in os.listdir(subs) if n.endswith(".jsonl")]
    except FileNotFoundError:
        return {"measured": True, "running": 0, "n_considered": 0, "running_ids": []}
    except OSError as e:
        return {"measured": False, "why_unmeasured": f"cannot list {subs}: {e}", "n_considered": 0}
    running: list[str] = []
    details: list[dict] = []
    ended_by_restart = 0
    ended_by_stop_n = 0
    for n in names:
        stem = n[:-len(".jsonl")]
        try:
            mtime = os.stat(os.path.join(subs, n)).st_mtime
        except OSError:
            continue
        age = now - mtime
        try:
            with open(os.path.join(subs, stem + ".meta.json")) as fh:
                meta = json.load(fh)
        except (OSError, ValueError):
            meta = None
        if meta is None:
            live = age < warm_s
        else:
            live = not meta.get("finished") and age < stale_s
        if live and host_start is not None and mtime < host_start - 2:
            ended_by_restart += 1  # last written before this process started: the restart ended it
            continue
        if live and ended_by_stop(os.path.join(subs, n)):
            ended_by_stop_n += 1  # stopped: TaskStop writes the interrupt and never sets "finished"
            continue
        if live:
            running.append(stem)
            started = agent_started_at(os.path.join(subs, n))
            details.append({"id": stem[len("agent-"):] if stem.startswith("agent-") else stem,
                            "description": str((meta or {}).get("description") or "")[:80],
                            "age_s": round(now - started) if started is not None else None,
                            "idle_s": round(age)})
    details.sort(key=lambda d: -(d["age_s"] if d["age_s"] is not None else -1))
    ages = [d["age_s"] for d in details if d["age_s"] is not None]
    return {"measured": True, "running": len(running), "n_considered": len(names), "running_ids": sorted(running)[:12],
            "running_agents": details[:12], "oldest_age_s": max(ages) if ages else None,
            "ended_by_restart": ended_by_restart, "ended_by_stop": ended_by_stop_n,
            "host_start_measured": host_start is not None}


TS_RE = re.compile(r'"timestamp"\s*:\s*"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.\d+)?Z"')


def agent_started_at(path: str) -> float | None:
    """When a subagent started: the first transcript entry's timestamp (the
    prompt the parent sent), else the file's birth time, else None. Only the
    first 256 KB are read; a key inside the prompt text is escaped, so the
    first raw match is the entry's own field."""
    try:
        with open(path, "rb") as fh:
            head = fh.read(262144).decode("utf-8", "replace")
        m = TS_RE.search(head)
        if m:
            import calendar
            return float(calendar.timegm(time.strptime(m.group(1), "%Y-%m-%dT%H:%M:%S")))
        st = os.stat(path)
        return float(getattr(st, "st_birthtime", 0) or 0) or None
    except (OSError, ValueError):
        return None


def usage_headroom() -> dict:
    """Account headroom from amux's /api/usage, best effort (2s)."""
    fixed = os.environ.get("AMUX_USAGE_SAMPLE")  # tests: a fixed /api/usage body
    if fixed:
        body = json.loads(fixed)
    else:
        url = endpoint_url()
        if not url:
            return {"measured": False, "why_unmeasured": "no AMUX_URL and no endpoint.json"}
        import ssl
        import urllib.request

        try:
            ctx = ssl.create_default_context()
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
            with urllib.request.urlopen(f"{url}/api/usage", timeout=2, context=ctx) as r:
                body = json.load(r)
        except Exception as e:  # noqa: BLE001 - fail open, say why
            return {"measured": False, "why_unmeasured": f"GET /api/usage failed: {str(e)[:120]}"}
    if not body.get("measured") or body.get("available") is False:
        return {"measured": False, "why_unmeasured": "amux /api/usage has no measured reading"}
    windows = [w for w in body.get("limits") or [] if not w.get("scope") and isinstance(w.get("percent"), (int, float))]
    if not windows:
        return {"measured": False, "why_unmeasured": "amux /api/usage listed no account-wide window"}
    top = max(windows, key=lambda w: w["percent"])
    return {"measured": True, "headroom_pct": round(100 - float(top["percent"]), 1),
            "window": top.get("kind") or top.get("group") or "", "resets_at": top.get("resets_at") or ""}


def scoped_env_value(key: str) -> str | None:
    """The worker's live scoped value for key, from $AMUX_HOME/sessions/<worker>.env.

    The process env is fixed at launch, so a cap the owner raises in the Scope
    tab used to apply only after a restart (2026-10-26: a doubled cap for three
    lanes mid-incident read the old value). The env file is what the server
    writes on every scope change, so it wins when it names the key.
    """
    if not WORKER:
        return None
    try:
        with open(os.path.join(amux_home(), "sessions", WORKER + ".env")) as f:
            for line in f:
                k, sep, v = line.strip().partition("=")
                if sep and k == key:
                    return v.strip().strip('"').strip("'")
    except OSError:
        return None
    return None


def subagent_max(cfg: dict) -> int | None:
    key = cfg.get("env_max") or "AMUX_MAX_SUBAGENTS"
    raw = scoped_env_value(key)
    if raw is None:
        raw = os.environ.get(key)
    try:
        n = int(raw) if raw not in (None, "") else int(cfg.get("max_running_default", 4))
    except ValueError:
        n = int(cfg.get("max_running_default", 4))
    return n if n > 0 else None


def fmt_age(s: float | None) -> str:
    if s is None:
        return "age unknown"
    s = int(s)
    return f"{s // 3600}h{(s % 3600) // 60:02d}m" if s >= 3600 else f"{s // 60}m{s % 60:02d}s"


def priority_marker(cfg: dict, ti: dict) -> str | None:
    """The registry's priority marker this Agent call carries, if any. Matched
    against the description and the start of the prompt."""
    text = [str(ti.get("description") or ""), str(ti.get("prompt") or "")[:400]]
    for p in cfg.get("priority_markers") or []:
        for t in text:
            m = re.search(p, t, re.IGNORECASE | re.MULTILINE)
            if m:
                return m.group(0).strip()
    return None


def running_list(count: dict) -> str:
    rows = count.get("running_agents") or []
    return "\n".join(
        f"  - {r['id']}  {r.get('description') or '(no description)'}  running {fmt_age(r.get('age_s'))}, "
        f"last wrote {fmt_age(r.get('idle_s'))} ago" for r in rows)


def check_subagents(b: dict, tool: str, transcript_path: str, ti: dict | None = None) -> tuple[int, str]:
    cfg = b.get("subagents") or {}
    if tool not in (cfg.get("tools") or ["Agent", "Task"]):
        return 0, ""
    ti = ti or {}
    limit = subagent_max(cfg)
    floor = float(cfg.get("min_headroom_pct", 0) or 0)
    if limit is None and floor <= 0:
        return 0, ""
    count = (count_running_subagents(transcript_path, cfg, host_start=host_process_start()) if limit is not None
             else {"measured": False, "why_unmeasured": "cap off"})
    head = usage_headroom() if floor > 0 else {"measured": False, "why_unmeasured": "headroom floor off"}
    marker = priority_marker(cfg, ti)
    over_cap = max(0, int(cfg.get("over_cap_max", 0) or 0))
    base = {"boundary": b.get("id"), "tool": tool, "limit": limit, "min_headroom_pct": floor,
            "running": count.get("running"), "running_ids": count.get("running_ids"),
            "running_agents": count.get("running_agents"), "oldest_age_s": count.get("oldest_age_s"),
            "priority_marker": marker, "over_cap_max": over_cap,
            "ended_by_restart": count.get("ended_by_restart"), "ended_by_stop": count.get("ended_by_stop"),
            "host_start_measured": count.get("host_start_measured"),
            "measured": bool(count.get("measured")), "n_considered": count.get("n_considered", 0),
            "headroom_pct": head.get("headroom_pct"), "headroom_measured": bool(head.get("measured")),
            "window": head.get("window"), "resets_at": head.get("resets_at")}
    markers = " or ".join(f"`{m}`" for m in cfg.get("priority_example") or []) or "a priority marker"
    long_s = float(cfg.get("long_running_s", 0) or 0)
    if long_s > 0 and (count.get("oldest_age_s") or 0) >= long_s:
        # Logged whatever this call's outcome: a line a sweep can find when a subagent has run for hours.
        log({**base, "level": "WARN", "verdict": "subagent_long_running", "long_running_s": long_s})
    at_cap = limit is not None and bool(count.get("measured")) and count["running"] >= limit
    listing = running_list(count)
    preempt = ""
    if at_cap and marker and over_cap > 0 and count["running"] < limit + over_cap:
        # Priority-aware allowance (2026-10-27, MSG-98201): the lane's most
        # important task starts even with the cap full, bounded at
        # limit + over_cap_max. True preemption (stopping another agent) is
        # impossible from a PreToolUse hook: it can only allow or refuse this
        # call, and subagents run inside the Claude process with no handle the
        # hook could stop. The lane stops one itself (TaskStop). The usage
        # headroom floor below still applies to a marked call.
        preempt = (f"amux boundary '{b.get('id')}': started over the cap on priority marker {marker!r} "
                   f"({count['running']} running, limit {limit}, at most {limit + over_cap} with a marker). Stop your "
                   f"lowest-priority or stale subagent (TaskStop) so the next marked task has room:\n{listing}\n")
    if at_cap and not preempt:
        n = count["running"]
        cause = "over_cap_bound" if marker else "running_cap"
        log({**base, "level": "WARN", "verdict": "subagent_cap_refused", "cause": cause})
        why = (f"it carries priority marker {marker!r}, but you already use the {over_cap} over-cap slot(s) a marked "
               f"task may take (at most {limit + over_cap})" if marker else
               f"a task marked {markers} at the start of its description may start {over_cap} over the cap"
               if over_cap > 0 else "")
        return 2, (
            f"BLOCKED by amux boundary '{b.get('id')}': you already have {n} subagents running "
            f"and the limit is {limit} (AMUX_MAX_SUBAGENTS), so this new agent was not started"
            + (f"; {why}" if why else "") + ".\n"
            f"Running now (oldest first):\n{listing or '  (no details)'}\n"
            "If this task matters more than one of those, stop the lowest-priority or stale one yourself with "
            "TaskStop (its id is listed) and retry"
            + (f", or mark this one {markers}" if not marker and over_cap > 0 else "")
            + ". Retry when one of them finishes (a background agent notifies you when it stops), or do this step "
            "yourself. Fanning out past the account's capacity is how a usage limit stops every agent at once.\n"
            "(boundary-guard; the registry is boundaries.toml)\n"
        )
    if floor > 0 and head.get("measured") and head["headroom_pct"] < floor:
        log({**base, "level": "WARN", "verdict": "subagent_cap_refused", "cause": "usage_headroom"})
        running = f"{count['running']} running, limit {limit}" if count.get("measured") else "running count unmeasured"
        return 2, (
            f"BLOCKED by amux boundary '{b.get('id')}': the account has {head['headroom_pct']:.0f}% usage left in its "
            f"{head.get('window') or 'usage'} window, under the {floor:.0f}% floor for starting new subagents "
            f"({running}).\n"
            f"Retry after the window resets ({head.get('resets_at') or 'see `amux usage`'}), finish the work with the "
            "agents you have, or do this step yourself.\n(boundary-guard; the registry is boundaries.toml)\n"
        )
    if preempt:
        log({**base, "level": "WARN", "verdict": "subagent_cap_preempt_allowed", "cause": "priority_marker"})
        sys.stderr.write(preempt)
    unmeasured = [k for k, r, on in (("count", count, limit is not None), ("headroom", head, floor > 0))
                  if on and not r.get("measured")]
    if unmeasured:
        log({**base, "level": "INFO", "verdict": "subagent_cap_allowed_unmeasured", "unmeasured": unmeasured,
             "why_unmeasured": "; ".join(str(r.get("why_unmeasured")) for k, r in (("count", count), ("headroom", head)) if k in unmeasured)})
    return 0, ""


# ---- modal prompts for orchestrator lanes (2026-10-27, MSG-98201) ----------------
#
# On 2026-10-24/25 the orchestrator amux-helper called AskUserQuestion while the
# owner slept. The tool blocks the whole turn until a human answers, so for
# about 8 h peer messages queued unread and its wakeup chain could not fire.
# For a worker the registry marks as an orchestrator (the `orchestrators` list,
# or the scoped env named by `env`), the modal tools are refused and the reason
# carries the question back as plain text, so the worker can decide and go on.

ON_VALUES = ("1", "true", "yes", "on")
OFF_VALUES = ("0", "false", "no", "off")


def is_orchestrator(cfg: dict) -> tuple[bool, str]:
    """(orchestrator?, why). The scoped env wins over the registry list."""
    key = cfg.get("env") or ""
    raw = scoped_env_value(key) if key else None
    src = "scope"
    if raw is None and key:
        raw, src = os.environ.get(key), "env"
    val = (raw or "").strip().lower()
    if val in ON_VALUES:
        return True, f"{key}={raw} ({src})"
    if val in OFF_VALUES:
        return False, f"{key}={raw} ({src})"
    if WORKER and WORKER in (cfg.get("orchestrators") or []):
        return True, "listed in orchestrators"
    return False, "not listed"


def question_text(ti: dict) -> str:
    """AskUserQuestion's questions as numbered plain-text options."""
    out = []
    for q in (ti.get("questions") or [])[:4]:
        if not isinstance(q, dict):
            continue
        out.append(f"  Q: {str(q.get('question') or '').strip()[:300]}")
        for n, o in enumerate(q.get("options") or [], 1):
            o = o if isinstance(o, dict) else {"label": str(o)}
            desc = str(o.get("description") or "").strip()
            out.append(f"    {n}. {str(o.get('label') or '').strip()[:120]}" + (f": {desc[:200]}" if desc else ""))
    return "\n".join(out)


def check_modal(b: dict, tool: str, ti: dict) -> tuple[int, str]:
    cfg = b.get("modal") or {}
    if tool not in (cfg.get("tools") or []):
        return 0, ""
    orch, why = is_orchestrator(cfg)
    if not orch:
        return 0, ""
    qs = question_text(ti) if tool == "AskUserQuestion" else ""
    log({"level": "WARN", "verdict": cfg.get("verdict") or "modal_prompt_refused", "boundary": b.get("id"),
         "tool": tool, "orchestrator_by": why, "subject": redact(qs.replace("\n", " ")) if qs else "",
         "measured": True, "n_considered": 1})
    return 2, (
        f"BLOCKED by amux boundary '{b.get('id')}' ({b.get('title', '')}): this worker is an orchestrator ({why}), "
        f"and {tool} would block the whole turn until a human answers.\n"
        + (f"Your question:\n{qs}\n" if qs else "")
        + "Write the question in your turn output as plain text with numbered options, pick the option you "
        "recommend inside your standing authority, say which one you took and why, and keep working. "
        "If it touches money, an outside send, production data or a foreign push, leave that one step for the "
        "owner and carry on with the rest.\n(boundary-guard; the registry is boundaries.toml)\n"
    )


def main() -> None:
    if len(sys.argv) == 3 and sys.argv[1] == "--measure-disk":
        # The detached measurement disk_usage() starts. Bounded so a wedged
        # filesystem cannot leave it running; the lock is removed either way.
        try:
            measure_disk(sys.argv[2], DISK_DU_TIMEOUT_S)
        finally:
            try:
                os.unlink(disk_cache_path(sys.argv[2]) + ".measuring")
            except OSError:
                pass
        sys.exit(0)
    try:
        payload = json.load(sys.stdin)
    except Exception:
        sys.exit(0)
    # An isolated worker's launch unsets AMUX_SESSION (its routing key) and
    # exports AMUX_ISOLATED_WORKER as identity only. The guard reads that name
    # so apply_to_isolated actually covers isolated workers; before 2026-10-18
    # it exited here for every one of them.
    session = os.environ.get("AMUX_SESSION", "")
    isolated_env = os.environ.get("AMUX_ISOLATED_WORKER", "") if not session else ""
    session = session or isolated_env
    global WORKER, WORKER_ISOLATED
    WORKER, WORKER_ISOLATED = session, bool(isolated_env)
    if not session and os.environ.get("AMUX_BOUNDARIES_FORCE") != "1":
        sys.exit(0)  # not an amux worker
    tool = payload.get("tool_name") or ""
    ti = payload.get("tool_input") or {}
    command = (ti.get("command") or "") if tool == "Bash" else ""
    path = ti.get("file_path") or ti.get("notebook_path") or ti.get("path") or ""
    if tool in ("Grep", "Glob"):
        # A directory search: match the directory as a directory, plus the
        # Glob pattern or Grep glob, so ~/.amux/vault and ~/.amux/vault/* both hit.
        base = os.path.expanduser(ti.get("path") or "")
        pat = os.path.expanduser((ti.get("pattern") if tool == "Glob" else ti.get("glob")) or "")
        joined = pat if pat.startswith("/") else (base.rstrip("/") + "/" + pat if base else pat)
        path = (base.rstrip("/") + "/" if base else "") + "\n" + joined
    content = "\n".join(str(ti.get(k) or "") for k in ("content", "new_string", "new_source"))
    if tool == "MultiEdit":
        content += "\n".join(str(e.get("new_string") or "") for e in ti.get("edits") or [])
    try:
        boundaries, settings = load_registry()
    except Exception as e:
        log({"verdict": "boundary_registry_unloadable", "error": str(e)[:300], "measured": False, "n_considered": 0})
        sys.stderr.write(f"amux boundary-guard: registry did not load ({e}); boundaries are NOT enforced.\n")
        sys.exit(0)
    if session and not settings.get("apply_to_isolated", True) and (isolated_env or is_isolated(session)):
        sys.exit(0)  # the owner exempted isolated workers in the registry
    for b in boundaries:
        for i, rule in enumerate(b.get("deny") or []):
            try:
                hit = rule_applies(rule, tool, command, path, content)
            except re.error as e:
                log({"verdict": "boundary_rule_invalid", "boundary": b.get("id"), "rule": i, "error": str(e)})
                continue
            if hit and rule.get("requires_receipt"):
                # The whole-command match is only the cheap prefilter; the
                # rule is decided per segment against the receipts.
                try:
                    code, msg = check_receipts(b, i, rule, command)
                except re.error as e:
                    log({"verdict": "boundary_rule_invalid", "boundary": b.get("id"), "rule": i, "error": str(e)})
                    continue
                except Exception as e:  # noqa: BLE001 - this rule fails closed
                    log({"level": "WARN", "verdict": "receipt_check_failed", "boundary": b.get("id"), "rule": i,
                         "subject": redact(command), "error": str(e)[:300], "measured": False, "n_considered": 1})
                    code, msg = 2, (f"BLOCKED by amux boundary '{b.get('id')}': the receipt check failed ({e}), and "
                                    "this rule fails closed.\n" + b.get("rule", "").strip() + "\n(boundary-guard)\n")
                if code:
                    sys.stderr.write(msg)
                    sys.exit(code)
                continue
            if hit:
                log({"verdict": "boundary_refused", "boundary": b.get("id"), "rule": i, "tool": tool,
                     "subject": redact(command or path), "measured": True, "n_considered": 1})
                sys.stderr.write(
                    f"BLOCKED by amux boundary '{b.get('id')}' ({b.get('title', '')}).\n"
                    f"{rule.get('reason', '').strip()}\n\n{b.get('rule', '').strip()}\n"
                    "(boundary-guard; the registry is boundaries.toml, GET $AMUX_URL/api/boundaries)\n"
                )
                sys.exit(2)
        if b.get("kind") == "resources" and tool == "Bash" and command:
            code, msg = check_resources(b, command, session)
            if code:
                sys.stderr.write(msg)
                sys.exit(code)
        if b.get("kind") == "push_gates" and tool == "Bash" and command:
            try:
                code, msg = check_push_gates(b, command, str(payload.get("cwd") or ""))
            except Exception as e:  # noqa: BLE001 - this check fails closed
                log({"level": "WARN", "verdict": "push_gate_check_failed", "boundary": b.get("id"),
                     "subject": redact(command), "error": str(e)[:300], "measured": False, "n_considered": 1})
                code, msg = 2, (f"BLOCKED by amux boundary '{b.get('id')}': the push gate check failed ({e}), and "
                                "this check fails closed.\n" + b.get("rule", "").strip() + "\n(boundary-guard)\n")
            if code:
                sys.stderr.write(msg)
                sys.exit(code)
        if b.get("kind") == "delete_gate" and tool == "Bash" and command:
            try:
                code, msg = check_delete_gate(b, command)
            except Exception as e:  # noqa: BLE001 - this check fails closed
                log({"level": "WARN", "verdict": (b.get("delete_gate") or {}).get("verdict_refused") or "delete_gate_refused",
                     "boundary": b.get("id"), "subject": redact(command), "targets": [{"cause": "check_failed"}],
                     "error": str(e)[:300], "measured": False, "n_considered": 1})
                code, msg = 2, (f"BLOCKED by amux boundary '{b.get('id')}': the delete gate check failed ({e}), and "
                                "this check fails closed.\n" + b.get("rule", "").strip() + "\n(boundary-guard)\n")
            if code:
                sys.stderr.write(msg)
                sys.exit(code)
        if b.get("kind") == "modal":
            code, msg = check_modal(b, tool, ti)
            if code:
                sys.stderr.write(msg)
                sys.exit(code)
        if b.get("kind") == "subagents":
            code, msg = check_subagents(b, tool, str(payload.get("transcript_path") or ""), ti)
            if code:
                sys.stderr.write(msg)
                sys.exit(code)
    sys.exit(0)


if __name__ == "__main__":
    main()
