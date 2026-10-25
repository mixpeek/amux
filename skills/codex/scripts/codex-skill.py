#!/usr/bin/env python3
"""codex-skill: read-only introspection of Codex threads (Desktop app, CLI, exec, App Server clients, cloud).

  codex-skill capabilities                    which interfaces answer on this machine, with probes
  codex-skill list [--active] [--project P] [--status S] [--source S] [--keyword K] [--days N]
  codex-skill read <thread> [--since CKPT] [--limit N] [--types a,b] [--full]
  codex-skill status <thread>
  codex-skill events <thread> [--since CKPT]
  codex-skill search "<text>" [--days N] [--project P]
  codex-skill watch <thread>... | --active [--interval S] [--duration S] [--until-idle]

Every command prints one JSON object (watch prints one JSON object per line). <thread> is a thread
UUID, an exact thread name, or a fragment that matches one name (same matching as codex-send.py).

Access order, all read-only:
  1. `codex app-server` over stdio (the JSON-RPC server behind the Desktop app and the TUI), with a
     method allowlist: initialize, thread/list, thread/read, thread/turns/list, thread/items/list,
     thread/loaded/list. Nothing that starts, resumes, steers, archives or names a thread can be sent.
  2. `codex cloud list --json` for cloud tasks.
  3. Local artifacts, opened read-only: ~/.codex/state_5.sqlite (git branch/origin, which the API
     leaves null), thread_history_1.sqlite (turn status projection), queue_1.sqlite (queued inputs),
     and the rollout file's mtime plus which process holds it open (lsof).

Why status is not taken from the API alone: a thread is owned by the process that loaded it (the
Desktop app runs its own app-server over a private stdio pipe). A second app-server sees that thread
as `notLoaded` and reports its in-progress turn as `interrupted` with completedAt null (measured
2026-10-23 on a turn that was writing every few seconds). So an unfinished turn is classified from
observed facts: no terminal record, rollout last-write age, and the live process holding the file.
`status.basis` says whether the state was observed or inferred, and `status.evidence` carries the facts.

Checkpoints are rollout ordinals (an integer). `read`/`events` return `checkpoint`; pass it back as
--since to get only items completed after it. Items are persisted only when they complete, so an
exclusive ordinal cursor cannot skip or repeat one.

Secrets: auth.json and config.toml are never opened. Every string in the output passes a redactor
(API keys, tokens, JWTs, private keys, password/secret assignments); `redactions` counts hits.
"""
import argparse, base64, datetime as dt, json, os, re, select, shutil, sqlite3, subprocess, sys, time
from pathlib import Path

VERSION = "1.0"
CODEX = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))
UUID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.I)
READ_ONLY_METHODS = {"initialize", "thread/list", "thread/read", "thread/turns/list",
                     "thread/items/list", "thread/loaded/list"}
ALL_SOURCE_KINDS = ["cli", "vscode", "exec", "appServer", "subAgent", "subAgentReview", "subAgentCompact",
                    "subAgentThreadSpawn", "subAgentOther", "unknown"]
ACTIVE_WINDOW_S = 180
FRESH_ROLLOUT_S = 3600         # rollouts this recent are checked for threads the state DB lacks yet          # an open turn whose rollout was written this recently is "running"
DEFAULT_MAX_CHARS = 4000
LSOF = next((p for p in ("/usr/sbin/lsof", "/usr/bin/lsof", shutil.which("lsof") or "") if p and os.access(p, os.X_OK)), None)
DROP_KEYS = {"appContext", "mcpAppUi", "encrypted_content", "text_elements", "clientId"}

# ---------------------------------------------------------------------------------------- output

SECRET_PATTERNS = [
    re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", re.S),
    re.compile(r"\b(?:sk|rk|pk)-(?:proj-|live-|test-|ant-)?[A-Za-z0-9_\-]{20,}"),
    re.compile(r"\bgh[pousr]_[A-Za-z0-9]{30,}\b"),
    re.compile(r"\bgithub_pat_[A-Za-z0-9_]{40,}\b"),
    re.compile(r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"),
    re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    re.compile(r"\bAIza[0-9A-Za-z_\-]{35}\b"),
    re.compile(r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}"),
    re.compile(r"(?i)\b(bearer)\s+[A-Za-z0-9._\-]{20,}"),
    re.compile(r"(?i)\b([A-Z0-9_]*(?:PASSWORD|PASSWD|SECRET|API_KEY|APIKEY|ACCESS_KEY|AUTH_TOKEN|TOKEN)[A-Z0-9_]*)"
               r"(\s*[=:]\s*)(['\"]?)[^\s'\"]{6,}"),
]


class Redactor:
    def __init__(self):
        self.hits = 0

    def text(self, s):
        for i, pat in enumerate(SECRET_PATTERNS):
            if i == len(SECRET_PATTERNS) - 1:
                s, n = pat.subn(lambda m: f"{m.group(1)}{m.group(2)}{m.group(3)}[REDACTED]", s)
            elif "bearer" in pat.pattern:
                s, n = pat.subn(lambda m: f"{m.group(1)} [REDACTED]", s)
            else:
                s, n = pat.subn("[REDACTED]", s)
            self.hits += n
        return s


RED = Redactor()


def clip(v, max_chars):
    """Redact every string; cut strings longer than max_chars and say so beside the cut."""
    if isinstance(v, str):
        v = RED.text(v)
        if max_chars and len(v) > max_chars:
            return v[:max_chars] + f"\n[... truncated, {len(v)} chars total]"
        return v
    if isinstance(v, list):
        return [clip(x, max_chars) for x in v]
    if isinstance(v, dict):
        return {k: clip(x, max_chars) for k, x in v.items() if k not in DROP_KEYS}
    return v


def iso(ts):
    if ts is None:
        return None
    if ts > 10**11:
        ts = ts / 1000
    return dt.datetime.fromtimestamp(ts, dt.timezone.utc).isoformat().replace("+00:00", "Z")


def now_iso():
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def emit(obj, code=0):
    obj.setdefault("redactions", RED.hits)
    print(json.dumps(obj, ensure_ascii=False))
    sys.stdout.flush()
    return code


def envelope(command, **kw):
    return {"tool": "codex-skill", "version": VERSION, "command": command, "generated_at": now_iso(), **kw}


class Fail(Exception):
    def __init__(self, code, message, exit_code=1, **extra):
        super().__init__(message)
        self.code, self.message, self.exit_code, self.extra = code, message, exit_code, extra

# ------------------------------------------------------------------------------------ app server


def codex_bin():
    """The first codex binary that runs. The Codex app and its daemon ship working binaries even
    when an npm install on PATH is broken."""
    cands = [os.environ.get("CODEX_BIN", ""),
             str(CODEX / "packages/app-server-daemon/current/bin/codex"),
             "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
             "/Applications/Codex.app/Contents/Resources/codex",
             shutil.which("codex") or "", "/opt/homebrew/bin/codex", "/usr/local/bin/codex"]
    for c in dict.fromkeys(c for c in cands if c):
        if os.access(c, os.X_OK):
            try:
                r = subprocess.run([c, "--version"], capture_output=True, text=True, timeout=20)
                if r.returncode == 0:
                    return c, r.stdout.strip()
            except (OSError, subprocess.TimeoutExpired):
                pass
    return None, None


class AppServer:
    """A private `codex app-server` on stdio. Only READ_ONLY_METHODS can be sent."""

    def __init__(self, exe, timeout=30):
        self.timeout = timeout
        self.p = subprocess.Popen([exe, "app-server", "--listen", "stdio://"], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
                                  cwd=str(Path.home()), start_new_session=True)
        self.n = 0
        self.info = self.call("initialize", {"clientInfo": {"name": "codex-skill", "version": VERSION}})
        self.p.stdin.write(json.dumps({"method": "initialized"}) + "\n")
        self.p.stdin.flush()

    def call(self, method, params):
        if method not in READ_ONLY_METHODS:
            raise Fail("method_not_allowed", f"{method} is not in the read-only allowlist")
        self.n += 1
        self.p.stdin.write(json.dumps({"id": self.n, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        end = time.time() + self.timeout
        while time.time() < end:
            r, _, _ = select.select([self.p.stdout], [], [], max(0.1, end - time.time()))
            if not r:
                continue
            line = self.p.stdout.readline()
            if not line:
                raise Fail("app_server_closed", f"app-server exited during {method}")
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if msg.get("id") == self.n:
                if "error" in msg:
                    raise Fail("app_server_error", f"{method}: {msg['error'].get('message', msg['error'])}",
                               rpc_error=msg["error"])
                return msg.get("result") or {}
        raise Fail("app_server_timeout", f"{method}: no answer in {self.timeout}s")

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()


_APP = None


def app():
    global _APP
    if _APP is None:
        exe, _ = codex_bin()
        if not exe:
            raise Fail("codex_not_found", "no runnable codex binary (checked the app bundle, the daemon package and PATH)")
        _APP = AppServer(exe)
    return _APP

# ------------------------------------------------------------------------------- local artifacts


def ro_db(name):
    path = CODEX / name
    if not path.exists():
        return None
    try:
        return sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5)
    except sqlite3.Error:
        return None


def state_rows(ids):
    """{id: {git_branch, git_origin_url, git_sha, archived, name}} from state_5.sqlite."""
    con = ro_db("state_5.sqlite")
    if not con or not ids:
        return {}
    out = {}
    try:
        q = ",".join("?" * len(ids))
        for r in con.execute(f"select id, git_branch, git_origin_url, git_sha, archived, name from threads where id in ({q})", ids):
            out[r[0]] = {"git_branch": r[1], "git_origin_url": r[2], "git_sha": r[3], "archived": bool(r[4]), "name": r[5]}
    except sqlite3.Error:
        pass
    return out


def queued_counts(ids):
    con = ro_db("queue_1.sqlite")
    if not con or not ids:
        return {}
    try:
        q = ",".join("?" * len(ids))
        return dict(con.execute(f"select thread_id, count(*) from queued_items where thread_id in ({q}) group by 1", ids).fetchall())
    except sqlite3.Error:
        return {}


def projected_turn(tid):
    """Latest turn row from the history projection (only threads that have been projected)."""
    con = ro_db("thread_history_1.sqlite")
    if not con:
        return None
    try:
        r = con.execute("select turn_id, status, started_at, completed_at, error_json from thread_turns "
                        "where thread_id=? order by rollout_ordinal desc limit 1", (tid,)).fetchone()
    except sqlite3.Error:
        return None
    return r and {"turn_id": r[0], "status": r[1], "started_at": iso(r[2]), "completed_at": iso(r[3]), "error": r[4]}


def file_holders(path):
    """[{pid, command}] of processes holding the rollout open, or None when lsof is unavailable."""
    if not LSOF or not path or not os.path.exists(path):
        return None
    try:
        r = subprocess.run([LSOF, "-Fpc", "--", path], capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.TimeoutExpired):
        return None
    out, cur = [], {}
    for line in r.stdout.splitlines():
        if line.startswith("p"):
            cur = {"pid": int(line[1:])}
            out.append(cur)
        elif line.startswith("c"):
            cur["command"] = line[1:]
    return out

# ------------------------------------------------------------------------------------ threads


def surface(t):
    """Where a thread came from. `originator` is written by the client that created the thread."""
    orig = t.get("originator") or ""
    src = t.get("source")
    if t.get("parentThreadId") or (isinstance(src, dict) and "subagent" in json.dumps(src).lower()):
        return "subagent"
    if orig == "Codex Desktop":
        return "desktop"
    if orig == "codex-tui":
        return "cli"
    if orig == "codex_exec" or src == "exec":
        return "exec"
    if orig:
        return "app_server_client"
    if src == "vscode":
        return "desktop_or_ide"
    if src == "cli":
        return "cli"
    return "unknown"


def repo_name(url):
    if not url:
        return None
    m = re.search(r"[:/]([^/:]+/[^/]+?)(?:\.git)?/?$", url)
    return m.group(1) if m else url


_CREATED = {}


def thread_meta(t, st=None):
    st = st or {}
    _CREATED[t["id"]] = t.get("createdAt")
    gi = t.get("gitInfo") or {}
    origin = gi.get("originUrl") or st.get("git_origin_url")
    return {
        "id": t["id"],
        "name": t.get("name") or st.get("name"),
        "title_preview": clip((t.get("preview") or "")[:200], 200),
        "source": surface(t),
        "originator": t.get("originator"),
        "cwd": t.get("cwd"),
        "repository": repo_name(origin),
        "git_origin_url": clip(origin, 300) if origin else None,
        "branch": gi.get("branch") or st.get("git_branch"),
        "git_sha": gi.get("sha") or st.get("git_sha"),
        "model": t.get("model"),
        "created_at": iso(t.get("createdAt")),
        "updated_at": iso(t.get("updatedAt")),
        "archived": st.get("archived", False),
        "section": (t.get("section") or {}).get("name"),
        "parent_thread_id": t.get("parentThreadId"),
        "forked_from_id": t.get("forkedFromId"),
        "rollout_path": real_path(t.get("path")),
        "api_status": (t.get("status") or {}).get("type"),
    }


def thread_status(tid, rollout_path, queued=0, loaded_ids=()):
    """Classify a thread from its last turn plus observed facts. Never invents a state: when the facts
    cannot separate two states it says which two."""
    turns = list_turns(tid, rollout_path, 1)
    last = turns[0] if turns else None
    ev = {"history_source": history_source(tid, rollout_path), "queued_inputs": queued,
          "loaded_in_this_app_server": tid in loaded_ids}
    if rollout_path and os.path.exists(rollout_path):
        age = time.time() - os.path.getmtime(rollout_path)
        ev["rollout_last_write_age_s"] = round(age, 1)
        ev["rollout_compressed"] = rollout_path.endswith(".zst")
    else:
        age = None
        ev["rollout_last_write_age_s"] = None
    if not last:
        created = _CREATED.get(tid)
        if (age is not None and age <= ACTIVE_WINDOW_S) or (created and time.time() - created <= ACTIVE_WINDOW_S):
            return {"state": "starting", "basis": "inferred", "evidence": ev, "last_turn": None,
                    "detail": "no turn recorded yet, but the thread was created or written in the last "
                              f"{ACTIVE_WINDOW_S}s"}
        if ev["history_source"] == "none":
            return {"state": "unknown", "basis": "unavailable", "evidence": ev, "last_turn": None,
                    "detail": "no history: the app-server has no turns for it and no rollout file exists"}
        return {"state": "new", "basis": "observed", "detail": "thread has no turns", "evidence": ev, "last_turn": None}
    lt = {"id": last["id"], "api_status": last.get("status"), "started_at": iso(last.get("startedAt")),
          "completed_at": iso(last.get("completedAt")), "duration_ms": last.get("durationMs"),
          "error": clip(last.get("error"), 1000)}
    if last.get("completedAt") is not None or last.get("status") == "completed":
        state = {"completed": "idle", "interrupted": "interrupted", "failed": "failed"}.get(last.get("status"), last.get("status"))
        if last.get("error") and state == "idle":
            state = "failed"
        return {"state": state, "basis": "observed", "detail": f"last turn ended {last.get('status')}",
                "evidence": ev, "last_turn": lt}
    # No terminal record for the last turn.
    proj = projected_turn(tid)
    if proj and proj["turn_id"] == last["id"]:
        ev["history_projection_status"] = proj["status"]
    holders = file_holders(rollout_path)
    ev["rollout_held_open_by"] = holders
    if age is not None and age <= ACTIVE_WINDOW_S and not ev.get("rollout_compressed"):
        return {"state": "running", "basis": "inferred",
                "detail": f"last turn has no end record and the rollout was written {round(age)}s ago",
                "evidence": ev, "last_turn": lt}
    if holders:
        return {"state": "waiting", "basis": "inferred",
                "detail": "last turn has no end record; a live process holds the rollout open but has not written "
                          f"for {round(age or 0)}s: waiting on an approval or input, or a long silent command. "
                          "Pending approvals are visible only to the owning client.",
                "evidence": ev, "last_turn": lt}
    if holders is None:
        return {"state": "open_turn_quiet", "basis": "inferred",
                "detail": "last turn has no end record and no recent writes; lsof unavailable, so waiting and "
                          "abandoned cannot be told apart", "evidence": ev, "last_turn": lt}
    return {"state": "abandoned", "basis": "inferred",
            "detail": "last turn has no end record, no process holds the rollout and it has not been written for "
                      f"{round(age or 0)}s: the process that ran it exited mid-turn", "evidence": ev, "last_turn": lt}


ACTIVE_STATES = {"starting", "running", "waiting", "open_turn_quiet"}


def load_names():
    con = ro_db("state_5.sqlite")
    if not con:
        return []
    try:
        return con.execute("select id, name from threads where name is not null and trim(name)!='' order by recency_at desc").fetchall()
    except sqlite3.Error:
        return []


def norm(s):
    s = s.strip().lower().lstrip("#")
    return " ".join(re.sub(r"[^\w\s]", " ", s).split())


def resolve(query):
    q = query.strip()
    if UUID.match(q):
        return q
    rows = load_names()
    n = norm(q)
    exact = [r for r in rows if norm(r[1]) == n]
    toks = n.split()
    cands = exact or [r for r in rows if all(t in norm(r[1]) for t in toks)]
    if len(cands) == 1:
        return cands[0][0]
    raise Fail("thread_ambiguous" if cands else "thread_not_found",
               f"'{query}' matches {len(cands)} named threads", exit_code=2,
               candidates=[{"id": r[0], "name": r[1]} for r in cands[:20]])


def read_thread(tid):
    try:
        return app().call("thread/read", {"threadId": tid, "includeTurns": False})["thread"]
    except Fail as e:
        if e.code == "app_server_error":
            raise Fail("thread_not_found", f"app-server cannot read thread {tid}: {e.message}", exit_code=2)
        raise


def iter_threads(archived=False, keyword=None, days=None, max_threads=2000):
    a = app()
    cutoff = time.time() - days * 86400 if days else None
    cursor, seen = None, 0
    while True:
        # useStateDbOnly: without it thread/list scans rollouts and REPAIRS state-DB metadata (a write).
        # sourceKinds/modelProviders: the defaults hide subagent, exec and non-default-provider threads.
        params = {"limit": 100, "archived": archived, "sortKey": "updated_at", "sortDirection": "desc",
                  "useStateDbOnly": True, "sourceKinds": ALL_SOURCE_KINDS, "modelProviders": []}
        if keyword:
            params["searchTerm"] = keyword
        if cursor:
            params["cursor"] = cursor
        res = a.call("thread/list", params)
        for t in res.get("data") or []:
            if cutoff and (t.get("updatedAt") or 0) < cutoff:
                return
            yield t
            seen += 1
            if seen >= max_threads:
                return
        cursor = res.get("nextCursor")
        if not cursor or not res.get("data"):
            return

# --------------------------------------------------------------------------------------- items


def norm_item(entry, max_chars, types=None):
    it = entry["item"]
    kind = it.get("type")
    if types and kind not in types:
        return None
    rec = {"kind": kind, "id": it.get("id"), "turn_id": entry.get("turnId"),
           "started_at": iso(entry.get("startedAtMs")), "completed_at": iso(entry.get("completedAtMs"))}
    if kind == "userMessage":
        rec["role"] = "user"
        rec["text"] = clip("\n".join(c.get("text", "") for c in it.get("content") or [] if c.get("type") == "text"), max_chars)
        other = [c.get("type") for c in it.get("content") or [] if c.get("type") != "text"]
        if other:
            rec["attachments"] = other
    elif kind == "agentMessage":
        rec["role"] = "assistant"
        rec["text"] = clip(it.get("text") or "", max_chars)
        rec["phase"] = it.get("phase")
    elif kind == "reasoning":
        rec["role"] = "assistant"
        rec["summary"] = clip(it.get("summary") or [], max_chars)
    elif kind == "commandExecution":
        rec["role"] = "tool"
        rec.update(command=clip(it.get("command"), max_chars), cwd=it.get("cwd"), status=it.get("status"),
                   exit_code=it.get("exitCode"), duration_ms=it.get("durationMs"),
                   output=clip(it.get("aggregatedOutput") or "", max_chars))
    elif kind == "mcpToolCall":
        rec["role"] = "tool"
        rec.update(server=it.get("server"), tool=it.get("tool"), status=it.get("status"),
                   arguments=clip(it.get("arguments"), max_chars), result=clip(it.get("result"), max_chars),
                   error=clip(it.get("error"), max_chars), duration_ms=it.get("durationMs"))
    else:
        rec["role"] = "tool" if kind not in ("contextCompaction",) else "system"
        rec["detail"] = clip({k: v for k, v in it.items() if k not in ("type", "id")}, max_chars)
    return rec


# Rollout files. The app-server pages history from its projection (thread_history_1.sqlite), which
# exists only for threads some app-server has projected: on 2026-10-23 that was 1 of 442. For every
# other thread thread/turns/list and thread/items/list answer an empty page while the rollout holds
# the whole record. So an empty API page on a thread with a rollout is read from the rollout, and the
# output says which one answered (`history_source`). Both use the same ordinal: the line number of
# the item_completed record, which is what the API cursor's rolloutOrdinal points at.

def snake_to_camel(k):
    head, *rest = k.split("_")
    return head + "".join(w[:1].upper() + w[1:] for w in rest)


def core_to_api(v):
    """A rollout (core) item into the app-server shape: PascalCase type to camelCase, snake keys to camel."""
    if isinstance(v, list):
        return [core_to_api(x) for x in v]
    if not isinstance(v, dict):
        return v
    out = {}
    for k, x in v.items():
        if k == "type" and isinstance(x, str) and x[:1].isupper():
            out[k] = x[:1].lower() + x[1:]
        elif k in ("cwd", "path") and isinstance(x, str) and x.startswith("file://"):
            from urllib.parse import unquote
            out[k] = unquote(x[len("file://"):])
        elif k == "duration" and isinstance(x, dict) and "secs" in x:
            out["durationMs"] = int(x.get("secs", 0) * 1000 + x.get("nanos", 0) / 1e6)
        else:
            out[snake_to_camel(k)] = core_to_api(x)
    if out.get("type") == "agentMessage" and "text" not in out:
        out["text"] = "".join(c.get("text", "") for c in out.get("content") or [] if isinstance(c, dict))
    if out.get("type") == "commandExecution" and isinstance(out.get("command"), list):
        import shlex
        out["command"] = shlex.join(out["command"])
    if out.get("type") == "reasoning" and "summary" not in out:
        out["summary"] = out.pop("summaryText", [])
    return out


def rollout_lines(path):
    if not path or not os.path.exists(path):
        return
    if path.endswith(".zst"):
        z = shutil.which("zstd") or next((p for p in ("/usr/local/bin/zstd", "/opt/homebrew/bin/zstd") if os.access(p, os.X_OK)), None)
        if not z:
            raise Fail("zstd_missing", f"{path} is zstd-compressed and no zstd binary was found")
        proc = subprocess.Popen([z, "-dc", path], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        src = proc.stdout
    else:
        src = open(path, encoding="utf-8", errors="replace")
    with src:
        for line in src:
            try:
                yield json.loads(line)
            except ValueError:
                continue


def rollout_items(path, since=None, limit=200, turn_id=None):
    """(entries in API shape, checkpoint, has_more) from item_completed records."""
    out, ck, more = [], since, False
    for d in rollout_lines(path):
        p = d.get("payload") or {}
        if d.get("type") != "event_msg" or p.get("type") != "item_completed":
            continue
        o = d.get("ordinal")
        if since is not None and (o is None or o <= since):
            continue
        if turn_id and p.get("turn_id") != turn_id:
            continue
        if len(out) >= limit:
            more = True
            break
        out.append({"turnId": p.get("turn_id"), "startedAtMs": p.get("started_at_ms"),
                    "completedAtMs": p.get("completed_at_ms"), "item": core_to_api(p.get("item") or {})})
        ck = o
    return out, ck, more


def rollout_turns(path):
    """Turns in API shape, oldest first, from task_started / task_complete / turn_aborted / error."""
    turns, order = {}, []
    for d in rollout_lines(path):
        p = d.get("payload") or {}
        if d.get("type") != "event_msg":
            continue
        t, tid = p.get("type"), p.get("turn_id")
        if t == "task_started" and tid:
            if tid not in turns:
                order.append(tid)
            turns[tid] = {"id": tid, "status": "inProgress", "startedAt": p.get("started_at"), "completedAt": None,
                          "durationMs": None, "error": None}
        elif t in ("task_complete", "turn_aborted") and tid in turns:
            turns[tid].update(status="completed" if t == "task_complete" else "interrupted",
                              completedAt=p.get("completed_at") or (d.get("timestamp") and
                                                                    dt.datetime.fromisoformat(d["timestamp"].replace("Z", "+00:00")).timestamp()),
                              durationMs=p.get("duration_ms"))
            if t == "turn_aborted":
                turns[tid]["error"] = {"reason": p.get("reason")}
        elif t == "error" and order:
            turns[order[-1]]["error"] = {"message": p.get("message")}
            turns[order[-1]]["status"] = "failed"
    return [turns[x] for x in order]


_SRC = {}


def real_path(path):
    """The rollout as it exists on disk: the API reports the .jsonl name after Codex compressed it to .jsonl.zst."""
    if path and not os.path.exists(path) and os.path.exists(path + ".zst"):
        return path + ".zst"
    return path


def history_source(tid, path=None):
    """'app_server' when the API has this thread's history, 'rollout' when only the file does, else 'none'."""
    if tid not in _SRC:
        res = app().call("thread/turns/list", {"threadId": tid, "limit": 1, "itemsView": "notLoaded"})
        if res.get("data"):
            _SRC[tid] = "app_server"
        else:
            if path is None:
                path = read_thread(tid).get("path")
            path = real_path(path)
            _SRC[tid] = "rollout" if path and os.path.exists(path) else "none"
        _SRC[tid + ":path"] = path
    src = _SRC[tid]
    if src == "none":          # a brand-new thread may gain history on the next poll
        _SRC.pop(tid)
    return src


def list_turns(tid, path, limit=1, newest_first=True):
    src = history_source(tid, path)
    if src == "rollout":
        ts = rollout_turns(_SRC.get(tid + ":path") or path)
        ts = ts[::-1] if newest_first else ts
        return ts[:limit]
    if src == "none":
        return []
    out, cur = [], None
    while len(out) < limit:
        params = {"threadId": tid, "limit": min(100, limit - len(out)), "itemsView": "notLoaded",
                  "sortDirection": "desc" if newest_first else "asc"}
        if cur:
            params["cursor"] = cur
        res = app().call("thread/turns/list", params)
        out += res.get("data") or []
        cur = res.get("nextCursor")
        if not cur or not res.get("data"):
            break
    return out


def ckpt_of(cursor):
    try:
        return int(json.loads(cursor)["rolloutOrdinal"])
    except (TypeError, ValueError, KeyError):
        return None


def cursor_for(tid, ordinal):
    return json.dumps({"requestedThreadId": tid, "rolloutOrdinal": int(ordinal), "includeAnchor": False,
                       "scope": {"kind": "itemsByCreatedAtOrdinal"}})


def last_ordinal(tid, last_item_id):
    """Ordinal of `last_item_id`, found from the newest end (the API returns no cursor after the last page)."""
    a = app()
    cur = None
    for _ in range(200):
        params = {"threadId": tid, "limit": 1, "sortDirection": "desc"}
        if cur:
            params["cursor"] = cur
        res = a.call("thread/items/list", params)
        data = res.get("data") or []
        if not data:
            return None
        cur = res.get("nextCursor")
        if data[0]["item"].get("id") == last_item_id:
            return ckpt_of(cur)
        if not cur:
            return None
    return None


def first_ordinal(tid):
    res = app().call("thread/items/list", {"threadId": tid, "limit": 1})
    o = ckpt_of(res.get("nextCursor")) if res.get("data") else None
    return o if o is not None else 1 << 62


def fetch_items(tid, since=None, limit=200, turn_id=None, path=None):
    """Entries after checkpoint `since`, oldest first, at most `limit`. Returns (entries, checkpoint, has_more)."""
    src = history_source(tid, path)
    if src == "rollout":
        return rollout_items(_SRC[tid + ":path"], since, limit, turn_id)
    if src == "none":
        return [], since, False
    a = app()
    if since is not None and since < first_ordinal(tid):
        since_eff = None      # the API rejects a cursor before the first item ("outside thread lineage")
    else:
        since_eff = since
    out, cur = [], (cursor_for(tid, since_eff) if since_eff is not None else None)
    checkpoint, has_more = since, False
    while len(out) < limit:
        params = {"threadId": tid, "limit": min(500, limit - len(out))}
        if cur:
            params["cursor"] = cur
        if turn_id:
            params["turnId"] = turn_id
        res = a.call("thread/items/list", params)
        data = res.get("data") or []
        out += data
        nxt = res.get("nextCursor")
        if nxt and data:
            checkpoint, cur = ckpt_of(nxt), nxt
            if len(out) >= limit:
                has_more = True
            continue
        if data:
            checkpoint = last_ordinal(tid, data[-1]["item"].get("id")) if not turn_id else checkpoint
        break
    return out, checkpoint, has_more

# ------------------------------------------------------------------------------------ commands


def cmd_capabilities(a):
    caps = {}
    exe, ver = codex_bin()
    caps["codex_cli"] = {"available": bool(exe), "path": exe, "version": ver}
    try:
        srv = app()
        res = srv.call("thread/list", {"limit": 1, "useStateDbOnly": True})
        loaded = srv.call("thread/loaded/list", {})
        caps["app_server"] = {"available": True, "transport": "private stdio process", "codex_home": srv.info.get("codexHome"),
                              "user_agent": srv.info.get("userAgent"), "thread_list": True, "thread_read": True,
                              "turns_and_items_paginated": True, "sample_thread_count": len(res.get("data") or []),
                              "threads_loaded_in_this_process": len(loaded.get("data") or []),
                              "event_subscription": "only for threads this process loads; loading another client's "
                                                    "thread means thread/resume, which this tool never sends. Live "
                                                    "monitoring is incremental polling (watch)."}
    except Fail as e:
        caps["app_server"] = {"available": False, "error": e.code, "message": e.message}
    desktop_app = next((p for p in ("/Applications/ChatGPT.app", "/Applications/Codex.app") if os.path.exists(p)), None)
    ps = subprocess.run(["/bin/ps", "-axo", "pid=,args="], capture_output=True, text=True).stdout
    desktop_srv = [ln.strip().split(None, 1)[0] for ln in ps.splitlines() if "app-server" in ln and "Contents/Resources/codex-cli" in ln]
    caps["desktop_app"] = {"installed": bool(desktop_app), "path": desktop_app, "running_app_server_pids": desktop_srv,
                           "threads_visible": "yes, through thread/list (originator 'Codex Desktop')",
                           "live_status": "inferred (its app-server is a private pipe; see status.basis)",
                           "pending_approvals": "not observable: approval requests go only to the owning client"}
    sock = CODEX / "app-server-control" / "app-server-control.sock"
    caps["app_server_daemon"] = {"control_socket": str(sock), "running": sock.exists(),
                                 "note": "when running, `codex app-server proxy` reaches it; this tool does not use it yet"}
    stores = {}
    for name in ("state_5.sqlite", "thread_history_1.sqlite", "queue_1.sqlite"):
        con = ro_db(name)
        stores[name] = {"readable": bool(con)}
        if con and name == "state_5.sqlite":
            try:
                stores[name]["threads"] = con.execute("select count(*) from threads").fetchone()[0]
            except sqlite3.Error as e:
                stores[name] = {"readable": False, "error": str(e)}
    sess = CODEX / "sessions"
    files = list(sess.rglob("rollout-*")) if sess.exists() else []
    stores["sessions_dir"] = {"path": str(sess), "rollouts_jsonl": sum(1 for f in files if f.suffix == ".jsonl"),
                              "rollouts_zst": sum(1 for f in files if f.suffix == ".zst")}
    stores["never_opened"] = ["auth.json", "config.toml"]
    caps["local_artifacts"] = stores
    caps["lsof"] = {"available": bool(LSOF), "path": LSOF, "used_for": "which process holds a rollout open"}
    if a.no_cloud:
        caps["cloud"] = {"probed": False, "why": "--no-cloud"}
    elif exe:
        caps["cloud"] = cloud_list(exe, 5)
    return emit(envelope("capabilities", ok=True, measured=True, capabilities=caps))


def cloud_list(exe, limit):
    import tempfile
    # `codex cloud` writes error.log into its cwd (seen 2026-10-23), so give it a throwaway one.
    with tempfile.TemporaryDirectory(prefix="codex-skill-cloud-") as scratch:
        try:
            r = subprocess.run([exe, "cloud", "list", "--json", "--limit", str(min(20, max(1, limit)))],
                               capture_output=True, text=True, timeout=45, cwd=scratch)
        except subprocess.TimeoutExpired:
            return {"probed": True, "available": False, "error": "timeout after 45s"}
    if r.returncode != 0:
        return {"probed": True, "available": False, "error": clip(r.stderr.strip()[-500:], 500)}
    try:
        d = json.loads(r.stdout)
    except ValueError:
        return {"probed": True, "available": False, "error": "non-JSON output"}
    tasks = d.get("tasks") or []
    return {"probed": True, "available": True, "task_count": len(tasks), "cursor": d.get("cursor"),
            "tasks": clip(tasks, 500), "read": "codex cloud status/diff <task> (text only; no JSON history API)"}


def matches_project(meta, proj):
    p = proj.lower()
    return any(p in (meta.get(k) or "").lower() for k in ("cwd", "name", "repository", "git_origin_url", "title_preview"))


def hidden_threads(seen, days, archived, cap):
    """Threads thread/list never returns: it drops threads with no user message even with every
    sourceKind (measured 2026-10-23: 85 of 443, mostly subagents, 1.2B tokens between them). Taken
    from state_5.sqlite and read through thread/read so they carry the same fields."""
    con = ro_db("state_5.sqlite")
    if not con or cap <= 0:
        return []
    q = "select id from threads where archived = ?"
    args = [1 if archived else 0]
    if days:
        q += " and updated_at >= ?"
        args.append(int(time.time() - days * 86400))
    try:
        ids = [r[0] for r in con.execute(q + " order by updated_at desc", args) if r[0] not in seen][:cap]
    except sqlite3.Error:
        return []
    out = []
    for tid in ids:
        try:
            t = read_thread(tid)
        except Fail:
            continue
        t["_listed_by"] = "state_db"
        out.append(t)
    # A thread started seconds ago can have a rollout before its state-DB row (useStateDbOnly skips
    # the scan that would add it). Pick those up from recently written rollouts.
    seen = seen | set(ids)
    for tid in fresh_rollout_ids(FRESH_ROLLOUT_S):
        if tid in seen or archived or len(out) >= cap:
            continue
        try:
            t = read_thread(tid)
        except Fail:
            continue
        t["_listed_by"] = "rollout_scan"
        out.append(t)
    return out


ROLLOUT_ID = re.compile(r"rollout-.*-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$")


def fresh_rollout_ids(window_s):
    """Thread ids of .jsonl rollouts written in the last window_s seconds (today's and yesterday's dirs)."""
    out, now = [], time.time()
    for back in (0, 1):
        d = CODEX / "sessions" / time.strftime("%Y/%m/%d", time.localtime(now - back * 86400))
        if not d.is_dir():
            continue
        for f in d.iterdir():
            m = ROLLOUT_ID.search(f.name)
            try:
                if m and now - f.stat().st_mtime <= window_s:
                    out.append(m.group(1))
            except OSError:
                pass
    return out


def discover(days, archived=False, keyword=None, scan=2000):
    """thread/list plus what it cannot see (hidden_threads), newest first, each tagged `_listed_by`."""
    rows = list(iter_threads(archived=archived, keyword=keyword, days=days, max_threads=scan))
    for t in rows:
        t["_listed_by"] = "thread/list"
    if not keyword and len(rows) < scan:
        rows += hidden_threads({t["id"] for t in rows}, days, archived, scan - len(rows))
        rows.sort(key=lambda t: t.get("updatedAt") or 0, reverse=True)
    return rows


def cmd_list(a):
    srv = app()
    loaded = {t if isinstance(t, str) else t.get("id") for t in srv.call("thread/loaded/list", {}).get("data") or []}
    days = a.days if a.days is not None else (7 if a.active else None)
    rows = discover(days, a.archived, a.keyword, a.scan)
    st = state_rows([t["id"] for t in rows])
    metas = [thread_meta(t, st.get(t["id"])) | {"listed_by": t["_listed_by"]} for t in rows]
    if a.project:
        metas = [m for m in metas if matches_project(m, a.project)]
    if a.source:
        metas = [m for m in metas if m["source"] == a.source or (a.source == "desktop" and m["source"] == "desktop_or_ide")]
    need_status = a.active or a.status or not a.no_status
    if need_status:
        qc = queued_counts([m["id"] for m in metas])
        recent = time.time() - a.status_days * 86400 if a.status_days else None
        for m in metas:
            upd = dt.datetime.fromisoformat(m["updated_at"].replace("Z", "+00:00")).timestamp() if m["updated_at"] else 0
            if recent and not (a.active or a.status) and upd < recent:
                m["status"] = {"state": "not_computed", "basis": "skipped",
                               "detail": f"not updated in {a.status_days:g} days; pass --status-days 0 or run status <id>"}
                continue
            try:
                m["status"] = thread_status(m["id"], m["rollout_path"], qc.get(m["id"], 0), loaded)
            except Fail as e:
                m["status"] = {"state": "unknown", "basis": "unavailable", "detail": e.message}
    if a.active:
        metas = [m for m in metas if m["status"]["state"] in ACTIVE_STATES]
    if a.status:
        want = set(a.status.split(","))
        metas = [m for m in metas if m["status"]["state"] in want]
    total = len(metas)
    metas = metas[a.offset:a.offset + a.limit]
    out = envelope("list", ok=True, measured=True, n_scanned=len(rows),
                   n_listed_by={k: sum(1 for t in rows if t["_listed_by"] == k) for k in ("thread/list", "state_db", "rollout_scan")},
                   n_matched=total, returned=len(metas),
                   next_offset=(a.offset + len(metas)) if a.offset + len(metas) < total else None,
                   filters={k: getattr(a, k) for k in ("active", "project", "status", "source", "keyword", "archived")} | {"days": days},
                   threads=metas)
    if a.cloud:
        exe, _ = codex_bin()
        out["cloud"] = cloud_list(exe, 20)
    return emit(out)


def cmd_read(a):
    tid = resolve(a.thread)
    t = read_thread(tid)
    types = set(a.types.split(",")) if a.types else None
    max_chars = 0 if a.full else a.max_chars
    entries, ckpt, more = fetch_items(tid, a.since, a.limit, path=real_path(t.get("path")))
    items = [r for r in (norm_item(e, max_chars, types) for e in entries) if r]
    return emit(envelope("read", ok=True, measured=True, thread=thread_meta(t, state_rows([tid]).get(tid)),
                         history_source=history_source(tid),
                         since=a.since, checkpoint=ckpt, has_more=more, n_items=len(items), n_fetched=len(entries),
                         order="chronological (completion order)", items=items,
                         note="items are recorded when they complete; a command still running is not listed yet"))


def cmd_status(a):
    tid = resolve(a.thread)
    t = read_thread(tid)
    srv = app()
    loaded = {x if isinstance(x, str) else x.get("id") for x in srv.call("thread/loaded/list", {}).get("data") or []}
    meta = thread_meta(t, state_rows([tid]).get(tid))
    status = thread_status(tid, meta["rollout_path"], queued_counts([tid]).get(tid, 0), loaded)
    turns = list_turns(tid, meta["rollout_path"], a.turns)
    recent = [{"id": x["id"], "api_status": x.get("status"), "started_at": iso(x.get("startedAt")),
               "completed_at": iso(x.get("completedAt")), "duration_ms": x.get("durationMs"),
               "error": clip(x.get("error"), 500)} for x in turns]
    last_turn_items = []
    if turns:
        entries, _, _ = fetch_items(tid, None, 2000, turn_id=turns[0]["id"])
        last_turn_items = [norm_item(e, 600) for e in entries]
    failures = [i for i in last_turn_items if (i.get("exit_code") not in (None, 0)) or i.get("status") == "failed"]
    last_msg = next((i for i in reversed(last_turn_items) if i["kind"] == "agentMessage"), None)
    counts = {}
    for i in last_turn_items:
        counts[i["kind"]] = counts.get(i["kind"], 0) + 1
    return emit(envelope("status", ok=True, measured=True, thread=meta, status=status, recent_turns=recent,
                         last_turn={"item_counts": counts, "failures": failures[-10:], "last_agent_message": last_msg,
                                    "last_items": last_turn_items[-5:]}))


def event_of(rec):
    k = rec["kind"]
    e = {"kind": k, "id": rec["id"], "turn_id": rec["turn_id"], "at": rec["completed_at"] or rec["started_at"]}
    if k == "commandExecution":
        e["summary"] = (rec.get("command") or "")[:200]
        e["exit_code"], e["status"] = rec.get("exit_code"), rec.get("status")
        e["failed"] = rec.get("exit_code") not in (None, 0) or rec.get("status") == "failed"
    elif k == "mcpToolCall":
        e["summary"] = f"{rec.get('server')}/{rec.get('tool')}"
        e["status"], e["failed"] = rec.get("status"), rec.get("status") == "failed"
    elif k in ("userMessage", "agentMessage"):
        e["summary"] = (rec.get("text") or "")[:300]
        if k == "agentMessage":
            e["phase"] = rec.get("phase")
    elif k == "fileChange":
        ch = (rec.get("detail") or {}).get("changes") or []
        e["summary"] = ", ".join(str(c.get("path")) for c in ch if isinstance(c, dict))[:300]
    elif k == "reasoning":
        return None
    else:
        e["summary"] = json.dumps(rec.get("detail"))[:200]
    return e


def cmd_events(a):
    tid = resolve(a.thread)
    t = read_thread(tid)
    entries, ckpt, more = fetch_items(tid, a.since, a.limit, path=real_path(t.get("path")))
    events = [e for e in (event_of(norm_item(x, 600)) for x in entries) if e]
    turns = []
    if a.since is None:
        for x in list_turns(tid, real_path(t.get("path")), 10000, newest_first=False):
            turns.append({"id": x["id"], "api_status": x.get("status"), "started_at": iso(x.get("startedAt")),
                          "completed_at": iso(x.get("completedAt")), "duration_ms": x.get("durationMs"),
                          "error": clip(x.get("error"), 500)})
    return emit(envelope("events", ok=True, measured=True, thread_id=tid, history_source=history_source(tid),
                         since=a.since, checkpoint=ckpt,
                         has_more=more, n_events=len(events), turns=turns if a.since is None else None, events=events))


def item_haystack(it):
    parts = []
    for k in ("text", "command", "aggregatedOutput", "query"):
        if isinstance(it.get(k), str):
            parts.append(it[k])
    for c in it.get("content") or []:
        if isinstance(c, dict) and isinstance(c.get("text"), str):
            parts.append(c["text"])
    for k in ("arguments", "result", "error", "changes"):
        if it.get(k) is not None:
            parts.append(json.dumps(it[k], ensure_ascii=False))
    return "\n".join(parts)


def cmd_search(a):
    q = a.query.lower()
    rows = discover(a.days, a.archived, None, a.max_threads)
    st = state_rows([t["id"] for t in rows])
    metas = {t["id"]: thread_meta(t, st.get(t["id"])) for t in rows}
    if a.project:
        metas = {k: m for k, m in metas.items() if matches_project(m, a.project)}
    hits, title_hits, scanned_items, errors = [], [], 0, []
    t0 = time.time()
    for tid, m in metas.items():
        if q in (m.get("name") or "").lower() or q in (m.get("title_preview") or "").lower():
            title_hits.append({"thread_id": tid, "name": m["name"], "source": m["source"], "updated_at": m["updated_at"]})
        if a.titles_only or len(hits) >= a.limit:
            continue
        try:
            entries, _, _ = fetch_items(tid, None, a.max_items, path=m["rollout_path"])
        except Fail as e:
            errors.append({"thread_id": tid, "error": e.message})
            continue
        for e in entries:
            scanned_items += 1
            hay = item_haystack(e["item"])
            i = hay.lower().find(q)
            if i < 0:
                continue
            lo, hi = max(0, i - 160), min(len(hay), i + len(q) + 160)
            hits.append({"thread_id": tid, "name": m["name"], "source": m["source"], "turn_id": e.get("turnId"),
                         "item_id": e["item"].get("id"), "kind": e["item"].get("type"),
                         "at": iso(e.get("completedAtMs")), "excerpt": clip(hay[lo:hi], 0)})
            if len(hits) >= a.limit:
                break
    return emit(envelope("search", ok=True, measured=True, query=a.query, n_threads_scanned=len(metas),
                         n_items_scanned=scanned_items, elapsed_s=round(time.time() - t0, 2),
                         truncated=len(hits) >= a.limit, title_matches=title_hits, matches=hits, errors=errors))


def cmd_watch(a):
    srv = app()
    if a.active:
        rows = discover(a.days or 7, scan=a.scan)
        for t in rows:
            thread_meta(t)
        tids = [t["id"] for t in rows if thread_status(t["id"], real_path(t.get("path")))["state"] in ACTIVE_STATES]
    else:
        tids = [resolve(x) for x in a.threads]
    if not tids:
        emit(envelope("watch", event="watch_end", reason="nothing to watch", threads=[]))
        return 0
    state = {}
    for tid in tids:
        t = read_thread(tid)
        ck = a.since
        if ck is None:
            ents = srv.call("thread/items/list", {"threadId": tid, "limit": 1, "sortDirection": "desc"}).get("data") or []
            ck = last_ordinal(tid, ents[0]["item"]["id"]) if ents else None
        state[tid] = {"checkpoint": ck, "path": real_path(t.get("path")), "sig": None, "status": None, "name": t.get("name")}
        emit(envelope("watch", event="watch_start", thread_id=tid, name=t.get("name"), checkpoint=ck))
    end = time.time() + a.duration
    seen_ids = set()
    while True:
        for tid, s in state.items():
            p = s["path"]
            sig = (os.path.getmtime(p), os.path.getsize(p)) if p and os.path.exists(p) else None
            if sig != s["sig"] or s["status"] is None:
                s["sig"] = sig
                entries, ck, _ = fetch_items(tid, s["checkpoint"], 500)
                if ck is not None:
                    s["checkpoint"] = ck
                for e in entries:
                    rec = norm_item(e, a.max_chars)
                    if rec["id"] in seen_ids:
                        continue
                    seen_ids.add(rec["id"])
                    if rec["kind"] == "reasoning" and not a.reasoning:
                        continue
                    emit({"event": "item", "thread_id": tid, "checkpoint": s["checkpoint"], "item": rec})
            st = thread_status(tid, p, queued_counts([tid]).get(tid, 0))
            if st["state"] != s["status"]:
                emit({"event": "status", "thread_id": tid, "at": now_iso(), "from": s["status"], "to": st["state"],
                      "basis": st["basis"], "detail": st["detail"], "last_turn": st.get("last_turn")})
                s["status"] = st["state"]
        if a.until_idle and all(s["status"] not in ACTIVE_STATES for s in state.values()):
            reason = "all watched threads idle"
            break
        if time.time() >= end:
            reason = f"duration {a.duration}s reached"
            break
        time.sleep(a.interval)
    emit({"event": "watch_end", "reason": reason, "at": now_iso(),
          "checkpoints": {tid: s["checkpoint"] for tid, s in state.items()},
          "status": {tid: s["status"] for tid, s in state.items()}, "redactions": RED.hits})
    return 0


def md_fence(text, lang=""):
    text = text or ""
    fence = "```"
    while fence in text:
        fence += "`"
    return f"{fence}{lang}\n{text.rstrip()}\n{fence}"


def md_item(rec):
    """One normalised item as markdown. Messages in full; tool output as cut by norm_item."""
    k, at = rec["kind"], (rec.get("completed_at") or rec.get("started_at") or "")[:19].replace("T", " ")
    if k == "userMessage":
        extra = f" (+{', '.join(rec['attachments'])})" if rec.get("attachments") else ""
        return f"#### User, {at}{extra}\n\n{rec.get('text') or ''}"
    if k == "agentMessage":
        phase = f" ({rec['phase']})" if rec.get("phase") and rec["phase"] != "final_answer" else ""
        return f"#### Codex, {at}{phase}\n\n{rec.get('text') or ''}"
    if k == "commandExecution":
        head = f"**Command**, {at}, exit {rec.get('exit_code')}"
        if rec.get("duration_ms") is not None:
            head += f", {rec['duration_ms'] / 1000:.1f}s"
        out = md_fence(rec.get("command") or "", "bash")
        if rec.get("output"):
            out += "\n\nOutput:\n\n" + md_fence(rec["output"])
        return head + "\n\n" + out
    if k == "mcpToolCall":
        body = md_fence(json.dumps(rec.get("arguments"), ensure_ascii=False, indent=1) if rec.get("arguments") is not None else "", "json")
        res = rec.get("error") or rec.get("result")
        if res is not None:
            body += "\n\nResult:\n\n" + md_fence(res if isinstance(res, str) else json.dumps(res, ensure_ascii=False, indent=1))
        return f"**Tool** `{rec.get('server')}/{rec.get('tool')}`, {at}, {rec.get('status')}\n\n{body}"
    if k == "reasoning":
        summ = [x.get("text", x) if isinstance(x, dict) else x for x in rec.get("summary") or []]
        return f"_Reasoning, {at}:_ " + " ".join(str(x) for x in summ) if summ else None
    if k == "fileChange":
        ch = (rec.get("detail") or {}).get("changes") or []
        paths = [f"`{c.get('path')}` ({c.get('kind', {}).get('type') if isinstance(c.get('kind'), dict) else c.get('kind', '')})"
                 for c in ch if isinstance(c, dict)]
        return f"**File change**, {at}: " + (", ".join(paths) or "(no paths recorded)")
    if k == "webSearch":
        return f"**Web search**, {at}: {(rec.get('detail') or {}).get('query', '')}"
    if k == "contextCompaction":
        return f"_Context compacted, {at}_"
    return f"**{k}**, {at}\n\n" + md_fence(json.dumps(rec.get("detail"), ensure_ascii=False, indent=1)[:2000], "json")


def cmd_export(a):
    """Markdown transcripts, newest thread first. Messages are complete; tool outputs are cut at
    --output-chars. With --out the file is replaced atomically and a JSON summary is printed."""
    if a.threads:
        tids = [resolve(x) for x in a.threads]
    else:
        rows = discover(a.days, scan=a.scan)
        rx = re.compile(a.exclude, re.I) if a.exclude else None
        tids = [t["id"] for t in rows if not (rx and rx.search(t.get("name") or ""))][:a.recent]
    parts, summary = [], []
    for tid in tids:
        t = read_thread(tid)
        meta = thread_meta(t, state_rows([tid]).get(tid))
        st = thread_status(tid, meta["rollout_path"], queued_counts([tid]).get(tid, 0))
        entries, ck, more = fetch_items(tid, None, a.max_items, path=meta["rollout_path"])
        recs = []
        for e in entries:
            r = norm_item(e, a.output_chars)
            if r.get("kind") in ("userMessage", "agentMessage"):
                r = norm_item(e, 0)          # messages are never cut
            recs.append(r)
        lines = [f"## {meta['name'] or meta['title_preview'][:80] or tid}", "",
                 f"- id: `{tid}`", f"- source: {meta['source']} ({meta['originator'] or 'no originator'})",
                 f"- status: **{st['state']}** ({st['basis']}): {st['detail']}",
                 f"- created {meta['created_at']}, updated {meta['updated_at']}",
                 f"- cwd: `{meta['cwd']}`"]
        if meta["repository"]:
            lines.append(f"- repo: {meta['repository']} @ {meta['branch'] or '?'}")
        counts = {}
        for r in recs:
            counts[r["kind"]] = counts.get(r["kind"], 0) + 1
        lines.append(f"- items: {len(recs)} ({', '.join(f'{k} {v}' for k, v in sorted(counts.items()))}), "
                     f"history from {history_source(tid)}" + (f"; first {a.max_items} only" if more else ""))
        lines.append("")
        turn = None
        for r in recs:
            if r.get("turn_id") != turn:
                turn = r.get("turn_id")
                lines += ["---", "", f"### Turn `{turn}`", ""]
            block = md_item(r)
            if block:
                lines += [block, ""]
        parts.append("\n".join(lines))
        summary.append({"id": tid, "name": meta["name"], "items": len(recs), "truncated_items": more,
                        "status": st["state"], "updated_at": meta["updated_at"]})
    doc = (f"# Codex threads: {len(tids)} most recent\n\n"
           f"Generated {now_iso()} by `codex-skill export`. Messages are complete; command and tool "
           f"outputs over {a.output_chars} characters are cut and marked. Secrets are replaced with "
           f"[REDACTED] ({RED.hits} so far). Reasoning is encrypted by Codex, so only summaries appear.\n\n"
           + "\n\n".join(parts) + "\n")
    if not a.out:
        sys.stdout.write(doc)
        return 0
    out = Path(a.out).expanduser()
    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_name(f".{out.name}.{os.getpid()}.tmp")
    tmp.write_text(doc, encoding="utf-8")
    os.replace(tmp, out)              # readers (an open MDAI tab) never see half a file
    return emit(envelope("export", ok=True, measured=True, path=str(out), bytes=len(doc.encode()),
                         threads=summary))


def main(argv=None):
    ap = argparse.ArgumentParser(prog="codex-skill", description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("capabilities")
    c.add_argument("--no-cloud", action="store_true", help="skip the cloud probe (network)")
    c = sub.add_parser("list")
    c.add_argument("--active", action="store_true", help="only running/waiting threads (scans the last --days, default 7)")
    c.add_argument("--project", help="substring of cwd, name, repository or origin url")
    c.add_argument("--status", help="comma list of states: running,waiting,idle,interrupted,failed,abandoned,new")
    c.add_argument("--source", help="desktop, cli, exec, app_server_client, subagent")
    c.add_argument("--keyword", help="title filter, passed to thread/list searchTerm")
    c.add_argument("--days", type=float, help="only threads updated in the last N days")
    c.add_argument("--archived", action="store_true")
    c.add_argument("--limit", type=int, default=50)
    c.add_argument("--offset", type=int, default=0)
    c.add_argument("--scan", type=int, default=2000, help="max threads read from thread/list")
    c.add_argument("--no-status", action="store_true", help="skip per-thread status (faster)")
    c.add_argument("--status-days", type=float, default=7,
                   help="compute status only for threads updated in the last N days (0 = all; old compressed "
                        "rollouts cost ~0.2s each to parse)")
    c.add_argument("--cloud", action="store_true", help="also list Codex Cloud tasks")
    c = sub.add_parser("read")
    c.add_argument("thread")
    c.add_argument("--since", type=int, help="checkpoint from a previous read/events")
    c.add_argument("--limit", type=int, default=200)
    c.add_argument("--types", help="comma list of item kinds, e.g. userMessage,agentMessage,commandExecution")
    c.add_argument("--max-chars", type=int, default=DEFAULT_MAX_CHARS)
    c.add_argument("--full", action="store_true", help="no truncation (secrets are still redacted)")
    c = sub.add_parser("status")
    c.add_argument("thread")
    c.add_argument("--turns", type=int, default=5)
    c = sub.add_parser("events")
    c.add_argument("thread")
    c.add_argument("--since", type=int)
    c.add_argument("--limit", type=int, default=500)
    c = sub.add_parser("search")
    c.add_argument("query")
    c.add_argument("--days", type=float, default=30)
    c.add_argument("--project")
    c.add_argument("--archived", action="store_true")
    c.add_argument("--limit", type=int, default=50, help="max content matches")
    c.add_argument("--max-threads", type=int, default=300)
    c.add_argument("--max-items", type=int, default=5000, help="items scanned per thread")
    c.add_argument("--titles-only", action="store_true")
    c = sub.add_parser("export", help="markdown transcripts")
    c.add_argument("threads", nargs="*", help="threads to export; default the --recent newest")
    c.add_argument("--recent", type=int, default=5)
    c.add_argument("--exclude", help="regex on thread names to skip when picking --recent")
    c.add_argument("--days", type=float, default=30)
    c.add_argument("--scan", type=int, default=500)
    c.add_argument("--max-items", type=int, default=20000)
    c.add_argument("--output-chars", type=int, default=3000, help="cut command/tool output at N chars")
    c.add_argument("--out", help="write the markdown here (atomic replace) and print a JSON summary")
    c = sub.add_parser("watch")
    c.add_argument("threads", nargs="*")
    c.add_argument("--active", action="store_true")
    c.add_argument("--since", type=int)
    c.add_argument("--interval", type=float, default=5)
    c.add_argument("--duration", type=float, default=300, help="stop after N seconds (bounded by design)")
    c.add_argument("--until-idle", action="store_true", help="stop once no watched thread is running or waiting")
    c.add_argument("--days", type=float)
    c.add_argument("--scan", type=int, default=300)
    c.add_argument("--max-chars", type=int, default=1500)
    c.add_argument("--reasoning", action="store_true", help="include reasoning items")
    a = ap.parse_args(argv)
    if a.cmd == "watch" and not a.threads and not a.active:
        ap.error("watch needs a thread or --active")
    try:
        return {"capabilities": cmd_capabilities, "list": cmd_list, "read": cmd_read, "status": cmd_status,
                "events": cmd_events, "search": cmd_search, "watch": cmd_watch, "export": cmd_export}[a.cmd](a)
    except Fail as e:
        return emit(envelope(a.cmd, ok=False, measured=False, error=e.code, message=e.message, **clip(e.extra, 500)), e.exit_code)
    except KeyboardInterrupt:
        return 130
    finally:
        if _APP:
            _APP.close()


if __name__ == "__main__":
    sys.exit(main())
