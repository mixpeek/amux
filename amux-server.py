#!/usr/bin/env python3
"""amux serve — web dashboard for Claude Code session manager."""

# ═══════════════════════════════════════════
# CONFIGURATION & GLOBALS
# ═══════════════════════════════════════════

import base64
import json
import os
import re
import shlex
import socket
import sqlite3
import ssl
import subprocess
import sys
import threading
import time
import uuid
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from pathlib import Path
from urllib.parse import urlparse, parse_qs

# Strip Claude Code env vars so child processes (new sessions) don't inherit them
for _cv in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"):
    os.environ.pop(_cv, None)

# Support both ~/.amux (new) and legacy dirs for migration
_amux_home = Path.home() / ".amux"
for _old_home in [Path.home() / ".cmux", Path.home() / ".cc"]:
    if not _amux_home.exists() and _old_home.exists():
        _old_home.rename(_amux_home)
        break
CC_HOME = Path(os.environ.get("CC_HOME", _amux_home))
CC_SESSIONS = CC_HOME / "sessions"
CC_LOGS = CC_HOME / "logs"
CC_MEMORY = CC_HOME / "memory"
CC_BOARD_DIR = CC_HOME / "board"
CC_UPLOADS = CC_HOME / "uploads"
CC_LOGS.mkdir(parents=True, exist_ok=True)
CC_MEMORY.mkdir(parents=True, exist_ok=True)
CC_BOARD_DIR.mkdir(parents=True, exist_ok=True)
CC_UPLOADS.mkdir(parents=True, exist_ok=True)

UPLOAD_ALLOWED_EXTS = {
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp",
    ".pdf", ".txt", ".md", ".csv", ".json", ".log",
}
UPLOAD_MAX_BYTES = 20 * 1024 * 1024  # 20 MB
CLAUDE_HOME = Path.home() / ".claude"
MAX_LOG_BYTES = 10 * 1024 * 1024  # 10MB per session
SERVER_LOG = CC_LOGS / "server.log"
_server_log_lock = threading.Lock()

def slog(*args):
    """Append a timestamped line to ~/.amux/logs/server.log and stderr."""
    import datetime
    msg = " ".join(str(a) for a in args)
    line = f"{datetime.datetime.now().strftime('%Y-%m-%d %H:%M:%S')} {msg}\n"
    sys.stderr.write(line)
    try:
        with _server_log_lock:
            with open(SERVER_LOG, "a") as f:
                f.write(line)
    except Exception:
        pass

# SSE shared cache — avoids redundant subprocess calls when multiple tabs connect
_sse_cache = {
    "sessions": {"data": None, "json": "", "time": 0},
    "board": {"data": None, "json": "", "time": 0},
}
_SSE_CACHE_TTL = 2  # seconds

# Per-session token cache — refreshed every 30s, keyed by resolved dir
_token_cache = {"data": {}, "timestamps": {}, "time": 0}
_TOKEN_CACHE_TTL = 30

def _refresh_token_cache():
    """Rebuild per-directory token counts and last-activity timestamps from Claude JSONL files."""
    now = time.time()
    if now - _token_cache["time"] < _TOKEN_CACHE_TTL:
        return
    from datetime import datetime, timezone
    result = {}
    ts_result = {}
    projects_dir = Path.home() / ".claude" / "projects"
    if not projects_dir.is_dir():
        _token_cache["data"] = result
        _token_cache["timestamps"] = ts_result
        _token_cache["time"] = now
        return
    for proj_dir in projects_dir.iterdir():
        if not proj_dir.is_dir():
            continue
        # Find most recent JSONL
        jsonl_files = sorted(proj_dir.glob("*.jsonl"), key=lambda f: f.stat().st_mtime, reverse=True)
        if not jsonl_files:
            continue
        total = 0
        last_ts = 0
        # Read tail of top 5 most recent JSONL files to cover multiple conversations
        for jf in jsonl_files[:5]:
            try:
                size = jf.stat().st_size
                with jf.open("rb") as fh:
                    offset = max(0, size - 200_000)
                    if offset > 0:
                        fh.seek(offset)
                        fh.readline()  # skip partial line
                    prev_sig = None
                    for raw_line in fh:
                        try:
                            entry = json.loads(raw_line)
                            # Track last Claude API call timestamp
                            ts_str = entry.get("timestamp", "")
                            if ts_str:
                                try:
                                    ts_unix = int(datetime.fromisoformat(
                                        ts_str.replace("Z", "+00:00")
                                    ).timestamp())
                                    if ts_unix > last_ts:
                                        last_ts = ts_unix
                                except Exception:
                                    pass
                            usage = entry.get("message", {}).get("usage", {})
                            if usage:
                                sig = (usage.get("input_tokens", 0),
                                       usage.get("cache_read_input_tokens", 0),
                                       usage.get("output_tokens", 0))
                                if sig == prev_sig:
                                    continue
                                prev_sig = sig
                                total += usage.get("input_tokens", 0)
                                total += usage.get("cache_read_input_tokens", 0)
                                total += usage.get("cache_creation_input_tokens", 0)
                                total += usage.get("output_tokens", 0)
                            else:
                                prev_sig = None
                        except (json.JSONDecodeError, AttributeError):
                            continue
            except Exception:
                continue
        if total > 0:
            result[proj_dir.name] = total
        if last_ts > 0:
            ts_result[proj_dir.name] = last_ts
    _token_cache["data"] = result
    _token_cache["timestamps"] = ts_result
    _token_cache["time"] = now

# ═══════════════════════════════════════════
# SESSION FILE HELPERS
# ═══════════════════════════════════════════

def parse_env_file(path: Path) -> dict:
    """Parse a amux session .env file into a dict."""
    data = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        m = re.match(r'^(\w+)="(.*)"$', line)
        if m:
            data[m.group(1)] = m.group(2)
            continue
        m = re.match(r"^(\w+)='(.*)'$", line)
        if m:
            data[m.group(1)] = m.group(2)
            continue
        m = re.match(r"^(\w+)=(.*)$", line)
        if m:
            data[m.group(1)] = m.group(2)
    return data


def _write_env(path: Path, cfg: dict):
    """Write a cfg dict back to a amux .env file."""
    lines = [f'# updated: {__import__("datetime").datetime.now().isoformat()}']
    for k, v in cfg.items():
        lines.append(f'{k}="{v}"')
    path.write_text("\n".join(lines) + "\n")


# ═══════════════════════════════════════════
# TMUX HELPERS
# ═══════════════════════════════════════════

def tmux_name(session: str) -> str:
    # Migrate cmux-* or cc-* → amux-* if old name exists
    new = f"amux-{session}"
    for old in [f"cmux-{session}", f"cc-{session}"]:
        try:
            r = subprocess.run(["tmux", "has-session", "-t", old], capture_output=True)
            if r.returncode == 0:
                subprocess.run(["tmux", "rename-session", "-t", old, new], capture_output=True, timeout=5)
                break
        except Exception:
            pass
    return new


def is_running(session: str) -> bool:
    try:
        subprocess.run(
            ["tmux", "has-session", "-t", tmux_name(session)],
            capture_output=True, check=True,
        )
        return True
    except (subprocess.CalledProcessError, FileNotFoundError):
        return False


def tmux_capture(session: str, lines: int = 500) -> str:
    try:
        r = subprocess.run(
            ["tmux", "capture-pane", "-t", tmux_name(session), "-p", "-S", f"-{lines}"],
            capture_output=True, text=True, timeout=5,
        )
        # Strip leading/trailing blank lines so content isn't cut off
        return r.stdout.strip()
    except Exception:
        return ""


def _log_path(session: str) -> Path:
    return CC_LOGS / f"{session}.log"


def save_session_log(session: str, content: str):
    """Append new content to session log, keeping file under MAX_LOG_BYTES."""
    if not content.strip():
        return
    lp = _log_path(session)
    try:
        # Write full capture each time (tmux scrollback is the source of truth)
        data = content.encode("utf-8", errors="replace")
        if len(data) > MAX_LOG_BYTES:
            data = data[-MAX_LOG_BYTES:]
        lp.write_bytes(data)
    except Exception:
        pass


def load_session_log(session: str) -> str:
    """Load saved session log from disk."""
    lp = _log_path(session)
    if lp.exists():
        try:
            return lp.read_text(errors="replace")
        except Exception:
            pass
    return ""


def _snapshot_all_sessions():
    """Capture scrollback for all running sessions and save to disk."""
    for f in CC_SESSIONS.glob("*.env"):
        name = f.stem
        if is_running(name):
            try:
                output = tmux_capture(name, 5000)
                if output:
                    save_session_log(name, output)
            except Exception:
                pass


def _snapshot_loop():
    """Background thread: snapshot all sessions every 60 seconds."""
    while True:
        try:
            _snapshot_all_sessions()
        except Exception:
            pass
        time.sleep(60)


def get_claude_stats(work_dir: str) -> dict:
    """Get token usage and last activity from Claude Code session files for a directory."""
    if not work_dir:
        return {"tokens": 0, "last_active": ""}
    # Map dir path to Claude project directory name
    project_name = work_dir.replace("/", "-")
    project_dir = CLAUDE_HOME / "projects" / project_name
    if not project_dir.is_dir():
        return {"tokens": 0, "last_active": ""}
    # Find the most recent JSONL file
    jsonl_files = sorted(project_dir.glob("*.jsonl"), key=lambda f: f.stat().st_mtime, reverse=True)
    if not jsonl_files:
        return {"tokens": 0, "last_active": ""}
    # Sum tokens from most recent session, get last timestamp
    total_in = 0
    total_out = 0
    last_ts = ""
    try:
        with jsonl_files[0].open() as f:
            for line in f:
                try:
                    entry = json.loads(line)
                    ts = entry.get("timestamp", "")
                    if ts:
                        last_ts = ts
                    msg = entry.get("message", {})
                    usage = msg.get("usage", {})
                    if usage:
                        total_in += usage.get("input_tokens", 0)
                        total_in += usage.get("cache_read_input_tokens", 0)
                        total_out += usage.get("output_tokens", 0)
                except (json.JSONDecodeError, AttributeError):
                    continue
    except Exception:
        pass
    return {"tokens": total_in + total_out, "last_active": last_ts}


def detect_active_model(work_dir: str) -> str:
    """Detect the model in use from the most recent Claude JSONL entries."""
    if not work_dir:
        return ""
    resolved = str(Path(work_dir).expanduser().resolve())
    project_name = resolved.replace("/", "-")
    project_dir = CLAUDE_HOME / "projects" / project_name
    if not project_dir.is_dir():
        return ""
    jsonl_files = sorted(project_dir.glob("*.jsonl"), key=lambda f: f.stat().st_mtime, reverse=True)
    if not jsonl_files:
        return ""
    try:
        # Search from end in increasing chunks until we find a model
        f = jsonl_files[0]
        size = f.stat().st_size
        for chunk_size in [200_000, 1_000_000, size]:
            with f.open("rb") as fh:
                offset = max(0, size - chunk_size)
                if offset > 0:
                    fh.seek(offset)
                    fh.readline()  # skip partial line
                data = fh.read().decode("utf-8", errors="replace")
            for line in reversed(data.splitlines()):
                try:
                    entry = json.loads(line)
                    model = entry.get("message", {}).get("model", "")
                    if model:
                        return model
                except (json.JSONDecodeError, AttributeError):
                    continue
            if chunk_size >= size:
                break
    except Exception:
        pass
    return ""


_TOKEN_BASELINE_FILE = CC_HOME / "token_baseline.json"


def _load_token_baseline() -> dict:
    if _TOKEN_BASELINE_FILE.exists():
        try:
            data = json.loads(_TOKEN_BASELINE_FILE.read_text())
            from datetime import datetime
            if data.get("date") == datetime.now().strftime("%Y-%m-%d"):
                return data
        except Exception:
            pass
    return {}


def save_token_baseline(stats: dict):
    from datetime import datetime
    baseline = {"date": datetime.now().strftime("%Y-%m-%d"), "sessions": {}}
    # Key by project dir (stable) not by display label (can change)
    for s in stats.get("sessions", []):
        key = s.get("proj_dir", s["name"])
        baseline["sessions"][key] = {"input": s["input"], "output": s["output"]}
    baseline["total_input"] = stats.get("total_input", 0)
    baseline["total_output"] = stats.get("total_output", 0)
    _TOKEN_BASELINE_FILE.write_text(json.dumps(baseline))


# ═══════════════════════════════════════════
# BOARD PERSISTENCE — SQLite
# ═══════════════════════════════════════════

_BOARD_FILE = CC_BOARD_DIR / "items.json"
# Migrate legacy board.json → board/items.json
_legacy_board = CC_HOME / "board.json"
if _legacy_board.exists() and not _BOARD_FILE.exists():
    import shutil as _shutil
    _shutil.move(str(_legacy_board), str(_BOARD_FILE))

_DB_PATH = CC_HOME / "amux.db"
_db_local = threading.local()

_DB_SCHEMA = """
CREATE TABLE IF NOT EXISTS statuses (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    is_builtin  INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO statuses (id, label, position, is_builtin) VALUES
    ('backlog',   'Backlog',      0, 1),
    ('todo',      'To Do',        1, 1),
    ('doing',     'In Progress',  2, 1),
    ('done',      'Done',         3, 1),
    ('discarded', 'Discarded',    4, 1);
CREATE TABLE IF NOT EXISTS issues (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    desc        TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'todo',
    session     TEXT,
    creator     TEXT NOT NULL DEFAULT '',
    due         TEXT,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    deleted     INTEGER,
    owner_type  TEXT NOT NULL DEFAULT 'human'
);
CREATE TABLE IF NOT EXISTS issue_tags (
    issue_id    TEXT NOT NULL,
    tag         TEXT NOT NULL,
    PRIMARY KEY (issue_id, tag),
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS issue_counters (
    prefix      TEXT PRIMARY KEY,
    next_n      INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_issues_status  ON issues(status);
CREATE INDEX IF NOT EXISTS idx_issues_session ON issues(session);
CREATE INDEX IF NOT EXISTS idx_issues_updated ON issues(updated);
CREATE INDEX IF NOT EXISTS idx_issues_due     ON issues(due) WHERE due IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_issue_tags_tag ON issue_tags(tag);
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    session     TEXT NOT NULL,
    text        TEXT NOT NULL,
    done        INTEGER NOT NULL DEFAULT 0,
    pos         INTEGER NOT NULL DEFAULT 0,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session);
"""


def get_db() -> sqlite3.Connection:
    """Return a per-thread SQLite connection with WAL mode enabled."""
    if not hasattr(_db_local, "conn"):
        conn = sqlite3.connect(str(_DB_PATH), check_same_thread=False)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA synchronous=NORMAL")
        conn.execute("PRAGMA foreign_keys=ON")
        conn.execute("PRAGMA busy_timeout=5000")
        _db_local.conn = conn
    return _db_local.conn


def _init_db():
    """Create SQLite tables if they don't exist."""
    db = get_db()
    db.executescript(_DB_SCHEMA)
    # Ensure built-in statuses have correct positions (idempotent for existing DBs)
    for pos, (sid, label) in enumerate([
        ("backlog",   "Backlog"),
        ("todo",      "To Do"),
        ("doing",     "In Progress"),
        ("done",      "Done"),
        ("discarded", "Discarded"),
    ]):
        db.execute(
            "INSERT INTO statuses (id, label, position, is_builtin) VALUES (?, ?, ?, 1)"
            " ON CONFLICT(id) DO UPDATE SET position = ?, is_builtin = 1",
            (sid, label, pos, pos),
        )
    db.commit()
    # Migrations: add columns that may not exist on older DBs
    for migration in [
        "ALTER TABLE issues ADD COLUMN owner_type TEXT NOT NULL DEFAULT 'human'",
    ]:
        try:
            db.execute(migration)
            db.commit()
        except Exception:
            pass  # column already exists


def _load_board_raw() -> dict:
    """Read items.json — used only for migration."""
    if _BOARD_FILE.exists():
        try:
            return json.loads(_BOARD_FILE.read_text())
        except Exception:
            pass
    return {}


def _migrate_flat_to_sqlite():
    """One-time import of items.json into SQLite. Safe to call on every startup."""
    db = get_db()
    existing = db.execute("SELECT COUNT(*) FROM issues").fetchone()[0]
    if existing > 0:
        return  # already migrated
    raw = _load_board_raw()
    if not raw and not _BOARD_FILE.exists():
        return  # nothing to migrate
    # Import statuses
    statuses = raw.get("statuses", list(_DEFAULT_STATUSES))
    builtin_ids = {"backlog", "todo", "doing", "done", "discarded"}
    existing_ids = {s["id"] for s in statuses}
    for s in _DEFAULT_STATUSES:
        if s["id"] not in existing_ids:
            statuses.append(s)
    for i, s in enumerate(statuses):
        db.execute(
            "INSERT OR IGNORE INTO statuses (id, label, position, is_builtin) VALUES (?, ?, ?, ?)",
            (s["id"], s["label"], i, 1 if s["id"] in builtin_ids else 0),
        )
    # Import items
    items = raw.get("items", [])
    counters = raw.get("counters", {})
    now = int(time.time())
    for item in items:
        # Handle old id/key inconsistency: prefer key if present, else id
        item_id = item.get("key") or item.get("id")
        if not item_id:
            continue
        status = item.get("status", "todo")
        # Ensure any unknown status exists in statuses table
        db.execute(
            "INSERT OR IGNORE INTO statuses (id, label, position, is_builtin) VALUES (?, ?, 99, 0)",
            (status, status.title()),
        )
        db.execute(
            """INSERT OR IGNORE INTO issues
               (id, title, desc, status, session, creator, due, created, updated)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)""",
            (
                item_id,
                item.get("title", ""),
                item.get("desc", ""),
                status,
                item.get("session") or None,
                item.get("creator", ""),
                item.get("due") or None,
                item.get("created", now),
                item.get("updated", now),
            ),
        )
        for tag in item.get("tags", []):
            if tag:
                db.execute(
                    "INSERT OR IGNORE INTO issue_tags (issue_id, tag) VALUES (?, ?)",
                    (item_id, tag),
                )
    for prefix, n in counters.items():
        db.execute(
            "INSERT OR IGNORE INTO issue_counters (prefix, next_n) VALUES (?, ?)",
            (prefix, n + 1),
        )
    db.commit()
    slog(f"DB migration: imported {len(items)} issues, {len(statuses)} statuses from items.json")


def _meta_path(name: str) -> Path:
    return CC_SESSIONS / f"{name}.meta.json"


def _load_meta(name: str) -> dict:
    p = _meta_path(name)
    if p.exists():
        try:
            return json.loads(p.read_text())
        except Exception:
            pass
    return {}


def _save_meta(name: str, meta: dict):
    _meta_path(name).write_text(json.dumps(meta))


def _update_meta(name: str, **kwargs):
    meta = _load_meta(name)
    meta.update(kwargs)
    _save_meta(name, meta)


_DEFAULT_STATUSES = [
    {"id": "backlog",   "label": "Backlog"},
    {"id": "todo",      "label": "To Do"},
    {"id": "doing",     "label": "In Progress"},
    {"id": "done",      "label": "Done"},
    {"id": "discarded", "label": "Discarded"},
]


def _load_board() -> list:
    """Load all non-deleted issues from SQLite, with tags joined."""
    db = get_db()
    rows = db.execute(
        """SELECT i.id, i.title, i.desc, i.status, i.session, i.creator,
                  i.due, i.created, i.updated, i.owner_type,
                  GROUP_CONCAT(t.tag) AS tags_csv
           FROM issues i
           LEFT JOIN issue_tags t ON t.issue_id = i.id
           WHERE i.deleted IS NULL
           GROUP BY i.id
           ORDER BY i.updated DESC"""
    ).fetchall()
    result = []
    for row in rows:
        item = dict(row)
        tags_csv = item.pop("tags_csv") or ""
        item["tags"] = [t for t in tags_csv.split(",") if t]
        result.append(item)
    return result


def _load_board_statuses() -> list:
    """Load kanban statuses from SQLite."""
    db = get_db()
    rows = db.execute("SELECT id, label FROM statuses ORDER BY position").fetchall()
    return [dict(r) for r in rows] if rows else list(_DEFAULT_STATUSES)


def _item_by_id(bid: str) -> dict | None:
    """Fetch a single non-deleted issue by id."""
    db = get_db()
    row = db.execute(
        """SELECT i.id, i.title, i.desc, i.status, i.session, i.creator,
                  i.due, i.created, i.updated, i.owner_type,
                  GROUP_CONCAT(t.tag) AS tags_csv
           FROM issues i
           LEFT JOIN issue_tags t ON t.issue_id = i.id
           WHERE i.id = ? AND i.deleted IS NULL
           GROUP BY i.id""",
        (bid,),
    ).fetchone()
    if not row:
        return None
    item = dict(row)
    tags_csv = item.pop("tags_csv") or ""
    item["tags"] = [t for t in tags_csv.split(",") if t]
    return item


def _prefix_from_session(session: str) -> str:
    """Derive issue key prefix from session name (e.g. 'my-project' → 'MP')."""
    words = [w for w in re.split(r"[-_\s]+", session) if w] if session else []
    if not words:
        return "AMUX"
    if len(words) == 1:
        return re.sub(r"[^A-Z0-9]", "", words[0].upper())[:5] or "AMUX"
    return re.sub(r"[^A-Z0-9]", "", "".join(w[0] for w in words).upper())[:5] or "AMUX"


def _next_issue_id(prefix: str) -> str:
    """Atomically get and increment the counter for a prefix; return next id."""
    db = get_db()
    db.execute(
        "INSERT OR IGNORE INTO issue_counters (prefix, next_n) VALUES (?, 1)", (prefix,)
    )
    row = db.execute(
        "UPDATE issue_counters SET next_n = next_n + 1 WHERE prefix = ? RETURNING next_n - 1",
        (prefix,),
    ).fetchone()
    db.commit()
    return f"{prefix}-{row[0] if row else 1}"


def get_daily_token_stats() -> dict:
    """Get today's token usage across all Claude Code sessions and amux sessions."""
    from datetime import datetime, timezone
    today = datetime.now().strftime("%Y-%m-%d")
    projects_dir = CLAUDE_HOME / "projects"
    if not projects_dir.is_dir():
        return {"today": today, "total_tokens": 0, "total_input": 0, "total_output": 0, "sessions": []}

    # Get amux session dirs for labeling (multiple sessions can share a dir)
    amux_dirs = {}  # resolved_dir -> list of session names
    for f in CC_SESSIONS.glob("*.env"):
        cfg = parse_env_file(f)
        d = cfg.get("CC_DIR", "")
        if d:
            resolved = str(Path(d).expanduser().resolve()).replace("/", "-")
            amux_dirs.setdefault(resolved, []).append(f.stem)

    total_in = 0
    total_out = 0
    session_stats = []

    for proj_dir in projects_dir.iterdir():
        if not proj_dir.is_dir():
            continue
        proj_in = 0
        proj_out = 0
        for jf in proj_dir.glob("*.jsonl"):
            # Quick check: skip files not modified today
            try:
                mtime = datetime.fromtimestamp(jf.stat().st_mtime)
                if mtime.strftime("%Y-%m-%d") != today:
                    continue
            except Exception:
                continue
            try:
                prev_usage_sig = None
                with jf.open() as f:
                    for line in f:
                        try:
                            entry = json.loads(line)
                            ts = entry.get("timestamp", "")
                            if not ts or not ts.startswith(today):
                                msg = entry.get("message", {})
                                if not msg.get("usage"):
                                    prev_usage_sig = None
                                continue
                            msg = entry.get("message", {})
                            usage = msg.get("usage", {})
                            if usage:
                                # Deduplicate: Claude Code logs thinking + tool_use
                                # as separate entries with identical usage
                                sig = (usage.get("input_tokens", 0),
                                       usage.get("cache_read_input_tokens", 0),
                                       usage.get("output_tokens", 0))
                                if sig == prev_usage_sig:
                                    continue
                                prev_usage_sig = sig
                                proj_in += usage.get("input_tokens", 0)
                                proj_in += usage.get("cache_creation_input_tokens", 0)
                                proj_in += usage.get("cache_read_input_tokens", 0)
                                proj_out += usage.get("output_tokens", 0)
                            else:
                                prev_usage_sig = None
                        except (json.JSONDecodeError, AttributeError):
                            continue
            except Exception:
                continue
        if proj_in + proj_out > 0:
            proj_name = proj_dir.name
            amux_names = amux_dirs.get(proj_name, [])
            if amux_names:
                label = ", ".join(sorted(amux_names))
            else:
                # Show short path: ~/Dev/project instead of /Users/you/Dev/project
                # proj_name is like "-Users-you-Dev" → "/Users/you/Dev"
                full = "/" + proj_name.lstrip("-").replace("-", "/")
                home = str(Path.home())
                label = "~" + full[len(home):] if full.startswith(home) else full
            session_stats.append({
                "name": label,
                "proj_dir": proj_name,
                "amux": bool(amux_names),
                "input": proj_in,
                "output": proj_out,
                "total": proj_in + proj_out,
            })
            total_in += proj_in
            total_out += proj_out

    # Subtract baseline if reset was used today
    baseline = _load_token_baseline()
    if baseline:
        bl_sessions = baseline.get("sessions", {})
        for s in session_stats:
            # Try matching by proj_dir first (stable), then by label (legacy)
            bl = bl_sessions.get(s["proj_dir"], bl_sessions.get(s["name"], {}))
            s["input"] = max(0, s["input"] - bl.get("input", 0))
            s["output"] = max(0, s["output"] - bl.get("output", 0))
            s["total"] = s["input"] + s["output"]
        total_in = max(0, total_in - baseline.get("total_input", 0))
        total_out = max(0, total_out - baseline.get("total_output", 0))

    session_stats = [s for s in session_stats if s["total"] > 0]
    session_stats.sort(key=lambda s: s["total"], reverse=True)
    amux_tokens = sum(s["total"] for s in session_stats if s["amux"])
    return {
        "today": today,
        "total_tokens": total_in + total_out,
        "total_input": total_in,
        "total_output": total_out,
        "amux_tokens": amux_tokens,
        "sessions": session_stats,
    }


def _detect_claude_status(raw_output: str) -> str:
    """Detect Claude Code status from tmux output.

    Uses Claude Code's known terminal UI patterns to determine state.
    Scans bottom-up for the most recent definitive signal.

    Returns: 'active', 'waiting', 'idle', or ''.
    """
    if not raw_output:
        return ""
    clean = re.sub(
        r'\x1b\[[0-9;]*[a-zA-Z]|\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07'
        r'|\x1b\][^\x1b]*\x1b\\|\x1b[()][A-Z0-9]|\x1b[\x20-\x2f]*[\x40-\x7e]',
        '', raw_output,
    )
    lines = [l for l in clean.splitlines() if l.strip()]
    if not lines:
        return ""

    # ── 1. Status bar (bottom 3 lines — always the very last line when visible) ──
    status_bar = ""
    for l in reversed(lines[-3:]):
        ls = l.strip()
        # Status bar indicators: ⏵⏵, bypass permissions, plan mode, auto-compact
        if "\u23f5\u23f5" in ls or "bypass permissions" in ls.lower() or "plan mode" in ls.lower():
            status_bar = ls.lower()
            break

    # "esc to interrupt" (may be truncated to "esc to" or "esc t…") → active
    if status_bar and re.search(r"esc t", status_bar):
        return "active"

    # ── 2. Scan last 12 lines bottom-up for the most recent signal ──
    for l in reversed(lines[-12:]):
        s = l.strip()
        sl = s.lower()

        # ── Active signals ──
        # Spinner: dingbat char (U+2700-27BF) + verb + … (ellipsis = still running)
        #   e.g. "✻ Beaming… (1m 3s)" "✽ Tinkering…" "✳ Germinating…" "✢ Synthesizing…"
        if s and "\u2700" <= s[0] <= "\u27bf" and "\u2026" in s:
            return "active"
        # Tool execution in progress
        if s.startswith("Running\u2026") or re.match(r"Reading \d+ file", s):
            return "active"

        # ── Completed signals (= idle, Claude finished and returned to prompt) ──
        # Completed spinner: dingbat + past tense + "for" + duration (no ellipsis)
        #   e.g. "✻ Brewed for 1m 8s"  "✻ Sautéed for 4m 19s"
        if s and "\u2700" <= s[0] <= "\u27bf" and " for " in s and "\u2026" not in s:
            return "idle"

        # ── Waiting signals (needs user action) ──
        # Multi-choice / AskUserQuestion prompt
        if "enter to select" in sl:
            return "waiting"
        # Manual tool approval prompt ("Do you want to proceed? ❯ 1. Yes")
        if "do you want to proceed" in sl:
            return "waiting"
        if re.match(r".*\u276f\s*\d+\.", s):  # ❯ 1. Yes / ❯ 2. No selector
            return "waiting"
        # "Interrupted" with follow-up question
        if "interrupted" in sl and "what should claude do" in sl:
            return "waiting"

    # ── 3. Status bar secondary checks ──
    if status_bar:
        # Tool approval pending (only when bypass is off)
        bypass_on = "bypass permissions on" in status_bar
        if not bypass_on and re.search(r"\d+\s+(bash|tool|read|edit|write|glob|grep|notebook)", status_bar):
            return "waiting"
        if "approve" in status_bar:
            return "waiting"
        # Status bar visible but no active/waiting signal → idle at prompt
        return "idle"

    # ── 4. Fallback: check for shell prompt character ──
    # Only treat ❯ as a shell prompt when it's at the end of a line (not ❯ 1. Yes selector)
    for l in lines[-5:]:
        ls = l.strip()
        if ls.endswith("\u276f") or ls == "\u276f":
            return "idle"
        if "$ " in ls and not ls.startswith("❯"):
            return "idle"
    return ""


def _tmux_info_map() -> dict:
    """Get activity, creation time, and pane title for all tmux sessions."""
    result = {}
    try:
        r = subprocess.run(
            ["tmux", "list-panes", "-a", "-F",
             "#{session_name}\t#{window_activity}\t#{session_created}\t#{pane_title}"],
            capture_output=True, text=True, timeout=5,
        )
        if r.returncode != 0:
            return {}
        for line in r.stdout.strip().splitlines():
            parts = line.split("\t", 3)
            if len(parts) >= 3:
                name = parts[0]
                # Only keep first pane per session
                if name not in result:
                    title = parts[3].strip() if len(parts) >= 4 else ""
                    # Strip leading braille/dingbat status chars from pane title
                    clean_title = re.sub(r'^[\u2800-\u28ff\u2700-\u27bf\s]+', '', title).strip()
                    result[name] = {
                        "activity": int(parts[1]),
                        "created": int(parts[2]),
                        "pane_title": clean_title,
                    }
        return result
    except Exception:
        return {}


def _parse_task_time(raw_output: str) -> str:
    """Extract the current task duration from Claude Code's spinner line.
    e.g. '✻ Beaming… (1m 24s · ↓ 1.1k tokens)' → '1m 24s'
    e.g. '✻ Brewed for 4m 19s' → '4m 19s' (last completed task)
    """
    if not raw_output:
        return ""
    clean = re.sub(
        r'\x1b\[[0-9;]*[a-zA-Z]|\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07'
        r'|\x1b\][^\x1b]*\x1b\\|\x1b[()][A-Z0-9]|\x1b[\x20-\x2f]*[\x40-\x7e]',
        '', raw_output,
    )
    lines = [l for l in clean.splitlines() if l.strip()]
    for l in reversed(lines[-12:]):
        s = l.strip()
        if not s or s[0] < "\u2700" or s[0] > "\u27bf":
            continue
        # Active spinner: ✻ Verb… (Xm Ys · ...) — time is in parens
        m = re.search(r'\((\d[\dm\s]*s)\b', s)
        if m:
            return m.group(1)
        # Completed spinner: ✻ Verbed for Xm Ys
        m = re.search(r' for (\d[\dm\s]*s)\b', s)
        if m:
            return m.group(1)
    return ""


def list_sessions() -> list:
    sessions = []
    if not CC_SESSIONS.is_dir():
        return sessions
    tmux_info = _tmux_info_map()
    for f in sorted(CC_SESSIONS.glob("*.env")):
        name = f.stem
        cfg = parse_env_file(f)
        running = is_running(name)
        preview = ""
        preview_lines = []
        status = ""
        tinfo = tmux_info.get(tmux_name(name), {})
        # last_activity = when the last command was sent from the UI (meta.last_send),
        # falling back to last_started (set when session was started).
        # Deliberately avoid log/tmux mtime — those update every 60s from the snapshot loop.
        meta = _load_meta(name)
        last_activity = meta.get("last_send", 0) or meta.get("last_started", 0)
        session_created = tinfo.get("created", 0)
        pane_title = tinfo.get("pane_title", "")
        raw = ""
        if running:
            raw = tmux_capture(name, 30)
        elif _log_path(name).exists():
            # Load saved log for stopped sessions (last 30 lines worth)
            saved = load_session_log(name)
            if saved:
                raw = "\n".join(saved.splitlines()[-30:])
        if raw:
            strip_ansi = lambda t: re.sub(r'\x1b\[[0-9;]*[a-zA-Z]|\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07|\x1b\][^\x1b]*\x1b\\|\x1b[()][A-Z0-9]|\x1b[\x20-\x2f]*[\x40-\x7e]', '', t)
            lines = [l for l in raw.splitlines() if l.strip()]
            preview = strip_ansi(lines[-1][:120]) if lines else ""
            if running:
                status = _detect_claude_status(raw)
            # Filter for intelligible content lines
            intelligible = []
            for l in lines:
                cl = strip_ansi(l).strip()
                if not cl:
                    continue
                if "⏵⏵" in cl or "bypass permissions" in cl.lower() or "plan mode" in cl.lower():
                    continue
                alnum = sum(1 for c in cl if c.isalnum() or c == ' ')
                if len(cl) > 3 and alnum / len(cl) < 0.3:
                    continue
                if len(cl) <= 2:
                    continue
                if len(set(cl.replace(' ', ''))) <= 2:
                    continue
                intelligible.append(cl[:200])
            preview_lines = intelligible[-5:] if intelligible else []
        # Detect active model from JSONL (fast — reads last 50KB)
        raw_dir = cfg.get("CC_DIR", "")
        resolved_dir = str(Path(raw_dir).expanduser().resolve()) if raw_dir else ""
        active_model = detect_active_model(raw_dir)
        # Parse task time from spinner line
        task_time = _parse_task_time(raw) if raw else ""
        # Token count from JSONL cache
        _refresh_token_cache()
        proj_key = resolved_dir.replace("/", "-") if resolved_dir else ""
        tokens = _token_cache["data"].get(proj_key, 0)
        sessions.append({
            "name": name,
            "dir": resolved_dir,
            "desc": cfg.get("CC_DESC", ""),
            "pinned": cfg.get("CC_PINNED", "") == "1",
            "tags": [t.strip() for t in cfg.get("CC_TAGS", "").split(",") if t.strip()],
            "flags": cfg.get("CC_FLAGS", ""),
            "creator": cfg.get("CC_CREATOR", ""),
            "running": running,
            "status": status,
            "preview": preview,
            "preview_lines": preview_lines,
            "last_activity": last_activity,
            "active_model": active_model,
            "session_created": session_created,
            "task_time": task_time,
            "task_name": pane_title,
            "tokens": tokens,
        })
    # Pinned first, then working/waiting, then by last activity (most recent first)
    status_order = {"active": 0, "waiting": 0, "idle": 1, "": 1}
    sessions.sort(key=lambda s: (not s["pinned"], not s["running"], status_order.get(s["status"], 1), -s["last_activity"]))
    return sessions


def get_session_info(name: str) -> dict | None:
    f = CC_SESSIONS / f"{name}.env"
    if not f.exists():
        return None
    cfg = parse_env_file(f)
    raw_dir = cfg.get("CC_DIR", "")
    return {
        "name": name,
        "dir": str(Path(raw_dir).expanduser().resolve()) if raw_dir else "",
        "desc": cfg.get("CC_DESC", ""),
        "pinned": cfg.get("CC_PINNED", "") == "1",
        "tags": [t.strip() for t in cfg.get("CC_TAGS", "").split(",") if t.strip()],
        "flags": cfg.get("CC_FLAGS", ""),
        "running": is_running(name),
        "raw": f.read_text(),
    }


def _find_latest_session_id(work_dir: str) -> str:
    """Find the most recent Claude Code conversation session ID for a working directory.
    Skips snapshot-only files that have no user/assistant messages (claude --resume exits on those)."""
    resolved = str(Path(work_dir).expanduser().resolve())
    project_name = resolved.replace("/", "-")
    project_dir = CLAUDE_HOME / "projects" / project_name
    if not project_dir.is_dir():
        return ""
    jsonl_files = sorted(project_dir.glob("*.jsonl"), key=lambda f: f.stat().st_mtime, reverse=True)
    for f in jsonl_files:
        try:
            text = f.read_text(errors="replace")
            for line in text.splitlines():
                entry = json.loads(line)
                if entry.get("type") in ("user", "assistant"):
                    return f.stem
        except Exception:
            continue
    return ""


def _project_name(work_dir: str) -> str:
    """Return the Claude project folder name for a given work dir (mirrors Claude's own encoding)."""
    resolved = str(Path(work_dir).expanduser().resolve())
    return resolved.replace("/", "-")


def _session_actual_cwd(name: str) -> str | None:
    """Return the actual CWD of a running session's tmux pane, or None if not running."""
    try:
        r = subprocess.run(
            ["tmux", "display-message", "-t", tmux_name(name), "-p", "#{pane_current_path}"],
            capture_output=True, text=True, timeout=3,
        )
        if r.returncode == 0:
            cwd = r.stdout.strip()
            if cwd:
                return cwd
    except Exception:
        pass
    return None


_GLOBAL_MEM_FILE = CC_MEMORY / "_global.md"
_MEM_MARKER = "<!-- amux:session-memory -->"


def _session_mem_file(name: str) -> Path:
    """Return the per-session MEMORY.md file stored in ~/.amux/memory/.

    Memory is keyed by session name so each session has its own independent memory.
    """
    return CC_MEMORY / f"{name}.md"


def _session_work_dir(name: str) -> str:
    """Return the CC_DIR for a session, or empty string if not configured."""
    env_file = CC_SESSIONS / f"{name}.env"
    if env_file.exists():
        cfg = parse_env_file(env_file)
        wd = cfg.get("CC_DIR", "").strip()
        if wd:
            return str(Path(wd).expanduser().resolve())
    return ""


def _migrate_memory_files():
    """Startup migration: copy project-dir-keyed memory files to session-name-keyed.

    Only migrates sessions where there is a 1:1 mapping between session and project dir,
    and only when the session-keyed file is empty but the project-keyed file has content.
    Sessions sharing a project dir start with empty memory (no ambiguous inheritance).
    """
    if not CC_SESSIONS.exists():
        return
    from collections import defaultdict
    pname_to_sessions: dict = defaultdict(list)
    for env_file in sorted(CC_SESSIONS.glob("*.env")):
        name = env_file.stem
        session_file = CC_MEMORY / f"{name}.md"
        # Skip if session file already has content
        if session_file.exists() and session_file.stat().st_size > 0:
            continue
        wd = _session_work_dir(name)
        if not wd:
            continue
        pname = _project_name(wd)
        old_file = CC_MEMORY / f"{pname}.md"
        if old_file.exists() and old_file.stat().st_size > 0:
            pname_to_sessions[pname].append(name)
    for pname, sessions in pname_to_sessions.items():
        if len(sessions) == 1:
            name = sessions[0]
            old_file = CC_MEMORY / f"{pname}.md"
            new_file = CC_MEMORY / f"{name}.md"
            if not new_file.exists() or new_file.stat().st_size == 0:
                new_file.write_text(old_file.read_text(errors="replace"))


_migrate_memory_files()

# Backfill meta files for existing sessions that predate the metadata feature
if CC_SESSIONS.is_dir():
    for _env_f in CC_SESSIONS.glob("*.env"):
        _n = _env_f.stem
        if not _meta_path(_n).exists():
            _cfg = parse_env_file(_env_f)
            _save_meta(_n, {
                "created_at": int(_env_f.stat().st_mtime),
                "creator": _cfg.get("CC_CREATOR", ""),
                "start_count": 0,
            })


def _compose_memory(global_content: str, session_content: str) -> str:
    """Compose global + session memory into a single MEMORY.md for Claude."""
    parts = []
    if global_content.strip():
        parts.append(global_content.strip())
    parts.append(_MEM_MARKER)
    if session_content.strip():
        parts.append(session_content.strip())
    return "\n\n".join(parts) + "\n"


def _capture_claude_memory_changes(name: str, work_dir: str):
    """Capture changes Claude made to MEMORY.md during the previous session."""
    pname = _project_name(work_dir)
    claude_mem_file = CLAUDE_HOME / "projects" / pname / "memory" / "MEMORY.md"
    session_file = CC_MEMORY / f"{name}.md"
    if not claude_mem_file.exists() or claude_mem_file.is_symlink():
        return
    try:
        content = claude_mem_file.read_text(errors="replace")
        if _MEM_MARKER in content:
            session_part = content.split(_MEM_MARKER, 1)[1].strip()
        else:
            session_part = content.strip()
        if session_part:
            stored = session_file.read_text(errors="replace").strip() if session_file.exists() else ""
            if session_part != stored:
                session_file.write_text(session_part + "\n")
    except Exception:
        pass


def _write_claude_memory(name: str, work_dir: str):
    """Write composed (global + session) memory to Claude's project memory dir."""
    pname = _project_name(work_dir)
    session_file = CC_MEMORY / f"{name}.md"
    global_content = _GLOBAL_MEM_FILE.read_text(errors="replace") if _GLOBAL_MEM_FILE.exists() else ""
    session_content = session_file.read_text(errors="replace") if session_file.exists() else ""
    composed = _compose_memory(global_content, session_content)
    claude_mem_dir = CLAUDE_HOME / "projects" / pname / "memory"
    claude_mem_file = claude_mem_dir / "MEMORY.md"
    try:
        claude_mem_dir.mkdir(parents=True, exist_ok=True)
        if claude_mem_file.is_symlink():
            claude_mem_file.unlink()
        claude_mem_file.write_text(composed)
    except Exception:
        pass


def _ensure_memory(name: str, work_dir: str):
    """Ensure per-session memory file exists and write composed memory for Claude.

    Memory is keyed by session name (not project dir). Global memory from
    _global.md is composed above a marker so each session sees both.
    """
    mem_file = CC_MEMORY / f"{name}.md"

    _capture_claude_memory_changes(name, work_dir)

    if not mem_file.exists():
        mem_file.write_text("")
    if not _GLOBAL_MEM_FILE.exists():
        _GLOBAL_MEM_FILE.write_text("")

    _write_claude_memory(name, work_dir)


def start_session(name: str, extra_flags: str = "", _skip_conv_id: bool = False) -> tuple[bool, str]:
    """Start a session headless (no attach). Returns (success, message)."""
    f = CC_SESSIONS / f"{name}.env"
    if not f.exists():
        return False, f"session '{name}' not found"
    if is_running(name):
        return True, "already running"
    cfg = parse_env_file(f)
    work_dir = str(Path(cfg.get("CC_DIR", str(Path.home()))).expanduser().resolve())
    flags = cfg.get("CC_FLAGS", "")
    _ensure_memory(name, work_dir)

    # Determine session-specific conversation ID for isolation.
    # Each amux session keeps its own Claude conversation, regardless of directory.
    # Skip when the caller supplies explicit conversation flags (e.g. clone --fork-session).
    meta = _load_meta(name)
    if not _skip_conv_id:
        conv_id = meta.get("cc_conversation_id", "")
        if not conv_id:
            # First start — generate a fresh UUID so this session gets its own conversation
            conv_id = str(uuid.uuid4())
            meta["cc_conversation_id"] = conv_id
            session_flag = f"--session-id {conv_id}"
        else:
            # Subsequent start — resume this session's own conversation
            conv_file = (
                CLAUDE_HOME / "projects" / _project_name(work_dir) / f"{conv_id}.jsonl"
            )
            if conv_file.exists():
                session_flag = f"--resume {conv_id}"
            else:
                # Conversation was cleared/deleted — start fresh with the same ID
                session_flag = f"--session-id {conv_id}"
    else:
        session_flag = ""

    # Load defaults
    defaults_file = CC_HOME / "defaults.env"
    default_flags = ""
    if defaults_file.exists():
        dcfg = parse_env_file(defaults_file)
        default_flags = dcfg.get("CC_DEFAULT_FLAGS", "")

    cmd = "claude"
    if default_flags:
        cmd += f" {default_flags}"
    if flags:
        cmd += f" {flags}"
    if session_flag:
        cmd += f" {session_flag}"
    if extra_flags:
        cmd += f" {extra_flags}"
    # Default to sonnet if no --model specified anywhere
    if "--model" not in cmd:
        cmd += " --model sonnet"

    try:
        tmux_sess = tmux_name(name)
        # Create session with window naming options set upfront so Claude
        # cannot override the window title via terminal escape sequences.
        # Clear CLAUDECODE so nested-session detection doesn't block Claude
        # Source user profile to ensure PATH includes ~/.local/bin (where claude lives)
        # Then cd back to work_dir since the profile may override CWD (e.g. cd ~/Dev)
        shell_rc = "unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT; "
        for rc in [Path.home() / ".zprofile", Path.home() / ".bash_profile", Path.home() / ".profile"]:
            if rc.exists():
                shell_rc += f"source {rc} 2>/dev/null; cd {shlex.quote(work_dir)}; "
                break
        else:
            shell_rc += f"cd {shlex.quote(work_dir)}; "
        subprocess.run(
            ["tmux", "new-session", "-d", "-s", tmux_sess, "-n", name, "-c", work_dir,
             "-e", "TMUX_SESSION_NAME=" + name, shell_rc + cmd],
            check=True, capture_output=True, timeout=10,
        )
        # Lock the window name immediately (before Claude output can rename it)
        subprocess.run(
            ["tmux", "set-option", "-t", tmux_sess, "allow-rename", "off"],
            capture_output=True, timeout=5,
        )
        subprocess.run(
            ["tmux", "set-window-option", "-t", tmux_sess, "automatic-rename", "off"],
            capture_output=True, timeout=5,
        )
        # Force the window name back in case Claude already changed it
        subprocess.run(
            ["tmux", "rename-window", "-t", tmux_sess, name],
            capture_output=True, timeout=5,
        )
        meta["last_started"] = int(time.time())
        meta["start_count"] = meta.get("start_count", 0) + 1
        _save_meta(name, meta)
        return True, "started"
    except subprocess.CalledProcessError as e:
        return False, e.stderr.decode(errors="replace")
    except FileNotFoundError:
        return False, "tmux not found"


def stop_session(name: str) -> tuple[bool, str]:
    if not is_running(name):
        return True, "not running"
    try:
        subprocess.run(
            ["tmux", "kill-session", "-t", tmux_name(name)],
            check=True, capture_output=True, timeout=5,
        )
        return True, "stopped"
    except subprocess.CalledProcessError as e:
        return False, e.stderr.decode(errors="replace")


def send_text(name: str, text: str) -> tuple[bool, str]:
    if not is_running(name):
        return False, "not running"
    try:
        t = tmux_name(name)
        # Send text literally (-l) then Enter separately
        subprocess.run(
            ["tmux", "send-keys", "-t", t, "-l", text],
            check=True, capture_output=True, timeout=5,
        )
        subprocess.run(
            ["tmux", "send-keys", "-t", t, "Enter"],
            check=True, capture_output=True, timeout=5,
        )
        return True, "sent"
    except subprocess.CalledProcessError as e:
        return False, e.stderr.decode(errors="replace")


def send_keys(name: str, keys: str) -> tuple[bool, str]:
    if not is_running(name):
        return False, "not running"
    try:
        subprocess.run(
            ["tmux", "send-keys", "-t", tmux_name(name), keys],
            check=True, capture_output=True, timeout=5,
        )
        return True, "sent"
    except subprocess.CalledProcessError as e:
        return False, e.stderr.decode(errors="replace")


def list_tmux_sessions() -> list:
    """List all tmux sessions with their working dirs, excluding already-registered amux sessions."""
    registered = set()
    if CC_SESSIONS.is_dir():
        for f in CC_SESSIONS.glob("*.env"):
            registered.add(tmux_name(f.stem))
    try:
        r = subprocess.run(
            ["tmux", "list-sessions", "-F", "#{session_name}\t#{pane_current_path}"],
            capture_output=True, text=True, timeout=5,
        )
        if r.returncode != 0:
            return []
        results = []
        for line in r.stdout.strip().splitlines():
            parts = line.split("\t", 1)
            name = parts[0]
            cwd = parts[1] if len(parts) > 1 else ""
            if name not in registered:
                results.append({"tmux_name": name, "dir": cwd})
        return results
    except Exception:
        return []


def get_lan_ip() -> str:
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except Exception:
        return "127.0.0.1"


# ═══════════════════════════════════════════
# HTML DASHBOARD
# ═══════════════════════════════════════════

DASHBOARD_HTML = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">
<meta name="mobile-web-app-capable" content="yes">
<meta name="theme-color" content="#0d1117">
<link rel="manifest" href="/manifest.json">
<title>amux</title>
<link rel="icon" type="image/svg+xml" href="/icon.svg">
<link rel="icon" type="image/png" sizes="180x180" href="/icon.png">
<link rel="apple-touch-icon" href="/icon.png">
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/gridstack@7/dist/gridstack.min.css">
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  :root {
    --bg: #0d1117; --card: #161b22; --border: #30363d;
    --text: #e6edf3; --dim: #8b949e; --accent: #58a6ff;
    --green: #3fb950; --red: #f85149; --yellow: #d29922;
    --cyan: #39d2c0;
  }
  html { font-size: 16px; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
    background: var(--bg); color: var(--text);
    min-height: 100vh; min-height: 100dvh;
    max-width: 100vw; overflow-x: hidden;
    padding: 16px; padding-top: max(16px, env(safe-area-inset-top));
    padding-bottom: max(16px, env(safe-area-inset-bottom));
    -webkit-text-size-adjust: 100%;
  }
  h1 { font-size: 1.4rem; font-weight: 700; margin-bottom: 16px; }
  h1 .dim { color: var(--dim); font-weight: 400; font-size: 0.85rem; }

  /* Session cards — list mode (default) */
  .cards { display: flex; flex-direction: column; gap: 10px; }

  /* Layout view controls */
  .tile-controls { display: flex; gap: 4px; align-items: center; }
  .tile-btn { width: 28px; height: 28px; border-radius: 6px; border: 1px solid var(--border); background: transparent; color: var(--dim); cursor: pointer; font-size: 0.85rem; display: flex; align-items: center; justify-content: center; }
  .tile-btn:hover { border-color: var(--accent); color: var(--text); }
  .tile-btn.active { background: var(--accent); color: #000; border-color: var(--accent); }
  .tile-grid-only { display: none; }
  @media (min-width: 900px) {
    .tile-grid-only { display: flex; align-items: center; justify-content: center; }
    .cards.grid-mode { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 12px; align-items: start; }
  }
  /* Sortable drag feedback */
  .sortable-ghost { opacity: 0.25; background: var(--accent) !important; border-color: var(--accent) !important; }
  .sortable-chosen { box-shadow: 0 8px 32px rgba(0,0,0,0.5); z-index: 10; }
  .sortable-drag { opacity: 0; }
  .cards.grid-mode .card { cursor: grab; }
  .cards.grid-mode .card:active { cursor: grabbing; }

  .card {
    background: var(--card); border: 1px solid var(--border);
    border-radius: 10px; padding: 14px 16px; cursor: default;
    transition: border-color 0.15s; overflow: hidden;
    -webkit-tap-highlight-color: transparent; min-width: 0;
    user-select: text; -webkit-user-select: text;
  }
  .card:active { border-color: var(--accent); }
  .card-header { display: flex; flex-direction: column; gap: 4px; position: relative; min-width: 0; cursor: default; }
  .card-header-top { display: flex; align-items: center; gap: 10px; width: 100%; }
  .card-drag-handle {
    flex-shrink: 0; width: 16px; height: 20px; display: flex; align-items: center; justify-content: center;
    cursor: grab; color: var(--dim); opacity: 0; transition: opacity 0.15s; border-radius: 3px;
    user-select: none; -webkit-user-select: none; touch-action: none;
  }
  .card-drag-handle:active { cursor: grabbing; }
  .card:hover .card-drag-handle { opacity: 0.45; }
  .card-drag-handle:hover { opacity: 1 !important; color: var(--fg); background: rgba(139,148,158,0.1); }
  @media (hover: none) { .card-drag-handle { opacity: 0.35; } }
  .card-header-meta { display: flex; align-items: center; gap: 6px; margin-left: 20px; min-width: 0; }
  .card-menu-btn {
    width: 28px; height: 28px; border-radius: 6px; border: 1px solid var(--border);
    background: transparent; color: var(--dim); cursor: pointer;
    font-size: 1.2rem; font-weight: 700; line-height: 1;
    display: flex; align-items: center; justify-content: center;
    flex-shrink: 0; -webkit-tap-highlight-color: transparent; letter-spacing: 1px;
  }
  .card-menu-btn:active { background: var(--border); color: var(--text); }

  /* Card dropdown menu */
  .card-menu {
    display: none; position: fixed;
    background: var(--card); border: 1px solid var(--border);
    border-radius: 10px; min-width: 200px; z-index: 50;
    box-shadow: 0 8px 24px rgba(0,0,0,0.4);
    overflow-x: hidden; overflow-y: auto; max-height: 500px;
  }
  .card-menu.open { display: block; }
  .card-menu-item {
    display: flex; align-items: center; gap: 10px;
    padding: 12px 16px; cursor: pointer; font-size: 0.88rem;
    border-bottom: 1px solid var(--border); color: var(--text);
    -webkit-tap-highlight-color: transparent;
  }
  .card-menu-item:last-child { border-bottom: none; }
  .card-menu-item:active { background: var(--border); }
  .card-menu-item .mi { width: 18px; text-align: center; flex-shrink: 0; font-size: 0.85rem; }
  .card-menu-item.danger { color: var(--red); }
  .card-menu-sep { height: 1px; background: var(--border); }

  /* Edit modal */
  .edit-overlay {
    display: none; position: fixed; inset: 0; background: rgba(1,4,9,0.85);
    z-index: 300; align-items: center; justify-content: center; padding: 20px;
  }
  .edit-overlay.active { display: flex; }
  .edit-box {
    background: var(--card); border: 1px solid var(--border); border-radius: 12px;
    padding: 20px; width: 100%; max-width: 380px;
  }
  .edit-box h3 { font-size: 1rem; margin-bottom: 14px; }
  .edit-box input, .edit-box select, .edit-box textarea {
    width: 100%; font-size: 0.95rem; padding: 10px 12px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--bg); color: var(--text);
    outline: none; margin-bottom: 14px; font-family: inherit; box-sizing: border-box;
  }
  .edit-box textarea { resize: vertical; min-height: 72px; }
  .edit-box select { -webkit-appearance: menulist; }
  .edit-box input:focus, .edit-box select:focus, .edit-box textarea:focus {
    border-color: var(--accent); box-shadow: 0 0 0 3px rgba(88,166,255,0.12);
  }
  .edit-box .edit-actions { display: flex; gap: 8px; justify-content: flex-end; margin-top: 4px; }
  /* Labeled field groups */
  .field-group { margin-bottom: 14px; }
  .field-group > input, .field-group > textarea, .field-group > select,
  .field-group > .ac-wrap, .field-group > .ac-wrap > input { margin-bottom: 0; }
  .field-label { display: block; font-size: 0.72rem; color: var(--dim); font-weight: 600;
    text-transform: uppercase; letter-spacing: 0.04em; margin-bottom: 6px; }
  .field-optional { font-weight: 400; text-transform: none; letter-spacing: 0; font-size: 0.68rem; }
  .dot {
    width: 10px; height: 10px; border-radius: 50%; flex-shrink: 0;
  }
  .dot.running { background: var(--green); box-shadow: 0 0 6px var(--green); }
  .dot.stopped { background: var(--red); opacity: 0.5; }
  .card-name { font-weight: 600; font-size: 1.05rem; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .card-dir { color: var(--dim); font-size: 0.82rem; margin-top: 4px; margin-left: 20px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; display: flex; align-items: center; gap: 5px; }
  .card-dir-path { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .card-dir-edit { flex-shrink: 0; opacity: 0.3; transition: opacity 0.15s; cursor: pointer; font-size: 0.85rem; padding: 0 2px; border-radius: 3px; }
  .card-dir-edit:hover { color: var(--accent); opacity: 1 !important; }
  .card-preview { color: var(--dim); font-size: 0.78rem; margin-top: 4px; margin-left: 20px; font-family: "SF Mono", "Fira Code", "Cascadia Code", monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .card-preview-lines {
    color: var(--dim); font-size: 0.75rem; font-family: "SF Mono", "Fira Code", "Cascadia Code", monospace;
    white-space: pre-wrap; word-break: break-all; overflow-wrap: anywhere;
    background: rgba(1,4,9,0.5); border-radius: 6px; padding: 8px 10px;
    margin-bottom: 8px; line-height: 1.4; max-height: 80px; overflow: hidden;
  }
  .badges { display: flex; gap: 6px; margin-top: 6px; margin-left: 20px; flex-wrap: nowrap; overflow-x: auto; -webkit-overflow-scrolling: touch; scrollbar-width: none; }
  .badges::-webkit-scrollbar { display: none; }
  .badge {
    font-size: 0.7rem; padding: 2px 7px; border-radius: 4px;
    font-weight: 600; text-transform: uppercase; white-space: nowrap; flex-shrink: 0;
  }
  .badge.yolo { background: rgba(210,153,34,0.2); color: var(--yellow); }
  .badge.model { background: rgba(57,210,192,0.2); color: var(--cyan); }

  /* Expanded panel */
  .panel { display: none; margin-top: 12px; }
  .card.expanded .panel { display: block; }
  .panel-actions { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 10px; }
  .btn {
    font-size: 0.85rem; padding: 8px 14px; border-radius: 8px; border: 1px solid var(--border);
    background: var(--card); color: var(--text); cursor: pointer; font-weight: 500;
    -webkit-tap-highlight-color: transparent;
    min-height: 40px;
  }
  .btn:active { background: var(--border); }
  .btn.primary { background: var(--accent); color: #fff; border-color: var(--accent); }
  .btn.danger { border-color: var(--red); color: var(--red); }
  .chips { display: flex; gap: 6px; flex-wrap: nowrap; margin-bottom: 10px; overflow-x: auto; -webkit-overflow-scrolling: touch; scrollbar-width: none; }
  .chips::-webkit-scrollbar { display: none; }
  .chip {
    font-size: 0.78rem; padding: 6px 12px; border-radius: 16px;
    background: rgba(88,166,255,0.12); color: var(--accent);
    border: 1px solid rgba(88,166,255,0.25); cursor: pointer;
    -webkit-tap-highlight-color: transparent;
    min-height: 34px; display: flex; align-items: center;
    white-space: nowrap; flex-shrink: 0;
  }
  .chip:active { background: rgba(88,166,255,0.25); }
  .chip.danger { background: rgba(248,81,73,0.12); color: var(--red); border-color: rgba(248,81,73,0.25); }
  .chip.danger:active { background: rgba(248,81,73,0.25); }
  .send-row { display: flex; gap: 8px; min-width: 0; overflow: hidden; }
  .send-input {
    flex: 1; min-width: 0; font-size: 1rem; padding: 10px 14px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--bg); color: var(--text);
    outline: none; min-height: 44px; max-height: calc(1.4em * 10 + 28px);
    resize: none; overflow-x: hidden; overflow-y: auto; line-height: 1.4;
    font-family: inherit; field-sizing: content; word-break: break-word;
  }
  .send-input:focus { border-color: var(--accent); box-shadow: 0 0 0 3px rgba(88,166,255,0.12); }

  /* Peek overlay */
  .overlay {
    position: fixed; top: 0; left: 0; right: 0;
    height: 100%; height: 100dvh;
    background: var(--bg);
    z-index: 100; flex-direction: column;
  }
  /* board-detail sits above peek when opened from within it */
  #board-detail-overlay { z-index: 150; }
  .overlay {
    padding: 12px; padding-top: max(12px, env(safe-area-inset-top));
    overflow: hidden;
    display: flex; pointer-events: none; opacity: 0;
    transform: translateY(12px);
    transition: opacity 0.25s, transform 0.25s cubic-bezier(.4,0,.2,1);
  }
  .overlay.active { pointer-events: auto; opacity: 1; transform: none; }
  .overlay-header {
    display: flex; justify-content: space-between; align-items: center;
    margin-bottom: 8px; flex-shrink: 0;
  }
  .overlay-header h2 { font-size: 1.1rem; }
  .overlay-body {
    flex: 1; min-height: 0; overflow-x: hidden; overflow-y: auto;
    background: #010409; border-radius: 8px;
    padding: 10px; font-family: "SF Mono", "Fira Code", "Cascadia Code", monospace;
    font-size: 0.78rem; line-height: 1.4; white-space: pre-wrap;
    word-break: break-all; overflow-wrap: anywhere;
    -webkit-overflow-scrolling: touch;
    -webkit-user-select: text; user-select: text;
    -webkit-touch-callout: default; cursor: text;
    touch-action: pan-y;
  }
  .peek-copy-btn {
    position: absolute; top: 6px; right: 6px; z-index: 10;
    height: 28px; border-radius: 6px; padding: 0 8px;
    border: 1px solid rgba(255,255,255,0.15); background: rgba(13,17,23,0.85);
    color: var(--dim); font-size: 0.75rem; cursor: pointer;
    display: flex; align-items: center; gap: 4px;
    -webkit-tap-highlight-color: transparent; transition: all 0.15s;
    white-space: nowrap;
  }
  .peek-copy-btn:active { background: var(--accent); color: #fff; }
  .overlay-body a { color: var(--accent); text-decoration: underline; text-underline-offset: 2px; }
  .overlay-body a:active { color: #79c0ff; }
  .overlay-body .file-link { color: var(--cyan); text-decoration: none; border-bottom: 1px dashed var(--cyan); cursor: pointer; }
  .overlay-body .file-link:active { color: #79ead3; }
  .overlay-body .md-link { color: var(--yellow); text-decoration: none; border-bottom: 1px dashed var(--yellow); cursor: pointer; }
  .overlay-body .md-link:active { color: #e8c547; }
  .overlay-status { color: var(--dim); font-size: 0.75rem; margin-top: 6px; flex-shrink: 0; text-align: center; }

  /* File preview overlay */
  .file-overlay {
    display: none; position: fixed; inset: 0; background: rgba(1,4,9,0.92);
    z-index: 200; flex-direction: column;
    padding: 12px; padding-top: max(12px, env(safe-area-inset-top));
  }
  .file-overlay.active { display: flex; }
  .file-overlay-header {
    display: flex; justify-content: space-between; align-items: center;
    margin-bottom: 8px; flex-shrink: 0;
  }
  .file-overlay-header h2 { font-size: 1rem; word-break: break-all; flex: 1; margin-right: 8px; }
  .file-overlay-body {
    flex: 1; overflow: auto; background: #010409; border: 1px solid var(--border); border-radius: 8px;
    padding: 14px; font-family: "SF Mono", "Fira Code", "Cascadia Code", monospace;
    font-size: 0.78rem; line-height: 1.5; white-space: pre-wrap;
    word-break: break-word; -webkit-overflow-scrolling: touch;
  }
  .file-view-tabs { display: flex; gap: 3px; flex-shrink: 0; }
  .file-view-tab { padding: 4px 11px; border-radius: 6px; border: 1px solid var(--border);
    background: none; color: var(--dim); font-size: 0.78rem; cursor: pointer;
    -webkit-tap-highlight-color: transparent; transition: all 0.15s; }
  .file-view-tab.active { background: var(--accent); color: #fff; border-color: var(--accent); }
  .file-overlay-body.file-raw { white-space: pre-wrap; word-break: break-word; }
  .file-overlay-body.file-image { display:flex;align-items:center;justify-content:center;background:var(--bg);white-space:normal; }
  .file-overlay-body.file-pdf { padding:0;background:var(--bg);white-space:normal; }
  .file-overlay-body.file-csv { white-space:normal;overflow:auto; }
  .csv-wrap { overflow:auto; }
  .csv-table { border-collapse:collapse;font-size:0.78rem;width:max-content;min-width:100%; }
  .csv-table th,.csv-table td { border:1px solid var(--border);padding:4px 10px;text-align:left;white-space:nowrap; }
  .csv-table th { background:var(--card);font-weight:600;position:sticky;top:0;z-index:1; }
  .csv-table tr:nth-child(even) td { background:rgba(255,255,255,0.02); }
  .file-overlay-body.markdown { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif; font-size: 0.88rem; }
  .file-overlay-body.markdown h1, .file-overlay-body.markdown h2, .file-overlay-body.markdown h3 { margin: 16px 0 8px 0; font-weight: 700; }
  .file-overlay-body.markdown h1 { font-size: 1.3rem; border-bottom: 1px solid var(--border); padding-bottom: 6px; }
  .file-overlay-body.markdown h2 { font-size: 1.1rem; border-bottom: 1px solid var(--border); padding-bottom: 4px; }
  .file-overlay-body.markdown h3 { font-size: 0.95rem; }
  .file-overlay-body.markdown p { margin: 8px 0; }
  .file-overlay-body.markdown code { background: rgba(88,166,255,0.1); padding: 2px 5px; border-radius: 3px; font-family: "SF Mono", "Fira Code", monospace; font-size: 0.82rem; }
  .file-overlay-body.markdown pre { background: var(--card); border: 1px solid var(--border); border-radius: 6px; padding: 10px; overflow-x: auto; margin: 8px 0; }
  .file-overlay-body.markdown pre code { background: none; padding: 0; }
  .file-overlay-body.markdown ul, .file-overlay-body.markdown ol { padding-left: 20px; margin: 8px 0; }
  .file-overlay-body.markdown li { margin: 4px 0; }
  .file-overlay-body.markdown a { color: var(--accent); }
  .file-overlay-body.markdown blockquote { border-left: 3px solid var(--border); padding-left: 12px; color: var(--dim); margin: 8px 0; }
  .file-overlay-body.markdown strong { font-weight: 700; }
  .file-overlay-body.markdown em { font-style: italic; }
  .file-overlay-body.markdown hr { border: none; border-top: 1px solid var(--border); margin: 16px 0; }

  /* File explorer */
  .explore-breadcrumb { font-size: 0.8rem; overflow-x: auto; white-space: nowrap; scrollbar-width: none; -webkit-overflow-scrolling: touch; }
  .explore-breadcrumb::-webkit-scrollbar { display: none; }
  .explore-crumb { color: var(--accent); cursor: pointer; }
  .explore-crumb:hover { text-decoration: underline; }
  .explore-row { display: flex; align-items: center; gap: 10px; padding: 11px 16px; border-bottom: 1px solid var(--border); cursor: pointer; -webkit-tap-highlight-color: transparent; }
  .explore-row:active { background: var(--hover); }
  .explore-icon { font-size: 1rem; flex-shrink: 0; line-height: 1; }
  .explore-name { font-size: 0.88rem; flex: 1; word-break: break-all; min-width: 0; }
  .explore-size { font-size: 0.72rem; color: var(--dim); flex-shrink: 0; }
  .explore-menu-btn { flex-shrink: 0; background: none; border: none; color: var(--dim);
    cursor: pointer; font-size: 1rem; padding: 2px 6px; border-radius: 4px; line-height: 1;
    opacity: 0.4; transition: opacity 0.15s; }
  .explore-row:hover .explore-menu-btn, .explore-menu-btn:focus { opacity: 1; }
  .explore-menu-popup { position: fixed; background: var(--card); border: 1px solid var(--border);
    border-radius: 8px; box-shadow: 0 4px 16px rgba(0,0,0,0.3); z-index: 900; min-width: 140px;
    overflow: hidden; }
  .explore-menu-item { display: block; width: 100%; background: none; border: none; text-align: left;
    padding: 11px 16px; font-size: 0.88rem; color: var(--text); cursor: pointer; }
  .explore-menu-item:active, .explore-menu-item:hover { background: var(--hover); }

  /* Connect session list */
  /* Calendar */
  .cal-toolbar { display: flex; flex-direction: column; gap: 4px; padding: 8px 12px 4px; }
  .cal-nav-row { display: flex; align-items: center; gap: 6px; }
  .cal-controls-row { display: flex; align-items: center; gap: 6px; }
  .cal-title { font-weight: 600; font-size: 0.95rem; flex: 1; text-align: center; }
  .cal-grid { display: grid; grid-template-columns: repeat(7, 1fr); gap: 1px; background: var(--border); border-radius: 8px; overflow: hidden; margin: 0 8px 16px; }
  .cal-day-header { background: var(--card); text-align: center; font-size: 0.68rem; color: var(--dim); padding: 5px 2px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.03em; }
  .cal-cell { background: var(--card); min-height: 76px; padding: 4px; position: relative; cursor: pointer; -webkit-tap-highlight-color: transparent; }
  .cal-cell:active { background: var(--hover); }
  .cal-cell.other-month { background: rgba(0,0,0,0.15); }
  .cal-cell.other-month .cal-cell-num { opacity: 0.35; }
  .cal-cell-num { font-size: 0.75rem; color: var(--dim); margin-bottom: 3px; width: 20px; height: 20px; display: flex; align-items: center; justify-content: center; border-radius: 50%; }
  .cal-cell.today .cal-cell-num { background: var(--accent); color: #fff; font-weight: 700; }
  .cal-chip { font-size: 0.66rem; line-height: 1.25; padding: 2px 4px; border-radius: 3px; margin-bottom: 2px; cursor: pointer; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 100%; display: block; }
  .cal-chip:active { opacity: 0.7; }
  .cal-more { font-size: 0.62rem; color: var(--dim); padding-left: 2px; }
  .cal-dots { display: none; gap: 3px; flex-wrap: wrap; padding: 3px 1px 0; }
  .cal-dot { width: 6px; height: 6px; border-radius: 50%; flex-shrink: 0; }
  @media (max-width: 480px) {
    .cal-grid { margin: 0 0 12px; border-radius: 0; gap: 0; background: none; border-top: 1px solid var(--border); border-left: 1px solid var(--border); }
    .cal-cell { min-height: unset; aspect-ratio: 1; padding: 3px; border-right: 1px solid var(--border); border-bottom: 1px solid var(--border); overflow: hidden; }
    .cal-day-header { font-size: 0.65rem; padding: 6px 2px; border-right: 1px solid var(--border); border-bottom: 1px solid var(--border); }
    .cal-cell-num { font-size: 0.7rem; width: 18px; height: 18px; margin-bottom: 0; }
    .cal-chip { display: none; }
    .cal-more { display: none; }
    .cal-dots { display: flex; }
    .cal-title { font-size: 0.88rem; }
    .cal-toolbar { padding: 6px 8px 2px; }
    .cal-nav-row .btn, .cal-controls-row .btn { padding: 5px 8px; font-size: 0.78rem; }
    .cal-view-tab { padding: 4px 10px; font-size: 0.75rem; }
    .cal-week-cell { min-height: 80px; }
    .cal-week-chip { display: none !important; }
    .cal-week-dot { display: block !important; }
  }
  /* Calendar view tabs */
  .cal-view-tabs { display: flex; gap: 3px; flex: 1; justify-content: center; }
  .cal-view-tab { padding: 5px 14px; border-radius: 6px; border: 1px solid var(--border);
    background: none; color: var(--dim); font-size: 0.82rem; cursor: pointer;
    -webkit-tap-highlight-color: transparent; transition: all 0.15s; }
  .cal-view-tab.active { background: var(--accent); color: #fff; border-color: var(--accent); }
  /* Week view */
  .cal-week-grid { display: grid; grid-template-columns: repeat(7,1fr); gap: 1px;
    background: var(--border); border-radius: 8px; overflow: hidden; margin: 0 8px 16px; }
  .cal-week-header { background: var(--card); text-align: center; font-size: 0.68rem;
    color: var(--dim); padding: 5px 2px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.03em; }
  .cal-week-cell { background: var(--card); min-height: 120px; padding: 6px 5px; cursor: pointer;
    -webkit-tap-highlight-color: transparent; }
  .cal-week-cell:active { background: var(--hover); }
  .cal-week-cell.today { border-top: 2px solid var(--accent); }
  .cal-week-cell.today .cal-week-num { color: var(--accent); font-weight: 700; }
  .cal-week-num { font-size: 0.72rem; color: var(--dim); margin-bottom: 4px; }
  .cal-week-chip { font-size: 0.67rem; line-height: 1.3; padding: 2px 5px; border-radius: 3px;
    margin-bottom: 2px; cursor: pointer; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
    display: block; }
  .cal-week-chip:active { opacity: 0.7; }
  .cal-week-more { font-size: 0.62rem; color: var(--dim); padding-left: 2px; }
  .cal-week-dot { display: none; width: 6px; height: 6px; border-radius: 50%; margin: 1px; }
  /* Day view */
  .cal-day-view { padding: 6px 12px 16px; }
  .cal-day-issue { background: var(--card); border: 1px solid var(--border); border-radius: 8px;
    padding: 10px 12px; margin-bottom: 8px; cursor: pointer;
    -webkit-tap-highlight-color: transparent; display: flex; align-items: flex-start; gap: 10px; }
  .cal-day-issue:active { border-color: var(--accent); }
  .cal-day-issue-text { flex: 1; min-width: 0; }
  .cal-day-issue-title { font-size: 0.9rem; font-weight: 500; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .cal-day-issue-desc { font-size: 0.78rem; color: var(--dim); margin-top: 3px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .cal-day-empty { color: var(--dim); font-size: 0.88rem; text-align: center; padding: 32px 0; }
  .cal-day-add { display: block; width: 100%; margin-top: 4px; text-align: center;
    padding: 10px; border-radius: 8px; border: 1px dashed var(--border); background: none;
    color: var(--dim); font-size: 0.82rem; cursor: pointer; -webkit-tap-highlight-color: transparent; }
  .cal-day-add:active { background: var(--hover); }
  /* Board collapse */
  .board-col-collapse { background: none; border: none; cursor: pointer; color: var(--dim);
    font-size: 0.65rem; padding: 4px 6px; border-radius: 3px; line-height: 1; flex-shrink: 0;
    -webkit-tap-highlight-color: transparent; transition: color 0.15s; min-width: 28px; min-height: 28px;
    display: flex; align-items: center; justify-content: center; }
  .board-col-collapse:hover { color: var(--text); }
  .board-col.col-collapsed { min-height: unset !important; }
  .board-col.col-collapsed > :not(.board-col-header) { display: none !important; }
  .skill-card {
    background: var(--card); border: 1px solid var(--border); border-radius: 8px;
    padding: 12px 14px; display: flex; flex-direction: column; gap: 4px;
  }
  .skill-card-name { font-family: "SF Mono","Fira Code",monospace; font-size: 0.88rem; font-weight: 600; color: var(--accent); }
  .skill-card-desc { font-size: 0.85rem; color: var(--text); }
  .skill-card-hint { font-size: 0.75rem; color: var(--dim); font-family: "SF Mono","Fira Code",monospace; }
  .connect-item {
    display: flex; align-items: center; justify-content: space-between;
    padding: 10px 12px; border: 1px solid var(--border); border-radius: 8px;
    margin-bottom: 8px; cursor: pointer; -webkit-tap-highlight-color: transparent;
  }
  .connect-item:active { border-color: var(--accent); background: rgba(88,166,255,0.08); }
  .connect-item-info { flex: 1; min-width: 0; }
  .connect-item-name { font-weight: 600; font-size: 0.95rem; }
  .connect-item-dir { color: var(--dim); font-size: 0.78rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .connect-empty { color: var(--dim); text-align: center; padding: 20px; font-size: 0.9rem; }

  /* Create session */
  .header-row {
    display: flex; align-items: center; justify-content: space-between;
    position: sticky; top: 0; z-index: 40;
    background: var(--bg); padding: 16px;
    padding-top: max(16px, env(safe-area-inset-top));
    margin: -16px -16px 0 -16px;
  }
  .header-row h1 { margin-bottom: 0; }
  .btn-create {
    font-size: 0.85rem; padding: 8px 14px; border-radius: 8px;
    border: 1px solid var(--accent); background: var(--accent); color: #fff;
    cursor: pointer; font-weight: 600; -webkit-tap-highlight-color: transparent;
  }
  .btn-create:active { opacity: 0.8; }

  /* Active sessions dropdown */
  .active-wrap { position: relative; }
  .btn-active {
    font-size: 0.85rem; padding: 8px 12px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--card); color: var(--text);
    cursor: pointer; font-weight: 500; -webkit-tap-highlight-color: transparent;
    min-height: 40px; display: flex; align-items: center; gap: 6px;
  }
  .btn-active:active { background: var(--border); }
  .btn-active .active-dot {
    width: 7px; height: 7px; border-radius: 50%;
    background: var(--green); box-shadow: 0 0 5px var(--green);
  }
  .btn-active .active-count {
    font-variant-numeric: tabular-nums;
  }
  .active-dropdown {
    display: none; position: fixed; top: auto; right: 16px;
    background: var(--card); border: 1px solid var(--border);
    border-radius: 10px; min-width: 240px; max-width: 320px; z-index: 60;
    box-shadow: 0 8px 24px rgba(0,0,0,0.4); overflow: hidden;
    max-height: 300px; overflow-y: auto;
  }
  .active-dropdown.open { display: block; }
  @media (max-width: 480px) {
    .active-dropdown { left: 16px; right: 16px; min-width: 0; max-width: none; }
  }
  .active-dropdown-item {
    display: flex; align-items: center; gap: 10px;
    padding: 10px 14px; cursor: pointer; border-bottom: 1px solid var(--border);
    -webkit-tap-highlight-color: transparent;
  }
  .active-dropdown-item:last-child { border-bottom: none; }
  .active-dropdown-item:active { background: rgba(88,166,255,0.1); }
  .active-dropdown-item .adi-info { flex: 1; min-width: 0; }
  .active-dropdown-item .adi-name { font-weight: 600; font-size: 0.88rem; }
  .active-dropdown-item .adi-dir { color: var(--dim); font-size: 0.72rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .active-dropdown-item .adi-preview { color: var(--dim); font-size: 0.7rem; font-family: "SF Mono", "Fira Code", monospace; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-top: 2px; }
  .active-dropdown-empty { color: var(--dim); text-align: center; padding: 16px; font-size: 0.85rem; }
  .active-dropdown-item .adi-arrow { color: var(--dim); font-size: 0.8rem; flex-shrink: 0; }

  /* Autocomplete dropdown */
  .ac-wrap { position: relative; }
  .ac-list {
    position: absolute; top: 100%; left: 0; right: 0; z-index: 10;
    background: var(--card); border: 1px solid var(--border); border-top: none;
    border-radius: 0 0 8px 8px; max-height: 180px; overflow-y: auto;
    display: none;
  }
  .ac-list.open { display: block; }
  .ac-list.slash-ac {
    top: auto; bottom: 100%; border-top: 1px solid var(--border); border-bottom: none;
    border-radius: 8px 8px 0 0; max-height: 220px;
  }
  .ac-item .ac-desc { font-family: -apple-system, sans-serif; color: var(--dim); font-size: 0.75rem; margin-left: 8px; }
  .ac-item {
    padding: 8px 12px; font-size: 0.88rem; cursor: pointer;
    font-family: "SF Mono", "Fira Code", monospace; color: var(--text);
    border-bottom: 1px solid var(--border);
  }
  .ac-item:last-child { border-bottom: none; }
  .ac-item:active, .ac-item.selected { background: rgba(88,166,255,0.15); }

  /* Search input with clear button */
  .search-wrap {
    position: relative; display: flex; align-items: center;
  }
  .search-wrap { flex: 1; }
  .search-input {
    font-size: 0.85rem; padding: 8px 28px 8px 12px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--card); color: var(--text);
    outline: none; min-height: 40px; width: 100%; box-sizing: border-box;
    -webkit-tap-highlight-color: transparent;
  }
  .search-input::placeholder { color: var(--dim); }
  .search-input:focus { border-color: var(--accent); box-shadow: 0 0 0 3px rgba(88,166,255,0.12); }
  .search-clear {
    position: absolute; right: 6px; top: 50%; transform: translateY(-50%);
    width: 20px; height: 20px; border-radius: 50%; border: none;
    background: var(--border); color: var(--dim); font-size: 0.7rem;
    cursor: pointer; display: none; align-items: center; justify-content: center;
    line-height: 1; -webkit-tap-highlight-color: transparent;
  }
  .search-clear:active { background: var(--dim); color: var(--text); }
  .search-wrap.has-value .search-clear { display: flex; }
  .search-count {
    position: absolute; right: 30px; top: 50%; transform: translateY(-50%);
    font-size: 0.65rem; color: var(--yellow); font-weight: 600;
    pointer-events: none; white-space: nowrap;
  }
  @media (max-width: 480px) {
    .search-input { min-width: 0; }
    .search-input:focus { }
    .header-row { gap: 4px; flex-wrap: nowrap; }
    .header-row h1 { font-size: 1.1rem; flex-shrink: 0; }
    .header-row > div { gap: 6px !important; flex-shrink: 1; min-width: 0; }
  }

  /* Header + dropdown */
  .header-add-wrap { position: relative; }
  .header-add-btn {
    font-size: 1.1rem; width: 40px; height: 40px; border-radius: 8px;
    border: 1px solid var(--accent); background: var(--accent); color: #fff;
    cursor: pointer; font-weight: 700; display: flex; align-items: center;
    justify-content: center; -webkit-tap-highlight-color: transparent;
  }
  .header-add-btn:active { opacity: 0.8; }
  .header-add-menu {
    display: none; position: absolute; top: calc(100% + 6px); right: 0;
    background: var(--card); border: 1px solid var(--border);
    border-radius: 10px; min-width: 180px; z-index: 60;
    box-shadow: 0 8px 24px rgba(0,0,0,0.4); overflow: hidden;
  }
  .header-add-menu.open { display: block; }
  .header-add-menu .card-menu-item { padding: 12px 16px; }

  /* Settings dropdown */
  /* ── DevTools Panel ── */
  #devtools-panel {
    position: fixed; bottom: 0; left: 0; right: 0; height: 340px;
    background: #0d1117; border-top: 2px solid #30363d; z-index: 10000;
    display: none; flex-direction: column; font-family: "SF Mono","Fira Code","Cascadia Code",monospace;
    font-size: 12px; color: #c9d1d9; box-shadow: 0 -4px 20px rgba(0,0,0,0.5);
  }
  #devtools-panel.open { display: flex; }
  .dt-resize-handle {
    height: 4px; cursor: ns-resize; background: transparent; flex-shrink: 0;
    border-bottom: 1px solid #30363d;
  }
  .dt-resize-handle:hover { background: var(--accent); }
  .dt-header {
    display: flex; align-items: center; gap: 0; padding: 0 8px;
    border-bottom: 1px solid #30363d; flex-shrink: 0; height: 32px;
  }
  .dt-tabs { display: flex; gap: 0; }
  .dt-tab {
    background: none; border: none; color: #8b949e; font-size: 11px; font-weight: 500;
    padding: 0 12px; height: 32px; cursor: pointer; border-bottom: 2px solid transparent;
    transition: color 0.15s; white-space: nowrap;
  }
  .dt-tab:hover { color: #c9d1d9; }
  .dt-tab.active { color: #58a6ff; border-bottom-color: #58a6ff; }
  .dt-toolbar { margin-left: auto; display: flex; gap: 4px; align-items: center; }
  .dt-toolbar-btn {
    background: none; border: none; color: #8b949e; font-size: 13px; padding: 3px 6px;
    cursor: pointer; border-radius: 4px; line-height: 1;
  }
  .dt-toolbar-btn:hover { background: #21262d; color: #c9d1d9; }
  .dt-panel { flex: 1; min-height: 0; display: none; flex-direction: column; }
  .dt-panel.active { display: flex; }
  .dt-log-area {
    flex: 1; overflow-y: auto; padding: 4px 0;
    scrollbar-width: thin; scrollbar-color: #30363d transparent;
  }
  .dt-log-area::-webkit-scrollbar { width: 6px; }
  .dt-log-area::-webkit-scrollbar-thumb { background: #30363d; border-radius: 3px; }
  .dt-entry {
    padding: 2px 10px; border-bottom: 1px solid rgba(48,54,61,0.3);
    white-space: pre-wrap; word-break: break-all; line-height: 1.5;
    display: flex; gap: 8px;
  }
  .dt-entry:hover { background: #161b22; }
  .dt-entry.error { color: #f85149; background: rgba(248,81,73,0.06); }
  .dt-entry.warn  { color: #d29922; background: rgba(210,153,34,0.06); }
  .dt-entry.info  { color: #58a6ff; }
  .dt-entry.debug { color: #6e7681; }
  .dt-ts { color: #6e7681; flex-shrink: 0; font-size: 10px; margin-top: 2px; }
  .dt-msg { flex: 1; min-width: 0; }
  .dt-repl-row {
    display: flex; align-items: center; gap: 6px; padding: 5px 8px;
    border-top: 1px solid #30363d; flex-shrink: 0; background: #0d1117;
  }
  .dt-prompt { color: #3fb950; font-weight: 700; flex-shrink: 0; }
  .dt-repl-input {
    flex: 1; background: none; border: none; color: #c9d1d9; font-family: inherit;
    font-size: 12px; outline: none; min-width: 0;
  }
  .dt-repl-input::placeholder { color: #484f58; }
  .dt-net-entry {
    padding: 3px 10px; border-bottom: 1px solid rgba(48,54,61,0.3);
    display: flex; gap: 10px; align-items: center; white-space: nowrap; overflow: hidden;
  }
  .dt-net-entry:hover { background: #161b22; }
  .dt-net-method { font-weight: 700; min-width: 42px; color: #58a6ff; }
  .dt-net-status { min-width: 36px; }
  .dt-net-status.ok { color: #3fb950; }
  .dt-net-status.error { color: #f85149; }
  .dt-net-status.err { color: #f85149; }
  .dt-net-status.pending { color: #6e7681; }
  .dt-net-ms { color: #6e7681; min-width: 52px; }
  .dt-net-url { overflow: hidden; text-overflow: ellipsis; color: #c9d1d9; }
  .dt-info-row { padding: 4px 10px; display: flex; gap: 8px; border-bottom: 1px solid rgba(48,54,61,0.3); }
  .dt-info-key { color: #58a6ff; min-width: 160px; flex-shrink: 0; }
  .dt-info-val { color: #c9d1d9; word-break: break-all; }
  .settings-wrap { position: relative; }
  .settings-btn {
    font-size: 1.1rem; width: 40px; height: 40px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--card); color: var(--dim);
    cursor: pointer; display: flex; align-items: center;
    justify-content: center; -webkit-tap-highlight-color: transparent;
  }
  .settings-btn:active { background: var(--border); }
  .settings-menu {
    display: none; position: absolute; top: calc(100% + 6px); right: 0;
    background: var(--card); border: 1px solid var(--border);
    border-radius: 10px; min-width: 260px; z-index: 60;
    box-shadow: 0 8px 24px rgba(0,0,0,0.4); overflow: hidden;
    padding: 10px 0;
  }
  .settings-menu.open { display: block; }
  .settings-section { padding: 6px 14px; }
  .settings-section-label { font-size: 0.68rem; color: var(--dim); text-transform: uppercase; letter-spacing: 0.04em; margin-bottom: 6px; font-weight: 600; }
  .settings-row { display: flex; align-items: center; gap: 8px; margin-bottom: 6px; }
  .settings-row input {
    flex: 1; font-size: 0.78rem; padding: 6px 8px; border-radius: 6px;
    border: 1px solid var(--border); background: var(--bg); color: var(--text);
    outline: none; box-sizing: border-box; min-width: 0;
  }
  .settings-row input:focus { border-color: var(--accent); box-shadow: 0 0 0 3px rgba(88,166,255,0.12); }
  .settings-row input::placeholder { color: var(--dim); }
  .settings-sep { height: 1px; background: var(--border); margin: 6px 0; }
  .settings-server-item {
    display: flex; align-items: center; justify-content: space-between;
    padding: 6px 8px; border-radius: 6px; cursor: pointer; margin-bottom: 2px;
    transition: background 0.12s;
  }
  .settings-server-item:active { background: rgba(88,166,255,0.1); }
  .settings-server-current { background: rgba(88,166,255,0.08); cursor: default; }
  .settings-server-name { font-size: 0.78rem; font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .settings-server-url { font-size: 0.65rem; color: var(--dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .settings-server-badge { font-size: 0.6rem; color: var(--accent); flex-shrink: 0; margin-left: 6px; }

  /* Peek search highlight */
  .peek-highlight { background: rgba(210,153,34,0.4); color: #fff; border-radius: 2px; }

  /* Peek command bar */
  .peek-cmd-bar { flex-shrink: 0; }
  .peek-cmd-toggle {
    width: 100%; padding: 6px; border: none; background: transparent;
    color: var(--dim); font-size: 0.75rem; cursor: pointer; text-align: center;
    -webkit-tap-highlight-color: transparent;
  }
  .peek-cmd-toggle:active { color: var(--text); }
  .peek-cmd-row {
    display: none; gap: 8px; padding-top: 6px;
  }
  .peek-cmd-row.open { display: flex; min-width: 0; overflow: hidden; }
  .peek-cmd-row .send-input { font-size: 0.85rem; padding: 8px 12px; min-height: 36px; min-width: 0; }
  .peek-cmd-row .btn { min-height: 36px; padding: 6px 12px; font-size: 0.82rem; }
  /* File attachment bar */
  .peek-attach-bar { display: none; gap: 6px; padding: 4px 0 2px; flex-wrap: wrap; width: 100%; }
  .peek-attach-bar.has-files { display: flex; }
  .peek-attach-chip {
    display: flex; align-items: center; gap: 5px; padding: 3px 6px 3px 5px;
    background: var(--card); border: 1px solid var(--border); border-radius: 20px;
    font-size: 0.72rem; max-width: 180px; user-select: none;
  }
  .peek-attach-chip.uploading { opacity: 0.55; }
  .peek-attach-chip img { width: 24px; height: 24px; object-fit: cover; border-radius: 3px; flex-shrink: 0; }
  .peek-attach-chip .chip-icon { font-size: 1rem; line-height: 1; flex-shrink: 0; }
  .peek-attach-chip .chip-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; min-width: 0; }
  .peek-attach-chip .chip-remove { cursor: pointer; color: var(--dim); flex-shrink: 0; font-size: 1.1rem; line-height: 1; opacity: 0.7; }
  .peek-attach-chip .chip-remove:hover { color: var(--red); opacity: 1; }
  .peek-attach-btn { background: none; border: 1px solid var(--border); border-radius: 6px;
    color: var(--dim); cursor: pointer; padding: 0 8px; font-size: 1.1rem; min-height: 36px;
    display: flex; align-items: center; flex-shrink: 0; }
  .peek-attach-btn:hover { color: var(--text); border-color: var(--accent); }
  /* Drag-over overlay */
  #peek-overlay.drag-over { outline: 2px dashed var(--accent); outline-offset: -3px; }
  #peek-overlay.drag-over .peek-drag-hint {
    display: flex !important; position: absolute; inset: 0; z-index: 200;
    align-items: center; justify-content: center; pointer-events: none;
    background: rgba(0,0,0,0.55); font-size: 1.3rem; color: var(--accent);
    border-radius: 12px; font-weight: 600; letter-spacing: 0.02em;
  }
  /* Peek tabs & memory panel */
  .peek-tabs { display: flex; border-bottom: 1px solid var(--border); flex-shrink: 0; padding: 0 12px; }
  .peek-tab { padding: 8px 14px; font-size: 0.82rem; background: none; border: none;
    border-bottom: 2px solid transparent; color: var(--dim); cursor: pointer;
    margin-bottom: -1px; -webkit-tap-highlight-color: transparent; }
  .peek-tab.active { color: var(--text); border-bottom-color: var(--accent); }
  .peek-tab:hover { color: var(--text); }
  .peek-dir-bar { display: flex; align-items: center; gap: 8px; padding: 6px 14px;
    font-size: 0.75rem; color: var(--dim); border-bottom: 1px solid var(--border);
    flex-shrink: 0; min-width: 0; overflow: hidden; }
  .peek-dir-bar span { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-width: 0; font-family: "SF Mono","Fira Code",monospace; }
  .peek-terminal-panel { display: flex; flex-direction: column; flex: 1; min-height: 0; }
  .peek-memory-editor { display: none; flex-direction: column; flex: 1; min-height: 0;
    padding: 14px 16px; gap: 10px; overflow: hidden; }
  .peek-memory-editor.active { display: flex; }
  .peek-memory-textarea { flex: 1; width: 100%; font-size: 0.88rem; line-height: 1.65;
    font-family: "SF Mono","Fira Code",monospace; background: var(--bg);
    border: 1px solid var(--border); border-radius: 8px; color: var(--text);
    padding: 10px 12px; resize: none; outline: none; box-sizing: border-box; min-height: 0; }
  .peek-memory-textarea:focus { border-color: var(--accent); box-shadow: 0 0 0 3px rgba(88,166,255,0.12); }
  /* Tasks panel */
  .peek-tasks-panel { display: none; flex-direction: column; flex: 1; min-height: 0; padding: 14px 16px; gap: 10px; }
  .peek-tasks-panel.active { display: flex; }
  .peek-tasks-add { display: flex; gap: 8px; flex-shrink: 0; }
  .peek-tasks-list { flex: 1; overflow-y: auto; display: flex; flex-direction: column; gap: 4px; }
  .peek-issue-item { display: flex; align-items: flex-start; gap: 8px; padding: 8px 10px;
    border-radius: 8px; border: 1px solid var(--border); cursor: pointer; transition: background 0.15s; }
  .peek-issue-item:hover { background: var(--hover); border-color: var(--accent); }
  .peek-issue-key { font-size: 0.72rem; color: var(--dim); font-family: monospace; flex-shrink: 0; margin-top: 2px; }
  .peek-issue-title { flex: 1; font-size: 0.87rem; line-height: 1.45; word-break: break-word; }
  .peek-issue-meta { display: flex; align-items: center; gap: 6px; flex-shrink: 0; flex-direction: column; align-items: flex-end; }
  .peek-issue-due { font-size: 0.72rem; color: var(--dim); }

  /* Card stats */
  .card-stats {
    display: flex; gap: 14px; margin-top: 6px; margin-left: 20px;
    color: var(--dim); font-size: 0.75rem;
  }
  .card-stats span { display: flex; align-items: center; gap: 4px; }
  .card-timing {
    display: flex; gap: 12px; flex-wrap: wrap; align-items: center;
    margin-top: 8px; padding: 8px 10px; border-radius: 8px;
    background: rgba(255,255,255,0.02); font-size: 0.75rem; color: var(--dim);
  }
  .card-timing .timing-item { display: flex; align-items: center; gap: 4px; }
  .card-timing .timing-label { opacity: 0.7; }
  .card-timing .timing-value { color: var(--text); font-weight: 500; font-variant-numeric: tabular-nums; }
  .card-timing .timing-value.accent { color: var(--accent); }
  .card-task-name {
    font-size: 0.8rem; font-weight: 600; color: var(--text);
    margin-top: 8px; padding: 0 2px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .card-task-name .tn-label { font-weight: 400; color: var(--dim); margin-right: 4px; }

  /* Pin indicator */
  .pin-icon { font-size: 0.75rem; opacity: 0.7; }

  /* Claude status badge */
  .status-badge {
    font-size: 0.65rem; padding: 1px 6px; border-radius: 4px;
    font-weight: 600; text-transform: uppercase; flex-shrink: 0;
    letter-spacing: 0.3px;
  }
  .status-badge.active { background: rgba(63,185,80,0.2); color: var(--green); }
  .status-badge.waiting { background: rgba(210,153,34,0.2); color: var(--yellow); }
  .status-badge.idle { background: rgba(139,148,158,0.15); color: var(--dim); }
  .last-active { font-size: 0.7rem; color: var(--dim); flex-shrink: 0; }
  .token-count { font-size: 0.65rem; color: var(--dim); flex-shrink: 0; font-family: "SF Mono","Fira Code",monospace; opacity: 0.7; }

  /* Tags */
  .tag {
    font-size: 0.68rem; padding: 4px 8px; border-radius: 4px;
    font-weight: 500; background: rgba(88,166,255,0.12); color: var(--accent);
    border: 1px solid rgba(88,166,255,0.2); white-space: nowrap; flex-shrink: 0;
    cursor: pointer; -webkit-tap-highlight-color: transparent;
  }
  .tag:active { background: rgba(88,166,255,0.25); }

  /* Tag filter bar */
  .tag-filters {
    display: flex; gap: 6px; flex-wrap: nowrap; margin-top: 8px; margin-bottom: 10px;
    overflow-x: auto; -webkit-overflow-scrolling: touch; scrollbar-width: none;
  }
  .tag-filters::-webkit-scrollbar { display: none; }
  .tag-filters:empty { display: none; }
  .tag-filter {
    font-size: 0.72rem; padding: 4px 10px; border-radius: 12px;
    background: rgba(88,166,255,0.08); color: var(--dim);
    border: 1px solid var(--border); cursor: pointer;
    -webkit-tap-highlight-color: transparent; transition: all 0.15s;
    white-space: nowrap; flex-shrink: 0;
  }
  .tag-filter:active { background: rgba(88,166,255,0.15); }
  .tag-filter.active {
    background: rgba(88,166,255,0.2); color: var(--accent);
    border-color: var(--accent);
  }

  /* Card description */
  .card-desc {
    color: var(--text); font-size: 0.82rem; margin-top: 4px; margin-left: 20px;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
    opacity: 0.85;
  }

  .empty {
    text-align: center; color: var(--dim); padding: 40px 16px;
    font-size: 0.95rem; line-height: 1.6;
  }
  .empty code { color: var(--accent); }

  /* Sync activity banner */
  .sync-banner {
    position: fixed; bottom: 0; left: 0; right: 0; z-index: 450;
    background: var(--card); border-top: 1px solid var(--accent);
    padding: 10px 16px; font-size: 0.8rem;
    transform: translateY(100%); transition: transform 0.2s;
    max-height: 40vh; overflow-y: auto;
  }
  .sync-banner.active { transform: translateY(0); }
  .sync-banner .sync-title { font-weight: 600; margin-bottom: 6px; display: flex; justify-content: space-between; align-items: center; }
  .sync-banner .sync-item { display: flex; align-items: center; gap: 6px; padding: 3px 0; font-size: 0.75rem; }
  .sync-banner .sync-item.pending { color: var(--dim); }
  .sync-banner .sync-item.running { color: var(--accent); }
  .sync-banner .sync-item.done { color: #4ade80; }
  .sync-banner .sync-item.failed { color: #f87171; }

  /* Sending indicator */
  .sending-indicator {
    position: absolute; top: 8px; left: 50%; transform: translateX(-50%);
    background: var(--accent); color: #fff; font-size: 0.7rem; font-weight: 600;
    padding: 3px 12px; border-radius: 12px; z-index: 10;
    animation: send-pulse 1s ease-in-out infinite;
    pointer-events: none;
  }
  @keyframes send-pulse { 0%,100% { opacity: 1; } 50% { opacity: 0.5; } }

  /* Toast notifications */
  .toast {
    position: fixed; bottom: 20px; right: 20px; z-index: 500;
    background: var(--card); border: 1px solid var(--border); border-radius: 8px;
    padding: 10px 16px; font-size: 0.85rem; color: var(--text);
    box-shadow: 0 4px 16px rgba(0,0,0,0.4);
    opacity: 0; transform: translateY(10px);
    transition: opacity 0.2s, transform 0.2s;
    pointer-events: none; max-width: 320px;
  }
  .toast.visible { opacity: 1; transform: translateY(0); pointer-events: auto; }

  /* Confirm / alert modal — replaces native confirm()/alert() which PWA blocks */
  .modal-backdrop {
    display: none; position: fixed; inset: 0; z-index: 600;
    background: rgba(0,0,0,0.6); align-items: center; justify-content: center;
  }
  .modal-backdrop.open { display: flex; }
  .modal-box {
    background: var(--card); border: 1px solid var(--border); border-radius: 12px;
    padding: 24px 20px 16px; max-width: min(480px, 94vw); width: 100%; text-align: center;
  }
  .modal-msg { font-size: 0.95rem; color: var(--text); margin-bottom: 20px; line-height: 1.5; }
  .modal-btns { display: flex; gap: 10px; justify-content: center; }
  .modal-btns .btn { min-width: 80px; }

  /* Connection status indicator — pill button */
  .conn-status {
    display: inline-flex; align-items: center; gap: 6px;
    font-size: 0.75rem; font-weight: 600; white-space: nowrap;
    cursor: pointer; -webkit-tap-highlight-color: transparent;
    flex-shrink: 0; transition: all 0.2s;
    padding: 4px 10px; border-radius: 12px; border: none;
    line-height: 1;
  }
  .conn-status::before {
    content: ''; width: 8px; height: 8px; border-radius: 50%;
    flex-shrink: 0; transition: background 0.2s;
  }
  .conn-status.online {
    color: #4ade80; background: rgba(74,222,128,0.1);
  }
  .conn-status.online::before { background: #4ade80; }
  .conn-status.polling {
    color: #facc15; background: rgba(250,204,21,0.1);
  }
  .conn-status.polling::before { background: #facc15; }
  .conn-status.offline {
    color: #f87171; background: rgba(248,113,113,0.15);
  }
  .conn-status.offline::before { background: #f87171; }

  /* Loading spinner */
  @keyframes _spin { to { transform: rotate(360deg); } }
  .loading-spinner {
    width: 18px; height: 18px; border: 2px solid var(--border);
    border-top-color: var(--accent); border-radius: 50%;
    animation: _spin 0.8s linear infinite;
    display: inline-block; vertical-align: middle; margin-right: 8px;
  }

  /* Queue modal */
  .queue-overlay {
    display: none; position: fixed; inset: 0; background: rgba(1,4,9,0.85);
    z-index: 400; align-items: center; justify-content: center; padding: 20px;
  }
  .queue-overlay.active { display: flex; }
  .queue-box {
    background: var(--card); border: 1px solid var(--border); border-radius: 12px;
    padding: 20px; width: 100%; max-width: 420px; max-height: 70vh; overflow-y: auto;
  }
  .queue-box h3 { font-size: 1rem; margin-bottom: 14px; }
  .queue-item {
    font-size: 0.78rem; padding: 8px 10px; border: 1px solid var(--border);
    border-radius: 6px; margin-bottom: 6px; font-family: "SF Mono", monospace;
    color: var(--dim); word-break: break-all;
  }
  .queue-item .queue-time { color: var(--yellow); font-size: 0.7rem; }
  .queue-empty { color: var(--dim); text-align: center; padding: 16px; font-size: 0.85rem; }

  /* Cached / draft indicators */
  .cached-badge, .draft-badge, .pending-badge {
    font-size: 0.6rem; padding: 1px 5px; border-radius: 3px;
    font-weight: 600; text-transform: uppercase; margin-left: 6px;
  }
  .cached-badge { background: rgba(139,148,158,0.15); color: var(--dim); }
  .draft-badge { background: rgba(210,153,34,0.2); color: var(--yellow); }
  .pending-badge { background: rgba(88,166,255,0.15); color: var(--accent); }
  .draft-prompt {
    font-size: 0.72rem; color: var(--dim); padding: 6px 8px; margin-top: 6px;
    background: rgba(139,148,158,0.06); border-radius: 5px;
    font-family: "SF Mono", "Menlo", monospace; white-space: pre-wrap;
    word-break: break-word; border-left: 2px solid var(--yellow);
  }

  /* Offline banner */
  .offline-banner {
    display: none; background: rgba(248,113,113,0.08);
    border: 1px solid rgba(248,113,113,0.25); border-radius: 10px;
    padding: 10px 14px; margin: 0 0 12px 0;
  }
  .offline-banner.active { display: block; }
  .offline-banner-header {
    display: flex; align-items: center; justify-content: space-between;
    font-size: 0.82rem; font-weight: 600; color: #f87171;
  }
  .offline-banner-header .sync-btn {
    font-size: 0.72rem; padding: 3px 10px; border-radius: 6px;
    background: rgba(248,113,113,0.15); color: #f87171; border: 1px solid rgba(248,113,113,0.3);
    cursor: pointer; font-weight: 600;
  }
  .offline-banner-header .sync-btn:active { opacity: 0.7; }
  .offline-queue-ops {
    margin-top: 8px; display: flex; flex-direction: column; gap: 4px;
  }
  .offline-op {
    font-size: 0.72rem; color: var(--dim); padding: 4px 8px;
    background: rgba(139,148,158,0.06); border-radius: 5px;
    font-family: "SF Mono", "Menlo", monospace;
    display: flex; justify-content: space-between; align-items: center;
  }
  .offline-op .op-action { color: var(--text); font-weight: 500; }
  .offline-op .op-time { color: var(--yellow); font-size: 0.65rem; }
  .offline-op .op-stale { color: var(--red); font-size: 0.65rem; font-style: italic; }

  /* Tab bar */
  .tab-bar {
    display: flex; gap: 0; margin: 0 -16px 12px -16px; padding: 0 16px;
    border-bottom: 1px solid var(--border);
    position: sticky; top: 60px; z-index: 39; background: var(--bg);
  }
  .tab-bar button {
    flex: 1; padding: 10px 0; font-size: 0.85rem; font-weight: 600;
    background: none; border: none; border-bottom: 2px solid transparent;
    color: var(--dim); cursor: pointer; transition: color 0.15s, border-color 0.15s;
    -webkit-tap-highlight-color: transparent;
  }
  .tab-bar button.active { color: var(--accent); border-bottom-color: var(--accent); }
  .tab-bar button:active { opacity: 0.7; }

  /* Board */
  .board-search-wrap {
    position: relative; margin-bottom: 4px;
  }
  .board-search-wrap .search-input { width: 100%; box-sizing: border-box; }
  .board-search-wrap .search-clear { display: none; }
  .board-search-wrap:has(.search-input:not(:placeholder-shown)) .search-clear { display: flex; }
  .board-columns {
    display: flex; gap: 12px; overflow-x: scroll;
    -webkit-overflow-scrolling: touch; padding-bottom: 16px; align-items: flex-start;
    min-height: 200px; touch-action: pan-x pan-y;
  }
  .board-columns::-webkit-scrollbar { display: none; }
  .board-col {
    flex: 1; min-width: 200px; max-width: 320px;
    display: flex; flex-direction: column; gap: 6px;
    background: rgba(255,255,255,0.02); border-radius: 10px; padding: 10px 8px;
    min-height: 80px; touch-action: pan-x pan-y;
  }
  .board-col-header {
    font-size: 0.72rem; font-weight: 600; color: var(--dim);
    text-transform: uppercase; letter-spacing: 0.06em;
    display: flex; justify-content: space-between; align-items: center;
    padding: 2px 4px 8px 4px; border-bottom: 1px solid var(--border); margin-bottom: 2px;
  }
  .board-col-header .col-count {
    font-weight: 500; font-size: 0.68rem; color: var(--dim);
    background: var(--border); border-radius: 8px; padding: 1px 6px;
  }
  .board-card {
    background: var(--card); border: 1px solid var(--border);
    border-radius: 8px; padding: 10px 12px;
    cursor: pointer; transition: border-color 0.2s, box-shadow 0.2s, transform 0.35s cubic-bezier(.4,0,.2,1), opacity 0.3s;
    -webkit-tap-highlight-color: transparent;
    will-change: transform, opacity;
    position: relative;
  }
  .board-card:active { border-color: var(--accent); box-shadow: 0 0 0 1px rgba(88,166,255,0.2); }
  .board-drag-handle {
    position: absolute; top: 8px; right: 8px;
    width: 18px; height: 18px;
    display: flex; align-items: center; justify-content: center;
    cursor: grab; color: var(--dim); opacity: 0;
    transition: opacity 0.15s;
    border-radius: 3px;
  }
  .board-drag-handle:active { cursor: grabbing; }
  .board-card:hover .board-drag-handle { opacity: 0.55; }
  .board-drag-handle:hover { opacity: 1 !important; color: var(--fg); background: rgba(139,148,158,0.12); }
  @media (hover: none) { .board-drag-handle { opacity: 0.35; } }
  .board-card.card-enter { animation: cardEnter 0.3s cubic-bezier(.4,0,.2,1) both; }
  .board-card.card-flip { transition: transform 0.35s cubic-bezier(.4,0,.2,1); }
  @keyframes cardEnter {
    from { opacity: 0; transform: scale(0.95) translateY(-6px); }
    to { opacity: 1; transform: none; }
  }
  .col-count { transition: transform 0.15s; }
  .col-count.bump { animation: countBump 0.25s; }
  @keyframes countBump { 50% { transform: scale(1.3); } }
  .board-card-title {
    font-size: 0.87rem; font-weight: 500; line-height: 1.35;
    word-break: break-word; margin-bottom: 4px;
  }
  .board-card-desc {
    font-size: 0.74rem; color: var(--dim); margin-bottom: 6px;
    word-break: break-word; line-height: 1.4;
    display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden;
  }
  .board-card-footer {
    display: flex; align-items: center; gap: 6px; flex-wrap: wrap;
    margin-top: 6px;
  }
  .board-card-key { font-size: 0.62rem; color: var(--dim); font-family: "SF Mono","Fira Code",monospace; letter-spacing: 0.02em; white-space: nowrap; }
  .board-card-tag, .board-card-session {
    font-size: 0.62rem; border-radius: 4px; padding: 3px 6px; white-space: nowrap;
    cursor: pointer; -webkit-tap-highlight-color: transparent;
  }
  .board-card-tag { background: rgba(139,148,158,0.1); color: var(--dim); border: 1px solid rgba(139,148,158,0.15); }
  .board-card-session {
    background: rgba(88,166,255,0.08); color: var(--accent); font-weight: 500;
    border: 1px solid rgba(88,166,255,0.18); overflow: hidden; text-overflow: ellipsis; max-width: 90px;
  }
  .board-card-tag:active, .board-card-session:active { background: rgba(88,166,255,0.15); color: var(--accent); border-color: rgba(88,166,255,0.3); }
  .board-card-time { font-size: 0.62rem; color: var(--dim); margin-left: auto; white-space: nowrap; }
  .board-add-btn {
    width: 100%; padding: 7px 0; font-size: 0.8rem; font-weight: 500;
    border: 1px dashed rgba(255,255,255,0.08); border-radius: 8px;
    background: none; color: var(--dim); cursor: pointer;
    transition: border-color 0.15s, color 0.15s;
    -webkit-tap-highlight-color: transparent; margin-top: 2px;
  }
  .board-add-btn:active { border-color: var(--accent); color: var(--accent); }
  .board-empty { text-align: center; color: rgba(139,148,158,0.5); font-size: 0.78rem; padding: 20px 0; }
  .board-card { -webkit-user-select: none; user-select: none; -webkit-touch-callout: none; }
  .board-sortable-ghost { opacity: 0.25; background: rgba(88,166,255,0.15) !important; border-color: var(--accent) !important; }
  .board-sortable-chosen { box-shadow: 0 8px 24px rgba(0,0,0,0.5); z-index: 10; }
  .board-sortable-drag { opacity: 0; }
  .col-del-btn {
    background: none; border: none; color: var(--dim); cursor: pointer; font-size: 0.75rem;
    padding: 0 2px; line-height: 1; opacity: 0.5; transition: opacity 0.15s, color 0.15s;
    -webkit-tap-highlight-color: transparent;
  }
  .col-del-btn:hover, .col-del-btn:active { opacity: 1; color: var(--red); }
  .board-add-col-btn {
    flex-shrink: 0; align-self: flex-start; min-width: 120px; padding: 10px 14px;
    font-size: 0.8rem; font-weight: 500; border: 1px dashed rgba(255,255,255,0.1);
    border-radius: 10px; background: rgba(255,255,255,0.01); color: var(--dim);
    cursor: pointer; transition: border-color 0.15s, color 0.15s; white-space: nowrap;
    -webkit-tap-highlight-color: transparent;
  }
  .board-add-col-btn:active { border-color: var(--accent); color: var(--accent); }
  .board-col-new { min-width: 180px; max-width: 220px; }
  .new-status-input {
    width: 100%; box-sizing: border-box; background: var(--input-bg, rgba(255,255,255,0.05));
    border: 1px solid var(--border); border-radius: 6px; color: var(--text);
    font-size: 0.85rem; padding: 6px 8px; outline: none; font-family: inherit;
    transition: border-color 0.15s;
  }
  .new-status-input:focus { border-color: var(--accent); }
  .board-filters { display: flex; gap: 6px; flex-wrap: wrap; padding: 6px 0 8px; align-items: center; }
  .board-filter-label { font-size: 0.68rem; color: var(--dim); white-space: nowrap; }
  .board-filter-chip {
    font-size: 0.72rem; padding: 3px 10px; border-radius: 12px;
    border: 1px solid var(--border); background: none; color: var(--dim);
    cursor: pointer; -webkit-tap-highlight-color: transparent; transition: all 0.12s; white-space: nowrap;
  }
  .board-filter-chip.active { background: rgba(88,166,255,0.15); color: var(--accent); border-color: rgba(88,166,255,0.3); }
  .board-filter-chip.active-session { background: rgba(139,148,158,0.15); color: var(--text); border-color: var(--dim); }
  /* Board toolbar + view toggle */
  .board-toolbar { display: flex; gap: 8px; align-items: center; }
  .board-view-toggle { display: flex; gap: 2px; background: rgba(255,255,255,0.04); border-radius: 6px; padding: 2px; flex-shrink: 0; }
  .bv-btn {
    padding: 5px 8px; border: none; background: none; color: var(--dim);
    font-size: 0.85rem; cursor: pointer; border-radius: 4px;
    -webkit-tap-highlight-color: transparent; transition: all 0.12s; line-height: 1;
  }
  .bv-btn.active { background: rgba(88,166,255,0.15); color: var(--accent); }
  .bv-btn:active { background: rgba(88,166,255,0.1); }
  /* Session-grouped view */
  .board-session-group { margin-bottom: 8px; min-width: 0; }
  .board-session-header {
    display: flex; align-items: center; gap: 8px; padding: 8px 10px;
    cursor: pointer; -webkit-tap-highlight-color: transparent;
    border-radius: 8px; transition: background 0.12s; user-select: none;
  }
  .board-session-header:active { background: rgba(255,255,255,0.04); }
  .board-session-chevron {
    font-size: 0.6rem; color: var(--dim); transition: transform 0.2s; flex-shrink: 0; width: 12px;
  }
  .board-session-chevron.open { transform: rotate(90deg); }
  .board-session-name { font-size: 0.82rem; font-weight: 600; color: var(--text); }
  .board-session-counts {
    display: flex; gap: 6px; margin-left: auto; flex-shrink: 0;
  }
  .board-session-count {
    font-size: 0.62rem; padding: 2px 6px; border-radius: 8px; font-weight: 500;
  }
  .board-session-count.todo { background: rgba(139,148,158,0.12); color: var(--dim); }
  .board-session-count.doing { background: rgba(210,153,34,0.15); color: var(--yellow); }
  .board-session-count.done { background: rgba(63,185,80,0.15); color: var(--green); }
  .board-session-items { padding: 0 4px 4px 20px; }
  .board-session-items .board-card { margin-bottom: 6px; }
  .tag-group-body { padding: 0 0 4px 0; min-width: 0; overflow: hidden; }
  .board-session-items .board-card .board-status-dot {
    display: inline-block; width: 7px; height: 7px; border-radius: 50%; margin-right: 6px; flex-shrink: 0;
  }
  .board-status-dot.backlog { background: var(--accent); }
  .board-status-dot.todo { background: var(--dim); }
  .board-status-dot.doing { background: var(--yellow); }
  .board-status-dot.done { background: var(--green); }
  .board-status-dot.discarded { background: rgba(139,148,158,0.4); }
  .board-session-empty { color: rgba(139,148,158,0.4); font-size: 0.75rem; padding: 10px 0; text-align: center; }
  .board-columns-list { display: block; padding-bottom: 16px; min-height: 200px; }
  /* Board card detail */
  .board-detail-body { flex: 1; min-height: 0; overflow-y: auto; padding: 4px 0 12px; -webkit-overflow-scrolling: touch; }
  .board-detail-key { font-size: 0.72rem; color: var(--dim); font-family: "SF Mono","Fira Code",monospace; margin-bottom: 8px; }
  .board-detail-title-input {
    width: 100%; font-size: 1.25rem; font-weight: 700; background: none; border: none;
    color: var(--text); outline: none; padding: 0; margin-bottom: 10px;
    font-family: inherit; line-height: 1.35; box-sizing: border-box;
    resize: none; overflow: hidden; word-wrap: break-word;
  }
  .board-detail-title-input::placeholder { color: var(--dim); }
  .board-detail-row { display: flex; align-items: center; gap: 8px; margin-bottom: 12px; flex-wrap: wrap; }
  .board-detail-status-row { display: flex; gap: 6px; margin-bottom: 12px; flex-wrap: wrap; }
  .board-detail-status-btn {
    padding: 4px 13px; border-radius: 20px; font-size: 0.75rem; font-weight: 600;
    border: 1px solid var(--border); background: none; color: var(--dim);
    cursor: pointer; transition: all 0.15s; -webkit-tap-highlight-color: transparent;
  }
  .board-detail-status-btn.active-backlog { background: rgba(88,166,255,0.15); color: var(--accent); border-color: var(--accent); }
  .board-detail-status-btn.active-todo { background: rgba(139,148,158,0.15); color: var(--text); border-color: var(--dim); }
  .board-detail-status-btn.active-doing { background: rgba(210,153,34,0.15); color: var(--yellow); border-color: var(--yellow); }
  .board-detail-status-btn.active-done { background: rgba(63,185,80,0.15); color: var(--green); border-color: var(--green); }
  .board-detail-status-btn.active-discarded { background: rgba(139,148,158,0.08); color: rgba(139,148,158,0.5); border-color: rgba(139,148,158,0.2); }
  .board-detail-session-select {
    padding: 4px 8px; border-radius: 6px; font-size: 0.8rem;
    border: 1px solid var(--border); background: var(--card); color: var(--text);
    outline: none; font-family: inherit; max-width: 200px;
  }
  .board-detail-tabs { display: flex; border-bottom: 1px solid var(--border); margin-bottom: 2px; }
  .board-detail-tab {
    padding: 6px 14px; font-size: 0.8rem; font-weight: 500; cursor: pointer;
    border: none; background: none; color: var(--dim); border-bottom: 2px solid transparent;
    margin-bottom: -1px; -webkit-tap-highlight-color: transparent;
  }
  .board-detail-tab.active { color: var(--text); border-bottom-color: var(--accent); }
  .board-detail-desc-input {
    width: 100%; min-height: 200px; font-size: 0.92rem; background: none;
    border: none; color: var(--text); outline: none; padding: 10px 0;
    font-family: inherit; line-height: 1.65; resize: none; box-sizing: border-box;
  }
  .board-detail-desc-input::placeholder { color: var(--dim); }
  .board-detail-preview { min-height: 200px; font-size: 0.92rem; line-height: 1.65; color: var(--text); padding: 10px 0; }
  .board-detail-preview h1{font-size:1.2rem;font-weight:700;margin:10px 0 5px}
  .board-detail-preview h2{font-size:1.05rem;font-weight:700;margin:9px 0 4px}
  .board-detail-preview h3{font-size:0.95rem;font-weight:700;margin:8px 0 4px}
  .board-detail-preview p{margin:0 0 9px}
  .board-detail-preview ul,.board-detail-preview ol{margin:0 0 9px;padding-left:20px}
  .board-detail-preview li{margin-bottom:3px}
  .board-detail-preview code{font-family:"SF Mono","Fira Code",monospace;font-size:0.85em;background:rgba(255,255,255,0.08);padding:1px 5px;border-radius:4px}
  .board-detail-preview pre{background:rgba(0,0,0,0.3);border-radius:6px;padding:12px;overflow-x:auto;margin:0 0 9px}
  .board-detail-preview pre code{background:none;padding:0;font-size:0.82em}
  .board-detail-preview a{color:var(--accent);text-decoration:underline}
  .board-detail-preview hr{border:none;border-top:1px solid var(--border);margin:10px 0}
  .board-detail-preview blockquote{border-left:3px solid var(--border);margin:0 0 9px;padding:2px 12px;color:var(--dim)}
  .board-detail-meta { margin-top: 12px; font-size: 0.78rem; color: var(--dim); border-top: 1px solid var(--border); padding-top: 10px; display: flex; flex-direction: column; gap: 5px; }
  .board-detail-meta-row { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  .board-detail-footer { display: flex; justify-content: space-between; align-items: center; padding-top: 10px; flex-shrink: 0; border-top: 1px solid var(--border); }


  /* Board inline edit */
  .board-edit-overlay {
    position: fixed; inset: 0; z-index: 600;
    background: rgba(0,0,0,0); align-items: center; justify-content: center;
    padding: 16px; display: flex; pointer-events: none;
    transition: background 0.25s;
  }
  .board-edit-overlay.active { background: rgba(0,0,0,0.6); pointer-events: auto; }
  .board-edit-overlay .board-edit-box {
    opacity: 0; transform: scale(0.95) translateY(8px);
    transition: opacity 0.25s, transform 0.25s cubic-bezier(.4,0,.2,1);
  }
  .board-edit-overlay.active .board-edit-box { opacity: 1; transform: none; }
  .board-edit-box {
    background: var(--card); border: 1px solid var(--border);
    border-radius: 12px; padding: 16px; width: 100%; max-width: 400px;
  }
  .board-edit-box input, .board-edit-box textarea, .board-edit-box select {
    width: 100%; padding: 8px 10px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--bg); color: var(--text);
    font-size: 0.85rem; font-family: inherit; margin-bottom: 8px;
    outline: none; box-sizing: border-box;
  }
  .board-edit-box textarea { resize: vertical; min-height: 60px; }
  .board-edit-box input:focus, .board-edit-box textarea:focus, .board-edit-box select:focus {
    border-color: var(--accent); box-shadow: 0 0 0 3px rgba(88,166,255,0.12);
  }
  .board-edit-box .field-group { margin-bottom: 8px; }
  .board-edit-box .field-group > input, .board-edit-box .field-group > textarea,
  .board-edit-box .field-group > select { margin-bottom: 0; }
  .board-edit-actions {
    display: flex; gap: 8px; margin-top: 4px;
  }
  .board-edit-actions button {
    flex: 1; padding: 8px 0; border-radius: 8px; font-size: 0.82rem;
    font-weight: 600; cursor: pointer; border: 1px solid var(--border);
    -webkit-tap-highlight-color: transparent;
  }
  .board-edit-actions .be-cancel { background: var(--card); color: var(--dim); }
  .board-edit-actions .be-save { background: var(--accent); color: #fff; border-color: var(--accent); }
  .board-edit-actions .be-cancel:active { background: var(--border); }
  .board-edit-actions .be-save:active { opacity: 0.8; }

  /* ═══ Grid Mode ═══ */
  #grid-view {
    display: none; position: fixed; top: 56px; left: 0; right: 0; bottom: 0; z-index: 39;
    background: #0a0d12; flex-direction: column;
  }
  #grid-view.active { display: flex; }
  .grid-toolbar {
    display: flex; align-items: center; gap: 8px; padding: 6px 12px;
    border-bottom: 1px solid var(--border); flex-shrink: 0;
    background: var(--card); min-height: 44px;
  }
  .grid-toolbar-title { font-size: 0.78rem; font-weight: 600; color: var(--dim); flex-shrink: 0; margin-right: 4px; }
  #grid-chips { display: flex; gap: 5px; flex: 1; overflow-x: auto; align-items: center; padding: 2px 0; }
  #grid-chips::-webkit-scrollbar { display: none; }
  .gp-chip {
    padding: 3px 10px; border-radius: 10px; font-size: 0.73rem; cursor: pointer;
    border: 1px solid var(--border); background: transparent; color: var(--dim);
    white-space: nowrap; flex-shrink: 0; transition: all 0.15s;
    -webkit-tap-highlight-color: transparent;
  }
  .gp-chip:hover { background: var(--hover); color: var(--text); }
  .gp-chip.on { background: var(--accent); color: #fff; border-color: var(--accent); }
  .ws-profile-chip {
    display: inline-flex; align-items: center; gap: 3px;
    padding: 3px 8px; border-radius: 10px; font-size: 0.73rem; cursor: pointer;
    border: 1px solid var(--accent); background: rgba(88,166,255,0.08); color: var(--accent);
    white-space: nowrap; flex-shrink: 0; transition: all 0.15s;
    -webkit-tap-highlight-color: transparent;
  }
  .ws-profile-chip:hover { background: rgba(88,166,255,0.2); }
  .ws-profile-del {
    background: none; border: none; color: inherit; opacity: 0.5; cursor: pointer;
    padding: 0 0 0 2px; font-size: 0.7rem; line-height: 1;
  }
  .ws-profile-del:hover { opacity: 1; }
  .ws-save-input {
    background: var(--input-bg, #0d1117); border: 1px solid var(--accent);
    border-radius: 6px; color: var(--text); font-size: 0.75rem;
    padding: 4px 8px; outline: none; width: 130px;
  }
  .ws-save-input::placeholder { color: var(--dim); }
  #gridstack-container { flex: 1; overflow-y: auto; padding: 4px; }
  .gp-header {
    display: flex; align-items: center; padding: 0 10px; gap: 8px;
    background: #161b22; border-bottom: 1px solid var(--border);
    height: 32px; cursor: move; flex-shrink: 0;
    user-select: none; -webkit-user-select: none;
  }
  .gp-title { font-size: 0.78rem; font-weight: 500; flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--text); }
  .gp-dot { width: 7px; height: 7px; border-radius: 50%; background: #6e7681; flex-shrink: 0; transition: background 0.3s; }
  .gp-dot.working { background: #3fb950; }
  .gp-dot.waiting { background: #d29922; }
  .gp-dot.idle { background: #58a6ff; }
  .gp-close {
    background: none; border: none; color: var(--dim); cursor: pointer;
    font-size: 0.82rem; padding: 2px 5px; border-radius: 3px; line-height: 1;
    flex-shrink: 0; -webkit-tap-highlight-color: transparent;
  }
  .gp-close:hover { background: rgba(248,81,73,0.15); color: #f85149; }
  .gp-body {
    flex: 1; overflow: auto; padding: 10px;
    font-family: "SF Mono","Fira Code","Cascadia Code",monospace;
    font-size: 0.76rem; line-height: 1.45; white-space: pre-wrap; word-break: break-all;
    -webkit-overflow-scrolling: touch; color: #c9d1d9;
    user-select: text; -webkit-user-select: text; cursor: text;
  }
  #gridstack-container .grid-stack-item-content {
    border: 1px solid var(--border); border-radius: 6px;
    overflow: hidden; display: flex; flex-direction: column;
    background: #010409;
  }
  #gridstack-container .ui-resizable-handle { opacity: 0.3; }
  #gridstack-container .ui-resizable-handle:hover { opacity: 0.8; }
  .gp-send {
    display: flex; flex-direction: column; gap: 0; padding: 8px 10px 10px;
    border-top: 1px solid var(--border); flex-shrink: 0;
    background: #161b22;
  }
  /* chips row inside workspace panes — same as session cards */
  .gp-send .chips { margin-bottom: 8px; }
  /* send row inside workspace panes — same as session cards */
  .gp-send .send-row { gap: 8px; }
  #tab-grid { display: none; }
  @media (min-width: 769px) { #tab-grid { display: block; } }
</style>
</head>
<body>

<div class="header-row">
  <div style="display:flex;gap:8px;align-items:center;">
    <h1 style="margin:0;cursor:pointer;" onclick="openAbout()">amux</h1>
    <span id="conn-status" class="conn-status online" onclick="showQueueModal()"></span>
  </div>
  <div style="display:flex;gap:8px;align-items:center;">
    <div class="active-wrap">
      <button class="btn-active" id="active-btn" onclick="event.stopPropagation();toggleActiveDropdown()">
        <span class="active-dot"></span>
        <span class="active-count" id="active-count">0</span>
      </button>
      <div class="active-dropdown" id="active-dropdown"></div>
    </div>
    <div class="header-add-wrap">
      <button class="header-add-btn" id="add-btn" onclick="event.stopPropagation();toggleAddMenu()">+</button>
      <div class="header-add-menu" id="add-menu">
        <div class="card-menu-item" onclick="event.stopPropagation();closeAddMenu();openCreate()"><span class="mi">&#x2795;</span> New session</div>
        <div class="card-menu-item" onclick="event.stopPropagation();closeAddMenu();openConnect()"><span class="mi">&#x1F517;</span> Connect tmux</div>
      </div>
    </div>
    <div class="settings-wrap">
      <button class="settings-btn" id="settings-btn" onclick="event.stopPropagation();toggleSettings()">&#x2699;</button>
      <div class="settings-menu" id="settings-menu">
        <div class="settings-section">
          <div class="settings-section-label">Device</div>
          <div id="settings-device-current" style="font-size:0.88rem;font-weight:600;margin-bottom:6px;"></div>
          <div class="settings-row">
            <input id="settings-device-name" type="text" autocomplete="off"
              onchange="saveDeviceName(this.value)">
          </div>
        </div>
        <div class="settings-sep"></div>
        <div class="settings-section">
          <div style="display:flex;justify-content:space-between;align-items:center;">
            <div class="settings-section-label" style="margin-bottom:0;">Servers</div>
            <button class="btn" style="font-size:0.6rem;padding:1px 8px;" onclick="toggleSettingsAddServer()">+ Add</button>
          </div>
          <div id="settings-add-server" style="display:none;margin-top:8px;">
            <input id="settings-new-server-name" class="search-input" type="text" placeholder="Label" style="width:100%;margin-bottom:4px;font-size:0.72rem;padding:5px 8px;box-sizing:border-box;">
            <input id="settings-new-server-url" class="search-input" type="text" placeholder="https://host:8822" style="width:100%;margin-bottom:6px;font-size:0.72rem;padding:5px 8px;box-sizing:border-box;">
            <div style="display:flex;gap:6px;justify-content:flex-end;">
              <button class="btn" style="font-size:0.6rem;padding:1px 6px;" onclick="toggleSettingsAddServer()">Cancel</button>
              <button class="btn" style="font-size:0.6rem;padding:1px 6px;background:var(--accent);color:#000;" onclick="saveSettingsNewServer()">Save</button>
            </div>
          </div>
          <div id="settings-server-list" style="margin-top:6px;"></div>
        </div>
        <div class="settings-sep"></div>
        <div class="settings-section" style="text-align:center;display:flex;flex-direction:column;gap:8px;">
          <span style="font-size:0.7rem;color:var(--dim);cursor:pointer;" onclick="openSkills();closeSettings()">&#x26A1; Skills &amp; commands</span>
          <span style="font-size:0.7rem;color:var(--dim);cursor:pointer;" onclick="openAbout();closeSettings()">About amux &amp; token stats</span>
          <span style="font-size:0.7rem;color:var(--dim);cursor:pointer;" onclick="openDevtools();closeSettings()">&#x1F6E0; Developer tools</span>
        </div>
      </div>
    </div>
  </div>
</div>
<div class="tab-bar">
  <button id="tab-sessions" class="active" onclick="switchView('sessions')">Sessions</button>
  <button id="tab-board" onclick="switchView('board')">Board</button>
  <button id="tab-calendar" onclick="switchView('calendar')">Calendar</button>
  <button id="tab-grid" onclick="enterGridMode()">Workspace</button>
</div>
<div id="session-view">
<div style="padding:0 12px;margin-top:4px;display:flex;align-items:center;gap:8px;">
  <div class="search-wrap" id="search-wrap" style="flex:1;">
    <input class="search-input" id="search-input" type="text" placeholder="Search sessions..." autocomplete="off" autocorrect="off"
      oninput="searchQuery=this.value;document.getElementById('search-wrap').classList.toggle('has-value',!!this.value);render()">
    <button class="search-clear" onclick="event.stopPropagation();clearSearch()">&#x2715;</button>
  </div>
  <div class="tile-controls">
    <button class="tile-btn" id="tile-list-btn" onclick="setLayoutMode('list')" title="List view">&#x2630;</button>
    <button class="tile-btn" id="tile-group-btn" onclick="setLayoutMode('group')" title="Group by status" style="font-size:0.75rem;font-weight:700;">#</button>
    <button class="tile-btn tile-grid-only" id="tile-grid-btn" onclick="setLayoutMode('grid')" title="Grid view">&#x268F;</button>
  </div>
</div>
<div id="tag-filters" class="tag-filters"></div>
<div id="offline-banner" class="offline-banner">
  <div class="offline-banner-header">
    <span id="offline-banner-title">&#x26A0; Offline</span>
    <button class="sync-btn" onclick="forceRetry()">Retry now</button>
  </div>
  <div id="offline-ops" class="offline-queue-ops"></div>
</div>
<div id="cards" class="cards"></div>
</div>
<div id="board-view" style="display:none;">
  <div class="board-toolbar">
    <div class="board-search-wrap" style="flex:1;">
      <input id="board-search" class="search-input" type="text" placeholder="Search board..." oninput="boardSearchQuery=this.value.toLowerCase();renderBoard()">
      <button class="search-clear" onclick="document.getElementById('board-search').value='';boardSearchQuery='';renderBoard()">&#x2715;</button>
    </div>
    <div class="board-view-toggle">
      <button id="bv-session" class="bv-btn" onclick="setBoardView('session')" title="Group by session">&#x25A4;</button>
      <button id="bv-status" class="bv-btn" onclick="setBoardView('status')" title="Group by status">&#x2630;</button>
    </div>
  </div>
  <div class="board-filters" id="board-filters"></div>
  <div class="board-columns" id="board-columns"></div>
</div>
<!-- Calendar view -->
<div id="calendar-view" style="display:none;">
  <div class="cal-toolbar">
    <div class="cal-nav-row">
      <button class="btn" onclick="calPrev()">&#x2039;</button>
      <span id="cal-title" class="cal-title"></span>
      <button class="btn" onclick="calNext()">&#x203A;</button>
    </div>
    <div class="cal-controls-row">
      <button id="cal-today-btn" class="btn" onclick="calToday()">Today</button>
      <div class="cal-view-tabs">
        <button class="cal-view-tab" id="cal-tab-month" onclick="calSetView('month')">Month</button>
        <button class="cal-view-tab" id="cal-tab-week" onclick="calSetView('week')">Week</button>
        <button class="cal-view-tab" id="cal-tab-day" onclick="calSetView('day')">Day</button>
      </div>
      <button class="btn" onclick="showIcalInfo()" title="Subscribe in Google / Apple Calendar" style="font-size:0.8rem;">&#x1F4C5;</button>
    </div>
  </div>
  <div id="cal-body"></div>
</div>

<!-- Board card "add" small modal -->
<div id="board-edit-overlay" class="board-edit-overlay" onclick="if(event.target===this)closeBoardEdit()">
  <div class="board-edit-box">
    <div class="field-group">
      <label class="field-label">Title</label>
      <input id="be-title" type="text" placeholder="What needs to be done?" autocomplete="off"
        onkeydown="if(event.key==='Enter'){event.preventDefault();document.getElementById('be-desc').focus();}">
    </div>
    <div class="field-group">
      <label class="field-label">Notes <span class="field-optional">(optional)</span></label>
      <textarea id="be-desc" placeholder="Add details or context..."></textarea>
    </div>
    <div class="field-group" id="be-session-row">
      <label class="field-label">Session <span class="field-optional">(optional)</span></label>
      <select id="be-session-add"></select>
    </div>
    <div class="field-group">
      <label class="field-label">Status</label>
      <select id="be-status"><option value="backlog">Backlog</option><option value="todo">To Do</option><option value="doing">In Progress</option><option value="done">Done</option><option value="discarded">Discarded</option></select>
    </div>
    <div class="field-group">
      <label class="field-label">Due date <span class="field-optional">(optional)</span></label>
      <input id="be-due" type="date">
    </div>
    <div class="board-edit-actions">
      <button class="be-cancel" onclick="closeBoardEdit()">Cancel</button>
      <button class="be-save" onclick="saveBoardEdit()">Save</button>
    </div>
  </div>
</div>

<!-- Board card detail (full-screen) -->
<div id="board-detail-overlay" class="overlay">
  <div class="overlay-header">
    <div style="display:flex;align-items:center;gap:10px;min-width:0;">
      <button class="btn" onclick="closeBoardDetail()">&#x2190; Back</button>
      <span id="bd-key" class="board-detail-key"></span>
    </div>
    <button class="btn" onclick="boardDetailDelete()" style="color:var(--red);border-color:rgba(248,81,73,0.3);">Delete</button>
  </div>
  <div class="board-detail-body">
    <textarea id="bd-title" class="board-detail-title-input" placeholder="Untitled" autocomplete="off" autocorrect="on" spellcheck="true" rows="1" oninput="this.style.height='auto';this.style.height=this.scrollHeight+'px'"></textarea>
    <div class="board-detail-status-row" id="bd-status-row"></div>
    <div class="board-detail-row" id="bd-session-row">
      <span style="font-size:0.78rem;color:var(--dim);">Session:</span>
      <select id="bd-session" class="board-detail-session-select"></select>
    </div>
    <div class="board-detail-row">
      <span style="font-size:0.78rem;color:var(--dim);">Due:</span>
      <input type="date" id="bd-due" class="board-detail-session-select" style="flex:1;cursor:pointer;">
    </div>
    <div class="board-detail-tabs">
      <button class="board-detail-tab active" id="bd-tab-edit" onclick="boardDetailTab('edit')">Edit</button>
      <button class="board-detail-tab" id="bd-tab-preview" onclick="boardDetailTab('preview')">Preview</button>
    </div>
    <textarea id="bd-desc" class="board-detail-desc-input" placeholder="Add notes, description, or context... (supports Markdown)"></textarea>
    <div id="bd-preview" class="board-detail-preview" style="display:none;"></div>
    <div class="board-detail-meta" id="bd-meta"></div>
  </div>
  <div class="board-detail-footer">
    <span id="bd-save-status" style="font-size:0.78rem;color:var(--dim);"></span>
    <button class="btn primary" onclick="boardDetailSave()">Save</button>
  </div>
</div>

<!-- Create session modal -->
<div id="create-overlay" class="edit-overlay" onclick="if(event.target===this)closeCreate()">
  <div class="edit-box">
    <h3>New session</h3>
    <div class="field-group">
      <label class="field-label">Name</label>
      <input id="create-name" type="text" placeholder="my-project" autocomplete="off" autocorrect="off"
        onkeydown="if(event.key==='Enter'){event.preventDefault();document.getElementById('create-dir').focus({preventScroll:true});}">
    </div>
    <div class="field-group">
      <label class="field-label">Working directory</label>
      <div class="ac-wrap">
        <input id="create-dir" type="text" placeholder="/path/to/project" autocomplete="off" autocorrect="off"
          oninput="acFetch(this.value)" onfocus="acFetch(this.value)"
          onkeydown="acKeydown(event)">
        <div id="ac-list" class="ac-list"></div>
      </div>
    </div>
    <div class="field-group">
      <label class="field-label">Initial prompt <span class="field-optional">(optional)</span></label>
      <textarea id="create-prompt" rows="3" placeholder="What should Claude work on first?"></textarea>
    </div>
    <div class="edit-actions">
      <button class="btn" onclick="closeCreate()">Cancel</button>
      <button class="btn primary" onclick="submitCreate()">Create</button>
    </div>
  </div>
</div>

<!-- Connect session modal -->
<div id="connect-overlay" class="edit-overlay" onclick="if(event.target===this)closeConnect()">
  <div class="edit-box">
    <h3>Connect to tmux session</h3>
    <div id="connect-list" style="max-height:260px;overflow-y:auto;margin-bottom:14px;"></div>
    <div class="edit-actions">
      <button class="btn" onclick="closeConnect()">Cancel</button>
    </div>
  </div>
</div>

<!-- Peek overlay -->
<div id="peek-overlay" class="overlay">
  <div class="overlay-header" style="flex-direction:column;gap:6px;padding-bottom:10px;">
    <div style="display:flex;align-items:center;gap:10px;min-width:0;">
      <h2 id="peek-title" style="margin:0;font-size:1.05rem;font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;">peek</h2>
      <span id="peek-session-status"></span>
    </div>
    <div style="display:flex;gap:8px;align-items:center;">
      <div class="search-wrap" id="peek-search-wrap">
        <input class="search-input" id="peek-search" type="text" placeholder="Find..." autocomplete="off" autocorrect="off"
          oninput="peekSearchQuery=this.value;document.getElementById('peek-search-wrap').classList.toggle('has-value',!!this.value);applyPeekSearch()">
        <span class="search-count" id="peek-search-count"></span>
        <button class="search-clear" onclick="event.stopPropagation();clearPeekSearch()">&#x2715;</button>
      </div>
      <button class="btn" id="peek-explore-btn" onclick="openExplore(peekSessionDir)" title="Browse files">&#x1F4C2;</button>
      <button class="btn" onclick="closePeek()">Close</button>
    </div>
  </div>
  <!-- Tab bar -->
  <div class="peek-tabs">
    <button class="peek-tab active" id="peek-tab-terminal" onclick="setPeekTab('terminal')">Terminal</button>
    <button class="peek-tab" id="peek-tab-issues" onclick="setPeekTab('issues')">Issues</button>
    <button class="peek-tab" id="peek-tab-memory" onclick="setPeekTab('memory')">Memory</button>
  </div>
  <!-- Working directory bar -->
  <div class="peek-dir-bar">
    <span style="flex-shrink:0;opacity:0.6;">&#x1F4C1;</span>
    <span id="peek-dir-text"></span>
    <span class="card-dir-edit" id="peek-dir-edit" onclick="editField(peekSession,'dir',peekSessionDir)" title="Change directory">&#x270E;</span>
  </div>
  <!-- Terminal panel -->
  <div id="peek-terminal-panel" class="peek-terminal-panel">
    <div style="position:relative;flex:1;min-height:0;">
      <div id="peek-body" class="overlay-body" style="position:absolute;inset:0;"></div>
      <button class="peek-copy-btn" id="peek-copy-btn" onclick="copyPeekContent()" title="Copy all">&#x2398; Copy</button>
    </div>
    <div id="peek-status" class="overlay-status"></div>
    <div class="peek-cmd-bar">
      <button class="peek-cmd-toggle" id="peek-cmd-toggle" onclick="togglePeekCmd()">&#x25BC; Send command</button>
      <div class="peek-cmd-row open" id="peek-cmd-row" style="flex-wrap:wrap;">
        <div class="chips" style="width:100%;margin:0;">
          <div class="chip" onclick="peekQuickKeys('C-c')">Ctrl-C</div>
          <div class="chip" onclick="peekQuickKeys('Up')">&#x2191;</div>
          <div class="chip" onclick="peekQuickKeys('Down')">&#x2193;</div>
          <div class="chip" onclick="peekQuickKeys('Enter')">Enter</div>
          <div class="chip" onclick="peekQuickSend('/mcp')">/mcp</div>
          <div class="chip" onclick="peekQuickSend('/status')">/status</div>
          <div class="chip" onclick="peekQuickSend('/cost')">/cost</div>
          <div class="chip" onclick="peekQuickKeys('Escape')">Esc</div>
          <div class="chip danger" onclick="peekQuickSend('/compact')">/compact</div>
          <div class="chip danger" onclick="peekQuickSend('/clear')">/clear</div>
        </div>
        <!-- Attachment chips -->
        <div class="peek-attach-bar" id="peek-attach-bar"></div>
        <!-- Input row -->
        <div class="ac-wrap" style="flex:1;min-width:0;overflow:hidden;">
          <textarea class="send-input" id="peek-cmd-input" rows="1" placeholder="Type a message or drop a file..."
            autocomplete="off" autocorrect="on" autocapitalize="sentences" spellcheck="true"
            enterkeyhint="enter" style="width:100%;"
            oninput="autoGrow(this);slashAcUpdate();cmdHistoryReset()" onkeydown="slashAcKeydown(event)"
            onpaste="handlePeekPaste(event)"></textarea>
          <div id="slash-ac-list" class="ac-list slash-ac"></div>
        </div>
        <input type="file" id="peek-file-input" multiple accept="image/*,.pdf,.txt,.md,.csv,.json,.log"
          style="display:none" onchange="handlePeekFileInput(event)">
        <label for="peek-file-input" class="peek-attach-btn" title="Attach file">&#128206;</label>
        <button class="btn primary" onclick="sendPeekCmd()">Send</button>
      </div>
      <!-- Drag-over hint (shown by CSS when drag-over class is on peek-overlay) -->
      <div class="peek-drag-hint" style="display:none;">&#128206; Drop to attach</div>
    </div>
  </div>
  <!-- Issues panel (board issues for this session) -->
  <div id="peek-issues-panel" class="peek-tasks-panel">
    <div class="peek-tasks-add" style="gap:10px;">
      <span id="peek-issues-count" style="flex:1;font-size:0.82rem;color:var(--dim);align-self:center;"></span>
      <button class="btn primary" style="font-size:0.8rem;padding:5px 12px;" onclick="openBoardAdd('backlog')">+ New issue</button>
    </div>
    <div class="peek-tasks-list" id="peek-issues-list"></div>
  </div>
  <!-- Memory editor panel -->
  <div id="peek-memory-panel" class="peek-memory-editor">
    <div style="display:flex;align-items:center;justify-content:space-between;flex-shrink:0;">
      <div class="board-detail-tabs" style="border-bottom:none;margin:0;">
        <button class="board-detail-tab active" id="pm-tab-edit" onclick="peekMemoryTab('edit')">Session</button>
        <button class="board-detail-tab" id="pm-tab-preview" onclick="peekMemoryTab('preview')">Preview</button>
        <button class="board-detail-tab" id="pm-tab-global" onclick="peekMemoryTab('global')" title="Global memory shared by all sessions">Global</button>
      </div>
      <div style="display:flex;gap:6px;">
        <button class="btn" id="peek-memory-pull" onclick="pullPeekMemory()" title="Pull latest from Claude's memory file">↻</button>
        <button class="btn primary" id="peek-memory-save" onclick="savePeekMemory()">Save</button>
      </div>
    </div>
    <textarea id="peek-memory-input" class="peek-memory-textarea"
      placeholder="No memory yet. Add notes, context, or conventions that Claude should always remember for this session..."></textarea>
    <div id="peek-memory-preview" class="board-detail-preview" style="display:none;flex:1;overflow-y:auto;min-height:0;"></div>
    <textarea id="peek-global-input" class="peek-memory-textarea" style="display:none;"
      placeholder="Global memory — applied to ALL sessions. Add conventions, tools, or preferences shared across all your sessions..."></textarea>
  </div>
</div>

<!-- Edit modal -->
<div id="edit-overlay" class="edit-overlay" onclick="if(event.target===this)closeEdit()">
  <div class="edit-box">
    <h3 id="edit-title">Edit</h3>
    <div class="ac-wrap" id="edit-input-wrap">
      <input id="edit-input" type="text" autocomplete="off" autocorrect="off"
        oninput="if(editState&&editState.field==='dir')editAcFetch(this.value);if(editState&&editState.field==='tags')tagAcUpdate(this.value)"
        onfocus="if(editState&&editState.field==='dir')editAcFetch(this.value);if(editState&&editState.field==='tags')tagAcUpdate(this.value)"
        onkeydown="editAcKeydown(event)">
      <div id="edit-ac-list" class="ac-list"></div>
    </div>
    <select id="edit-select" style="display:none;" onchange="submitEdit()">
      <option value="">Default (sonnet)</option>
      <option value="opus">opus</option>
      <option value="sonnet">sonnet</option>
      <option value="haiku">haiku</option>
      <option value="claude-opus-4-6">claude-opus-4-6</option>
      <option value="claude-sonnet-4-6">claude-sonnet-4-6</option>
      <option value="claude-haiku-4-5-20251001">claude-haiku-4-5-20251001</option>
    </select>
    <div class="edit-actions">
      <button class="btn" onclick="closeEdit()">Cancel</button>
      <button class="btn primary" onclick="submitEdit()">Save</button>
    </div>
  </div>
</div>

<!-- Queue modal -->
<div id="queue-overlay" class="queue-overlay" onclick="if(event.target===this)closeQueueModal()">
  <div class="queue-box">
    <h3>Offline Queue</h3>
    <div id="queue-list"></div>
    <div style="display:flex;gap:8px;justify-content:flex-end;margin-top:14px;">
      <button class="btn danger" onclick="clearQueue()">Clear queue</button>
      <button class="btn primary" onclick="forceRetry()">Retry now</button>
      <button class="btn" onclick="closeQueueModal()">Close</button>
    </div>
  </div>
</div>

<!-- About modal -->
<div id="about-overlay" class="queue-overlay" onclick="if(event.target===this)this.classList.remove('active')">
  <div class="queue-box" style="max-width:340px;">
    <div style="text-align:center;">
      <h3 style="margin:0 0 4px;">amux</h3>
      <div style="color:var(--dim);font-size:0.8rem;">Claude Code Multiplexer</div>
      <div style="color:var(--dim);font-size:0.7rem;font-family:monospace;margin-top:2px;"><script>document.write(location.host)</script></div>
      <div style="margin:8px 0 4px;font-size:0.95rem;font-weight:600;cursor:pointer;" onclick="forceUpdate()" title="Tap to force update">v0.6.0 &#x21BB;</div>
      <div id="update-status" style="color:var(--dim);font-size:0.75rem;min-height:1.2em;"></div>
    </div>
    <div id="daily-stats" style="margin-top:12px;border-top:1px solid var(--border);padding-top:12px;">
      <div style="color:var(--dim);font-size:0.75rem;text-align:center;">Loading token stats...</div>
    </div>
    <div id="server-switcher" style="margin-top:12px;border-top:1px solid var(--border);padding-top:12px;">
      <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:8px;">
        <div style="font-size:0.8rem;font-weight:600;">Servers</div>
        <button class="btn" style="font-size:0.65rem;padding:2px 8px;" onclick="toggleAddServer()">+ Add</button>
      </div>
      <div id="add-server-form" style="display:none;margin-bottom:8px;">
        <input id="add-server-name" class="search-input" type="text" placeholder="Label (e.g. Work laptop)" style="width:100%;margin-bottom:4px;font-size:0.75rem;padding:6px 8px;box-sizing:border-box;">
        <input id="add-server-url" class="search-input" type="text" placeholder="https://host:8822" style="width:100%;margin-bottom:6px;font-size:0.75rem;padding:6px 8px;box-sizing:border-box;">
        <div style="display:flex;gap:6px;justify-content:flex-end;">
          <button class="btn" style="font-size:0.65rem;padding:2px 8px;" onclick="toggleAddServer()">Cancel</button>
          <button class="btn" style="font-size:0.65rem;padding:2px 8px;background:var(--accent);color:#000;" onclick="saveNewServer()">Save</button>
        </div>
      </div>
      <div id="server-list"></div>
    </div>
    <div id="debug-panel" style="margin-top:12px;border-top:1px solid var(--border);padding-top:10px;">
      <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:6px;">
        <div style="font-size:0.78rem;font-weight:600;">Connection</div>
        <button class="btn" style="font-size:0.65rem;padding:2px 8px;" onclick="pingServer()">Ping</button>
      </div>
      <div id="debug-info" style="font-size:0.7rem;font-family:monospace;color:var(--dim);line-height:1.6;"></div>
    </div>
    <div style="display:flex;gap:8px;justify-content:center;margin-top:14px;">
      <button class="btn" onclick="document.getElementById('about-overlay').classList.remove('active')">Close</button>
    </div>
  </div>
</div>

<!-- Sync activity banner -->
<div id="sync-banner" class="sync-banner">
  <div class="sync-title">
    <span id="sync-title-text">Syncing...</span>
    <button class="btn" style="font-size:0.7rem;padding:2px 8px;" onclick="document.getElementById('sync-banner').classList.remove('active')">Dismiss</button>
  </div>
  <div id="sync-items"></div>
</div>

<!-- Toast -->
<div id="toast" class="toast"></div>

<!-- Confirm / alert modal -->
<div id="modal-backdrop" class="modal-backdrop" onclick="_modalBgClick(event)">
  <div class="modal-box">
    <div id="modal-msg" class="modal-msg"></div>
    <div class="modal-btns" id="modal-btns"></div>
  </div>
</div>

<!-- Skills modal -->
<div id="skills-modal" class="overlay" style="z-index:200;" onclick="if(event.target===this)closeSkills()">
  <div style="display:flex;flex-direction:column;height:100%;max-width:560px;margin:0 auto;width:100%;">
    <div class="overlay-header">
      <h2>&#x26A1; Skills &amp; commands</h2>
      <button class="btn" onclick="closeSkills()">&#x2715;</button>
    </div>
    <p style="font-size:0.8rem;color:var(--dim);margin:0 0 14px;">Use these with <code style="background:var(--card);padding:1px 5px;border-radius:4px;font-size:0.78rem;">/command</code> in any Claude Code session.</p>
    <div id="skills-list" style="flex:1;overflow-y:auto;display:flex;flex-direction:column;gap:8px;"></div>
  </div>
</div>

<!-- File preview overlay (z-index 300 so it stacks above explorer at 250) -->
<div id="file-overlay" class="file-overlay" style="z-index:300;">
  <div class="file-overlay-header">
    <h2 id="file-title">file</h2>
    <div class="file-view-tabs" id="file-view-tabs" style="display:none;">
      <button class="file-view-tab active" id="file-tab-preview" onclick="setFileViewMode('preview')">Preview</button>
      <button class="file-view-tab" id="file-tab-raw" onclick="setFileViewMode('raw')">Raw</button>
    </div>
    <button class="btn" onclick="closeFilePreview()" style="flex-shrink:0;">&#x2715;</button>
  </div>
  <div id="file-body" class="file-overlay-body"></div>
</div>

<!-- File explorer overlay -->
<div id="explore-overlay" class="file-overlay" style="z-index:250;">
  <div class="file-overlay-header">
    <div class="explore-breadcrumb" id="explore-breadcrumb" style="flex:1;margin-right:8px;"></div>
    <button class="btn" id="explore-hidden-btn" onclick="toggleExploreHidden()" style="font-size:0.7rem;padding:2px 8px;" title="Show hidden files">.*</button>
    <button class="btn" onclick="closeExplore()">&#x2715;</button>
  </div>
  <div id="explore-body" class="file-overlay-body" style="padding:0;overflow-y:auto;"></div>
</div>

<script>
// ═══════ STATE & GLOBALS ═══════
const API = '';
let sessions = [];
let _initialLoad = true;   // true until first data arrives from server
let _lastDataTime = null;  // timestamp of last successful data
let _debugLog = [];        // recent connection events (capped at 12)
let _liveSSE = false;      // true only when SSE is actively receiving messages
let expanded = new Set();
let searchQuery = '';
let activeTag = '';
let peekSession = null;
let peekTimer = null;
let peekSessionDir = '';
let peekSearchQuery = '';
let lastPeekHTML = '';
const _peekDrafts = {};  // session name → command text

// ═══════ ZOOM ═══════
const ZOOM_STEPS = [50, 60, 70, 75, 80, 85, 90, 95, 100, 110, 120, 130, 150, 175, 200];
let _zoomLevel = 100;
function _applyZoom() {
  document.documentElement.style.zoom = (_zoomLevel / 100);
}
function zoomIn() {
  var idx = ZOOM_STEPS.indexOf(_zoomLevel);
  if (idx === -1) { for (var i = 0; i < ZOOM_STEPS.length; i++) { if (ZOOM_STEPS[i] > _zoomLevel) { idx = i - 1; break; } } if (idx === -1) idx = ZOOM_STEPS.length - 2; }
  if (idx < ZOOM_STEPS.length - 1) { _zoomLevel = ZOOM_STEPS[idx + 1]; _applyZoom(); }
}
function zoomOut() {
  var idx = ZOOM_STEPS.indexOf(_zoomLevel);
  if (idx === -1) { for (var i = ZOOM_STEPS.length - 1; i >= 0; i--) { if (ZOOM_STEPS[i] < _zoomLevel) { idx = i + 1; break; } } if (idx === -1) idx = 1; }
  if (idx > 0) { _zoomLevel = ZOOM_STEPS[idx - 1]; _applyZoom(); }
}
function resetZoom() { _zoomLevel = 100; _applyZoom(); }
// Keyboard shortcuts: Cmd/Ctrl +/- for zoom
document.addEventListener('keydown', function(e) {
  if ((e.metaKey || e.ctrlKey) && (e.key === '=' || e.key === '+')) { e.preventDefault(); zoomIn(); }
  else if ((e.metaKey || e.ctrlKey) && e.key === '-') { e.preventDefault(); zoomOut(); }
  else if ((e.metaKey || e.ctrlKey) && e.key === '0') { e.preventDefault(); resetZoom(); }
});

// Connection & offline state
let online = true;
window.addEventListener('offline', () => setOnline(false));
window.addEventListener('online', () => { consecutiveFailures = 0; setOnline(true); });
// Migrate localStorage keys from cc_ to amux_
['offline_queue','sessions_cache','drafts'].forEach(k => {
  const old = localStorage.getItem('cc_' + k);
  if (old && !localStorage.getItem('amux_' + k)) {
    localStorage.setItem('amux_' + k, old);
    localStorage.removeItem('cc_' + k);
  }
});
let offlineQueue = JSON.parse(localStorage.getItem('amux_offline_queue') || '[]');
function saveQueue() {
  localStorage.setItem('amux_offline_queue', JSON.stringify(offlineQueue));
  if (typeof _idb !== 'undefined') _idb.set('offline_queue', offlineQueue);
}

// ═══════ DEVICE NAME ═══════
function _getDeviceName() {
  const custom = localStorage.getItem('amux_device_name');
  if (custom) return custom;
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return 'iPhone';
  if (/iPad/.test(ua)) return 'iPad';
  if (/Android/.test(ua)) return 'Android';
  if (/Windows/.test(ua)) return 'Windows';
  if (/Mac/.test(ua)) return 'Mac';
  if (/Linux/.test(ua)) return 'Linux';
  return 'Unknown';
}

// ═══════ DRAFTS — offline-created sessions ═══════
let drafts = JSON.parse(localStorage.getItem('amux_drafts') || '[]');
// Draft shape: { name, dir, prompt, created_at, syncing }

function saveDrafts() {
  localStorage.setItem('amux_drafts', JSON.stringify(drafts));
  if (typeof _idb !== 'undefined') _idb.set('drafts', drafts);
}

function addDraft(name, dir, prompt) {
  drafts.push({ name, dir: dir || '', prompt: prompt || '', creator: _getDeviceName(), created_at: Date.now(), syncing: false });
  saveDrafts();
}

function removeDraft(name) {
  drafts = drafts.filter(d => d.name !== name);
  saveDrafts();
}

function getDraftPrompt(name) {
  const d = drafts.find(d => d.name === name);
  return d ? d.prompt : '';
}
let consecutiveFailures = 0;

// Tailscale URL rewriting
const remoteHost = (location.hostname !== 'localhost' && location.hostname !== '127.0.0.1') ? location.host : null;
const remoteHostname = remoteHost ? location.hostname : null;

// Toast system
let toastTimer = null;
function showToast(msg) {
  const el = document.getElementById('toast');
  el.textContent = msg;
  el.classList.add('visible');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove('visible'), 3000);
}

// Modal: replaces confirm() / alert() — both blocked in PWA standalone mode
let _modalResolve = null;
function _modalBgClick(e) { if (e.target === document.getElementById('modal-backdrop')) _modalClose(false); }
function _modalClose(val) {
  document.getElementById('modal-backdrop').classList.remove('open');
  if (_modalResolve) { _modalResolve(val); _modalResolve = null; }
}
function showConfirm(msg, confirmLabel = 'Confirm', danger = false) {
  return new Promise(resolve => {
    _modalResolve = resolve;
    document.getElementById('modal-msg').textContent = msg;
    const btns = document.getElementById('modal-btns');
    btns.innerHTML = `
      <button class="btn ${danger ? 'danger' : 'primary'}" onclick="_modalClose(true)">${confirmLabel}</button>
      <button class="btn" onclick="_modalClose(false)">Cancel</button>`;
    document.getElementById('modal-backdrop').classList.add('open');
  });
}
function showAlert(msg) {
  return new Promise(resolve => {
    _modalResolve = resolve;
    document.getElementById('modal-msg').textContent = msg;
    const btns = document.getElementById('modal-btns');
    btns.innerHTML = `<button class="btn primary" onclick="_modalClose(true)">OK</button>`;
    document.getElementById('modal-backdrop').classList.add('open');
  });
}

async function showSessionInfo(name) {
  const r = await fetch(API + '/api/sessions/' + name + '/meta');
  const m = await r.json();
  const ts = t => t ? new Date(t * 1000).toLocaleString() : '—';
  const row = (label, val) => val ? `<div style="display:flex;gap:8px;padding:5px 0;border-bottom:1px solid var(--border);font-size:0.85rem;"><span style="color:var(--dim);min-width:110px;flex-shrink:0;">${label}</span><span style="word-break:break-all;">${val}</span></div>` : '';
  const html = `<div style="text-align:left;">
    <div style="font-size:1.05rem;font-weight:700;margin-bottom:12px;">${esc(name)}</div>
    ${row('Created', ts(m.created_at))}
    ${row('Creator', m.creator)}
    ${row('Last started', ts(m.last_started))}
    ${row('Start count', m.start_count !== undefined ? m.start_count : '—')}
    ${row('Env updated', ts(m.env_updated))}
    ${row('Directory', m.dir)}
    ${row('Model / flags', m.flags || '(default sonnet)')}
    ${m.desc ? row('Description', m.desc) : ''}
    ${m.tags && m.tags.length ? row('Tags', m.tags.join(', ')) : ''}
    ${row('Memory size', m.mem_size ? m.mem_size + ' bytes' : '(empty)')}
    ${row('Memory path', m.mem_path)}
  </div>`;
  _modalResolve = null;
  document.getElementById('modal-msg').innerHTML = html;
  document.getElementById('modal-btns').innerHTML = '<button class="btn primary" onclick="_modalClose(true)">Close</button>';
  document.getElementById('modal-backdrop').classList.add('open');
}

// Describe a queued operation in human-readable form
function describeOp(item) {
  const url = item.url || '';
  const method = (item.options && item.options.method) || 'GET';
  // Parse: /api/sessions/<name>/<action>
  const m = url.match(/\/api\/sessions\/([^/]+)(?:\/(\w+))?/);
  if (!m) return method + ' ' + url;
  const name = decodeURIComponent(m[1]);
  const action = m[2] || '';
  if (action === 'start') return 'Start ' + name;
  if (action === 'stop') return 'Stop ' + name;
  if (action === 'send') {
    let text = '';
    try { text = JSON.parse(item.options.body).text || ''; } catch(e) {}
    const preview = text.length > 30 ? text.slice(0, 30) + '...' : text;
    return 'Send to ' + name + (preview ? ': ' + preview : '');
  }
  if (action === 'keys') return 'Keys to ' + name;
  if (action === 'delete') return 'Delete ' + name;
  if (action === 'clear') return 'Clear ' + name;
  if (action === 'config') return 'Update ' + name;
  if (action === 'duplicate') return 'Duplicate ' + name;
  if (action === 'clone') return 'Clone ' + name;
  if (!action && method === 'POST') return 'Create ' + name;
  return method + ' ' + name + (action ? '/' + action : '');
}

// Connection status
function updateConnectionStatus() {
  // Update all connection status indicators (main + peek)
  document.querySelectorAll('#conn-status').forEach(el => {
    if (!online) {
      el.className = 'conn-status offline';
      const total = offlineQueue.length + drafts.length;
      el.textContent = total ? total + ' pending' : 'Offline';
    } else if (_liveSSE) {
      el.className = 'conn-status online';
      el.textContent = 'Live';
    } else {
      el.className = 'conn-status polling';
      el.textContent = 'Polling';
    }
  });
  // Update offline banner
  const banner = document.getElementById('offline-banner');
  const ops = document.getElementById('offline-ops');
  const title = document.getElementById('offline-banner-title');
  if (!banner) return;
  const hasPending = offlineQueue.length || drafts.length;
  if (online || !hasPending) {
    banner.classList.remove('active');
    return;
  }
  banner.classList.add('active');
  const parts = [];
  if (drafts.length) parts.push(drafts.length + ' draft' + (drafts.length === 1 ? '' : 's'));
  if (offlineQueue.length) parts.push(offlineQueue.length + ' op' + (offlineQueue.length === 1 ? '' : 's'));
  title.innerHTML = '&#x26A0; Offline &mdash; ' + parts.join(', ') + ' pending';
  let html = '';
  html += drafts.map(d => {
    return '<div class="offline-op">' +
      '<span class="op-action">Create &amp; start ' + esc(d.name) + (d.prompt ? ' + prompt' : '') + '</span>' +
      '<span class="op-time" style="color:var(--yellow)">draft</span>' +
    '</div>';
  }).join('');
  html += offlineQueue.map(item => {
    const age = Math.floor((Date.now() - item.timestamp) / 60000);
    const timeStr = age < 1 ? 'just now' : age + 'm ago';
    return '<div class="offline-op">' +
      '<span class="op-action">' + esc(describeOp(item)) + '</span>' +
      '<span class="op-time">' + timeStr + '</span>' +
    '</div>';
  }).join('');
  ops.innerHTML = html;
}

function setOnline(val) {
  const was = online;
  online = val;
  if (val) consecutiveFailures = 0;
  if (!val) _liveSSE = false;
  updateConnectionStatus();
  if (!was && val) {
    showToast('Reconnected — syncing...');
    runSyncBanner();
    // Reconnect SSE (reset fallback so we can get back to Live mode)
    _sseFallback = false; _sseRetries = 0;
    if (!_sse) connectSSE();
  } else if (was && !val) {
    showToast('Server unreachable — offline mode');
  }
}

// ═══════ SYNC BANNER ORCHESTRATOR ═══════
async function runSyncBanner() {
  const banner = document.getElementById('sync-banner');
  const itemsEl = document.getElementById('sync-items');
  const titleEl = document.getElementById('sync-title-text');
  const draftCount = drafts.length;
  const rawQueue = [...offlineQueue];
  offlineQueue = [];
  saveQueue();
  const queue = reconcileQueue(rawQueue);
  const skipped = rawQueue.length - queue.length;
  const totalOps = draftCount + queue.length;
  if (!totalOps) return;

  // Build item list
  const items = [];
  drafts.forEach(d => items.push({ label: 'Create & start "' + d.name + '"', status: 'pending', type: 'draft', draft: d }));
  queue.forEach(q => items.push({ label: describeOp(q), status: 'pending', type: 'queue', item: q }));

  function renderBanner() {
    const done = items.filter(i => i.status === 'done').length;
    const failed = items.filter(i => i.status === 'failed').length;
    titleEl.textContent = 'Syncing ' + done + '/' + items.length + (failed ? ' (' + failed + ' failed)' : '') + (skipped ? ' (' + skipped + ' skipped)' : '');
    itemsEl.innerHTML = items.map(i => {
      const icon = i.status === 'done' ? '&#x2714;' : i.status === 'failed' ? '&#x2718;' : i.status === 'running' ? '&#x27A4;' : '&#x2022;';
      return '<div class="sync-item ' + i.status + '">' + icon + ' ' + esc(i.label) + '</div>';
    }).join('');
  }

  renderBanner();
  banner.classList.add('active');

  // Sync drafts first
  for (const item of items.filter(i => i.type === 'draft')) {
    item.status = 'running';
    renderBanner();
    try {
      const draft = item.draft;
      draft.syncing = true; saveDrafts(); render();
      const createResp = await fetch(API + '/api/sessions', {
        method: 'POST', headers: {'Content-Type':'application/json'},
        body: JSON.stringify({ name: draft.name, dir: draft.dir })
      });
      if (!createResp.ok && createResp.status !== 409) {
        item.status = 'failed'; draft.syncing = false; saveDrafts(); renderBanner(); continue;
      }
      const startResp = await fetch(API + '/api/sessions/' + encodeURIComponent(draft.name) + '/start', { method: 'POST' });
      if (draft.prompt && startResp.ok) {
        await new Promise(r => setTimeout(r, 5000));
        await fetch(API + '/api/sessions/' + encodeURIComponent(draft.name) + '/send', {
          method: 'POST', headers: {'Content-Type':'application/json'},
          body: JSON.stringify({ text: draft.prompt })
        });
      }
      removeDraft(draft.name); render();
      item.status = 'done';
    } catch(e) {
      item.status = 'failed'; item.draft.syncing = false; saveDrafts();
    }
    renderBanner();
  }

  // Then replay queue items
  for (const item of items.filter(i => i.type === 'queue')) {
    item.status = 'running';
    renderBanner();
    try {
      const r = await fetch(item.item.url, item.item.options);
      if (r.status >= 500) {
        offlineQueue.push(item.item); item.status = 'failed';
      } else {
        item.status = 'done';
      }
    } catch(e) {
      offlineQueue.push(item.item); item.status = 'failed';
    }
    renderBanner();
  }

  if (offlineQueue.length) saveQueue();
  const doneCount = items.filter(i => i.status === 'done').length;
  const failCount = items.filter(i => i.status === 'failed').length;
  titleEl.textContent = doneCount + ' synced' + (failCount ? ', ' + failCount + ' failed' : '') + (skipped ? ', ' + skipped + ' skipped' : '');
  updateConnectionStatus();
  fetchSessions();
  fetchBoard();
  // Auto-dismiss after 4s if all succeeded
  if (!failCount) setTimeout(() => banner.classList.remove('active'), 4000);
}

// Queue modal
function showQueueModal() {
  const el = document.getElementById('queue-list');
  if (!offlineQueue.length) {
    el.innerHTML = '<div class="queue-empty">No queued operations.</div>';
  } else {
    el.innerHTML = offlineQueue.map((item, i) =>
      '<div class="queue-item">' +
        esc(describeOp(item)) +
        '<br><span class="queue-time">' + new Date(item.timestamp).toLocaleTimeString() + '</span>' +
      '</div>'
    ).join('');
  }
  document.getElementById('queue-overlay').classList.add('active');
}
function closeQueueModal() {
  document.getElementById('queue-overlay').classList.remove('active');
}
function clearQueue() {
  offlineQueue = [];
  saveQueue();
  updateConnectionStatus();
  closeQueueModal();
  showToast('Queue cleared');
}
async function forceRetry() {
  closeQueueModal();
  if (!offlineQueue.length && !drafts.length) return;
  if (online) { runSyncBanner(); } else { setOnline(true); }
}

// Auto-retry queued ops when page becomes visible (returning from background on mobile)
document.addEventListener('visibilitychange', () => {
  if (!document.hidden) {
    if ((offlineQueue.length || drafts.length) && online) {
      runSyncBanner();
    } else if (!online && navigator.onLine !== false) {
      consecutiveFailures = 0;
      setOnline(true);
    }
  }
});

// apiCall — wraps mutation fetches; queues when offline or server unreachable
async function apiCall(url, options) {
  if (!online) {
    _queueOp(url, options);
    return null;
  }
  try {
    const r = await fetch(url, options);
    if (!r.ok) {
      showToast('Error: ' + r.status);
      return null;
    }
    consecutiveFailures = 0;
    return r;
  } catch(e) {
    // Server unreachable despite having internet — queue and mark offline
    console.error('apiCall failed:', e);
    consecutiveFailures++;
    if (consecutiveFailures >= 2) setOnline(false);
    _queueOp(url, options);
    return null;
  }
}
function _queueOp(url, options) {
  offlineQueue.push({ url, options: { method: options.method, headers: options.headers, body: options.body }, timestamp: Date.now() });
  saveQueue();
  // Register Background Sync so SW can replay queue when connectivity returns
  if ('serviceWorker' in navigator && 'SyncManager' in window) {
    navigator.serviceWorker.ready.then(r => r.sync.register('replay-queue').catch(() => {}));
  }
  updateConnectionStatus();
  showToast('Queued (' + offlineQueue.length + ' pending)');
}

// Reconcile queue: remove contradictory/stale operations before replay
function reconcileQueue(queue) {
  // Walk backwards and track the last action per session to skip superseded ops
  const lastAction = {};  // session -> last action seen
  const dominated = new Set();  // indices to skip
  for (let i = queue.length - 1; i >= 0; i--) {
    const m = (queue[i].url || '').match(/\/api\/sessions\/([^/]+)(?:\/(\w+))?/);
    if (!m) continue;
    const session = m[1];
    const action = m[2] || 'create';
    const key = session;
    if (!lastAction[key]) {
      lastAction[key] = action;
    } else {
      // Skip start if a later stop exists for same session (and vice versa)
      if ((action === 'start' && lastAction[key] === 'stop') ||
          (action === 'stop' && lastAction[key] === 'start')) {
        dominated.add(i);
      }
      // Skip delete if session was already created then deleted
      if (action === 'create' && lastAction[key] === 'delete') {
        dominated.add(i);
      }
    }
  }
  return queue.filter((_, i) => !dominated.has(i));
}

// (replayQueue and syncDrafts merged into runSyncBanner above)

// ═══════ API & CONNECTION ═══════
let lastSessionsJSON = '';
async function fetchSessions() {
  try {
    const r = await fetch(API + '/api/sessions');
    const data = await r.json();
    consecutiveFailures = 0;
    _lastDataTime = Date.now();
    if (_initialLoad) { _initialLoad = false; }
    if (!online) setOnline(true);
    const j = JSON.stringify(data);
    if (j !== lastSessionsJSON) {
      lastSessionsJSON = j;
      sessions = data;
      localStorage.setItem('amux_sessions_cache', j);
      render();
    }
  } catch(e) {
    console.error('fetch sessions:', e);
    consecutiveFailures++;
    if (consecutiveFailures >= 2 || navigator.onLine === false) {
      setOnline(false);
    }
  }
}

// ═══════ RENDERING ═══════
function updatePeekStatus() {
  const el = document.getElementById('peek-session-status');
  if (!el || !peekSession) { if (el) el.innerHTML = ''; return; }
  const s = sessions.find(s => s.name === peekSession);
  if (!s) { el.innerHTML = ''; return; }
  let badge = '';
  if (s.status === 'active')  badge = '<span class="status-badge active">working</span>';
  else if (s.status === 'waiting') badge = '<span class="status-badge waiting">needs input</span>';
  else if (s.status === 'idle')    badge = '<span class="status-badge idle">idle</span>';
  else if (!s.running)             badge = '<span class="status-badge" style="background:rgba(255,255,255,0.06);color:var(--dim);border:1px solid var(--border);">stopped</span>';
  el.innerHTML = badge;
}

function render() {
  // Skip render if a menu or edit overlay is open to prevent DOM clobbering
  if (openMenu || editState || document.getElementById('edit-overlay').classList.contains('active')) return;
  updatePeekStatus();
  const el = document.getElementById('cards');
  updateActiveCount();
  // Build tag filter bar
  const tagEl = document.getElementById('tag-filters');
  const allTags = [...new Set(sessions.flatMap(s => s.tags || []))].sort();
  if (allTags.length) {
    tagEl.innerHTML = allTags.map(t =>
      `<span class="tag-filter${activeTag === t ? ' active' : ''}" onclick="toggleTagFilter('${esc(t)}')">${esc(t)}</span>`
    ).join('');
  } else {
    tagEl.innerHTML = '';
  }
  if (!sessions.length && !drafts.length) {
    if (_initialLoad) {
      el.innerHTML = '<div class="empty"><span class="loading-spinner"></span>Connecting to server…</div>';
    } else {
      el.innerHTML = '<div class="empty">No sessions yet.<br>Tap <strong>+</strong> to create one.' +
        (!online ? '<br><span style="color:var(--yellow)">You\'re offline — sessions created now will sync when connected.</span>' : '') + '</div>';
    }
    return;
  }

  // Render draft cards at the top
  const draftCards = drafts.map(d => {
    const age = Math.floor((Date.now() - d.created_at) / 60000);
    const timeStr = age < 1 ? 'just now' : age + 'm ago';
    return `<div class="card" style="border-color:var(--yellow);opacity:${d.syncing?'0.6':'1'}">
      <div class="card-header">
        <div class="dot stopped" style="background:var(--yellow)"></div>
        <div class="card-name">${esc(d.name)}</div>
        <span class="draft-badge">${d.syncing ? 'syncing' : 'draft'}</span>
        <span class="last-active">${timeStr}</span>
        <button class="card-menu-btn" onclick="event.stopPropagation();removeDraft('${esc(d.name)}');render();" title="Remove draft">&#x2716;</button>
      </div>
      ${d.dir ? '<div class="card-dir">' + esc(d.dir) + '</div>' : ''}
      ${d.creator ? '<div class="card-dir" style="font-size:0.72rem;">' + esc(d.creator) + '</div>' : ''}
      ${d.prompt ? '<div class="draft-prompt">' + esc(d.prompt) + '</div>' : ''}
    </div>`;
  }).join('');

  // Filter by tag
  let list = activeTag ? sessions.filter(s => (s.tags || []).includes(activeTag)) : sessions;
  // Filter by search query
  const q = searchQuery.toLowerCase().trim();
  const filtered = q ? list.filter(s =>
    s.name.toLowerCase().includes(q) ||
    (s.dir || '').toLowerCase().includes(q) ||
    (s.desc || '').toLowerCase().includes(q) ||
    (s.tags || []).some(t => t.toLowerCase().includes(q))
  ) : list;
  if ((q || activeTag) && !filtered.length) {
    el.innerHTML = '<div class="empty">No matching sessions.</div>';
    return;
  }
  // Save input values and focus before re-rendering
  const savedInputs = {};
  el.querySelectorAll('.send-input').forEach(inp => {
    if (inp.value) savedInputs[inp.id] = inp.value;
  });
  const focusedId = document.activeElement && document.activeElement.classList.contains('send-input')
    ? document.activeElement.id : null;

  function _renderSessionCard(s) {
    const isExp = expanded.has(s.name);
    const flags = s.flags || '';
    const isYolo = flags.includes('--dangerously-skip-permissions');
    const modelMatch = flags.match(/--model\s+(\S+)/);
    const flagModel = modelMatch ? modelMatch[1] : null;
    const model = flagModel || s.active_model || null;
    const shortModel = model ? model.replace(/^claude-/, '').replace(/-\d{8}$/, '') : null;
    return `
    <div class="card ${isExp ? 'expanded' : ''}" data-session="${esc(s.name)}" onclick="event.stopPropagation();toggle('${s.name}')">
      <div class="card-header" onclick="headerTap('${s.name}', event)" onmousedown="tileMouseDown(event,'${s.name}')">
        <div class="card-header-top">
          <div class="card-drag-handle" title="Drag to reorder"><svg width="10" height="16" viewBox="0 0 10 16" fill="currentColor"><circle cx="3" cy="3" r="1.3"/><circle cx="7" cy="3" r="1.3"/><circle cx="3" cy="8" r="1.3"/><circle cx="7" cy="8" r="1.3"/><circle cx="3" cy="13" r="1.3"/><circle cx="7" cy="13" r="1.3"/></svg></div>
          <div class="dot ${s.running ? 'running' : 'stopped'}"></div>
          <div class="card-name">${s.pinned ? '<span class="pin-icon">&#x1F4CC;</span> ' : ''}${esc(s.name)}</div>
          <button class="card-menu-btn" onclick="event.stopPropagation();toggleMenu('${s.name}')" title="Options">&#x22EF;</button>
          <div class="card-menu" id="menu-${s.name}">
          <div class="card-menu-item" onclick="event.stopPropagation();closeAllMenus();openPeek('${s.name}')"><span class="mi">&#x1F4BB;</span> Peek terminal</div>
          <div class="card-menu-item" onclick="event.stopPropagation();closeAllMenus();showSessionInfo('${s.name}')"><span class="mi">&#x2139;</span> Info</div>
          ${s.running ? `<div class="card-menu-item danger" onclick="event.stopPropagation();doStop('${s.name}')"><span class="mi">&#x23F9;</span> Stop</div>` : ''}
          <div class="card-menu-item" onclick="event.stopPropagation();togglePin('${s.name}')"><span class="mi">${s.pinned?'&#x1F4CC;':'&#x1F4CC;'}</span> ${s.pinned ? 'Unpin' : 'Pin to top'}</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','name','${esc(s.name)}')"><span class="mi">&#x270E;</span> Rename</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','model','${esc(model||"")}')"><span class="mi">&#x2699;</span> Model${model ? ': '+esc(model) : ''}</div>
          <div class="card-menu-item" onclick="event.stopPropagation();toggleYolo('${s.name}')"><span class="mi">${isYolo?'&#x2611;':'&#x2610;'}</span> YOLO mode</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','desc','${esc(s.desc||"")}')"><span class="mi">&#x1F4DD;</span> Description</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','tags','${esc(s.tags.join(", "))}')"><span class="mi">&#x1F3F7;</span> Tags</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','dir','${esc(s.dir)}')"><span class="mi">&#x1F4C1;</span> Directory</div>
          ${s.running ? `<div class="card-menu-item" onclick="event.stopPropagation();clearScrollback('${s.name}')"><span class="mi">&#x239A;</span> Clear scrollback</div>` : ''}
          <div class="card-menu-item" onclick="event.stopPropagation();duplicateSession('${s.name}')"><span class="mi">&#x2398;</span> Duplicate</div>
          ${s.running ? `<div class="card-menu-item" onclick="event.stopPropagation();cloneSession('${s.name}')"><span class="mi">&#x1F504;</span> Clone &amp; continue</div>` : ''}
          ${!s.running ? `<div class="card-menu-item" onclick="event.stopPropagation();newConversation('${s.name}')"><span class="mi">&#x1F195;</span> New conversation</div>` : ''}
          <div class="card-menu-sep"></div>
          <div class="card-menu-item danger" onclick="event.stopPropagation();deleteSession('${s.name}')"><span class="mi">&#x2716;</span> Delete</div>
        </div>
        </div>
        ${(s.status || s.tokens || s.last_activity || !online) ? `<div class="card-header-meta">
          ${s.status === 'active' ? '<span class="status-badge active">working</span>' : ''}
          ${s.status === 'waiting' ? '<span class="status-badge waiting">needs input</span>' : ''}
          ${s.status === 'idle' ? '<span class="status-badge idle">idle</span>' : ''}
          ${s.tokens ? `<span class="token-count">${fmtTokens(s.tokens)}</span>` : ''}
          ${s.last_activity ? `<span class="last-active">${timeAgo(s.last_activity)}</span>` : ''}
          ${!online ? '<span class="cached-badge">cached</span>' : ''}
        </div>` : ''}
      </div>
      ${s.dir ? `<div class="card-dir"><span class="card-dir-path" onclick="event.stopPropagation();openExplore('${s.dir.replace(/'/g,"\\'")}')" style="cursor:pointer;" title="Browse files">${esc(s.dir)}</span></div>` : ''}
      ${s.creator ? `<div class="card-dir" style="font-size:0.72rem;">${esc(s.creator)}</div>` : ''}
      ${isExp && s.desc ? `<div class="card-desc">${esc(s.desc)}</div>` : ''}
      ${!isExp && s.task_name ? `<div class="card-preview">${esc(s.task_name)}</div>` : ''}
      ${isExp && s.preview ? `<div class="card-preview">${esc(s.preview)}</div>` : ''}
      ${(isYolo || model || s.tags.length) ? `<div class="badges">
        ${isYolo ? '<span class="badge yolo">YOLO</span>' : ''}
        ${model ? `<span class="badge model">${esc(model)}</span>` : ''}
        ${s.tags.map(t => `<span class="tag" data-tag="${esc(t)}" onclick="event.stopPropagation();toggleTagFilter('${esc(t)}')">${esc(t)}</span>`).join('')}
      </div>` : ''}
      <div class="panel" onclick="event.stopPropagation()">
        ${isExp && s.task_name ? `<div class="card-task-name"><span class="tn-label">Task:</span>${esc(s.task_name)}</div>` : ''}
        ${isExp && s.running ? `<div class="card-timing">
          ${s.session_created ? `<div class="timing-item"><span class="timing-label">Session</span><span class="timing-value">${fmtDuration(Math.floor(Date.now()/1000) - s.session_created)}</span></div>` : ''}
          ${s.task_time ? `<div class="timing-item"><span class="timing-label">Task</span><span class="timing-value accent">${esc(s.task_time)}</span></div>` : ''}
        </div>` : ''}
        ${s.preview_lines && s.preview_lines.length ? `<div class="card-preview-lines" onclick="event.stopPropagation();openPeek('${s.name}')" style="cursor:pointer;">${rewriteLocalhostUrls(s.preview_lines.map(l => esc(l)).join('\n'))}</div>` : ''}
        <div class="card-stats" id="stats-${s.name}"></div>
        <div class="panel-actions">
          ${!s.running ? `<button class="btn" id="start-btn-${s.name}" onclick="this.textContent='Starting...';this.disabled=true;doStart('${s.name}')">Start</button>` : ''}
        </div>
        ${s.running ? `
        <div class="chips">
          <div class="chip" onclick="chipToInput('${s.name}','/compact')">/compact</div>
          <div class="chip" onclick="chipToInput('${s.name}','/status')">/status</div>
          <div class="chip" onclick="chipToInput('${s.name}','/clear')">/clear</div>
          <div class="chip" onclick="chipToInput('${s.name}','/cost')">/cost</div>
          <div class="chip" onclick="doKeys('${s.name}','C-c')">Ctrl-C</div>
          <div class="chip" onclick="doKeys('${s.name}','Escape')">Esc</div>
          <div class="chip" onclick="doKeys('${s.name}','Enter')">Enter</div>
          <div class="chip" onclick="doKeys('${s.name}','Up')">&#x2191;</div>
          <div class="chip" onclick="doKeys('${s.name}','Down')">&#x2193;</div>
        </div>
        <div class="send-row" style="position:relative;">
          <div id="card-ac-${s.name}" class="ac-list slash-ac"></div>
          <textarea class="send-input" id="input-${s.name}" rows="1"
            placeholder="Send to ${esc(s.name)}..." autocomplete="off" autocorrect="on"
            autocapitalize="sentences" spellcheck="true" enterkeyhint="enter"
            oninput="autoGrow(this);cardSlashAcUpdate('${s.name}');cmdHistoryReset()"
            onkeydown="cardSlashAcKeydown('${s.name}',event)"></textarea>
          <button class="btn primary" onclick="sendFromInput('${s.name}')">Send</button>
        </div>` : ''}
      </div>
    </div>`;
  }

  // Grid mode: flat list sorted by saved card order, no grouping (desktop only)
  if (layoutMode === 'grid' && window.innerWidth >= 900) {
    const orderMap = {};
    cardOrder.forEach((name, i) => { orderMap[name] = i; });
    const sortedFiltered = [...filtered].sort((a, b) => {
      const ai = orderMap[a.name] !== undefined ? orderMap[a.name] : 9999;
      const bi = orderMap[b.name] !== undefined ? orderMap[b.name] : 9999;
      return ai - bi;
    });
    el.innerHTML = draftCards + sortedFiltered.map(_renderSessionCard).join('');
    for (const [id, val] of Object.entries(savedInputs)) { const inp = document.getElementById(id); if (inp) { inp.value = val; autoGrow(inp); } }
    if (focusedId) { const inp = document.getElementById(focusedId); if (inp) inp.focus({ preventScroll: true }); }
    requestAnimationFrame(initSortable);
    return;
  }

  // Group mode: group by session status
  if (layoutMode === 'group' && !activeTag && !q) {
    const STATUS_GROUPS = [
      { key: 'active',  label: 'Working',     defaultOpen: true  },
      { key: 'waiting', label: 'Needs Input', defaultOpen: true  },
      { key: 'idle',    label: 'Idle',        defaultOpen: true  },
      { key: 'stopped', label: 'Stopped',     defaultOpen: false },
    ];
    const buckets = { active: [], waiting: [], idle: [], stopped: [] };
    filtered.forEach(s => {
      if (!s.running)              buckets.stopped.push(s);
      else if (s.status === 'active')  buckets.active.push(s);
      else if (s.status === 'waiting') buckets.waiting.push(s);
      else                             buckets.idle.push(s);
    });
    STATUS_GROUPS.forEach(g => {
      if (_tagGroupCollapsed[g.key] === undefined) _tagGroupCollapsed[g.key] = !g.defaultOpen;
    });
    const nonEmpty = STATUS_GROUPS.filter(g => buckets[g.key].length);
    if (nonEmpty.length > 1) {
      let groupHtml = '';
      nonEmpty.forEach(g => {
        const items = buckets[g.key];
        const col = _tagGroupCollapsed[g.key];
        groupHtml += `<div class="board-session-group">
          <div class="board-session-header" onclick="toggleTagGroup('${g.key}')">
            <span class="board-session-chevron${col ? '' : ' open'}">&#x25B6;</span>
            <span class="board-session-name">${g.label}</span>
            <div class="board-session-counts">
              <span class="board-session-count">${items.length}</span>
            </div>
          </div>
          ${!col ? `<div class="tag-group-body">${items.map(_renderSessionCard).join('')}</div>` : ''}
        </div>`;
      });
      el.innerHTML = draftCards + groupHtml;
    } else {
      el.innerHTML = draftCards + filtered.map(_renderSessionCard).join('');
    }
  } else {
    // list mode (flat) or group mode with active filter: flat list
    let flatList = filtered;
    if (layoutMode === 'list' && cardOrder.length && !activeTag && !q) {
      const orderMap = {};
      cardOrder.forEach((n, i) => { orderMap[n] = i; });
      flatList = [...filtered].sort((a, b) => {
        const ai = orderMap[a.name] !== undefined ? orderMap[a.name] : 9999;
        const bi = orderMap[b.name] !== undefined ? orderMap[b.name] : 9999;
        return ai - bi;
      });
    }
    el.innerHTML = draftCards + flatList.map(_renderSessionCard).join('');
    if (layoutMode === 'list') requestAnimationFrame(initSortable);
  }

  // Restore input values and focus after re-rendering
  for (const [id, val] of Object.entries(savedInputs)) {
    const inp = document.getElementById(id);
    if (inp) { inp.value = val; autoGrow(inp); }
  }
  if (focusedId) {
    const inp = document.getElementById(focusedId);
    if (inp) inp.focus({ preventScroll: true });
  }

}


function esc(s) {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

function timeAgo(epoch) {
  if (!epoch) return '';
  const diff = Math.floor(Date.now()/1000) - epoch;
  if (diff < 60) return 'just now';
  if (diff < 3600) return Math.floor(diff/60) + 'm ago';
  if (diff < 86400) return Math.floor(diff/3600) + 'h ago';
  return Math.floor(diff/86400) + 'd ago';
}
function fmtDuration(sec) {
  if (sec < 0) sec = 0;
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  if (h > 0) return h + 'h ' + m + 'm';
  if (m > 0) return m + 'm';
  return sec + 's';
}

function toggle(name) {
  if (_tileJustDragged) { _tileJustDragged = false; return; }
  if (expanded.has(name)) { expanded.delete(name); } else { expanded.add(name); }
  closeAllMenus();
  render();
  if (expanded.has(name)) {
    fetchStats(name);
  }
}
// Double-tap header to peek
let _lastHeaderTap = { name: null, time: 0 };
function headerTap(name, e) {
  const now = Date.now();
  if (_lastHeaderTap.name === name && now - _lastHeaderTap.time < 400) {
    e.stopPropagation();
    openPeek(name);
    _lastHeaderTap = { name: null, time: 0 };
    return;
  }
  _lastHeaderTap = { name, time: now };
}

async function fetchStats(name) {
  const el = document.getElementById('stats-' + name);
  if (!el) return;
  try {
    const r = await fetch(API + '/api/sessions/' + name + '/stats');
    const data = await r.json();
    if (data.tokens) {
      const tk = data.tokens;
      const fmt = tk >= 1000000 ? (tk/1000000).toFixed(1) + 'M' : tk >= 1000 ? (tk/1000).toFixed(0) + 'k' : tk;
      // Prepend tokens before the existing timestamp
      const existing = el.innerHTML;
      el.innerHTML = `<span>&#x1F4CA; ${fmt} tokens</span>` + existing;
    }
  } catch(e) {}
}

// ═══════ MODALS & MENUS ═══════
let openMenu = null;
function toggleMenu(name) {
  if (openMenu === name) { closeAllMenus(); return; }
  closeAllMenus();
  const el = document.getElementById('menu-' + name);
  if (!el) return;
  // Position fixed menu relative to the ellipsis button
  const btn = el.previousElementSibling;
  if (btn) {
    const r = btn.getBoundingClientRect();
    const vw = document.documentElement.clientWidth || window.innerWidth;
    const vh = window.innerHeight;
    let left = r.right - 200;
    if (left < 8) left = 8;
    if (left + 200 > vw) left = vw - 208;
    // Check if menu would overflow bottom of viewport — if so, open upward
    el.style.maxHeight = '';
    const spaceBelow = vh - r.bottom - 8;
    const spaceAbove = r.top - 8;
    if (spaceBelow < 260 && spaceAbove > spaceBelow) {
      // Open upward
      el.style.bottom = (vh - r.top + 4) + 'px';
      el.style.top = 'auto';
      el.style.maxHeight = Math.min(500, spaceAbove) + 'px';
    } else {
      el.style.top = (r.bottom + 4) + 'px';
      el.style.bottom = 'auto';
      el.style.maxHeight = Math.min(500, spaceBelow) + 'px';
    }
    el.style.left = left + 'px';
    el.style.right = 'auto';
  }
  el.classList.add('open');
  openMenu = name;
}
function closeAllMenus() {
  if (openMenu) {
    const el = document.getElementById('menu-' + openMenu);
    if (el) el.classList.remove('open');
  }
  openMenu = null;
}
document.addEventListener('click', e => { closeAllMenus(); closeActiveDropdown(e); closeAddMenu(); });

// ── Active sessions dropdown ──
let activeDropdownOpen = false;
function toggleActiveDropdown() {
  const dd = document.getElementById('active-dropdown');
  if (activeDropdownOpen) {
    dd.classList.remove('open');
    activeDropdownOpen = false;
    return;
  }
  const running = sessions.filter(s => s.running);
  if (!running.length) {
    dd.innerHTML = '<div class="active-dropdown-empty">No active sessions</div>';
  } else {
    dd.innerHTML = running.map(s => `
      <div class="active-dropdown-item" onclick="event.stopPropagation();closeActiveDropdown();openPeek('${s.name}')">
        <div class="adi-info">
          <div class="adi-name">${esc(s.name)}</div>
          ${s.dir ? `<div class="adi-dir">${esc(s.dir)}</div>` : ''}
          ${s.preview ? `<div class="adi-preview">${esc(s.preview)}</div>` : ''}
        </div>
        <span class="adi-arrow">&#x203A;</span>
      </div>
    `).join('');
  }
  // Position below the button
  const btn = document.getElementById('active-btn');
  if (btn) {
    const rect = btn.getBoundingClientRect();
    dd.style.top = (rect.bottom + 6) + 'px';
  }
  dd.classList.add('open');
  activeDropdownOpen = true;
}
function closeActiveDropdown(e) {
  if (!activeDropdownOpen) return;
  const wrap = document.querySelector('.active-wrap');
  if (e && wrap && wrap.contains(e.target)) return;
  document.getElementById('active-dropdown').classList.remove('open');
  activeDropdownOpen = false;
}
function updateActiveCount() {
  const count = sessions.filter(s => s.running).length;
  const el = document.getElementById('active-count');
  const btn = document.getElementById('active-btn');
  if (el) el.textContent = count;
  if (btn) btn.style.display = count > 0 ? 'flex' : 'none';
}

// ── Header + dropdown ──
let addMenuOpen = false;
function toggleAddMenu() {
  const menu = document.getElementById('add-menu');
  if (addMenuOpen) { menu.classList.remove('open'); addMenuOpen = false; return; }
  menu.classList.add('open'); addMenuOpen = true;
}
function closeAddMenu() {
  if (!addMenuOpen) return;
  document.getElementById('add-menu').classList.remove('open');
  addMenuOpen = false;
}

// ── Edit modal ──
let editState = null;  // {session, field, current}
function editField(session, field, current) {
  closeAllMenus();
  const titles = { name: 'Rename session', model: 'Change model', dir: 'Change directory', desc: 'Set description', tags: 'Edit tags', duplicate: 'Duplicate session', clone: 'Clone & continue' };
  const placeholders = { name: 'Session name', model: 'e.g. opus, sonnet, haiku', dir: '/path/to/project', desc: 'Brief description...', tags: 'e.g. work, frontend, urgent', duplicate: 'New session name', clone: 'New session name' };
  document.getElementById('edit-title').textContent = titles[field] || 'Edit';
  const inp = document.getElementById('edit-input');
  const sel = document.getElementById('edit-select');
  const inpWrap = document.getElementById('edit-input-wrap');
  if (field === 'model') {
    inpWrap.style.display = 'none';
    sel.style.display = 'block';
    sel.value = current || '';
    if (current && !Array.from(sel.options).some(o => o.value === current)) {
      // Add custom model as option if not in list
      const opt = document.createElement('option');
      opt.value = current; opt.textContent = current;
      sel.appendChild(opt);
      sel.value = current;
    }
  } else {
    inpWrap.style.display = '';
    sel.style.display = 'none';
    inp.value = current || '';
    inp.placeholder = placeholders[field] || '';
  }
  document.getElementById('edit-overlay').classList.add('active');
  editState = { session, field };
  if (field !== 'model') setTimeout(() => { inp.focus({ preventScroll: true }); inp.select(); }, 100);
}
function closeEdit() {
  document.getElementById('edit-overlay').classList.remove('active');
  document.getElementById('edit-ac-list').classList.remove('open');
  document.getElementById('edit-input-wrap').style.display = '';
  document.getElementById('edit-select').style.display = 'none';
  tagAcItems = []; tagAcSelected = -1;
  editState = null;
}
async function submitEdit() {
  if (!editState) return;
  const val = editState.field === 'model'
    ? document.getElementById('edit-select').value.trim()
    : document.getElementById('edit-input').value.trim();
  if (!val && editState.field !== 'desc' && editState.field !== 'tags' && editState.field !== 'model') return;
  const { session, field } = editState;
  closeEdit();
  if (field === 'duplicate') {
    await apiCall(API + '/api/sessions/' + session + '/duplicate', {
      method: 'POST', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ new_name: val })
    });
  } else if (field === 'clone') {
    await apiCall(API + '/api/sessions/' + session + '/clone', {
      method: 'POST', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ new_name: val })
    });
  } else if (field === 'name') {
    await apiCall(API + '/api/sessions/' + session + '/config', {
      method: 'PATCH', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ rename: val })
    });
  } else if (field === 'model') {
    await apiCall(API + '/api/sessions/' + session + '/config', {
      method: 'PATCH', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ model: val })
    });
  } else if (field === 'dir') {
    await apiCall(API + '/api/sessions/' + session + '/config', {
      method: 'PATCH', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ dir: val })
    });
  } else if (field === 'desc') {
    await apiCall(API + '/api/sessions/' + session + '/config', {
      method: 'PATCH', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ desc: val })
    });
  } else if (field === 'tags') {
    await apiCall(API + '/api/sessions/' + session + '/config', {
      method: 'PATCH', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ tags: val })
    });
  }
  await fetchSessions();
}

// Edit modal dir autocomplete
let editAcTimer = null;
let editAcItems = [];
let editAcSelected = -1;
function editAcFetch(query) {
  clearTimeout(editAcTimer);
  const el = document.getElementById('edit-ac-list');
  if (!query || query.length < 2 || !editState || editState.field !== 'dir') {
    el.classList.remove('open'); return;
  }
  editAcTimer = setTimeout(async () => {
    try {
      const r = await fetch(API + '/api/autocomplete/dir?q=' + encodeURIComponent(query));
      editAcItems = await r.json();
      editAcSelected = -1;
      if (!editAcItems.length) { el.classList.remove('open'); return; }
      el.innerHTML = editAcItems.map((item, i) =>
        `<div class="ac-item" onmousedown="editAcPick(${i})">${esc(item)}</div>`
      ).join('');
      el.classList.add('open');
    } catch(e) {}
  }, 150);
}
function editAcPick(i) {
  const inp = document.getElementById('edit-input');
  inp.value = editAcItems[i];
  document.getElementById('edit-ac-list').classList.remove('open');
  setTimeout(() => editAcFetch(inp.value), 50);
}
function editAcKeydown(e) {
  const el = document.getElementById('edit-ac-list');
  const isTagField = editState && editState.field === 'tags';
  const acItems = isTagField ? tagAcItems : editAcItems;
  const pickFn = isTagField ? tagAcPick : editAcPick;
  const highlightFn = isTagField ? (() => {
    const items = el.querySelectorAll('.ac-item');
    items.forEach((item, i) => item.classList.toggle('selected', i === tagAcSelected));
    if (items[tagAcSelected]) items[tagAcSelected].scrollIntoView({ block: 'nearest' });
  }) : editAcHighlight;
  if (!el.classList.contains('open')) {
    if (e.key === 'Enter') { e.preventDefault(); submitEdit(); }
    return;
  }
  if (e.key === 'ArrowDown') {
    e.preventDefault();
    if (isTagField) { tagAcSelected = Math.min(tagAcSelected + 1, acItems.length - 1); highlightFn(); }
    else { editAcSelected = Math.min(editAcSelected + 1, acItems.length - 1); highlightFn(); }
  } else if (e.key === 'ArrowUp') {
    e.preventDefault();
    if (isTagField) { tagAcSelected = Math.max(tagAcSelected - 1, 0); highlightFn(); }
    else { editAcSelected = Math.max(editAcSelected - 1, 0); highlightFn(); }
  } else if (e.key === 'Enter') {
    e.preventDefault();
    const sel = isTagField ? tagAcSelected : editAcSelected;
    if (sel >= 0) pickFn(sel);
    else { el.classList.remove('open'); submitEdit(); }
  } else if (e.key === 'Tab' && acItems.length) {
    e.preventDefault();
    const sel = isTagField ? tagAcSelected : editAcSelected;
    pickFn(sel >= 0 ? sel : 0);
  } else if (e.key === 'Escape') {
    el.classList.remove('open');
  }
}
function editAcHighlight() {
  const items = document.getElementById('edit-ac-list').querySelectorAll('.ac-item');
  items.forEach((el, i) => el.classList.toggle('selected', i === editAcSelected));
  if (items[editAcSelected]) items[editAcSelected].scrollIntoView({ block: 'nearest' });
}

// ── Tag autocomplete ──
let tagAcItems = [];
let tagAcSelected = -1;
function tagAcUpdate(val) {
  const el = document.getElementById('edit-ac-list');
  const parts = val.split(',');
  const token = parts[parts.length - 1].trim().toLowerCase();
  if (!token) { el.classList.remove('open'); tagAcItems = []; return; }
  const used = parts.slice(0, -1).map(p => p.trim().toLowerCase());
  const allTags = [...new Set(sessions.flatMap(s => s.tags || []))];
  tagAcItems = allTags.filter(t => t.toLowerCase().startsWith(token) && !used.includes(t.toLowerCase()));
  tagAcSelected = -1;
  if (!tagAcItems.length) { el.classList.remove('open'); return; }
  el.innerHTML = tagAcItems.map((t, i) =>
    `<div class="ac-item" onmousedown="tagAcPick(${i})">${esc(t)}</div>`
  ).join('');
  el.classList.add('open');
}
function tagAcPick(i) {
  const inp = document.getElementById('edit-input');
  const parts = inp.value.split(',');
  const prefix = parts.length > 1 ? ' ' : '';
  parts[parts.length - 1] = prefix + tagAcItems[i];
  inp.value = parts.join(',');
  document.getElementById('edit-ac-list').classList.remove('open');
  tagAcItems = [];
  inp.focus({ preventScroll: true });
}

async function toggleYolo(session) {
  closeAllMenus();
  await apiCall(API + '/api/sessions/' + session + '/config', {
    method: 'PATCH', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ toggle_yolo: true })
  });
  await fetchSessions();
}

async function togglePin(session) {
  closeAllMenus();
  await apiCall(API + '/api/sessions/' + session + '/config', {
    method: 'PATCH', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ toggle_pin: true })
  });
  await fetchSessions();
}

async function clearScrollback(session) {
  closeAllMenus();
  await apiCall(API + '/api/sessions/' + session + '/keys', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ keys: '' })
  });
  await apiCall(API + '/api/sessions/' + session + '/clear', { method: 'POST' });
}

function duplicateSession(session) {
  closeAllMenus();
  editField(session, 'duplicate', '');
}

function cloneSession(session) {
  closeAllMenus();
  editField(session, 'clone', '');
}

async function newConversation(session) {
  closeAllMenus();
  if (!await showConfirm('Start a fresh conversation for "' + session + '"?\n\nThe next time you start this session, it will begin a new Claude conversation (history in the old conversation is preserved but won\'t be continued).', 'Reset', true)) return;
  await apiCall(API + '/api/sessions/' + session + '/config', {
    method: 'PATCH', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ new_conversation: true })
  });
}

async function deleteSession(session) {
  closeAllMenus();
  if (!await showConfirm('Delete session "' + session + '"?', 'Delete', true)) return;
  await apiCall(API + '/api/sessions/' + session + '/delete', { method: 'POST' });
  expanded.delete(session);
  await fetchSessions();
}

async function doStart(name) {
  const r = await apiCall(API + '/api/sessions/' + name + '/start', { method: 'POST' });
  if (!r) return;
  const data = await r.json();
  if (!data.ok) { await showAlert('Failed to start: ' + (data.message || data.error || 'unknown error')); return; }
  // Poll until session shows as running (up to 5s)
  for (let i = 0; i < 10; i++) {
    await new Promise(r => setTimeout(r, 500));
    await fetchSessions();
    if (sessions.find(s => s.name === name && s.running)) break;
  }
}

async function doStop(name) {
  await apiCall(API + '/api/sessions/' + name + '/stop', { method: 'POST' });
  await new Promise(r => setTimeout(r, 500));
  await fetchSessions();
}

// ── Sending indicator ──
let _sendingSnapshot = null; // peek HTML snapshot before send
let _sendingTimer = null;

function showSendingIndicator() {
  // Snapshot current peek output to detect change
  _sendingSnapshot = lastPeekHTML;
  // Show in peek mode if open
  const peekWrap = document.querySelector('#peek-body')?.parentElement;
  if (peekWrap && document.getElementById('peek-overlay')?.classList.contains('active')) {
    let ind = document.getElementById('sending-ind');
    if (!ind) {
      ind = document.createElement('div');
      ind.id = 'sending-ind';
      ind.className = 'sending-indicator';
      ind.textContent = 'Sending\u2026';
      peekWrap.appendChild(ind);
    }
    ind.style.display = '';
    // Rapid refresh burst to detect output change quickly
    setTimeout(refreshPeek, 500);
    setTimeout(refreshPeek, 1500);
  }
  // Auto-clear after 15s as safety net
  clearTimeout(_sendingTimer);
  _sendingTimer = setTimeout(clearSendingIndicator, 15000);
}

function clearSendingIndicator() {
  _sendingSnapshot = null;
  clearTimeout(_sendingTimer);
  const ind = document.getElementById('sending-ind');
  if (ind) ind.style.display = 'none';
}

async function doSend(name, text) {
  showSendingIndicator();
  await apiCall(API + '/api/sessions/' + name + '/send', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({text})
  });
}

async function doKeys(name, keys) {
  showSendingIndicator();
  await apiCall(API + '/api/sessions/' + name + '/keys', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({keys})
  });
}

function autoGrow(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(el.scrollHeight, parseFloat(getComputedStyle(el).maxHeight) || 999) + 'px';
}
async function sendFromInput(name) {
  const inp = document.getElementById('input-' + name);
  if (!inp || !inp.value.trim()) return;
  const text = inp.value.trim();
  cmdHistoryAdd(text);
  inp.value = '';
  inp.style.height = 'auto';
  await doSend(name, text);
  inp.style.borderColor = 'var(--green)';
  setTimeout(() => { inp.style.borderColor = ''; }, 400);
}

let _peekTab = 'terminal';
function setPeekTab(tab) {
  _peekTab = tab;
  document.getElementById('peek-tab-terminal').classList.toggle('active', tab === 'terminal');
  document.getElementById('peek-tab-issues').classList.toggle('active', tab === 'issues');
  document.getElementById('peek-tab-memory').classList.toggle('active', tab === 'memory');
  document.getElementById('peek-terminal-panel').style.display = tab === 'terminal' ? '' : 'none';
  const issues = document.getElementById('peek-issues-panel');
  if (tab === 'issues') { issues.classList.add('active'); renderPeekIssues(); }
  else { issues.classList.remove('active'); }
  const mem = document.getElementById('peek-memory-panel');
  if (tab === 'memory') { mem.classList.add('active'); loadPeekMemory(); }
  else { mem.classList.remove('active'); }
}

// ── Peek Issues (board issues for this session) ──────────────────────────────
function renderPeekIssues() {
  const list = document.getElementById('peek-issues-list');
  const count = document.getElementById('peek-issues-count');
  const items = (boardItems || []).filter(i => i.session === peekSession && !i.deleted);
  count.textContent = items.length ? items.length + ' issue' + (items.length === 1 ? '' : 's') : '';
  if (!items.length) {
    list.innerHTML = '<div style="color:var(--dim);font-size:0.85rem;padding:12px 4px;">No issues for this session yet.</div>';
    return;
  }
  list.innerHTML = items.map(item => {
    const sty = statusStyle(item.status || 'todo');
    const badge = '<span class="status-badge" style="background:' + sty.bg + ';color:' + sty.color + ';border:1px solid ' + sty.border + ';font-size:0.7rem;padding:1px 6px;border-radius:10px;">' + esc(item.status || 'todo') + '</span>';
    const due = item.due ? '<span class="peek-issue-due">' + esc(item.due) + '</span>' : '';
    return '<div class="peek-issue-item" onclick="openBoardDetail(\'' + esc(item.id) + '\')">' +
      '<span class="peek-issue-key">' + esc(item.id) + '</span>' +
      '<span class="peek-issue-title">' + esc(item.title) + '</span>' +
      '<span class="peek-issue-meta">' + badge + due + '</span>' +
      '</div>';
  }).join('');
}
function peekMemoryTab(tab) {
  document.getElementById('pm-tab-edit').classList.toggle('active', tab === 'edit');
  document.getElementById('pm-tab-preview').classList.toggle('active', tab === 'preview');
  document.getElementById('pm-tab-global').classList.toggle('active', tab === 'global');
  const inp = document.getElementById('peek-memory-input');
  const preview = document.getElementById('peek-memory-preview');
  const globalInp = document.getElementById('peek-global-input');
  const saveBtn = document.getElementById('peek-memory-save');
  if (tab === 'global') {
    inp.style.display = 'none';
    preview.style.display = 'none';
    globalInp.style.display = '';
    saveBtn.onclick = saveGlobalMemory;
    loadGlobalMemory();
  } else {
    globalInp.style.display = 'none';
    saveBtn.onclick = savePeekMemory;
    if (tab === 'preview') {
      inp.style.display = 'none';
      preview.style.display = '';
      preview.innerHTML = renderMarkdown(inp.value) || '<span style="color:var(--dim);font-size:0.85rem;">Nothing to preview</span>';
    } else {
      inp.style.display = '';
      preview.style.display = 'none';
      inp.focus();
    }
  }
}
async function loadPeekMemory() {
  const inp = document.getElementById('peek-memory-input');
  const save = document.getElementById('peek-memory-save');
  peekMemoryTab('edit'); // always start on edit tab
  inp.value = 'Loading...'; inp.disabled = true; save.disabled = true;
  try {
    const r = await fetch(API + '/api/sessions/' + peekSession + '/memory');
    const data = await r.json();
    inp.value = data.content || '';
  } catch(e) { inp.value = ''; }
  inp.disabled = false; save.disabled = false;
  inp.focus();
}
async function pullPeekMemory() {
  const inp = document.getElementById('peek-memory-input');
  const btn = document.getElementById('peek-memory-pull');
  const save = document.getElementById('peek-memory-save');
  btn.disabled = true; btn.textContent = '…';
  inp.disabled = true; save.disabled = true;
  try {
    const r = await fetch(API + '/api/sessions/' + peekSession + '/memory?pull=1');
    const data = await r.json();
    inp.value = data.content || '';
    showToast('Pulled latest from Claude');
  } catch(e) { showToast('Pull failed'); }
  btn.disabled = false; btn.textContent = '↻';
  inp.disabled = false; save.disabled = false;
}
async function savePeekMemory() {
  const inp = document.getElementById('peek-memory-input');
  const save = document.getElementById('peek-memory-save');
  save.disabled = true; save.textContent = 'Saving...';
  try {
    await fetch(API + '/api/sessions/' + peekSession + '/memory', {
      method: 'POST', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ content: inp.value })
    });
    save.textContent = 'Syncing...';
    await syncPeekMemory();
    save.textContent = 'Saved!';
    setTimeout(() => { save.disabled = false; save.textContent = 'Save'; }, 1500);
  } catch(e) {
    save.disabled = false; save.textContent = 'Save';
    showToast('Failed to save memory');
  }
}
async function syncPeekMemory() {
  const prompt = 'Please update your memory file now with any new facts, decisions, constraints, ' +
    'API details, file paths, or patterns from our recent work. Be concise and add only what ' +
    'is not already captured. Do not remove existing entries unless they are wrong.';
  await fetch(API + '/api/sessions/' + peekSession + '/send', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ text: prompt })
  });
  showToast('Memory saved — sync requested');
  setTimeout(() => setPeekTab('terminal'), 1000);
}

let _globalMemLoaded = false;
async function loadGlobalMemory() {
  const inp = document.getElementById('peek-global-input');
  const save = document.getElementById('peek-memory-save');
  if (_globalMemLoaded) { inp.focus(); return; }
  inp.value = 'Loading...'; inp.disabled = true; save.disabled = true;
  try {
    const r = await fetch(API + '/api/memory/global');
    const data = await r.json();
    inp.value = data.content || '';
    _globalMemLoaded = true;
  } catch(e) { inp.value = ''; }
  inp.disabled = false; save.disabled = false;
  inp.focus();
}
async function saveGlobalMemory() {
  const inp = document.getElementById('peek-global-input');
  const save = document.getElementById('peek-memory-save');
  save.disabled = true; save.textContent = 'Saving...';
  try {
    await fetch(API + '/api/memory/global', {
      method: 'POST', headers: {'Content-Type':'application/json'},
      body: JSON.stringify({ content: inp.value })
    });
    _globalMemLoaded = false; // force reload next open
    save.textContent = 'Saved!';
    showToast('Global memory saved — all sessions will see it');
    setTimeout(() => { save.disabled = false; save.textContent = 'Save'; }, 1500);
  } catch(e) {
    save.disabled = false; save.textContent = 'Save';
    showToast('Failed to save global memory');
  }
}

function openPeek(name) {
  if (peekTimer) { clearInterval(peekTimer); peekTimer = null; }
  clearPeekFiles();  // clear any stale attachments from previous peek
  peekSession = name;
  peekSessionDir = (sessions.find(s => s.name === name) || {}).dir || '';
  // Reset to terminal tab
  if (_peekTab !== 'terminal') setPeekTab('terminal');
  document.getElementById('peek-terminal-panel').style.display = '';
  document.getElementById('peek-memory-panel').classList.remove('active');
  // Update dir bar
  document.getElementById('peek-dir-text').textContent = peekSessionDir || '(unknown)';
  peekSearchQuery = '';
  lastPeekHTML = '';
  const searchInp = document.getElementById('peek-search');
  if (searchInp) searchInp.value = '';
  const draft = _peekDrafts[name] || '';
  const cmdInp = document.getElementById('peek-cmd-input');
  cmdInp.value = draft;
  autoGrow(cmdInp);
  peekCmdOpen = true;
  document.getElementById('peek-cmd-row').classList.add('open');
  document.getElementById('peek-cmd-toggle').innerHTML = '&#x25BC; Send command';
  if (draft) setTimeout(() => document.getElementById('peek-cmd-input').focus({ preventScroll: true }), 50);
  document.getElementById('peek-title').textContent = name;
  updatePeekStatus();
  document.getElementById('peek-body').innerHTML = '<span style="color:var(--dim)">Loading...</span>';
  updateConnectionStatus();
  document.getElementById('peek-overlay').classList.add('active');
  // Load cached peek instantly while fetching fresh data
  _idb.get('peek_' + name).then(cached => {
    if (cached && (!lastPeekHTML || lastPeekHTML.includes('Loading...'))) {
      lastPeekHTML = linkifyOutput(stripAnsi(cached.output));
      applyPeekSearch();
      const ago = Math.floor((Date.now() - cached.time) / 60000);
      document.getElementById('peek-status').textContent = 'Cached ' + (ago < 1 ? 'just now' : ago + 'm ago');
      const body = document.getElementById('peek-body');
      body.scrollTop = body.scrollHeight;
    }
  });
  refreshPeek();
  peekTimer = setInterval(refreshPeek, 3000);
}

function copyPeekContent() {
  const body = document.getElementById('peek-body');
  if (!body) return;
  const text = body.innerText || body.textContent || '';
  navigator.clipboard.writeText(text).then(() => {
    const btn = document.getElementById('peek-copy-btn');
    btn.innerHTML = '&#x2713; Copied';
    btn.style.color = '#4ade80';
    setTimeout(() => { btn.innerHTML = '&#x2398; Copy'; btn.style.color = ''; }, 1500);
  }).catch(() => showToast('Copy failed'));
}

function closePeek() {
  // Save command draft for this session
  if (peekSession) {
    const inp = document.getElementById('peek-cmd-input');
    const val = inp ? inp.value : '';
    if (val.trim()) _peekDrafts[peekSession] = val;
    else delete _peekDrafts[peekSession];
  }
  peekSession = null;
  peekSearchQuery = '';
  lastPeekHTML = '';
  clearPeekFiles();
  document.getElementById('peek-overlay').classList.remove('active');
  if (peekTimer) { clearInterval(peekTimer); peekTimer = null; }
}

// Swipe right to close peek (but never when touching the terminal body — preserve text selection)
(function() {
  const el = document.getElementById('peek-overlay');
  const body = document.getElementById('peek-body');
  let sx = 0, sy = 0, tracking = false;
  el.addEventListener('touchstart', e => {
    if (!el.classList.contains('active')) return;
    // Let the terminal body handle its own touches (scrolling + text selection)
    if (body && body.contains(e.target)) { tracking = false; return; }
    const t = e.touches[0];
    sx = t.clientX; sy = t.clientY; tracking = true;
    el.style.transition = 'none';
  }, {passive: true});
  el.addEventListener('touchmove', e => {
    if (!tracking || !el.classList.contains('active')) return;
    const dx = e.touches[0].clientX - sx;
    const dy = Math.abs(e.touches[0].clientY - sy);
    if (dy > 30 && dx < 30) { tracking = false; el.style.transform = ''; el.style.transition = ''; return; }
    if (dx > 10) el.style.transform = 'translateX(' + dx + 'px)';
  }, {passive: true});
  el.addEventListener('touchend', e => {
    if (!tracking) { el.style.transition = ''; return; }
    tracking = false;
    const dx = e.changedTouches[0].clientX - sx;
    el.style.transition = 'transform 0.25s cubic-bezier(.4,0,.2,1), opacity 0.25s, pointer-events 0s';
    if (dx > 80) {
      el.style.transform = 'translateX(100%)';
      setTimeout(() => { closePeek(); el.style.transform = ''; el.style.transition = ''; }, 260);
    } else {
      el.style.transform = '';
      setTimeout(() => { el.style.transition = ''; }, 260);
    }
  }, {passive: true});
})();

// Rewrite localhost/127.0.0.1/0.0.0.0 URLs to use the actual server hostname
// so links work when viewing the dashboard from another device
function rewriteLocalhostUrls(html) {
  if (!remoteHostname) return html;
  return html.replace(/(https?):\/\/(localhost|127\.0\.0\.1|0\.0\.0\.0)(:\d+)?/g,
    (match, scheme, host, port) => scheme + '://' + remoteHostname + (port || ''));
}

// ═══════ PEEK MODE ═══════
function stripAnsi(text) {
  // Strip ANSI escape sequences (colors, cursor movement, OSC hyperlinks, etc.)
  return text
    .replace(/\x1b\]8;[^\x1b]*\x1b\\/g, '')  // OSC 8 hyperlinks
    .replace(/\x1b\[[0-9;]*[a-zA-Z]/g, '')    // CSI sequences (colors, etc.)
    .replace(/\x1b\][^\x07]*\x07/g, '')        // OSC sequences (BEL terminated)
    .replace(/\x1b\][^\x1b]*\x1b\\/g, '')      // OSC sequences (ST terminated)
    .replace(/\x1b[()][A-Z0-9]/g, '')          // Character set selection
    .replace(/\x1b[\x20-\x2f]*[\x40-\x7e]/g, '');  // Other escape sequences
}

function linkifyOutput(text) {
  // Split text into segments: URLs, file paths, and plain text
  // URL regex: match http/https URLs
  const urlRe = /https?:\/\/[^\s<>\]\)'"`,;]+/g;
  // File path regex: absolute paths or relative paths with extensions
  const fileRe = /(?:^|[\s(])((\/[\w./-]+(?:\.\w+)(?::[\d]+)?)|(\.\/[\w./-]+(?:\.\w+)(?::[\d]+)?))/gm;

  const parts = [];
  let last = 0;

  // First pass: find all URLs
  const matches = [];
  let m;
  while ((m = urlRe.exec(text)) !== null) {
    // Strip trailing punctuation that's likely not part of URL
    let url = m[0].replace(/[.,;:!?)]+$/, '');
    matches.push({ start: m.index, end: m.index + url.length, type: 'url', value: url });
  }

  // Second pass: find file paths (skip if overlapping with URL)
  while ((m = fileRe.exec(text)) !== null) {
    const path = m[1];
    const pathStart = m.index + m[0].indexOf(path);
    const pathEnd = pathStart + path.length;
    const overlaps = matches.some(x => pathStart < x.end && pathEnd > x.start);
    if (!overlaps) {
      matches.push({ start: pathStart, end: pathEnd, type: 'file', value: path });
    }
  }

  matches.sort((a, b) => a.start - b.start);

  // Build HTML
  let html = '';
  for (const match of matches) {
    if (match.start > last) {
      html += esc(text.slice(last, match.start));
    }
    if (match.type === 'url') {
      html += `<a href="${esc(match.value)}" target="_blank" rel="noopener">${esc(match.value)}</a>`;
    } else if (match.type === 'file') {
      const rawPath = match.value.replace(/:[\d]+$/, '');  // strip :linenum
      const isMd = /\.md$/i.test(rawPath);
      const cls = isMd ? 'md-link' : 'file-link';
      html += `<span class="${cls}" onclick="if(window.getSelection().toString())return;event.preventDefault();event.stopPropagation();openFilePreview('${esc(rawPath)}')">${esc(match.value)}</span>`;
    }
    last = match.end;
  }
  if (last < text.length) {
    html += esc(text.slice(last));
  }
  return rewriteLocalhostUrls(html);
}

let peekSelecting = false;
async function refreshPeek() {
  if (!peekSession) return;
  // Skip refresh while user is selecting text
  if (peekSelecting) return;
  const sel = window.getSelection();
  if (sel && sel.toString().length > 0) return;
  const body = document.getElementById('peek-body');
  const statusEl = document.getElementById('peek-status');
  try {
    const r = await fetch(API + '/api/sessions/' + peekSession + '/peek?lines=500');
    const data = await r.json();
    const output = data.output || '(no output)';
    const atBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 40;
    const newHTML = linkifyOutput(stripAnsi(output));
    // Re-check: user may have started selecting text during the async fetch
    if (peekSelecting || (window.getSelection()?.toString().length > 0)) return;
    // Clear sending indicator when output changes
    if (_sendingSnapshot && newHTML !== _sendingSnapshot) clearSendingIndicator();
    lastPeekHTML = newHTML;
    applyPeekSearch();
    if (atBottom) body.scrollTop = body.scrollHeight;
    statusEl.textContent = (data.saved ? 'Saved log' : 'Updated') + ' ' + new Date().toLocaleTimeString();
    // Cache peek output for offline browsing
    _idb.set('peek_' + peekSession, { output, time: Date.now() });
  } catch(e) {
    console.error('peek:', e);
    // Offline: load cached peek
    if (!lastPeekHTML || lastPeekHTML.includes('Loading...')) {
      const cached = await _idb.get('peek_' + peekSession);
      if (cached) {
        lastPeekHTML = linkifyOutput(stripAnsi(cached.output));
        applyPeekSearch();
        const ago = Math.floor((Date.now() - cached.time) / 60000);
        statusEl.textContent = 'Cached ' + (ago < 1 ? 'just now' : ago + 'm ago');
      } else {
        body.innerHTML = '<span style="color:var(--dim)">No cached output available</span>';
        statusEl.textContent = 'Offline — no cache';
      }
    }
  }
}

function applyPeekSearch() {
  const body = document.getElementById('peek-body');
  const countEl = document.getElementById('peek-search-count');
  if (!body) return;
  const q = peekSearchQuery.trim();
  if (!q) {
    body.innerHTML = lastPeekHTML;
    if (countEl) countEl.textContent = '';
    return;
  }
  // Highlight matches in text nodes only (not inside tags)
  const escaped = q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const re = new RegExp('(' + escaped + ')', 'gi');
  const parts = lastPeekHTML.split(/(<[^>]+>)/);
  let matchCount = 0;
  body.innerHTML = parts.map(p => {
    if (p.startsWith('<')) return p;
    return p.replace(re, (match) => { matchCount++; return `<span class="peek-highlight">${esc(match)}</span>`; });
  }).join('');
  if (countEl) countEl.textContent = matchCount > 0 ? matchCount + ' found' : 'no matches';
  // Scroll to first match
  const first = body.querySelector('.peek-highlight');
  if (first) first.scrollIntoView({ block: 'center', behavior: 'smooth' });
}

// ── Peek command bar ──
let peekCmdOpen = true;
function togglePeekCmd() {
  peekCmdOpen = !peekCmdOpen;
  const row = document.getElementById('peek-cmd-row');
  const toggle = document.getElementById('peek-cmd-toggle');
  row.classList.toggle('open', peekCmdOpen);
  toggle.innerHTML = peekCmdOpen ? '&#x25BC; Send command' : '&#x25B2; Send command';
  if (peekCmdOpen) setTimeout(() => document.getElementById('peek-cmd-input').focus({ preventScroll: true }), 50);
}
// ── File attachments ──
let peekFiles = []; // [{name, path, url, isImage, previewUrl}]

function _fileIcon(name) {
  const ext = name.split('.').pop().toLowerCase();
  if (['png','jpg','jpeg','gif','webp','bmp'].includes(ext)) return '🖼';
  if (ext === 'pdf') return '📄';
  if (['txt','md','log'].includes(ext)) return '📝';
  if (['csv','json'].includes(ext)) return '📊';
  return '📎';
}

function renderPeekFiles() {
  const bar = document.getElementById('peek-attach-bar');
  if (!bar) return;
  bar.classList.toggle('has-files', peekFiles.length > 0);
  bar.innerHTML = peekFiles.map((f, i) => {
    const isUploading = !f.path;
    let thumb = '';
    if (f.isImage && f.previewUrl) {
      thumb = `<img src="${f.previewUrl}" alt="">`;
    } else {
      thumb = `<span class="chip-icon">${_fileIcon(f.name)}</span>`;
    }
    return `<div class="peek-attach-chip${isUploading ? ' uploading' : ''}">
      ${thumb}
      <span class="chip-name">${esc(f.name)}</span>
      ${isUploading ? '<span style="color:var(--dim);font-size:0.7rem;">↑</span>' : `<span class="chip-remove" onclick="removePeekFile(${i})">×</span>`}
    </div>`;
  }).join('');
}

function removePeekFile(idx) {
  const f = peekFiles[idx];
  if (f && f.previewUrl) URL.revokeObjectURL(f.previewUrl);
  peekFiles.splice(idx, 1);
  renderPeekFiles();
}

function clearPeekFiles() {
  peekFiles.forEach(f => { if (f && f.previewUrl) URL.revokeObjectURL(f.previewUrl); });
  peekFiles = [];
  renderPeekFiles();
}

async function uploadAndAttach(file) {
  if (file.size > 20 * 1024 * 1024) { showToast('File too large (max 20 MB)'); return; }
  const ext = '.' + file.name.split('.').pop().toLowerCase();
  const allowed = ['.png','.jpg','.jpeg','.gif','.webp','.bmp','.pdf','.txt','.md','.csv','.json','.log'];
  if (!allowed.includes(ext)) { showToast('Unsupported file type: ' + ext); return; }

  const isImage = file.type.startsWith('image/');
  let previewUrl = null;
  if (isImage) previewUrl = URL.createObjectURL(file);

  // Add placeholder chip while uploading
  const placeholder = { name: file.name, path: null, url: null, isImage, previewUrl };
  const idx = peekFiles.length;
  peekFiles.push(placeholder);
  renderPeekFiles();

  try {
    const buf = await file.arrayBuffer();
    // Chunk the conversion to avoid call-stack overflow on large files
    const bytes = new Uint8Array(buf);
    let binary = '';
    for (let i = 0; i < bytes.length; i += 8192) {
      binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 8192));
    }
    const b64 = btoa(binary);
    const r = await fetch(API + '/api/upload', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ name: file.name, data: b64 })
    });
    const d = await r.json();
    if (!r.ok || d.error) { showToast('Upload failed: ' + (d.error || r.status)); peekFiles.splice(idx, 1); }
    else { peekFiles[idx] = { name: file.name, path: d.path, url: d.url, isImage, previewUrl }; }
  } catch(e) {
    console.error('Upload error:', e);
    showToast('Upload failed: ' + e.message); peekFiles.splice(idx, 1);
  }
  renderPeekFiles();
}

function handlePeekFileInput(e) {
  for (const f of e.target.files) uploadAndAttach(f);
  e.target.value = '';
}

function handlePeekPaste(e) {
  const items = e.clipboardData?.items;
  if (!items) return;
  for (const item of items) {
    if (item.kind === 'file') {
      e.preventDefault();
      uploadAndAttach(item.getAsFile());
      return;
    }
  }
}

// Drag-and-drop on peek overlay
(function() {
  function getOverlay() { return document.getElementById('peek-overlay'); }
  let dragCount = 0;
  document.addEventListener('dragenter', e => {
    if (!getOverlay()?.classList.contains('active')) return;
    if ([...e.dataTransfer.types].includes('Files')) { dragCount++; getOverlay().classList.add('drag-over'); }
  });
  document.addEventListener('dragleave', e => {
    if (!getOverlay()?.classList.contains('active')) return;
    dragCount = Math.max(0, dragCount - 1);
    if (dragCount === 0) getOverlay().classList.remove('drag-over');
  });
  document.addEventListener('dragover', e => {
    if (getOverlay()?.classList.contains('active')) e.preventDefault();
  });
  document.addEventListener('drop', e => {
    const overlay = getOverlay();
    if (!overlay?.classList.contains('active')) return;
    overlay.classList.remove('drag-over');
    dragCount = 0;
    e.preventDefault();
    if (!document.getElementById('peek-cmd-row')?.classList.contains('open')) {
      togglePeekCmd(); // auto-open the send bar on drop
    }
    for (const f of e.dataTransfer.files) uploadAndAttach(f);
  });
})();

async function sendPeekCmd() {
  if (!peekSession) return;
  const inp = document.getElementById('peek-cmd-input');
  const text = inp.value.trim();
  const files = peekFiles.filter(f => f.path); // only successfully uploaded
  if (!text && files.length === 0) return;
  cmdHistoryAdd(text);

  // Build message: inline @path references (no newlines — tmux treats \n as Enter,
  // which would split the message and send the path as a separate submit)
  let message = text;
  if (files.length > 0) {
    const refs = files.map(f => '@' + f.path).join(' ');
    message = text ? `${text} ${refs}` : refs;
  }

  inp.value = '';
  inp.style.height = 'auto';
  delete _peekDrafts[peekSession];
  clearPeekFiles();

  await doSend(peekSession, message);
  inp.style.borderColor = 'var(--green)';
  setTimeout(() => { inp.style.borderColor = ''; }, 400);
  setTimeout(refreshPeek, 500);
}
async function peekQuickSend(text) {
  if (!peekSession) return;
  await doSend(peekSession, text);
  setTimeout(refreshPeek, 500);
}
async function peekQuickKeys(keys) {
  if (!peekSession) return;
  await doKeys(peekSession, keys);
  setTimeout(refreshPeek, 500);
}

// ── Slash command autocomplete ──
const SLASH_COMMANDS = [
  { cmd: '/compact', desc: 'Compact conversation history' },
  { cmd: '/status', desc: 'Show session status' },
  { cmd: '/cost', desc: 'Show token usage and cost' },
  { cmd: '/clear', desc: 'Clear conversation history' },
  { cmd: '/help', desc: 'Show available commands' },
  { cmd: '/init', desc: 'Initialize project CLAUDE.md' },
  { cmd: '/memory', desc: 'Edit CLAUDE.md memory' },
  { cmd: '/model', desc: 'Switch model' },
  { cmd: '/permissions', desc: 'View/manage permissions' },
  { cmd: '/review', desc: 'Review a pull request' },
  { cmd: '/terminal-setup', desc: 'Set up terminal integration' },
  { cmd: '/vim', desc: 'Edit prompt in Vim' },
  { cmd: '/bug', desc: 'Report a bug' },
  { cmd: '/login', desc: 'Switch account or log in' },
  { cmd: '/logout', desc: 'Log out of current account' },
  { cmd: '/doctor', desc: 'Check installation health' },
  { cmd: '/config', desc: 'Open config panel' },
];
let slashAcItems = [];
let slashAcSelected = -1;

function slashAcUpdate() {
  const inp = document.getElementById('peek-cmd-input');
  const el = document.getElementById('slash-ac-list');
  const val = inp.value;
  if (!val.startsWith('/')) { el.classList.remove('open'); slashAcItems = []; return; }
  const q = val.toLowerCase();
  slashAcItems = SLASH_COMMANDS.filter(c => c.cmd.startsWith(q));
  slashAcSelected = -1;
  if (!slashAcItems.length) { el.classList.remove('open'); return; }
  el.innerHTML = slashAcItems.map((c, i) =>
    `<div class="ac-item" onmousedown="slashAcPick(${i})">${esc(c.cmd)}<span class="ac-desc">${esc(c.desc)}</span></div>`
  ).join('');
  el.classList.add('open');
}

function slashAcPick(i) {
  const inp = document.getElementById('peek-cmd-input');
  inp.value = slashAcItems[i].cmd;
  document.getElementById('slash-ac-list').classList.remove('open');
  slashAcItems = [];
  inp.focus({ preventScroll: true });
}

function slashAcKeydown(e) {
  const inp = document.getElementById('peek-cmd-input');
  const el = document.getElementById('slash-ac-list');
  if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); sendPeekCmd(); return; }
  if (!el.classList.contains('open')) {
    if (e.key === 'ArrowUp' && inp.value.indexOf('\n') === -1) { e.preventDefault(); cmdHistoryUp(inp); return; }
    if (e.key === 'ArrowDown' && _cmdHistoryIdx !== -1) { e.preventDefault(); cmdHistoryDown(inp); return; }
    return;
  }
  if (e.key === 'ArrowUp') {
    e.preventDefault();
    slashAcSelected = slashAcSelected <= 0 ? slashAcItems.length - 1 : slashAcSelected - 1;
    slashAcHighlight();
  } else if (e.key === 'ArrowDown') {
    e.preventDefault();
    slashAcSelected = slashAcSelected >= slashAcItems.length - 1 ? 0 : slashAcSelected + 1;
    slashAcHighlight();
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (slashAcSelected >= 0) slashAcPick(slashAcSelected);
    else { el.classList.remove('open'); }
  } else if (e.key === 'Tab' && slashAcItems.length) {
    e.preventDefault();
    slashAcPick(slashAcSelected >= 0 ? slashAcSelected : 0);
  } else if (e.key === 'Escape') {
    el.classList.remove('open');
  }
}

function slashAcHighlight() {
  const items = document.getElementById('slash-ac-list').querySelectorAll('.ac-item');
  items.forEach((el, i) => el.classList.toggle('selected', i === slashAcSelected));
  if (items[slashAcSelected]) items[slashAcSelected].scrollIntoView({ block: 'nearest' });
}

// ── Command History (Up/Down arrow navigation in send inputs) ──
let _cmdHistory = JSON.parse(localStorage.getItem('amux_cmd_history') || '[]');
let _cmdHistoryIdx = -1;   // -1 = not browsing history
let _cmdHistoryDraft = ''; // saved current input when starting to browse

function cmdHistoryAdd(text) {
  if (!text.trim()) return;
  if (_cmdHistory.length && _cmdHistory[_cmdHistory.length - 1] === text) { _cmdHistoryIdx = -1; return; }
  _cmdHistory.push(text);
  if (_cmdHistory.length > 500) _cmdHistory = _cmdHistory.slice(-500);
  localStorage.setItem('amux_cmd_history', JSON.stringify(_cmdHistory));
  _cmdHistoryIdx = -1;
}

function cmdHistoryReset() { _cmdHistoryIdx = -1; }

function cmdHistoryUp(inp) {
  if (!_cmdHistory.length) return;
  if (_cmdHistoryIdx === -1) { _cmdHistoryDraft = inp.value; _cmdHistoryIdx = _cmdHistory.length - 1; }
  else if (_cmdHistoryIdx > 0) { _cmdHistoryIdx--; }
  inp.value = _cmdHistory[_cmdHistoryIdx];
  autoGrow(inp);
  requestAnimationFrame(() => { inp.selectionStart = inp.selectionEnd = inp.value.length; });
}

function cmdHistoryDown(inp) {
  if (_cmdHistoryIdx === -1) return;
  if (_cmdHistoryIdx < _cmdHistory.length - 1) { _cmdHistoryIdx++; inp.value = _cmdHistory[_cmdHistoryIdx]; }
  else { _cmdHistoryIdx = -1; inp.value = _cmdHistoryDraft; }
  autoGrow(inp);
  requestAnimationFrame(() => { inp.selectionStart = inp.selectionEnd = inp.value.length; });
}

// ── Chip populates input ──
function chipToInput(name, text) {
  const inp = document.getElementById('input-' + name);
  if (!inp) return;
  inp.value = text;
  inp.focus({ preventScroll: true });
  autoGrow(inp);
  cardSlashAcUpdate(name);
}

// ── Card send input slash autocomplete ──
let _cardAcItems = [];
let _cardAcSelected = -1;
let _cardAcName = '';

function cardSlashAcUpdate(name) {
  const inp = document.getElementById('input-' + name);
  const el = document.getElementById('card-ac-' + name);
  if (!inp || !el) return;
  // Close any other card's autocomplete
  if (_cardAcName && _cardAcName !== name) {
    const prev = document.getElementById('card-ac-' + _cardAcName);
    if (prev) prev.classList.remove('open');
  }
  _cardAcName = name;
  const val = inp.value;
  if (!val.startsWith('/')) { el.classList.remove('open'); _cardAcItems = []; return; }
  const q = val.toLowerCase();
  _cardAcItems = SLASH_COMMANDS.filter(c => c.cmd.startsWith(q));
  _cardAcSelected = -1;
  if (!_cardAcItems.length) { el.classList.remove('open'); return; }
  el.innerHTML = _cardAcItems.map((c, i) =>
    `<div class="ac-item" onmousedown="cardSlashAcPick('${esc(name)}',${i})">${esc(c.cmd)}<span class="ac-desc">${esc(c.desc)}</span></div>`
  ).join('');
  el.classList.add('open');
}

function cardSlashAcPick(name, i) {
  const inp = document.getElementById('input-' + name);
  const el = document.getElementById('card-ac-' + name);
  inp.value = _cardAcItems[i].cmd;
  el.classList.remove('open');
  _cardAcItems = [];
  inp.focus({ preventScroll: true });
}

function cardSlashAcKeydown(name, e) {
  const inp = document.getElementById('input-' + name);
  const el = document.getElementById('card-ac-' + name);
  if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); sendFromInput(name); return; }
  if (!el || !el.classList.contains('open')) {
    if (e.key === 'ArrowUp' && inp && inp.value.indexOf('\n') === -1) { e.preventDefault(); cmdHistoryUp(inp); return; }
    if (e.key === 'ArrowDown' && _cmdHistoryIdx !== -1) { e.preventDefault(); if (inp) cmdHistoryDown(inp); return; }
    return;
  }
  if (e.key === 'ArrowUp') {
    e.preventDefault();
    _cardAcSelected = _cardAcSelected <= 0 ? _cardAcItems.length - 1 : _cardAcSelected - 1;
    cardSlashAcHighlight(name);
  } else if (e.key === 'ArrowDown') {
    e.preventDefault();
    _cardAcSelected = _cardAcSelected >= _cardAcItems.length - 1 ? 0 : _cardAcSelected + 1;
    cardSlashAcHighlight(name);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (_cardAcSelected >= 0) cardSlashAcPick(name, _cardAcSelected);
    else { el.classList.remove('open'); }
  } else if (e.key === 'Tab' && _cardAcItems.length) {
    e.preventDefault();
    cardSlashAcPick(name, _cardAcSelected >= 0 ? _cardAcSelected : 0);
  } else if (e.key === 'Escape') {
    el.classList.remove('open');
  }
}

function cardSlashAcHighlight(name) {
  const items = document.getElementById('card-ac-' + name).querySelectorAll('.ac-item');
  items.forEach((el, i) => el.classList.toggle('selected', i === _cardAcSelected));
  if (items[_cardAcSelected]) items[_cardAcSelected].scrollIntoView({ block: 'nearest' });
}

// ── Search clear helpers ──
function toggleTagFilter(tag) {
  activeTag = activeTag === tag ? '' : tag;
  render();
}
function toggleTagGroup(tag) {
  _tagGroupCollapsed[tag] = !_tagGroupCollapsed[tag];
  localStorage.setItem('amux_status_collapsed', JSON.stringify(_tagGroupCollapsed));
  render();
}
function clearSearch() {
  const inp = document.getElementById('search-input');
  inp.value = '';
  searchQuery = '';
  document.getElementById('search-wrap').classList.remove('has-value');
  render();
}
function clearPeekSearch() {
  const inp = document.getElementById('peek-search');
  inp.value = '';
  peekSearchQuery = '';
  document.getElementById('peek-search-wrap').classList.remove('has-value');
  document.getElementById('peek-search-count').textContent = '';
  applyPeekSearch();
}

// ── File preview ──
function renderCsvTable(csv) {
  const lines = csv.split('\n').filter(l => l.trim());
  if (!lines.length) return '<em style="color:var(--dim)">Empty file</em>';
  const parseLine = line => {
    const cells = []; let cur = '', inQ = false;
    for (const ch of line) {
      if (ch === '"') { inQ = !inQ; }
      else if (ch === ',' && !inQ) { cells.push(cur); cur = ''; }
      else { cur += ch; }
    }
    cells.push(cur);
    return cells.map(c => c.trim().replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'));
  };
  const header = parseLine(lines[0]);
  const rows = lines.slice(1).map(parseLine);
  const thead = '<tr>' + header.map(h => `<th>${h}</th>`).join('') + '</tr>';
  const tbody = rows.map(r => '<tr>' + r.map(c => `<td>${c}</td>`).join('') + '</tr>').join('');
  return `<div class="csv-wrap"><table class="csv-table"><thead>${thead}</thead><tbody>${tbody}</tbody></table></div>`;
}

let _fileData = null;
let _fileViewMode = 'preview';

function _renderFileBody(data, mode) {
  const body = document.getElementById('file-body');
  const isBinary = data.is_image || data.is_pdf;
  // Binary — no tabs, just render
  if (data.is_image) {
    body.className = 'file-overlay-body file-image';
    const img = document.createElement('img');
    img.src = data.data_url;
    img.alt = data.path ? data.path.split('/').pop() : '';
    img.style.cssText = 'max-width:100%;height:auto;border-radius:4px;display:block;margin:auto;';
    body.innerHTML = ''; body.appendChild(img);
    return;
  }
  if (data.is_pdf) {
    body.className = 'file-overlay-body file-pdf';
    body.innerHTML = `<embed src="${data.data_url}" type="application/pdf" style="width:100%;height:100%;min-height:520px;border-radius:4px;">`;
    return;
  }
  // Text files — Raw / Preview
  if (mode === 'raw') {
    body.className = 'file-overlay-body file-raw';
    body.textContent = data.content;
    return;
  }
  // Preview
  if (data.is_csv) {
    body.className = 'file-overlay-body file-csv';
    body.innerHTML = renderCsvTable(data.content);
  } else if (data.is_markdown) {
    body.className = 'file-overlay-body markdown';
    body.innerHTML = renderMarkdown(data.content);
  } else {
    // plain text, html source, etc. — preview = same as raw
    body.className = 'file-overlay-body file-raw';
    body.textContent = data.content;
  }
}

function setFileViewMode(mode) {
  _fileViewMode = mode;
  document.getElementById('file-tab-preview').classList.toggle('active', mode === 'preview');
  document.getElementById('file-tab-raw').classList.toggle('active', mode === 'raw');
  if (_fileData) _renderFileBody(_fileData, mode);
}

async function openFilePreview(path) {
  _fileData = null;
  _fileViewMode = 'preview';
  document.getElementById('file-title').textContent = path.split('/').pop();
  document.getElementById('file-body').className = 'file-overlay-body';
  document.getElementById('file-body').textContent = 'Loading...';
  document.getElementById('file-view-tabs').style.display = 'none';
  document.getElementById('file-tab-preview').classList.add('active');
  document.getElementById('file-tab-raw').classList.remove('active');
  document.getElementById('file-overlay').classList.add('active');
  try {
    let url = API + '/api/file?path=' + encodeURIComponent(path);
    if (peekSessionDir) url += '&cwd=' + encodeURIComponent(peekSessionDir);
    const r = await fetch(url);
    const data = await r.json();
    if (data.error) {
      document.getElementById('file-body').textContent = 'Error: ' + data.error;
      return;
    }
    _fileData = data;
    // Show tabs only for text files
    if (!data.is_image && !data.is_pdf) {
      document.getElementById('file-view-tabs').style.display = '';
    }
    _renderFileBody(data, _fileViewMode);
  } catch(e) {
    document.getElementById('file-body').textContent = 'Failed to load file.';
  }
}

function closeFilePreview() {
  document.getElementById('file-overlay').classList.remove('active');
  _fileData = null;
}

// ═══════ FILE EXPLORER ═══════
let _explorePath = '';
let _exploreShowHidden = false;
function openExplore(startPath) {
  _explorePath = startPath || '/';
  document.getElementById('explore-overlay').classList.add('active');
  loadExplore(_explorePath);
}
function closeExplore() {
  document.getElementById('explore-overlay').classList.remove('active');
}
function toggleExploreHidden() {
  _exploreShowHidden = !_exploreShowHidden;
  const btn = document.getElementById('explore-hidden-btn');
  btn.style.background = _exploreShowHidden ? 'var(--accent)' : '';
  btn.style.color = _exploreShowHidden ? '#000' : '';
  loadExplore(_explorePath);
}
function _fmtSize(bytes) {
  if (bytes == null) return '';
  if (bytes < 1024) return bytes + 'B';
  if (bytes < 1048576) return (bytes / 1024).toFixed(0) + 'K';
  return (bytes / 1048576).toFixed(1) + 'M';
}
function _showExploreMenu(path, btn) {
  // Remove any existing popup
  document.querySelectorAll('.explore-menu-popup').forEach(el => el.remove());
  const popup = document.createElement('div');
  popup.className = 'explore-menu-popup';
  const copyItem = document.createElement('button');
  copyItem.className = 'explore-menu-item';
  copyItem.textContent = 'Copy path';
  copyItem.onclick = () => { popup.remove(); _copyExplorePath(path); };
  popup.appendChild(copyItem);
  document.body.appendChild(popup);
  // Position near button
  const r = btn.getBoundingClientRect();
  const pw = popup.offsetWidth || 140;
  let left = r.right - pw;
  if (left < 8) left = 8;
  let top = r.bottom + 4;
  if (top + 80 > window.innerHeight) top = r.top - 80;
  popup.style.left = left + 'px';
  popup.style.top = top + 'px';
  // Dismiss on outside tap
  setTimeout(() => {
    const dismiss = e => { if (!popup.contains(e.target)) { popup.remove(); document.removeEventListener('pointerdown', dismiss, true); } };
    document.addEventListener('pointerdown', dismiss, true);
  }, 0);
}
function _copyExplorePath(path) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(path).catch(() => _copyExplorePathFallback(path));
  } else {
    _copyExplorePathFallback(path);
  }
}
function _copyExplorePathFallback(path) {
  const ta = document.createElement('textarea');
  ta.value = path; ta.style.cssText = 'position:fixed;top:0;left:0;opacity:0';
  document.body.appendChild(ta); ta.focus(); ta.select();
  try { document.execCommand('copy'); } catch(e) {}
  document.body.removeChild(ta);
}
async function loadExplore(path) {
  const body = document.getElementById('explore-body');
  body.innerHTML = '<div style="padding:16px;color:var(--dim)">Loading...</div>';
  _explorePath = path;
  // Build breadcrumb
  const parts = path.split('/').filter(Boolean);
  let crumbHtml = `<span class="explore-crumb" onclick="loadExplore('/')">/</span>`;
  let cum = '';
  for (const part of parts) {
    cum += '/' + part;
    const cp = cum;
    crumbHtml += `<span class="explore-crumb" onclick="loadExplore('${cp.replace(/'/g,"\\'")}')"> ${esc(part)}</span><span style="color:var(--dim)">/</span>`;
  }
  document.getElementById('explore-breadcrumb').innerHTML = crumbHtml;
  try {
    const r = await fetch(API + '/api/ls?path=' + encodeURIComponent(path) + (_exploreShowHidden ? '&hidden=1' : ''));
    const data = await r.json();
    if (data.error) { body.innerHTML = `<div style="padding:16px;color:var(--dim)">${esc(data.error)}</div>`; return; }
    body.innerHTML = '';
    // Back row if not at root
    if (data.parent && data.parent !== data.path) {
      const back = document.createElement('div');
      back.className = 'explore-row';
      back.innerHTML = `<span class="explore-icon">&#x2B05;</span><span class="explore-name" style="color:var(--dim)">.. (up)</span>`;
      back.onclick = () => loadExplore(data.parent);
      body.appendChild(back);
    }
    if (!data.entries.length) {
      body.innerHTML += '<div style="padding:16px;color:var(--dim)">Empty directory</div>';
      return;
    }
    for (const entry of data.entries) {
      const row = document.createElement('div');
      row.className = 'explore-row';
      const icon = entry.type === 'dir' ? '&#x1F4C2;' : '&#x1F4C4;';
      const displayName = entry.name + (entry.type === 'dir' ? '/' : '');
      const entryPath = path.replace(/\/$/, '') + '/' + entry.name;
      const menuBtn = `<button class="explore-menu-btn" title="Options" onclick="event.stopPropagation();_showExploreMenu('${entryPath.replace(/'/g,"\\'")}',this)">⋯</button>`;
      row.innerHTML = `<span class="explore-icon">${icon}</span><span class="explore-name">${esc(displayName)}</span><span class="explore-size">${esc(_fmtSize(entry.size))}</span>${menuBtn}`;
      if (entry.type === 'dir') {
        row.onclick = () => loadExplore(entryPath);
      } else {
        row.onclick = () => openFilePreview(entryPath);
      }
      body.appendChild(row);
    }
  } catch(e) {
    body.innerHTML = '<div style="padding:16px;color:var(--dim)">Failed to load directory.</div>';
  }
}

// Swipe right to close file preview
(function() {
  const el = document.getElementById('file-overlay');
  let sx = 0, sy = 0, tracking = false;
  el.addEventListener('touchstart', e => {
    sx = e.touches[0].clientX; sy = e.touches[0].clientY; tracking = true;
    el.style.transition = 'none';
  }, {passive: true});
  el.addEventListener('touchmove', e => {
    if (!tracking) return;
    const dx = e.touches[0].clientX - sx;
    const dy = Math.abs(e.touches[0].clientY - sy);
    if (dy > 30 && dx < 30) { tracking = false; el.style.transform = ''; el.style.transition = ''; return; }
    if (dx > 10) el.style.transform = 'translateX(' + dx + 'px)';
  }, {passive: true});
  el.addEventListener('touchend', e => {
    if (!tracking) { el.style.transition = ''; return; }
    tracking = false;
    const dx = e.changedTouches[0].clientX - sx;
    el.style.transition = 'transform 0.25s cubic-bezier(.4,0,.2,1)';
    if (dx > 80) {
      el.style.transform = 'translateX(100%)';
      setTimeout(() => { closeFilePreview(); el.style.transform = ''; el.style.transition = ''; }, 260);
    } else {
      el.style.transform = '';
      setTimeout(() => { el.style.transition = ''; }, 260);
    }
  });
})();

// Swipe right to close explorer
(function() {
  const el = document.getElementById('explore-overlay');
  let sx = 0, sy = 0, tracking = false;
  el.addEventListener('touchstart', e => {
    sx = e.touches[0].clientX; sy = e.touches[0].clientY; tracking = true;
    el.style.transition = 'none';
  }, {passive: true});
  el.addEventListener('touchmove', e => {
    if (!tracking) return;
    const dx = e.touches[0].clientX - sx;
    const dy = Math.abs(e.touches[0].clientY - sy);
    if (dy > 30 && dx < 30) { tracking = false; el.style.transform = ''; el.style.transition = ''; return; }
    if (dx > 10) el.style.transform = 'translateX(' + dx + 'px)';
  }, {passive: true});
  el.addEventListener('touchend', e => {
    if (!tracking) { el.style.transition = ''; return; }
    tracking = false;
    const dx = e.changedTouches[0].clientX - sx;
    el.style.transition = 'transform 0.25s cubic-bezier(.4,0,.2,1)';
    if (dx > 80) {
      el.style.transform = 'translateX(100%)';
      setTimeout(() => { closeExplore(); el.style.transform = ''; el.style.transition = ''; }, 260);
    } else {
      el.style.transform = '';
      setTimeout(() => { el.style.transition = ''; }, 260);
    }
  });
})();

function renderMarkdown(md) {
  // Lightweight markdown renderer — handles common elements
  let html = md;
  // Escape HTML first
  html = html.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  // Code blocks (``` ... ```)
  html = html.replace(/```(\w*)\n([\s\S]*?)```/g, (_, lang, code) => `<pre><code>${code.trim()}</code></pre>`);
  // Inline code
  html = html.replace(/`([^`]+)`/g, '<code>$1</code>');
  // Headers
  html = html.replace(/^### (.+)$/gm, '<h3>$1</h3>');
  html = html.replace(/^## (.+)$/gm, '<h2>$1</h2>');
  html = html.replace(/^# (.+)$/gm, '<h1>$1</h1>');
  // Bold / italic
  html = html.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
  html = html.replace(/\*(.+?)\*/g, '<em>$1</em>');
  // Blockquotes
  html = html.replace(/^&gt; (.+)$/gm, '<blockquote>$1</blockquote>');
  // Horizontal rules
  html = html.replace(/^---+$/gm, '<hr>');
  // Links: [text](url)
  html = html.replace(/\[([^\]]+)\]\((https?:\/\/[^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
  // Unordered lists
  html = html.replace(/^[-*] (.+)$/gm, '<li>$1</li>');
  html = html.replace(/((?:<li>.*<\/li>\n?)+)/g, '<ul>$1</ul>');
  // Paragraphs (double newline)
  html = html.replace(/\n\n+/g, '</p><p>');
  html = '<p>' + html + '</p>';
  // Clean up empty paragraphs
  html = html.replace(/<p>\s*<\/p>/g, '');
  html = html.replace(/<p>\s*(<h[123]>)/g, '$1');
  html = html.replace(/(<\/h[123]>)\s*<\/p>/g, '$1');
  html = html.replace(/<p>\s*(<pre>)/g, '$1');
  html = html.replace(/(<\/pre>)\s*<\/p>/g, '$1');
  html = html.replace(/<p>\s*(<ul>)/g, '$1');
  html = html.replace(/(<\/ul>)\s*<\/p>/g, '$1');
  html = html.replace(/<p>\s*(<hr>)/g, '$1');
  html = html.replace(/(<hr>)\s*<\/p>/g, '$1');
  html = html.replace(/<p>\s*(<blockquote>)/g, '$1');
  html = html.replace(/(<\/blockquote>)\s*<\/p>/g, '$1');
  // Linkify remaining URLs in text
  html = html.replace(/(^|[^"'>])(https?:\/\/[^\s<]+)/g, '$1<a href="$2" target="_blank" rel="noopener">$2</a>');
  return html;
}

// ── Connect to existing tmux session ──
async function openConnect() {
  const el = document.getElementById('connect-list');
  el.innerHTML = '<div class="connect-empty">Loading...</div>';
  document.getElementById('connect-overlay').classList.add('active');
  try {
    const r = await fetch(API + '/api/tmux-sessions');
    const items = await r.json();
    if (!items.length) {
      el.innerHTML = '<div class="connect-empty">No unregistered tmux sessions found.</div>';
      return;
    }
    el.innerHTML = items.map(s =>
      `<div class="connect-item" onclick="doConnect('${esc(s.tmux_name)}')">
        <div class="connect-item-info">
          <div class="connect-item-name">${esc(s.tmux_name)}</div>
          ${s.dir ? `<div class="connect-item-dir">${esc(s.dir)}</div>` : ''}
        </div>
      </div>`
    ).join('');
  } catch(e) {
    el.innerHTML = '<div class="connect-empty">Failed to load sessions.</div>';
  }
}
function closeConnect() {
  document.getElementById('connect-overlay').classList.remove('active');
}
async function doConnect(tmuxName) {
  closeConnect();
  await apiCall(API + '/api/sessions/connect', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ tmux_name: tmuxName })
  });
  await fetchSessions();
}

// ── Create session ──
function openCreate() {
  document.getElementById('create-name').value = '';
  document.getElementById('create-dir').value = '';
  document.getElementById('create-prompt').value = '';
  document.getElementById('ac-list').innerHTML = '';
  document.getElementById('ac-list').classList.remove('open');
  document.getElementById('create-overlay').classList.add('active');
  setTimeout(() => document.getElementById('create-name').focus({ preventScroll: true }), 100);
}
function closeCreate() {
  document.getElementById('create-overlay').classList.remove('active');
  document.getElementById('ac-list').classList.remove('open');
}
async function submitCreate() {
  const name = document.getElementById('create-name').value.trim();
  const dir = document.getElementById('create-dir').value.trim();
  const prompt = document.getElementById('create-prompt').value.trim();
  if (!name) { document.getElementById('create-name').focus({ preventScroll: true }); return; }
  closeCreate();

  if (!online) {
    // Offline: save as draft, will sync when connected
    addDraft(name, dir, prompt);
    showToast('Saved draft — will sync when online');
    render();
    return;
  }

  // Online: create immediately, optionally queue prompt
  const r = await apiCall(API + '/api/sessions', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ name, dir, creator: _getDeviceName() })
  });
  if (r && r.ok && prompt) {
    // Start session then send prompt
    await apiCall(API + '/api/sessions/' + encodeURIComponent(name) + '/start', { method: 'POST' });
    // Wait a moment for Claude to initialize
    setTimeout(async () => {
      await apiCall(API + '/api/sessions/' + encodeURIComponent(name) + '/send', {
        method: 'POST', headers: {'Content-Type':'application/json'},
        body: JSON.stringify({ text: prompt })
      });
    }, 5000);
  }
  await fetchSessions();
}

// ── Directory autocomplete ──
let acTimer = null;
let acItems = [];
let acSelected = -1;
function acFetch(query) {
  clearTimeout(acTimer);
  if (!query || query.length < 2) {
    document.getElementById('ac-list').classList.remove('open');
    return;
  }
  acTimer = setTimeout(async () => {
    try {
      const r = await fetch(API + '/api/autocomplete/dir?q=' + encodeURIComponent(query));
      acItems = await r.json();
      acSelected = -1;
      const el = document.getElementById('ac-list');
      if (!acItems.length) { el.classList.remove('open'); return; }
      el.innerHTML = acItems.map((item, i) =>
        `<div class="ac-item" onmousedown="acPick(${i})">${esc(item)}</div>`
      ).join('');
      el.classList.add('open');
    } catch(e) {}
  }, 150);
}
function acPick(i) {
  const inp = document.getElementById('create-dir');
  inp.value = acItems[i];
  document.getElementById('ac-list').classList.remove('open');
  // If they picked a dir, fetch its contents
  setTimeout(() => acFetch(inp.value), 50);
}
function acKeydown(e) {
  const el = document.getElementById('ac-list');
  if (!el.classList.contains('open')) {
    if (e.key === 'Enter') { e.preventDefault(); submitCreate(); }
    return;
  }
  if (e.key === 'ArrowDown') {
    e.preventDefault();
    acSelected = Math.min(acSelected + 1, acItems.length - 1);
    acHighlight();
  } else if (e.key === 'ArrowUp') {
    e.preventDefault();
    acSelected = Math.max(acSelected - 1, 0);
    acHighlight();
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (acSelected >= 0) acPick(acSelected);
    else { el.classList.remove('open'); submitCreate(); }
  } else if (e.key === 'Tab' && acItems.length) {
    e.preventDefault();
    acPick(acSelected >= 0 ? acSelected : 0);
  } else if (e.key === 'Escape') {
    el.classList.remove('open');
  }
}
function acHighlight() {
  const items = document.getElementById('ac-list').querySelectorAll('.ac-item');
  items.forEach((el, i) => el.classList.toggle('selected', i === acSelected));
  if (items[acSelected]) items[acSelected].scrollIntoView({ block: 'nearest' });
}
// Close autocomplete when clicking outside
document.addEventListener('click', e => {
  if (!e.target.closest('.ac-wrap')) {
    document.getElementById('ac-list').classList.remove('open');
  }
  // Close card slash autocomplete
  if (!e.target.closest('.send-row') && _cardAcName) {
    const el = document.getElementById('card-ac-' + _cardAcName);
    if (el) el.classList.remove('open');
    _cardAcItems = [];
  }
});

// ═══════ EVENT HANDLERS ═══════
// Pause peek refresh while selecting text (keep paused until selection is cleared or copied)
let peekSelectTimer = null;
function peekCheckSelection() {
  clearTimeout(peekSelectTimer);
  const sel = window.getSelection();
  if (sel && sel.toString().length > 0) {
    peekSelecting = true;
    peekSelectTimer = setTimeout(peekCheckSelection, 500);
  } else {
    peekSelecting = false;
  }
}
document.getElementById('peek-body').addEventListener('mousedown', () => { peekSelecting = true; clearTimeout(peekSelectTimer); });
document.getElementById('peek-body').addEventListener('touchstart', () => { peekSelecting = true; clearTimeout(peekSelectTimer); }, {passive: true});
// Force URLs in peek output to open in the system browser (PWA desktop + mobile).
// Handle both click (desktop) and touchend (iOS/Android) for reliability.
function _peekOpenLink(e) {
  const a = e.target.closest('a[href]');
  if (!a) return;
  const href = a.href;
  if (href && /^https?:\/\//.test(href)) {
    e.preventDefault();
    e.stopPropagation();
    window.open(href, '_blank', 'noopener,noreferrer');
  }
}
document.getElementById('peek-body').addEventListener('click', _peekOpenLink);
document.getElementById('peek-body').addEventListener('touchend', _peekOpenLink, {passive: false});
document.addEventListener('mouseup', () => { peekCheckSelection(); });
document.addEventListener('touchend', () => { peekCheckSelection(); });

// ── Clipboard: copy/paste events (most reliable in Chrome PWA desktop) ──
// The 'copy' and 'paste' DOM events give direct clipboardData access without
// any permission prompt, and Chrome fires them for Cmd/Ctrl+C/V even in
// standalone PWA mode where no native Edit menu exists.

document.addEventListener('copy', function(e) {
  const ae = document.activeElement;
  if (ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA' || ae.isContentEditable)) return;
  const sel = window.getSelection()?.toString();
  if (sel) { e.clipboardData.setData('text/plain', sel); e.preventDefault(); }
});

document.addEventListener('paste', function(e) {
  const ae = document.activeElement;
  if (ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA' || ae.isContentEditable)) return;
  const text = e.clipboardData?.getData('text/plain');
  if (!text) return;
  const peekOpen = document.getElementById('peek-overlay')?.classList.contains('active');
  const boardOpen = document.getElementById('board-detail-overlay')?.classList.contains('active');
  const inp = peekOpen ? document.getElementById('peek-cmd-input')
    : boardOpen ? document.querySelector('#board-detail-overlay textarea, #board-detail-overlay input')
    : document.querySelector('.card.open .send-input') || document.getElementById('search');
  if (!inp) return;
  e.preventDefault();
  const s = inp.selectionStart ?? inp.value.length;
  const en = inp.selectionEnd ?? inp.value.length;
  inp.value = inp.value.slice(0, s) + text + inp.value.slice(en);
  inp.selectionStart = inp.selectionEnd = s + text.length;
  inp.focus({ preventScroll: true });
  inp.dispatchEvent(new Event('input', { bubbles: true }));
});

// ── PWA clipboard polyfill ────────────────────────────────────────
// macOS desktop PWAs don't fire native copy/paste/select-all events even when
// focus is on an editable element. This helper intercepts those shortcuts and
// implements them via the Clipboard API. Called first in all keydown handlers
// so it works in every app context (sessions, peek, board detail, workspace).
// Returns true if the event was fully handled.
function _pwaCb(e) {
  if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return false;
  const k = e.key.toLowerCase();
  if (k !== 'a' && k !== 'c' && k !== 'x' && k !== 'v') return false;
  const ae = document.activeElement;
  const inp = (ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA')) ? ae : null;

  // Cmd+A — select all in focused input
  if (k === 'a' && inp) { e.preventDefault(); inp.select(); return true; }

  // Cmd+C / Cmd+X — copy (or cut) selected text.
  // Works for both focused inputs AND plain text selected in terminal output divs.
  if ((k === 'c' || k === 'x') && navigator.clipboard?.writeText) {
    // Case 1: focused input/textarea with a text selection
    if (inp) {
      const sel = inp.value.slice(inp.selectionStart, inp.selectionEnd);
      if (sel) {
        e.preventDefault();
        navigator.clipboard.writeText(sel).catch(() => {});
        if (k === 'x') {
          const s = inp.selectionStart, en = inp.selectionEnd;
          inp.value = inp.value.slice(0, s) + inp.value.slice(en);
          inp.selectionStart = inp.selectionEnd = s;
          inp.dispatchEvent(new Event('input', { bubbles: true }));
          if (typeof autoGrow === 'function') autoGrow(inp);
        }
        return true;
      }
    }
    // Case 2: text selected in a terminal output div (peek body, workspace pane body, etc.)
    if (k === 'c') {
      const sel = window.getSelection()?.toString();
      if (sel) {
        e.preventDefault();
        navigator.clipboard.writeText(sel).catch(() => {});
        return true;
      }
    }
  }

  // Cmd+V — paste into best available input.
  // Use Clipboard API directly — native paste events are unreliable in Chrome desktop PWAs.
  if (k === 'v') {
    if (!navigator.clipboard?.readText) return false;

    const peekOpen  = document.getElementById('peek-overlay')?.classList.contains('active');
    const boardOpen = document.getElementById('board-detail-overlay')?.classList.contains('active');
    const gridOpen  = document.getElementById('grid-view')?.classList.contains('active');
    const target = inp
      || (peekOpen  && document.getElementById('peek-cmd-input'))
      || (boardOpen && document.querySelector('#board-detail-overlay textarea, #board-detail-overlay input'))
      || (gridOpen  && document.activeElement?.closest('#grid-view') && document.activeElement)
      || document.querySelector('.card.open .send-input')
      || document.getElementById('search');
    if (target) {
      e.preventDefault();
      navigator.clipboard.readText().then(text => {
        if (!text) return;
        target.focus({ preventScroll: true });
        const s = target.selectionStart ?? target.value.length;
        const en = target.selectionEnd ?? target.value.length;
        target.value = target.value.slice(0, s) + text + target.value.slice(en);
        target.selectionStart = target.selectionEnd = s + text.length;
        target.dispatchEvent(new Event('input', { bubbles: true }));
        if (typeof autoGrow === 'function') autoGrow(target);
      }).catch(() => {});
      return true;
    }
  }

  return false;
}

document.addEventListener('keydown', (e) => {
  // Clipboard shortcuts work everywhere — run before any context-specific early returns
  if (_pwaCb(e)) return;

  if (document.getElementById('grid-view').classList.contains('active')) {
    if (e.key === 'Escape') { e.preventDefault(); exitGridMode(); return; }
    if ((e.ctrlKey || e.metaKey) && e.key === 'c' && !e.altKey && !e.shiftKey) {
      const hasSelection = window.getSelection().toString().length > 0;
      if (!hasSelection) {
        const names = Object.keys(_gridPanes);
        const target = _lastActivePane && _gridPanes[_lastActivePane] ? _lastActivePane
          : names.length === 1 ? names[0] : null;
        if (target) { e.preventDefault(); gpDoKeys(target, 'C-c'); return; }
      }
    }
    return;
  }
  if (document.getElementById('board-detail-overlay').classList.contains('active')) {
    if (e.key === 'Escape') { e.preventDefault(); closeBoardDetail(); return; }
    if ((e.metaKey || e.ctrlKey) && e.key === 's') { e.preventDefault(); boardDetailSave(); return; }
    return;
  }
  if (!document.getElementById('peek-overlay').classList.contains('active')) return;
  if (e.key === 'Escape') { e.preventDefault(); closePeek(); return; }
  // Ctrl+C with no selection → send interrupt; Ctrl+X (outside input) → send Ctrl-X
  if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey) {
    const ae = document.activeElement;
    const inInput = ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA');
    const hasPageSelection = window.getSelection().toString().length > 0;
    const hasInputSelection = inInput && ae.selectionStart !== ae.selectionEnd;
    if (e.key === 'c' && !hasPageSelection && !hasInputSelection) {
      e.preventDefault(); peekQuickKeys('C-c'); return;
    }
    if (e.key === 'x' && !inInput && !hasPageSelection) {
      e.preventDefault(); peekQuickKeys('C-x'); return;
    }
  }
});

// ═══════ LAYOUT MODES (list / grid) ═══════
let layoutMode = localStorage.getItem('amux_layout') || 'group';
let cardOrder = JSON.parse(localStorage.getItem('amux_card_order') || '[]');
let _sortable = null;
let _tileJustDragged = false; // keep for toggle() guard

function setLayoutMode(mode) {
  layoutMode = mode;
  localStorage.setItem('amux_layout', mode);
  document.getElementById('tile-list-btn').classList.toggle('active', mode === 'list');
  document.getElementById('tile-group-btn').classList.toggle('active', mode === 'group');
  document.getElementById('tile-grid-btn').classList.toggle('active', mode === 'grid');
  const cards = document.querySelector('.cards');
  if (cards) cards.classList.toggle('grid-mode', mode === 'grid');
  if (mode === 'group') destroySortable();
  render();
}

function initSortable() {
  if (typeof Sortable === 'undefined') return;
  destroySortable();
  const cards = document.querySelector('.cards');
  if (!cards) return;
  _sortable = Sortable.create(cards, {
    handle: '.card-drag-handle',
    animation: 150,
    ghostClass: 'sortable-ghost',
    chosenClass: 'sortable-chosen',
    dragClass: 'sortable-drag',
    onStart: function(evt) {
      _tileJustDragged = false;
      console.log('[amux:drag] onStart', { session: evt.item?.dataset?.session, oldIndex: evt.oldIndex });
    },
    onMove: function(evt) {
      console.log('[amux:drag] onMove', { dragged: evt.dragged?.dataset?.session, related: evt.related?.dataset?.session, willInsertAfter: evt.willInsertAfter });
    },
    onEnd: function(evt) {
      _tileJustDragged = evt.oldIndex !== evt.newIndex;
      const allCards = cards.querySelectorAll('.card[data-session]');
      cardOrder = Array.from(allCards).map(c => c.dataset.session);
      localStorage.setItem('amux_card_order', JSON.stringify(cardOrder));
      console.log('[amux:drag] onEnd', { session: evt.item?.dataset?.session, oldIndex: evt.oldIndex, newIndex: evt.newIndex, moved: _tileJustDragged, cardOrder });
    }
  });
}

function destroySortable() {
  if (_sortable) { try { _sortable.destroy(); } catch(e) {} _sortable = null; }
}

function tileMouseDown(e, name) {} // no-op — kept so card HTML doesn't break

// Initialize layout on load
document.addEventListener('DOMContentLoaded', function() {
  const cards = document.querySelector('.cards');
  if (layoutMode === 'grid' && window.innerWidth >= 900) {
    if (cards) cards.classList.add('grid-mode');
    document.getElementById('tile-grid-btn').classList.add('active');
    setTimeout(initSortable, 200);
  } else if (layoutMode === 'group') {
    document.getElementById('tile-group-btn').classList.add('active');
  } else {
    layoutMode = 'list';
    document.getElementById('tile-list-btn').classList.add('active');
    setTimeout(initSortable, 200);
  }
});

// ═══════ BOARD ═══════
let activeView = 'sessions';
let boardItems = [];
let boardStatuses = [{id:'backlog',label:'Backlog'},{id:'todo',label:'To Do'},{id:'doing',label:'In Progress'},{id:'done',label:'Done'},{id:'discarded',label:'Discarded'}];
let _boardSortables = [];
let boardTimer = null;
let boardEditId = null;
let boardEditStatus = 'todo';
let lastBoardJSON = '';
let lastStatusesJSON = '';
let boardFilterTag = null;
let boardFilterSession = null;
let boardSearchQuery = '';
let _boardDragId = null;
let boardViewMode = localStorage.getItem('amux_board_view') || 'session';
let _sessionGroupCollapsed = JSON.parse(localStorage.getItem('amux_board_collapsed') || '{}');
let _tagGroupCollapsed = JSON.parse(localStorage.getItem('amux_status_collapsed') || '{}');
let _collapsedCols = new Set(JSON.parse(localStorage.getItem('amux_col_collapsed') || '[]'));

const _BUILT_IN_STATUS_STYLE = {
  'backlog':   {bg:'rgba(88,166,255,0.12)',color:'var(--accent)',border:'rgba(88,166,255,0.3)',dot:'var(--accent)'},
  'todo':      {bg:'rgba(139,148,158,0.12)',color:'var(--dim)',border:'rgba(139,148,158,0.3)',dot:'var(--dim)'},
  'doing':     {bg:'rgba(210,153,34,0.15)',color:'var(--yellow)',border:'rgba(210,153,34,0.4)',dot:'var(--yellow)'},
  'done':      {bg:'rgba(63,185,80,0.15)',color:'var(--green)',border:'rgba(63,185,80,0.4)',dot:'var(--green)'},
  'discarded': {bg:'rgba(139,148,158,0.08)',color:'rgba(139,148,158,0.5)',border:'rgba(139,148,158,0.2)',dot:'rgba(139,148,158,0.4)'},
};
const _CUSTOM_STATUS_PALETTE = [
  {bg:'rgba(88,166,255,0.15)',color:'var(--accent)',border:'rgba(88,166,255,0.4)',dot:'var(--accent)'},
  {bg:'rgba(188,140,255,0.15)',color:'#bc8cff',border:'rgba(188,140,255,0.4)',dot:'#bc8cff'},
  {bg:'rgba(255,140,80,0.15)',color:'#ff8c50',border:'rgba(255,140,80,0.4)',dot:'#ff8c50'},
  {bg:'rgba(248,81,73,0.15)',color:'var(--red)',border:'rgba(248,81,73,0.4)',dot:'var(--red)'},
  {bg:'rgba(57,210,192,0.15)',color:'var(--cyan)',border:'rgba(57,210,192,0.4)',dot:'var(--cyan)'},
  {bg:'rgba(255,100,180,0.15)',color:'#ff64b4',border:'rgba(255,100,180,0.4)',dot:'#ff64b4'},
];
function statusStyle(id) {
  if (_BUILT_IN_STATUS_STYLE[id]) return _BUILT_IN_STATUS_STYLE[id];
  const customs = boardStatuses.filter(s => !_BUILT_IN_STATUS_STYLE[s.id]);
  const idx = customs.findIndex(s => s.id === id);
  return _CUSTOM_STATUS_PALETTE[Math.max(0, idx) % _CUSTOM_STATUS_PALETTE.length];
}

function switchView(view) {
  activeView = view;
  document.getElementById('session-view').style.display = view === 'sessions' ? '' : 'none';
  document.getElementById('board-view').style.display = view === 'board' ? '' : 'none';
  document.getElementById('calendar-view').style.display = view === 'calendar' ? '' : 'none';
  document.getElementById('tab-sessions').classList.toggle('active', view === 'sessions');
  document.getElementById('tab-board').classList.toggle('active', view === 'board');
  document.getElementById('tab-calendar').classList.toggle('active', view === 'calendar');
  if (view === 'board') {
    renderBoard();
    fetchBoard();
    // Only poll if SSE is not active (SSE pushes board updates)
    if (_sseFallback && !boardTimer) boardTimer = setInterval(fetchBoard, 5000);
  } else if (view === 'calendar') {
    fetchBoard().then(() => renderCalendar());
  } else {
    if (boardTimer) { clearInterval(boardTimer); boardTimer = null; }
  }
}

async function fetchBoard() {
  try {
    const [r, rs] = await Promise.all([
      fetch(API + '/api/board'),
      fetch(API + '/api/board/statuses'),
    ]);
    const data = await r.json();
    const statusData = await rs.json();
    consecutiveFailures = 0;
    if (!online) setOnline(true);
    const sj = JSON.stringify(statusData);
    const j = JSON.stringify(data);
    const statusesChanged = sj !== lastStatusesJSON;
    const itemsChanged = j !== lastBoardJSON;
    if (statusesChanged) {
      lastStatusesJSON = sj;
      boardStatuses = statusData;
    }
    if (itemsChanged || statusesChanged) {
      lastBoardJSON = j;
      boardItems = data;
      localStorage.setItem('amux_board_cache', j);
      renderBoard();
    }
  } catch(e) {
    console.error('fetch board:', e);
    consecutiveFailures++;
    if (consecutiveFailures >= 2 || navigator.onLine === false) {
      setOnline(false);
    }
  }
}

function timeAgo(ts) {
  const diff = Math.floor(Date.now() / 1000) - ts;
  if (diff < 60) return 'just now';
  if (diff < 3600) return Math.floor(diff / 60) + 'm ago';
  if (diff < 86400) return Math.floor(diff / 3600) + 'h ago';
  return Math.floor(diff / 86400) + 'd ago';
}

function renderMarkdown(raw) {
  if (!raw) return '';
  const parts = raw.split(/(```[\s\S]*?```)/g);
  return parts.map((part, i) => {
    if (i % 2 === 1) {
      const m = part.match(/^```(\w*)\n?([\s\S]*?)```$/);
      const lang = m ? esc(m[1]) : '';
      const code = m ? esc(m[2]) : esc(part);
      return '<pre><code' + (lang ? ' class="language-' + lang + '"' : '') + '>' + code + '</code></pre>';
    }
    let s = part.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
    s = s.replace(/^### (.+)$/gm, '<h3>$1</h3>');
    s = s.replace(/^## (.+)$/gm, '<h2>$1</h2>');
    s = s.replace(/^# (.+)$/gm, '<h1>$1</h1>');
    s = s.replace(/\*\*\*(.+?)\*\*\*/g, '<strong><em>$1</em></strong>');
    s = s.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
    s = s.replace(/\*(.+?)\*/g, '<em>$1</em>');
    s = s.replace(/_(.+?)_/g, '<em>$1</em>');
    s = s.replace(/`([^`]+)`/g, '<code>$1</code>');
    s = s.replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_, text, url) => {
      const safe = /^(https?:\/\/|\/|#)/.test(url) ? url : '#';
      return `<a href="${safe}" target="_blank" rel="noopener noreferrer">${text}</a>`;
    });
    s = s.replace(/^---$/gm, '<hr>');
    s = s.replace(/^&gt; (.+)$/gm, '<blockquote>$1</blockquote>');
    s = s.replace(/((?:^[-*] .+\n?)+)/gm, (block) => {
      const items = block.trim().split('\n').map(l => '<li>' + l.replace(/^[-*] /, '') + '</li>').join('');
      return '<ul>' + items + '</ul>';
    });
    s = s.replace(/((?:^\d+\. .+\n?)+)/gm, (block) => {
      const items = block.trim().split('\n').map(l => '<li>' + l.replace(/^\d+\. /, '') + '</li>').join('');
      return '<ol>' + items + '</ol>';
    });
    s = s.replace(/\n\n+/g, '</p><p>');
    s = s.replace(/\n/g, '<br>');
    if (s && !s.startsWith('<h') && !s.startsWith('<ul') && !s.startsWith('<ol') && !s.startsWith('<blockquote') && !s.startsWith('<hr') && !s.startsWith('<pre')) {
      s = '<p>' + s + '</p>';
    }
    return s;
  }).join('');
}

function renderBoardFilters() {
  const el = document.getElementById('board-filters');
  if (!el) return;
  const allTags = [...new Set(boardItems.flatMap(i => i.tags || []))].sort();
  const allSessions = [...new Set(boardItems.map(i => i.session).filter(Boolean))].sort();
  let html = '';
  if (allTags.length) {
    html += '<span class="board-filter-label">Tag:</span>';
    allTags.forEach(t => {
      const active = boardFilterTag === t;
      html += "<button class='board-filter-chip" + (active ? " active" : "") + "' onclick='toggleBoardTag(" + JSON.stringify(t) + ")'>" + esc(t) + "</button>";
    });
  }
  if (allSessions.length) {
    if (allTags.length) html += '<span class="board-filter-sep">|</span>';
    html += '<span class="board-filter-label">Session:</span>';
    allSessions.forEach(s => {
      const active = boardFilterSession === s;
      html += "<button class='board-filter-chip" + (active ? " active" : "") + "' onclick='toggleBoardSession(" + JSON.stringify(s) + ")'>" + esc(s) + "</button>";
    });
  }
  if (boardFilterTag || boardFilterSession) {
    html += '<button class="board-filter-chip board-filter-clear" onclick="boardFilterTag=null;boardFilterSession=null;document.getElementById(\'board-search\').value=\'\';boardSearchQuery=\'\';renderBoard()">&#x2715; Clear</button>';
  }
  el.innerHTML = html;
}

function toggleBoardTag(tag) {
  boardFilterTag = boardFilterTag === tag ? null : tag;
  renderBoard();
}

function toggleBoardSession(session) {
  boardFilterSession = boardFilterSession === session ? null : session;
  renderBoard();
}

function boardDragStart(e, id) {
  _boardDragId = id;
  e.dataTransfer.effectAllowed = 'move';
  e.dataTransfer.setData('text/plain', id);
  setTimeout(() => {
    const el = document.querySelector('.board-card[data-id="' + id + '"]');
    if (el) el.classList.add('dragging');
  }, 0);
}

function boardDragEnd() {
  document.querySelectorAll('.board-card.dragging').forEach(el => el.classList.remove('dragging'));
  document.querySelectorAll('.board-col.drag-over').forEach(el => el.classList.remove('drag-over'));
}

function boardColDragOver(e, col) {
  e.preventDefault();
  e.dataTransfer.dropEffect = 'move';
  document.querySelectorAll('.board-col').forEach(el => el.classList.remove('drag-over'));
  e.currentTarget.classList.add('drag-over');
}

function boardColDragLeave(e) {
  if (!e.currentTarget.contains(e.relatedTarget)) {
    e.currentTarget.classList.remove('drag-over');
  }
}

async function boardColDrop(e, col) {
  e.preventDefault();
  e.currentTarget.classList.remove('drag-over');
  if (_boardDragId) {
    const item = boardItems.find(i => i.id === _boardDragId);
    if (item && item.status !== col) await moveBoardItem(_boardDragId, col);
    _boardDragId = null;
  }
}

let _prevCardRects = {};

function setBoardView(mode) {
  boardViewMode = mode;
  localStorage.setItem('amux_board_view', mode);
  renderBoard();
}

function toggleSessionGroup(name) {
  _sessionGroupCollapsed[name] = !_sessionGroupCollapsed[name];
  localStorage.setItem('amux_board_collapsed', JSON.stringify(_sessionGroupCollapsed));
  renderBoard();
}

function _renderBoardCard(item) {
  const tags = item.tags || [];
  const firstLine = (item.desc || '').split('\n')[0].slice(0, 80);
  let h = '<div class="board-card" data-id="' + item.id + '" onclick="openBoardDetail(\'' + item.id + '\')">';
  h += '<div class="board-drag-handle" onclick="event.stopPropagation()" title="Drag to move"><svg width="12" height="12" viewBox="0 0 12 12" fill="currentColor"><circle cx="3.5" cy="2.5" r="1.25"/><circle cx="8.5" cy="2.5" r="1.25"/><circle cx="3.5" cy="6" r="1.25"/><circle cx="8.5" cy="6" r="1.25"/><circle cx="3.5" cy="9.5" r="1.25"/><circle cx="8.5" cy="9.5" r="1.25"/></svg></div>';
  h += '<div class="board-card-key">' + esc(item.id) + '</div>';
  h += '<div class="board-card-title">';
  if (boardViewMode === 'session') { const _st = item.status || 'todo'; h += '<span class="board-status-dot" style="background:' + statusStyle(_st).dot + '"></span>'; }
  h += esc(item.title) + '</div>';
  if (firstLine) h += '<div class="board-card-desc">' + esc(firstLine) + ((item.desc || '').length > 80 ? '\u2026' : '') + '</div>';
  h += '<div class="board-card-footer">';
  if (boardViewMode !== 'session' && item.session) h += '<span class="board-card-session" data-session="' + esc(item.session) + '">' + esc(item.session) + '</span>';
  tags.forEach(function(t) { h += '<span class="board-card-tag" data-tag="' + esc(t) + '">' + esc(t) + '</span>'; });
  if (item.due) { const today = new Date().toISOString().slice(0,10); const overdue = item.due < today && item.status !== 'done'; h += '<span class="board-card-time" style="' + (overdue ? 'color:var(--red)' : 'color:var(--accent)') + '">&#x1F4C5; ' + item.due + '</span>'; }
  h += '<span class="board-card-time">' + timeAgo(item.updated || item.created) + '</span>';
  if (item.creator) h += '<span class="board-card-time">' + esc(item.creator) + '</span>';
  h += '</div></div>';
  return h;
}

function _renderBoardBySession(visible, container) {
  // Group all items by session
  const groups = {};
  const noSession = [];
  visible.forEach(function(item) {
    if (item.session) {
      if (!groups[item.session]) groups[item.session] = [];
      groups[item.session].push(item);
    } else {
      noSession.push(item);
    }
  });
  const sessionNames = Object.keys(groups).sort();
  if (noSession.length) sessionNames.push('');

  function _sessionCountsHtml(items) {
    const statusCounts = {};
    items.forEach(function(i) { const s = i.status || 'todo'; statusCounts[s] = (statusCounts[s] || 0) + 1; });
    let h = '';
    boardStatuses.forEach(function(stObj) {
      const c = statusCounts[stObj.id] || 0;
      if (!c) return;
      const sty = statusStyle(stObj.id);
      h += '<span class="board-session-count" style="background:' + sty.bg + ';color:' + sty.color + '">' + c + ' ' + esc(stObj.label.toLowerCase()) + '</span>';
    });
    return h;
  }

  let html = '';

  // ── Yours (human tasks) at the top ──
  if (humanItems.length) {
    const collapsed = _sessionGroupCollapsed['__human__'];
    html += '<div class="board-session-group board-human-group">';
    html += '<div class="board-session-header" onclick="toggleSessionGroup(\'__human__\')">';
    html += '<span class="board-session-chevron' + (collapsed ? '' : ' open') + '">\u25B6</span>';
    html += '<span class="board-session-name">&#x1F464; Yours</span>';
    html += '<div class="board-session-counts">' + _sessionCountsHtml(humanItems) + '</div></div>';
    if (!collapsed) {
      html += '<div class="board-session-items">';
      humanItems.forEach(function(item) { html += _renderBoardCard(item); });
      html += '</div>';
    }
    html += '</div>';
  }

  // ── Agent tasks grouped by session ──
  sessionNames.forEach(function(name) {
    const items = name ? groups[name] : noSession;
    const collapsed = _sessionGroupCollapsed[name || '__none__'];
    const groupKey = name || '__none__';
    html += '<div class="board-session-group">';
    html += '<div class="board-session-header" onclick="toggleSessionGroup(\'' + esc(groupKey) + '\')">';
    html += '<span class="board-session-chevron' + (collapsed ? '' : ' open') + '">\u25B6</span>';
    html += '<span class="board-session-name">' + (name ? esc(name) : '<span style="color:var(--dim)">Unassigned</span>') + '</span>';
    html += '<div class="board-session-counts">' + _sessionCountsHtml(items) + '</div></div>';
    if (!collapsed) {
      html += '<div class="board-session-items">';
      items.forEach(function(item) { html += _renderBoardCard(item); });
      html += '</div>';
    }
    html += '</div>';
  });

  if (!visible.length) {
    html = '<div class="board-session-empty">No board items yet</div>';
  }
  container.innerHTML = html;
}

function renderBoard() {
  renderBoardFilters();
  const container = document.getElementById('board-columns');

  // Update view toggle buttons
  var bvS = document.getElementById('bv-session');
  var bvC = document.getElementById('bv-status');
  if (bvS) bvS.classList.toggle('active', boardViewMode === 'session');
  if (bvC) bvC.classList.toggle('active', boardViewMode === 'status');

  let visible = boardItems;
  if (boardFilterTag) visible = visible.filter(i => (i.tags || []).includes(boardFilterTag));
  if (boardFilterSession) visible = visible.filter(i => i.session === boardFilterSession);
  if (boardSearchQuery) {
    const q = boardSearchQuery;
    visible = visible.filter(i =>
      (i.title || '').toLowerCase().includes(q) ||
      (i.desc || '').toLowerCase().includes(q) ||
      (i.id || '').toLowerCase().includes(q) ||
      (i.session || '').toLowerCase().includes(q) ||
      (i.tags || []).some(t => t.toLowerCase().includes(q))
    );
  }

  if (boardViewMode === 'session') {
    container.classList.remove('board-columns');
    container.classList.add('board-columns-list');
    _renderBoardBySession(visible, container);
    return;
  }

  container.classList.add('board-columns');
  container.classList.remove('board-columns-list');

  const cols = {};
  boardStatuses.forEach(s => { cols[s.id] = []; });
  visible.forEach(item => {
    const s = item.status || 'todo';
    if (cols[s] !== undefined) cols[s].push(item);
    else { cols['todo'] = cols['todo'] || []; cols['todo'].push(item); }
  });

  // FLIP step 1: snapshot current card positions
  const oldRects = {};
  const oldIds = new Set();
  container.querySelectorAll('.board-card[data-id]').forEach(el => {
    const id = el.dataset.id;
    oldIds.add(id);
    const r = el.getBoundingClientRect();
    oldRects[id] = { top: r.top, left: r.left };
  });

  const builtIn = new Set(['backlog','todo','doing','done','discarded']);
  let html = '';
  boardStatuses.forEach(stObj => {
    const st = stObj.id;
    const stCol = cols[st] || [];
    const sty = statusStyle(st);
    const isBuiltIn = builtIn.has(st);
    const collapsed = _collapsedCols.has(st);
    html += '<div class="board-col' + (collapsed ? ' col-collapsed' : '') + '" data-col="' + st + '">';
    html += '<div class="board-col-header">';
    html += '<span style="display:flex;align-items:center;gap:5px;">';
    html += '<button class="board-col-collapse" onclick="toggleColCollapse(\'' + st + '\')" title="' + (collapsed ? 'Expand' : 'Collapse') + '">' + (collapsed ? '&#x25B8;' : '&#x25BE;') + '</button>';
    html += '<span style="color:' + sty.color + '">' + esc(stObj.label) + '</span>';
    html += '</span>';
    html += '<span style="display:flex;align-items:center;gap:6px;">';
    html += '<span class="col-count" data-col="' + st + '">' + stCol.length + '</span>';
    if (!isBuiltIn) {
      html += '<button class="col-del-btn" onclick="event.stopPropagation();deleteBoardStatus(\'' + st + '\')" title="Delete column">&#x2715;</button>';
    }
    html += '</span></div>';
    if (stCol.length === 0) {
      html += '<div class="board-empty">Nothing here</div>';
    }
    stCol.forEach(item => { html += _renderBoardCard(item); });
    if (st === 'done' && stCol.length > 0) {
      html += '<button class="board-add-btn" style="color:var(--red);border-color:rgba(248,81,73,0.2);" onclick="clearDone()">Clear done</button>';
    }
    html += '<button class="board-add-btn" onclick="openBoardAdd(\'' + st + '\')">+ Add</button>';
    html += '</div>';
  });
  html += '<button class="board-add-col-btn" onclick="addBoardStatus()">+ Add column</button>';
  container.innerHTML = html;

  // Init Sortable.js on each column for touch-friendly cross-column drag
  if (typeof Sortable !== 'undefined') {
    _boardSortables.forEach(s => { try { s.destroy(); } catch(e) {} });
    _boardSortables = [];
    container.querySelectorAll('.board-col').forEach(colEl => {
      _boardSortables.push(Sortable.create(colEl, {
        group: 'board',
        animation: 150,
        handle: '.board-drag-handle',
        ghostClass: 'board-sortable-ghost',
        chosenClass: 'board-sortable-chosen',
        dragClass: 'board-sortable-drag',
        filter: '.board-col-header, .board-add-btn, .board-empty',
        preventOnFilter: false,
        delay: 150,
        delayOnTouchOnly: true,
        touchStartThreshold: 4,
        onEnd: function(evt) {
          const id = evt.item.dataset.id;
          const newStatus = evt.to.dataset.col;
          if (!id || !newStatus) return;
          const item = boardItems.find(i => i.id === id);
          if (item && item.status !== newStatus) moveBoardItem(id, newStatus);
        }
      }));
    });
  }

  // FLIP step 2: animate cards
  container.querySelectorAll('.board-card[data-id]').forEach(el => {
    const id = el.dataset.id;
    if (!oldIds.has(id)) {
      el.classList.add('card-enter');
      el.addEventListener('animationend', () => el.classList.remove('card-enter'), { once: true });
    } else if (oldRects[id]) {
      const newR = el.getBoundingClientRect();
      const dx = oldRects[id].left - newR.left;
      const dy = oldRects[id].top - newR.top;
      if (Math.abs(dx) > 1 || Math.abs(dy) > 1) {
        el.style.transform = 'translate(' + dx + 'px,' + dy + 'px)';
        el.style.transition = 'none';
        requestAnimationFrame(() => {
          el.classList.add('card-flip');
          el.style.transform = '';
          el.style.transition = '';
          el.addEventListener('transitionend', () => el.classList.remove('card-flip'), { once: true });
        });
      }
    }
  });

  // Bump column counts that changed
  container.querySelectorAll('.col-count').forEach(el => {
    const col = el.dataset.col;
    const prev = _prevCardRects[col] || 0;
    const cur = cols[col] ? cols[col].length : 0;
    if (prev !== cur) {
      el.classList.remove('bump');
      void el.offsetWidth;
      el.classList.add('bump');
      el.addEventListener('animationend', () => el.classList.remove('bump'), { once: true });
    }
  });
  boardStatuses.forEach(stObj => { _prevCardRects[stObj.id] = (cols[stObj.id] || []).length; });
}

// Event delegation for board tag + session clicks (cards + detail)
document.getElementById('board-columns').addEventListener('click', function(e) {
  const tag = e.target.closest('.board-card-tag[data-tag]');
  if (tag) { e.stopPropagation(); e.preventDefault(); toggleBoardTag(tag.dataset.tag); return; }
  const sess = e.target.closest('.board-card-session[data-session]');
  if (sess) { e.stopPropagation(); e.preventDefault(); toggleBoardSession(sess.dataset.session); }
});
document.getElementById('board-detail-overlay').addEventListener('click', function(e) {
  const tag = e.target.closest('.board-card-tag[data-tag]');
  if (tag) { e.stopPropagation(); e.preventDefault(); closeBoardDetail(); toggleBoardTag(tag.dataset.tag); return; }
  const sess = e.target.closest('.board-card-session[data-session]');
  if (sess) { e.stopPropagation(); e.preventDefault(); closeBoardDetail(); toggleBoardSession(sess.dataset.session); }
});

function _populateSessionSelect(selectId, current) {
  const sel = document.getElementById(selectId);
  if (!sel) return;
  const names = [...new Set(sessions.map(s => s.name))].sort();
  let html = '<option value="">— none —</option>';
  names.forEach(n => {
    html += '<option value="' + esc(n) + '"' + (n === current ? ' selected' : '') + '>' + esc(n) + '</option>';
  });
  sel.innerHTML = html;
  if (current) sel.value = current;
}

function openBoardAdd(statusOrDate, prefillDate) {
  // statusOrDate: can be a status string or a YYYY-MM-DD date (from calendar cell click)
  let status = 'backlog', dueDate = prefillDate || '';
  if (statusOrDate && /^\d{4}-\d{2}-\d{2}$/.test(statusOrDate)) {
    dueDate = statusOrDate;
  } else if (statusOrDate) {
    status = statusOrDate;
  }
  boardEditId = null;
  boardEditStatus = status;
  document.getElementById('be-title').value = '';
  document.getElementById('be-desc').value = '';
  const dueEl = document.getElementById('be-due');
  if (dueEl) dueEl.value = dueDate;
  const sel = document.getElementById('be-status');
  sel.innerHTML = boardStatuses.map(s => '<option value="' + s.id + '">' + esc(s.label) + '</option>').join('');
  sel.value = status;
  _populateSessionSelect('be-session-add', peekSession || '');
  document.getElementById('board-edit-overlay').classList.add('active');
  document.getElementById('be-title').focus();
}

function closeBoardEdit() {
  document.getElementById('board-edit-overlay').classList.remove('active');
  boardEditId = null;
}

async function saveBoardEdit() {
  const title = document.getElementById('be-title').value.trim();
  if (!title) return;
  const desc = document.getElementById('be-desc').value.trim();
  const status = document.getElementById('be-status').value;
  const sel = document.getElementById('be-session-add');
  const session = sel ? sel.value : '';
  const sess = sessions.find(s => s.name === session);
  const tags = sess ? (sess.tags || []) : [];
  const dueEl = document.getElementById('be-due');
  const due = dueEl ? dueEl.value : '';
  closeBoardEdit();
  await addBoardItem(title, desc, status, session, tags, due);
  if (_peekTab === 'issues') renderPeekIssues();
}


// ── Board detail (full-screen) ──
let boardDetailId = null;
let boardDetailStatus = 'todo';
const _boardDrafts = {};  // item id → { title, desc, session, status, due }

function openBoardDetail(id) {
  const item = boardItems.find(i => i.id === id);
  if (!item) return;
  boardDetailId = id;
  const draft = _boardDrafts[id];
  boardDetailStatus = draft ? draft.status : (item.status || 'todo');
  const titleEl = document.getElementById('bd-title');
  titleEl.value = draft ? draft.title : item.title;
  titleEl.style.height = 'auto';
  titleEl.style.height = titleEl.scrollHeight + 'px';
  document.getElementById('bd-desc').value = draft ? draft.desc : (item.desc || '');
  _renderDetailStatusBtns();
  const keyEl = document.getElementById('bd-key');
  if (keyEl) keyEl.textContent = item.id || '';
  _populateSessionSelect('bd-session', draft ? draft.session : (item.session || ''));
  const dueEl = document.getElementById('bd-due');
  if (dueEl) dueEl.value = draft ? (draft.due || '') : (item.due || '');
  boardDetailTab('edit');
  const meta = document.getElementById('bd-meta');
  const parts = [];
  const tags = item.tags || [];
  if (tags.length) parts.push('Tags: ' + tags.map(t => '<span class="board-card-tag" data-tag="' + esc(t) + '">' + esc(t) + '</span>').join(' '));
  if (item.creator) parts.push('From ' + esc(item.creator));
  if (item.created) parts.push('Created ' + timeAgo(item.created));
  if (item.updated && item.updated !== item.created) parts.push('Updated ' + timeAgo(item.updated));
  meta.innerHTML = parts.map(p => '<div class="board-detail-meta-row">' + p + '</div>').join('');
  document.getElementById('bd-save-status').textContent = '';
  document.getElementById('board-detail-overlay').classList.add('active');
  setTimeout(() => document.getElementById('bd-title').focus(), 100);
}

function boardDetailTab(tab) {
  const editBtn = document.getElementById('bd-tab-edit');
  const previewBtn = document.getElementById('bd-tab-preview');
  const desc = document.getElementById('bd-desc');
  const preview = document.getElementById('bd-preview');
  if (!editBtn || !previewBtn || !desc || !preview) return;
  if (tab === 'preview') {
    editBtn.classList.remove('active');
    previewBtn.classList.add('active');
    desc.style.display = 'none';
    preview.style.display = '';
    preview.innerHTML = renderMarkdown(desc.value);
  } else {
    editBtn.classList.add('active');
    previewBtn.classList.remove('active');
    desc.style.display = '';
    preview.style.display = 'none';
  }
}

function _renderDetailStatusBtns() {
  document.getElementById('bd-status-row').innerHTML = boardStatuses.map(s => {
    const sty = statusStyle(s.id);
    const isActive = boardDetailStatus === s.id;
    const activeStyle = isActive ? 'background:' + sty.bg + ';color:' + sty.color + ';border-color:' + sty.border : '';
    return '<button class="board-detail-status-btn" style="' + activeStyle + '" onclick="boardDetailSetStatus(\'' + s.id + '\')">' + esc(s.label) + '</button>';
  }).join('');
}

function boardDetailSetStatus(st) {
  boardDetailStatus = st;
  _renderDetailStatusBtns();
}

function closeBoardDetail() {
  // Save unsaved edits as draft
  if (boardDetailId) {
    const item = boardItems.find(i => i.id === boardDetailId);
    if (item) {
      const t = (document.getElementById('bd-title').value || '').trim();
      const d = (document.getElementById('bd-desc').value || '').trim();
      const sel = document.getElementById('bd-session');
      const s = sel ? sel.value : (item.session || '');
      const st = boardDetailStatus;
      const dueEl = document.getElementById('bd-due');
      const due = dueEl ? dueEl.value : (item.due || '');
      // Only save draft if something actually differs from saved state
      if (t !== (item.title || '') || d !== (item.desc || '') || s !== (item.session || '') || st !== (item.status || 'todo') || due !== (item.due || '')) {
        _boardDrafts[boardDetailId] = { title: t, desc: d, session: s, status: st, due };
      } else {
        delete _boardDrafts[boardDetailId];
      }
    }
  }
  document.getElementById('board-detail-overlay').classList.remove('active');
  boardDetailId = null;
  // Refresh peek issues panel if open
  if (_peekTab === 'issues') renderPeekIssues();
}

// Swipe right to close board detail
(function() {
  const el = document.getElementById('board-detail-overlay');
  let sx = 0, sy = 0, tracking = false;
  el.addEventListener('touchstart', e => {
    if (!el.classList.contains('active')) return;
    const t = e.touches[0];
    sx = t.clientX; sy = t.clientY; tracking = true;
    el.style.transition = 'none';
  }, {passive: true});
  el.addEventListener('touchmove', e => {
    if (!tracking || !el.classList.contains('active')) return;
    const dx = e.touches[0].clientX - sx;
    const dy = Math.abs(e.touches[0].clientY - sy);
    if (dy > 30 && dx < 30) { tracking = false; el.style.transform = ''; el.style.transition = ''; return; }
    if (dx > 10) el.style.transform = 'translateX(' + dx + 'px)';
  }, {passive: true});
  el.addEventListener('touchend', e => {
    if (!tracking) { el.style.transition = ''; return; }
    tracking = false;
    if (!el.classList.contains('active')) { el.style.transition = ''; return; }
    const dx = e.changedTouches[0].clientX - sx;
    el.style.transition = 'transform 0.25s cubic-bezier(.4,0,.2,1), opacity 0.25s, pointer-events 0s';
    if (dx > 80) {
      el.style.transform = 'translateX(100%)';
      setTimeout(() => { closeBoardDetail(); el.style.transform = ''; el.style.transition = ''; }, 260);
    } else {
      el.style.transform = '';
      setTimeout(() => { el.style.transition = ''; }, 260);
    }
  }, {passive: true});
})();

async function boardDetailSave() {
  if (!boardDetailId) return;
  const title = document.getElementById('bd-title').value.trim();
  if (!title) return;
  const desc = document.getElementById('bd-desc').value.trim();
  const sel = document.getElementById('bd-session');
  const session = sel ? sel.value : undefined;
  document.getElementById('bd-save-status').textContent = 'Saving...';
  const dueInput = document.getElementById('bd-due');
  const changes = { title, desc, status: boardDetailStatus, due: dueInput ? dueInput.value : '' };
  if (session !== undefined) {
    changes.session = session;
    const item = boardItems.find(i => i.id === boardDetailId);
    if (item && item.session !== session) {
      const sess = sessions.find(s => s.name === session);
      changes.tags = sess ? (sess.tags || []) : [];
    }
  }
  await updateBoardItem(boardDetailId, changes);
  delete _boardDrafts[boardDetailId];
  document.getElementById('bd-save-status').textContent = 'Saved';
  setTimeout(() => {
    const el = document.getElementById('bd-save-status');
    if (el) el.textContent = '';
  }, 1500);
  const item = boardItems.find(i => i.id === boardDetailId);
  if (item) {
    const meta = document.getElementById('bd-meta');
    const parts = [];
    const tags = item.tags || [];
    if (tags.length) parts.push('Tags: ' + tags.map(t => '<span class="board-card-tag" data-tag="' + esc(t) + '">' + esc(t) + '</span>').join(' '));
    if (item.creator) parts.push('From ' + esc(item.creator));
    if (item.created) parts.push('Created ' + timeAgo(item.created));
    if (item.updated && item.updated !== item.created) parts.push('Updated ' + timeAgo(item.updated));
    if (meta) meta.innerHTML = parts.map(p => '<div class="board-detail-meta-row">' + p + '</div>').join('');
  }
}

async function boardDetailDelete() {
  if (!boardDetailId) return;
  const id = boardDetailId;
  closeBoardDetail();
  await deleteBoardItem(id);
}

function saveBoardCache() {
  lastBoardJSON = JSON.stringify(boardItems);
  localStorage.setItem('amux_board_cache', lastBoardJSON);
}

async function addBoardItem(title, desc, status, session, tags, due, ownerType) {
  ownerType = ownerType || 'human';
  const tempId = Math.random().toString(16).slice(2, 8);
  const now = Math.floor(Date.now() / 1000);
  const tempItem = { id: tempId, title, desc, status, session: session || '', tags: tags || [], due: due || '', creator: _getDeviceName(), owner_type: ownerType, created: now, updated: now, _pending: true };
  boardItems.push(tempItem);
  saveBoardCache();
  renderBoard();
  const r = await apiCall(API + '/api/board', {
    method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ title, desc, status, session: session || '', tags: tags || [], due: due || '', creator: _getDeviceName(), owner_type: ownerType })
  });
  if (r) {
    const item = await r.json();
    const idx = boardItems.findIndex(i => i.id === tempId);
    if (idx >= 0) boardItems[idx] = item;
    saveBoardCache();
    renderBoard();
  }
}

async function updateBoardItem(id, changes) {
  const idx = boardItems.findIndex(i => i.id === id);
  if (idx >= 0) { boardItems[idx] = { ...boardItems[idx], ...changes, updated: Math.floor(Date.now() / 1000) }; }
  saveBoardCache();
  renderBoard();
  const r = await apiCall(API + '/api/board/' + id, {
    method: 'PATCH', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify(changes)
  });
  if (r) {
    const updated = await r.json();
    const idx2 = boardItems.findIndex(i => i.id === id);
    if (idx2 >= 0) boardItems[idx2] = updated;
    saveBoardCache();
    renderBoard();
  }
}

async function deleteBoardItem(id) {
  boardItems = boardItems.filter(i => i.id !== id);
  saveBoardCache();
  renderBoard();
  await apiCall(API + '/api/board/' + id, { method: 'DELETE' });
}

async function moveBoardItem(id, newStatus) {
  await updateBoardItem(id, { status: newStatus });
}

async function clearDone() {
  boardItems = boardItems.filter(i => i.status !== 'done');
  saveBoardCache();
  renderBoard();
  await apiCall(API + '/api/board/clear-done', { method: 'POST' });
}

function addBoardStatus() {
  const container = document.getElementById('board-columns');
  const existing = container.querySelector('.board-col-new');
  if (existing) { existing.querySelector('.new-status-input').focus(); return; }
  const div = document.createElement('div');
  div.className = 'board-col board-col-new';
  div.innerHTML = '<div style="padding:4px;">' +
    '<input id="new-status-input" class="new-status-input" type="text" placeholder="Column name..." maxlength="30"' +
    ' onkeydown="if(event.key===\'Enter\'){event.preventDefault();saveNewBoardStatus();}if(event.key===\'Escape\')cancelNewBoardStatus();" />' +
    '<div style="display:flex;gap:4px;margin-top:6px;">' +
    '<button class="btn" style="flex:1;font-size:0.78rem;" onclick="saveNewBoardStatus()">Add</button>' +
    '<button class="btn" style="font-size:0.78rem;" onclick="cancelNewBoardStatus()">&#x2715;</button>' +
    '</div></div>';
  // Insert before the "+ Add column" button (last child is the button)
  const addColBtn = container.querySelector('.board-add-col-btn');
  if (addColBtn) container.insertBefore(div, addColBtn);
  else container.appendChild(div);
  div.querySelector('.new-status-input').focus();
}

async function saveNewBoardStatus() {
  const inp = document.getElementById('new-status-input');
  if (!inp) return;
  const label = inp.value.trim();
  if (!label) { inp.style.borderColor = 'var(--red)'; return; }
  inp.disabled = true;
  const r = await apiCall(API + '/api/board/statuses', {
    method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({label})
  });
  if (r && r.ok) {
    const status = await r.json();
    boardStatuses.push(status);
    cancelNewBoardStatus();
    renderBoard();
  } else {
    inp.disabled = false;
    inp.style.borderColor = 'var(--red)';
    inp.focus();
  }
}

function cancelNewBoardStatus() {
  const el = document.querySelector('.board-col-new');
  if (el) el.remove();
}

function toggleColCollapse(st) {
  if (_collapsedCols.has(st)) _collapsedCols.delete(st);
  else _collapsedCols.add(st);
  localStorage.setItem('amux_col_collapsed', JSON.stringify([..._collapsedCols]));
  renderBoard();
}

async function deleteBoardStatus(id) {
  const stObj = boardStatuses.find(s => s.id === id);
  const label = stObj ? stObj.label : id;
  if (!await showConfirm('Delete "' + label + '" column? Items will move to To Do.', 'Delete', true)) return;
  const r = await apiCall(API + '/api/board/statuses/' + id, { method: 'DELETE' });
  if (r && r.ok) {
    boardStatuses = boardStatuses.filter(s => s.id !== id);
    boardItems.forEach(i => { if (i.status === id) i.status = 'todo'; });
    saveBoardCache();
    renderBoard();
  }
}


// ═══════ CALENDAR ═══════
let calYear = new Date().getFullYear();
let calMonth = new Date().getMonth(); // 0-indexed
let calDay = new Date().getDate();
let calViewMode = localStorage.getItem('amux_cal_view') || 'week'; // 'month' | 'week' | 'day'

function _calNavigate(delta) {
  let d;
  if (calViewMode === 'day') {
    d = new Date(calYear, calMonth, calDay + delta);
  } else if (calViewMode === 'week') {
    d = new Date(calYear, calMonth, calDay + delta * 7);
  } else {
    calMonth += delta;
    if (calMonth < 0) { calMonth = 11; calYear--; }
    else if (calMonth > 11) { calMonth = 0; calYear++; }
    renderCalendar(); return;
  }
  calYear = d.getFullYear(); calMonth = d.getMonth(); calDay = d.getDate();
  renderCalendar();
}
function calPrev() { _calNavigate(-1); }
function calNext() { _calNavigate(1); }
function calToday() {
  const n = new Date();
  calYear = n.getFullYear(); calMonth = n.getMonth(); calDay = n.getDate();
  renderCalendar();
}
function calSetView(mode) {
  calViewMode = mode;
  localStorage.setItem('amux_cal_view', mode);
  ['month','week','day'].forEach(m => {
    const el = document.getElementById('cal-tab-' + m);
    if (el) el.classList.toggle('active', m === mode);
  });
  renderCalendar();
}
function calSelectDay(y, m, d) {
  calYear = y; calMonth = m; calDay = d;
  calSetView('day');
}

function showIcalInfo() {
  const origin = window.location.origin;
  const icalUrl = origin + '/api/calendar.ics';
  const isLocal = origin.includes('localhost') || origin.includes('127.0.0.1');
  const googleUrl = 'https://calendar.google.com/calendar/r/settings/addbyurl?' +
    'url=' + encodeURIComponent(icalUrl);
  const appleUrl = 'webcal://' + window.location.host + '/api/calendar.ics';

  let html = '<div style="font-size:0.9rem;line-height:1.7;">';
  html += '<p style="margin-bottom:0.8rem;font-weight:600;">Subscribe to amux calendar</p>';

  if (isLocal) {
    html += '<p style="margin-bottom:0.8rem;color:var(--muted);font-size:0.82rem;">⚠️ You\'re on localhost — Google and Apple Calendar can\'t reach this URL directly.</p>';
    html += '<p style="margin-bottom:0.8rem;font-size:0.82rem;">To subscribe from Google or Apple Calendar, expose the feed via <strong>Tailscale Funnel</strong> on your cloud instance:</p>';
    html += '<code style="display:block;background:var(--card-bg);padding:0.6rem 0.8rem;border-radius:6px;font-size:0.78rem;margin-bottom:0.8rem;word-break:break-all;">ssh root@amux-cloud.tail5ce8f5.ts.net "tailscale funnel --bg 8822"</code>';
    html += '<p style="font-size:0.82rem;margin-bottom:0.8rem;">Then subscribe to:<br><code style="font-size:0.8rem;">https://amux-cloud.tail5ce8f5.ts.net/api/calendar.ics</code></p>';
  } else {
    html += '<p style="margin-bottom:0.6rem;font-size:0.82rem;">Feed URL:</p>';
    html += '<code style="display:block;background:var(--card-bg);padding:0.5rem 0.8rem;border-radius:6px;font-size:0.78rem;margin-bottom:1rem;word-break:break-all;">' + ical_url_esc(icalUrl) + '</code>';
    html += '<div style="display:flex;gap:0.5rem;flex-wrap:wrap;">';
    html += '<a href="' + googleUrl + '" target="_blank" class="btn" style="font-size:0.8rem;">Open in Google Calendar</a>';
    html += '<a href="' + appleUrl + '" class="btn" style="font-size:0.8rem;">Open in Apple Calendar</a>';
    html += '</div>';
  }

  // Always offer direct download
  html += '<hr style="margin:0.9rem 0;border:none;border-top:1px solid var(--border);">';
  html += '<a href="/api/calendar.ics" download="amux.ics" class="btn" style="font-size:0.8rem;">&#x2193; Download .ics file</a>';
  html += '</div>';

  function ical_url_esc(s) { return s.replace(/&/g,'&amp;').replace(/</g,'&lt;'); }

  // Reuse the board-edit overlay for a simple modal
  const overlay = document.getElementById('board-edit-overlay');
  const inner = overlay.querySelector('.board-edit-inner') || overlay;
  // Create a temporary modal instead
  const modal = document.createElement('div');
  modal.style.cssText = 'position:fixed;inset:0;z-index:2000;display:flex;align-items:center;justify-content:center;background:rgba(0,0,0,0.55);';
  const box = document.createElement('div');
  box.style.cssText = 'background:var(--bg);border:1px solid var(--border);border-radius:12px;padding:1.4rem;max-width:420px;width:90%;box-shadow:0 8px 32px rgba(0,0,0,0.4);';
  box.innerHTML = html + '<button onclick="this.closest(\'[data-ical-modal]\').remove()" class="btn" style="margin-top:0.8rem;font-size:0.8rem;">Close</button>';
  modal.setAttribute('data-ical-modal', '1');
  modal.appendChild(box);
  modal.addEventListener('click', e => { if (e.target === modal) modal.remove(); });
  document.body.appendChild(modal);
}

const _CAL_MONTHS = ['January','February','March','April','May','June','July','August','September','October','November','December'];
const _CAL_DAYS_LONG = ['Sunday','Monday','Tuesday','Wednesday','Thursday','Friday','Saturday'];
const _CAL_DAYS_SHORT = ['Sun','Mon','Tue','Wed','Thu','Fri','Sat'];
const _CAL_DAYS_MIN = ['S','M','T','W','T','F','S'];

function _calDateStr(y, m, d) {
  return y + '-' + String(m+1).padStart(2,'0') + '-' + String(d).padStart(2,'0');
}
function _calTodayStr() {
  const t = new Date();
  return _calDateStr(t.getFullYear(), t.getMonth(), t.getDate());
}
function _calItemsByDate() {
  const map = {};
  (boardItems || []).forEach(item => {
    if (item.due && !item.deleted) {
      if (!map[item.due]) map[item.due] = [];
      map[item.due].push(item);
    }
  });
  return map;
}

function renderCalendar() {
  const titleEl = document.getElementById('cal-title');
  const bodyEl = document.getElementById('cal-body');
  if (!titleEl || !bodyEl) return;
  // Sync active tab button state
  ['month','week','day'].forEach(m => {
    const el = document.getElementById('cal-tab-' + m);
    if (el) el.classList.toggle('active', m === calViewMode);
  });
  if (calViewMode === 'week') { _renderCalWeek(titleEl, bodyEl); return; }
  if (calViewMode === 'day')  { _renderCalDay(titleEl, bodyEl); return; }
  _renderCalMonth(titleEl, bodyEl);
}

function _renderCalMonth(titleEl, bodyEl) {
  const isMob = window.innerWidth <= 480;
  const dayNames = isMob ? _CAL_DAYS_MIN : _CAL_DAYS_SHORT;
  titleEl.textContent = _CAL_MONTHS[calMonth] + ' ' + calYear;
  const todayStr = _calTodayStr();
  const firstDay = new Date(calYear, calMonth, 1).getDay();
  const daysInMonth = new Date(calYear, calMonth+1, 0).getDate();
  const daysInPrevMonth = new Date(calYear, calMonth, 0).getDate();
  const itemsByDate = _calItemsByDate();
  let html = '<div id="cal-grid" class="cal-grid">';
  dayNames.forEach(d => { html += '<div class="cal-day-header">' + d + '</div>'; });
  const totalCells = Math.ceil((firstDay + daysInMonth) / 7) * 7;
  for (let i = 0; i < totalCells; i++) {
    let day, dateStr, isOther = false, cellY = calYear, cellM = calMonth;
    if (i < firstDay) {
      day = daysInPrevMonth - firstDay + i + 1;
      cellM = calMonth === 0 ? 11 : calMonth - 1;
      cellY = calMonth === 0 ? calYear - 1 : calYear;
      isOther = true;
    } else if (i >= firstDay + daysInMonth) {
      day = i - firstDay - daysInMonth + 1;
      cellM = calMonth === 11 ? 0 : calMonth + 1;
      cellY = calMonth === 11 ? calYear + 1 : calYear;
      isOther = true;
    } else {
      day = i - firstDay + 1; cellY = calYear; cellM = calMonth;
    }
    dateStr = _calDateStr(cellY, cellM, day);
    const isToday = dateStr === todayStr;
    const items = itemsByDate[dateStr] || [];
    html += '<div class="cal-cell' + (isOther ? ' other-month' : '') + (isToday ? ' today' : '') + '"'
          + ' onclick="openBoardAdd(\'' + dateStr + '\')">';
    html += '<div class="cal-cell-num">' + day + '</div>';
    if (isMob) {
      if (items.length) {
        html += '<div class="cal-dots">';
        items.slice(0, 7).forEach(item => {
          const sty = statusStyle(item.status || 'todo');
          html += '<div class="cal-dot" style="background:' + sty.color + '" title="' + esc(item.title) + '"></div>';
        });
        html += '</div>';
      }
    } else {
      items.slice(0, 3).forEach(item => {
        const sty = statusStyle(item.status || 'todo');
        html += '<div class="cal-chip" style="background:' + sty.bg + ';color:' + sty.color + '"'
              + ' onclick="event.stopPropagation();openBoardDetail(\'' + item.id + '\')"'
              + ' title="' + esc(item.title) + '">' + esc(item.title) + '</div>';
      });
      if (items.length > 3) html += '<div class="cal-more">+' + (items.length - 3) + ' more</div>';
    }
    html += '</div>';
  }
  html += '</div>';
  bodyEl.innerHTML = html;
}

function _renderCalWeek(titleEl, bodyEl) {
  const isMob = window.innerWidth <= 480;
  // Find Sunday of current week
  const anchor = new Date(calYear, calMonth, calDay);
  const weekStart = new Date(anchor);
  weekStart.setDate(anchor.getDate() - anchor.getDay());
  const weekEnd = new Date(weekStart); weekEnd.setDate(weekStart.getDate() + 6);
  const fmtDay = d => _CAL_MONTHS[d.getMonth()].slice(0,3) + ' ' + d.getDate();
  titleEl.textContent = fmtDay(weekStart) + ' – ' + fmtDay(weekEnd) + ', ' + weekEnd.getFullYear();
  const todayStr = _calTodayStr();
  const itemsByDate = _calItemsByDate();
  const dayNames = isMob ? _CAL_DAYS_MIN : _CAL_DAYS_SHORT;
  let html = '<div class="cal-week-grid">';
  for (let i = 0; i < 7; i++) {
    const d = new Date(weekStart); d.setDate(weekStart.getDate() + i);
    const dayLabel = dayNames[d.getDay()] + (isMob ? '' : ' ' + d.getDate());
    html += '<div class="cal-week-header">' + dayLabel + '</div>';
  }
  for (let i = 0; i < 7; i++) {
    const d = new Date(weekStart); d.setDate(weekStart.getDate() + i);
    const ds = _calDateStr(d.getFullYear(), d.getMonth(), d.getDate());
    const isToday = ds === todayStr;
    const items = itemsByDate[ds] || [];
    html += '<div class="cal-week-cell' + (isToday ? ' today' : '') + '"'
          + ' onclick="openBoardAdd(\'' + ds + '\')">';
    html += '<div class="cal-week-num">' + d.getDate() + '</div>';
    items.slice(0, 4).forEach(item => {
      const sty = statusStyle(item.status || 'todo');
      html += '<div class="cal-week-chip" style="background:' + sty.bg + ';color:' + sty.color + '"'
            + ' onclick="event.stopPropagation();openBoardDetail(\'' + item.id + '\')"'
            + ' title="' + esc(item.title) + '">' + esc(item.title) + '</div>';
      html += '<div class="cal-week-dot" style="background:' + sty.color + '"></div>';
    });
    if (items.length > 4) html += '<div class="cal-week-more">+' + (items.length - 4) + '</div>';
    html += '</div>';
  }
  html += '</div>';
  bodyEl.innerHTML = html;
}

function _renderCalDay(titleEl, bodyEl) {
  const d = new Date(calYear, calMonth, calDay);
  const dayName = _CAL_DAYS_LONG[d.getDay()];
  titleEl.textContent = dayName + ', ' + _CAL_MONTHS[calMonth].slice(0,3) + ' ' + calDay + ' ' + calYear;
  const ds = _calDateStr(calYear, calMonth, calDay);
  const items = (_calItemsByDate()[ds] || []);
  let html = '<div class="cal-day-view">';
  if (!items.length) {
    html += '<div class="cal-day-empty">No issues due on this day</div>';
  } else {
    items.forEach(item => {
      const sty = statusStyle(item.status || 'todo');
      html += '<div class="cal-day-issue" onclick="openBoardDetail(\'' + item.id + '\')">';
      html += '<span class="cal-dot" style="background:' + sty.color + ';width:8px;height:8px;flex-shrink:0;border-radius:50%;margin-top:5px;"></span>';
      html += '<div class="cal-day-issue-text">';
      html += '<div class="cal-day-issue-title">' + esc(item.title) + '</div>';
      if (item.desc) html += '<div class="cal-day-issue-desc">' + esc(item.desc) + '</div>';
      html += '</div>';
      html += '<span style="font-size:0.7rem;padding:2px 7px;border-radius:10px;background:' + sty.bg + ';color:' + sty.color + ';flex-shrink:0;">' + esc(item.status || 'todo') + '</span>';
      html += '</div>';
    });
  }
  html += '<button class="cal-day-add" onclick="openBoardAdd(\'' + ds + '\')">+ Add issue for this day</button>';
  html += '</div>';
  bodyEl.innerHTML = html;
}

// ═══════ GRID MODE ═══════
let _grid = null;
let _gridPanes = {}; // session name → { widget, timer }
let _lastActivePane = null; // most recently interacted pane name

function _gpSafeId(name) {
  return 'gp-' + name.replace(/[^a-zA-Z0-9]/g, '_');
}

function enterGridMode() {
  const view = document.getElementById('grid-view');
  // Position below the sticky header so the header remains visible
  const header = document.querySelector('.header-row');
  if (header) {
    const rect = header.getBoundingClientRect();
    view.style.top = rect.bottom + 'px';
  }
  view.classList.add('active');
  // Mark Grid tab as active, deactivate others
  ['sessions','board','calendar'].forEach(t => document.getElementById('tab-' + t)?.classList.remove('active'));
  document.getElementById('tab-grid').classList.add('active');
  _renderGridChips();
  _wsRenderProfileBar();
  if (!_grid) {
    _grid = GridStack.init({
      cellHeight: 60,
      minRow: 2,
      column: 12,
      margin: 6,
      animate: true,
      draggable: { handle: '.gp-header' },
      resizable: { handles: 'e,se,s,sw,w,n,ne,nw' },
    }, '#gridstack');
    _grid.on('change', _gridSaveLayout);
    _gridRestoreLayout();
  } else {
    // Resume paused update timers for existing panes
    Object.keys(_gridPanes).forEach(name => {
      if (!_gridPanes[name].timer) {
        _gridPanes[name].timer = setInterval(() => _updateGridPane(name), 2000);
        _updateGridPane(name);
      }
    });
  }
}

function exitGridMode() {
  // Save current layout, then pause timers — keep grid alive to avoid re-init bugs
  _gridSaveLayout();
  Object.values(_gridPanes).forEach(p => { if (p.timer) { clearInterval(p.timer); p.timer = null; } });
  document.getElementById('grid-view').classList.remove('active');
  document.getElementById('tab-grid').classList.remove('active');
  document.getElementById('tab-' + (activeView || 'sessions')).classList.add('active');
}

function _renderGridChips() {
  const el = document.getElementById('grid-chips');
  if (!el) return;
  el.innerHTML = (sessions || []).map(s => {
    const on = !!_gridPanes[s.name];
    return '<button class="gp-chip' + (on ? ' on' : '') + '" onclick="toggleGridPane(\'' + s.name.replace(/\\/g,'\\\\').replace(/'/g,"\\'") + '\')">' + esc(s.name) + '</button>';
  }).join('');
}

function toggleGridPane(name) {
  if (_gridPanes[name]) removeGridPane(name);
  else addGridPane(name);
}

function addGridPane(name, x, y, w, h) {
  if (!_grid || _gridPanes[name]) return;
  const sid = _gpSafeId(name);
  const safeName = name.replace(/\\/g,'\\\\').replace(/'/g,"\\'");
  const content =
    '<div class="gp-header">' +
      '<span class="gp-dot" id="' + sid + '-dot"></span>' +
      '<span class="gp-title">' + esc(name) + '</span>' +
      '<button class="gp-close" onclick="removeGridPane(\'' + safeName + '\')">&#x2715;</button>' +
    '</div>' +
    '<div class="gp-body overlay-body" id="' + sid + '-body" onclick="_lastActivePane=\'' + safeName + '\'">Loading\u2026</div>' +
    '<div class="gp-send">' +
      '<div class="chips">' +
        '<div class="chip" onclick="gpDoKeys(\'' + safeName + '\',\'C-c\')">Ctrl-C</div>' +
        '<div class="chip" onclick="gpDoKeys(\'' + safeName + '\',\'Up\')">&#x2191;</div>' +
        '<div class="chip" onclick="gpDoKeys(\'' + safeName + '\',\'Down\')">&#x2193;</div>' +
        '<div class="chip" onclick="gpDoKeys(\'' + safeName + '\',\'Enter\')">Enter</div>' +
        '<div class="chip" onclick="gpDoKeys(\'' + safeName + '\',\'Escape\')">Esc</div>' +
        '<div class="chip" onclick="gpChipToInput(\'' + safeName + '\',\'/status\')">/status</div>' +
        '<div class="chip" onclick="gpChipToInput(\'' + safeName + '\',\'/cost\')">/cost</div>' +
        '<div class="chip danger" onclick="gpChipToInput(\'' + safeName + '\',\'/compact\')">/compact</div>' +
        '<div class="chip danger" onclick="gpChipToInput(\'' + safeName + '\',\'/clear\')">/clear</div>' +
      '</div>' +
      '<div class="send-row">' +
        '<textarea class="send-input" id="' + sid + '-input" rows="1" placeholder="Send\u2026"' +
          ' oninput="autoGrow(this);cmdHistoryReset()"' +
          ' onfocus="_lastActivePane=\'' + safeName + '\'"' +
          ' onkeydown="gpSendKeydown(\'' + safeName + '\',event)"></textarea>' +
        '<button class="btn primary" onclick="sendGridCmd(\'' + safeName + '\')">Send</button>' +
      '</div>' +
    '</div>';
  const widget = _grid.addWidget({ id: name, x, y, w: w || 6, h: h || 7, content });
  _gridPanes[name] = { widget, timer: setInterval(() => _updateGridPane(name), 2000) };
  _updateGridPane(name);
  _renderGridChips();
  _gridSaveLayout();
}

function removeGridPane(name) {
  const pane = _gridPanes[name];
  if (!pane || !_grid) return;
  clearInterval(pane.timer);
  try { _grid.removeWidget(pane.widget); } catch(e) {}
  delete _gridPanes[name];
  _renderGridChips();
  _gridSaveLayout();
}

async function _updateGridPane(name) {
  const sid = _gpSafeId(name);
  const body = document.getElementById(sid + '-body');
  const dot  = document.getElementById(sid + '-dot');
  if (!body) { removeGridPane(name); return; }
  try {
    const data = await fetch(API + '/api/sessions/' + encodeURIComponent(name) + '/peek?lines=500').then(r => r.json());
    const atBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 40;
    body.innerHTML = linkifyOutput(stripAnsi(data.output || ''));
    if (atBottom) body.scrollTop = body.scrollHeight;
    if (dot) {
      const s = (sessions || []).find(s => s.name === name);
      dot.className = 'gp-dot' + (!s || !s.running ? '' : s.status === 'working' ? ' working' : s.status === 'needs_input' ? ' waiting' : ' idle');
    }
  } catch(e) {
    if (body) { body.textContent = '(error loading output)'; }
  }
}

function _gridSaveLayout() {
  if (!_grid) return;
  try { localStorage.setItem('amux_grid_layout', JSON.stringify(_grid.save(false))); } catch(e) {}
}

function _gridRestoreLayout() {
  try {
    const saved = JSON.parse(localStorage.getItem('amux_grid_layout') || '[]');
    saved.forEach(item => {
      if (item.id && (sessions || []).find(s => s.name === item.id))
        addGridPane(item.id, item.x, item.y, item.w, item.h);
    });
  } catch(e) {}
}

// ── Workspace Profiles (device-scoped) ──
function _wsDeviceId() {
  let id = localStorage.getItem('amux_device_id');
  if (!id) {
    id = 'dev-' + Math.random().toString(36).slice(2, 10) + '-' + Date.now().toString(36);
    localStorage.setItem('amux_device_id', id);
  }
  return id;
}

function _wsProfilesKey() { return 'amux_ws_profiles_' + _wsDeviceId(); }

function _wsLoadProfiles() {
  try { return JSON.parse(localStorage.getItem(_wsProfilesKey()) || '{}'); } catch(e) { return {}; }
}

function _wsSaveProfiles(profiles) {
  localStorage.setItem(_wsProfilesKey(), JSON.stringify(profiles));
}

function wsShowSaveInput() {
  const inp = document.getElementById('ws-save-input');
  const btn = document.getElementById('ws-save-btn');
  const ok  = document.getElementById('ws-save-ok');
  inp.style.display = 'block';
  ok.style.display  = 'block';
  btn.style.display = 'none';
  inp.focus();
}

function wsHideSaveInput() {
  const inp = document.getElementById('ws-save-input');
  const btn = document.getElementById('ws-save-btn');
  const ok  = document.getElementById('ws-save-ok');
  inp.style.display = 'none';
  ok.style.display  = 'none';
  btn.style.display = 'block';
  inp.value = '';
}

function wsSaveProfileConfirm() {
  const inp = document.getElementById('ws-save-input');
  const name = inp ? inp.value.trim() : '';
  if (!name || !_grid) { wsHideSaveInput(); return; }
  const layout = _grid.save(false);
  const profiles = _wsLoadProfiles();
  profiles[name] = layout;
  _wsSaveProfiles(profiles);
  wsHideSaveInput();
  _wsRenderProfileBar();
}

function wsClearWorkspace() {
  Object.keys(_gridPanes).slice().forEach(n => removeGridPane(n));
}

function wsLoadProfile(name) {
  const profiles = _wsLoadProfiles();
  const layout = profiles[name];
  if (!layout) return;
  // Clear current panes
  Object.keys(_gridPanes).forEach(n => removeGridPane(n));
  // Load profile panes
  layout.forEach(item => {
    if (item.id && (sessions || []).find(s => s.name === item.id))
      addGridPane(item.id, item.x, item.y, item.w, item.h);
  });
  _wsRenderProfileBar();
}

function wsDeleteProfile(name) {
  const profiles = _wsLoadProfiles();
  delete profiles[name];
  _wsSaveProfiles(profiles);
  _wsRenderProfileBar();
}

function _wsRenderProfileBar() {
  const el = document.getElementById('ws-profile-bar');
  if (!el) return;
  const profiles = _wsLoadProfiles();
  const names = Object.keys(profiles);
  if (!names.length) {
    el.innerHTML = '<span style="font-size:0.7rem;color:var(--dim);opacity:0.5;">No saved profiles</span>';
    return;
  }
  el.innerHTML = names.map(n => {
    const safe = n.replace(/'/g, "\\'");
    return `<span class="ws-profile-chip" onclick="wsLoadProfile('${safe}')">${esc(n)
      }<button class="ws-profile-del" onclick="event.stopPropagation();wsDeleteProfile('${safe}')" title="Delete">&#x2715;</button></span>`;
  }).join('');
}

function gpDoKeys(name, keys) { doKeys(name, keys); }

function gpChipToInput(name, text) {
  const inp = document.getElementById(_gpSafeId(name) + '-input');
  if (!inp) return;
  inp.value = text;
  inp.focus({ preventScroll: true });
  autoGrow(inp);
}

function gpSendKeydown(name, e) {
  const inp = document.getElementById(_gpSafeId(name) + '-input');
  if (e.key === 'Enter' && !e.shiftKey) {
    sendGridCmd(name); e.preventDefault();
  } else if ((e.ctrlKey || e.metaKey) && e.key === 'c' && !e.altKey && !e.shiftKey) {
    const hasSelection = inp && inp.selectionStart !== inp.selectionEnd;
    if (!hasSelection) { e.preventDefault(); gpDoKeys(name, 'C-c'); }
  } else if (e.key === 'ArrowUp' && inp && inp.value.indexOf('\n') === -1) {
    e.preventDefault(); cmdHistoryUp(inp);
  } else if (e.key === 'ArrowDown' && _cmdHistoryIdx !== -1) {
    e.preventDefault(); cmdHistoryDown(inp);
  }
}

async function sendGridCmd(name) {
  const sid = _gpSafeId(name);
  const inp = document.getElementById(sid + '-input');
  if (!inp) return;
  const text = inp.value.trim();
  if (!text) return;
  cmdHistoryAdd(text);
  inp.value = '';
  autoGrow(inp);
  await doSend(name, text);
  inp.style.borderColor = 'var(--green)';
  setTimeout(() => { inp.style.borderColor = ''; }, 400);
  setTimeout(() => _updateGridPane(name), 500);
}

// ═══════ INIT ═══════
// Load cached sessions immediately so offline startup renders content
const _cachedInit = localStorage.getItem('amux_sessions_cache');
if (_cachedInit) {
  try { sessions = JSON.parse(_cachedInit); } catch(e) {}
}
// Load cached board from localStorage (fast, synchronous)
const _cachedBoard = localStorage.getItem('amux_board_cache');
if (_cachedBoard) {
  try { boardItems = JSON.parse(_cachedBoard); lastBoardJSON = _cachedBoard; } catch(e) {}
}
if (sessions.length || drafts.length) render();
updateConnectionStatus();

// IndexedDB — v2: kv store + full issues/statuses mirror for offline sync
// Declared here (before first use) to avoid TDZ errors
const _idb = (() => {
  let db = null;
  const open = () => new Promise((resolve, reject) => {
    if (db) return resolve(db);
    const req = indexedDB.open('amux', 2);
    req.onupgradeneeded = () => {
      const d = req.result;
      if (!d.objectStoreNames.contains('kv')) d.createObjectStore('kv');
      if (!d.objectStoreNames.contains('issues')) {
        const s = d.createObjectStore('issues', { keyPath: 'id' });
        s.createIndex('by_updated', 'updated');
        s.createIndex('by_session', 'session');
        s.createIndex('by_status', 'status');
      }
      if (!d.objectStoreNames.contains('statuses')) {
        d.createObjectStore('statuses', { keyPath: 'id' });
      }
    };
    req.onsuccess = () => { db = req.result; resolve(db); };
    req.onerror = () => reject(req.error);
  });
  const _txw = (store, fn) => open().then(d => new Promise((resolve, reject) => {
    const tx = d.transaction(store, 'readwrite');
    tx.oncomplete = resolve; tx.onerror = () => reject(tx.error);
    fn(tx.objectStore(store));
  })).catch(() => {});
  return {
    set: (key, val) => open().then(d => {
      const tx = d.transaction('kv', 'readwrite');
      tx.objectStore('kv').put(val, key);
    }).catch(() => {}),
    get: (key) => open().then(d => new Promise((resolve) => {
      const tx = d.transaction('kv', 'readonly');
      const req = tx.objectStore('kv').get(key);
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => resolve(null);
    })).catch(() => null),
    // Apply delta: upsert live items, remove soft-deleted ones from local mirror
    applyIssueDelta: (issues) => _txw('issues', os => {
      issues.forEach(item => { if (item.deleted) os.delete(item.id); else os.put(item); });
    }),
    // Replace all statuses in local mirror
    putStatuses: (statuses) => _txw('statuses', os => {
      os.clear(); statuses.forEach(s => os.put(s));
    }),
    // Read all items from a store
    getAll: (store) => open().then(d => new Promise((resolve) => {
      const tx = d.transaction(store, 'readonly');
      const req = tx.objectStore(store).getAll();
      req.onsuccess = () => resolve(req.result || []);
      req.onerror = () => resolve([]);
    })).catch(() => []),
  };
})();

// IDB fallback: if localStorage was purged (iOS), restore from IndexedDB
if (!boardItems.length) {
  _idb.getAll('issues').then(items => {
    if (items && items.length) {
      boardItems = items.filter(i => !i.deleted);
      const j = JSON.stringify(boardItems);
      lastBoardJSON = j;
      localStorage.setItem('amux_board_cache', j);
      if (activeView === 'board') renderBoard();
      else if (activeView === 'calendar') renderCalendar();
    }
  });
}

// Delta sync: call /api/sync?since=last_sync_ts on startup to catch any missed updates
// Runs after queue replay so server has our writes before we read
async function _runDeltaSync() {
  try {
    const since = (await _idb.get('last_sync_ts')) || 0;
    const r = await fetch(API + '/api/sync?since=' + since);
    if (!r.ok) return;
    const data = await r.json();
    if (data.issues && data.issues.length) {
      // Apply delta to in-memory boardItems
      data.issues.forEach(item => {
        const idx = boardItems.findIndex(i => i.id === item.id);
        if (item.deleted) {
          if (idx >= 0) boardItems.splice(idx, 1);
        } else {
          if (idx >= 0) boardItems[idx] = item;
          else boardItems.push(item);
        }
      });
      const j = JSON.stringify(boardItems);
      lastBoardJSON = j;
      localStorage.setItem('amux_board_cache', j);
      _idb.applyIssueDelta(data.issues);
    }
    if (data.statuses && data.statuses.length) {
      boardStatuses = data.statuses;
      lastStatusesJSON = JSON.stringify(data.statuses);
      _idb.putStatuses(data.statuses);
    }
    if (data.ts) _idb.set('last_sync_ts', data.ts);
    if (activeView === 'board') renderBoard();
    else if (activeView === 'calendar') renderCalendar();
    _dbgLog('Delta sync: +' + (data.issues || []).length + ' issue changes');
  } catch(e) {
    _dbgLog('Delta sync failed: ' + e.message);
  }
}
// Run delta sync shortly after startup (after queue replay window)
setTimeout(_runDeltaSync, 2500);
(function() {
  function _syncTabTop() {
    const h = document.querySelector('.header-row');
    const t = document.querySelector('.tab-bar');
    if (h && t) t.style.top = h.offsetHeight + 'px';
  }
  _syncTabTop();
  window.addEventListener('resize', _syncTabTop);
})();

// ═══════ SSE — real-time push updates ═══════
let _sse = null;
let _sseRetries = 0;
let _sseFallback = false;
let _pollTimer = null;

function connectSSE() {
  if (_sseFallback || _sse) return;
  _sse = new EventSource(API + '/api/events');

  _sse.onmessage = function(e) {
    const wasOffline = !_liveSSE;
    _sseRetries = 0;
    _lastDataTime = Date.now();
    if (_initialLoad) { _initialLoad = false; render(); }
    if (!_liveSSE) { _liveSSE = true; updateConnectionStatus(); }
    if (!online) setOnline(true);
    // On reconnect after being offline: run delta sync to catch any missed changes
    if (wasOffline && _sseRetries > 0) setTimeout(_runDeltaSync, 500);
    try {
      const msg = JSON.parse(e.data);
      if (msg.type === 'sessions') {
        const j = JSON.stringify(msg.payload);
        if (j !== lastSessionsJSON) {
          lastSessionsJSON = j;
          sessions = msg.payload;
          localStorage.setItem('amux_sessions_cache', j);
          render();
        }
      } else if (msg.type === 'board') {
        const j = JSON.stringify(msg.payload);
        if (j !== lastBoardJSON) {
          lastBoardJSON = j;
          boardItems = msg.payload;
          localStorage.setItem('amux_board_cache', j);
          // Mirror to IDB for full offline durability (iOS-safe)
          _idb.applyIssueDelta(msg.payload);
          _idb.set('last_sync_ts', Math.floor(Date.now() / 1000));
          if (activeView === 'board') renderBoard();
          else if (activeView === 'calendar') renderCalendar();
        }
      }
    } catch(err) { console.error('SSE parse:', err); }
  };

  _sse.onerror = function() {
    _sseRetries++;
    _liveSSE = false;
    _sse.close();
    _sse = null;
    _dbgLog('SSE error (retry ' + _sseRetries + ')');
    updateConnectionStatus();
    if (_sseRetries >= 3) {
      _dbgLog('SSE failed — switching to polling');
      enablePollingFallback();
    } else {
      setTimeout(connectSSE, 2000 * _sseRetries);
    }
  };
}

function enablePollingFallback() {
  _sseFallback = true;
  if (_sse) { _sse.close(); _sse = null; }
  fetchSessions();
  if (!_pollTimer) _pollTimer = setInterval(fetchSessions, 5000);
}

// Start SSE (falls back to polling on failure)
connectSSE();

// Register service worker for offline asset caching
if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js').then(reg => {
    // Store full page HTML in localStorage as fallback if iOS evicts SW cache
    if (navigator.onLine !== false) {
      fetch('/').then(r => r.text()).then(html => {
        localStorage.setItem('amux_app_html', html);
        // Also ensure SW cache has it (in case cache was evicted but SW survived)
        if (reg.active) reg.active.postMessage({ type: 'CACHE_HTML', html });
      }).catch(() => {});
    }
    // Listen for Background Sync completion messages from SW
    navigator.serviceWorker.addEventListener('message', function(e) {
      if (e.data && e.data.type === 'SYNC_COMPLETE') {
        const { replayed, remaining } = e.data;
        if (replayed > 0) showToast('Synced ' + replayed + ' queued op' + (replayed > 1 ? 's' : ''));
        // Reload queue from IDB
        _idb.get('offline_queue').then(val => {
          offlineQueue = val || [];
          saveQueue();
          updateConnectionStatus();
        });
        // Refresh data and reconnect SSE
        fetchSessions();
        fetchBoard();
        if (!_sseFallback && !_sse) { _sseRetries = 0; connectSSE(); }
      }
    });
  // Auto-reload when a new SW takes control (ensures fresh HTML after update)
  navigator.serviceWorker.addEventListener('controllerchange', () => {
    location.reload();
  });
  }).catch(() => {});
}

// Dual-write drafts and queue to both localStorage and IndexedDB
function persistOfflineData() {
  localStorage.setItem('amux_drafts', JSON.stringify(drafts));
  saveQueue();
  _idb.set('drafts', drafts);
  _idb.set('offline_queue', offlineQueue);
}

// On startup, restore from IndexedDB if localStorage is empty (iOS purge recovery)
_idb.get('drafts').then(val => {
  if (val && !drafts.length && val.length) {
    drafts = val;
    saveDrafts();
    render();
  }
});
_idb.get('offline_queue').then(val => {
  if (val && !offlineQueue.length && val.length) {
    offlineQueue = val;
    saveQueue();
    updateConnectionStatus();
  }
  // On startup, register Background Sync if queue is non-empty
  if ((offlineQueue.length || (val && val.length)) && 'serviceWorker' in navigator && 'SyncManager' in window) {
    navigator.serviceWorker.ready.then(r => r.sync.register('replay-queue').catch(() => {}));
  }
  // Auto-retry queued ops on startup if online (fallback when BackgroundSync isn't available)
  if (offlineQueue.length || (val && val.length)) {
    setTimeout(() => {
      if (online && navigator.onLine !== false && (offlineQueue.length || drafts.length)) {
        runSyncBanner();
      }
    }, 4000);
  }
});

function fmtTokens(n) {
  if (n >= 1000000) return (n / 1000000).toFixed(1) + 'M';
  if (n >= 1000) return (n / 1000).toFixed(1) + 'k';
  return String(n);
}

function _dbgLog(msg) {
  _debugLog.unshift('[' + new Date().toLocaleTimeString() + '] ' + msg);
  if (_debugLog.length > 12) _debugLog.length = 12;
}

function _timeSince(ts) {
  if (!ts) return 'never';
  const s = Math.floor((Date.now() - ts) / 1000);
  if (s < 60) return s + 's ago';
  if (s < 3600) return Math.floor(s/60) + 'm ago';
  return Math.floor(s/3600) + 'h ago';
}

function renderDebugInfo() {
  const el = document.getElementById('debug-info');
  if (!el) return;
  const mode = !online ? '🔴 Offline' : _liveSSE ? '🟢 Live (SSE)' : '🟡 Polling (5s)';
  const rows = [
    ['Mode', mode],
    ['Last data', _timeSince(_lastDataTime)],
    ['Server', location.host],
    ['Sessions', sessions.length + ' loaded'],
    ['SSE retries', _sseRetries],
    ['Queue', offlineQueue.length + ' pending'],
  ];
  let html = rows.map(([k,v]) =>
    '<div style="display:flex;justify-content:space-between;gap:8px;">' +
    '<span style="color:var(--dim)">' + k + '</span>' +
    '<span style="color:var(--text);text-align:right">' + v + '</span></div>'
  ).join('');
  if (_debugLog.length) {
    html += '<div style="margin-top:6px;border-top:1px solid var(--border);padding-top:4px;color:var(--dim)">' +
      _debugLog.slice(0, 5).join('<br>') + '</div>';
  }
  el.innerHTML = html;
}

async function pingServer() {
  const btn = event.target;
  btn.textContent = '…';
  btn.disabled = true;
  const t0 = Date.now();
  try {
    await fetch(API + '/api/sessions');
    const ms = Date.now() - t0;
    _dbgLog('Ping OK ' + ms + 'ms');
    showToast('Server OK — ' + ms + 'ms');
  } catch(e) {
    _dbgLog('Ping FAILED: ' + e.message);
    showToast('Ping failed: ' + e.message);
  }
  btn.textContent = 'Ping';
  btn.disabled = false;
  renderDebugInfo();
}

async function openSkills() {
  const modal = document.getElementById('skills-modal');
  const list = document.getElementById('skills-list');
  modal.classList.add('active');
  list.innerHTML = '<div style="color:var(--dim);font-size:0.85rem;text-align:center;padding:20px;">Loading...</div>';
  try {
    const skills = await fetch(API + '/api/skills').then(r => r.json());
    if (!skills.length) {
      list.innerHTML = '<div style="color:var(--dim);font-size:0.85rem;text-align:center;padding:20px;">No skills found in ~/.claude/commands/</div>';
      return;
    }
    list.innerHTML = skills.map(s => {
      const cmd = '/' + esc(s.name);
      const hint = s.hint ? '<div class="skill-card-hint">' + esc(s.hint) + '</div>' : '';
      return '<div class="skill-card">' +
        '<div style="display:flex;align-items:center;justify-content:space-between;">' +
          '<span class="skill-card-name">' + cmd + '</span>' +
          '<button class="btn" style="font-size:0.65rem;padding:2px 8px;" onclick="navigator.clipboard.writeText(\'' + esc(s.name) + '\');showToast(\'Copied!\')">Copy</button>' +
        '</div>' +
        (s.description ? '<div class="skill-card-desc">' + esc(s.description) + '</div>' : '') +
        hint +
      '</div>';
    }).join('');
  } catch(e) {
    list.innerHTML = '<div style="color:var(--red);font-size:0.85rem;text-align:center;padding:20px;">Failed to load skills</div>';
  }
}
function closeSkills() {
  document.getElementById('skills-modal').classList.remove('active');
}

function openAbout() {
  document.getElementById('about-overlay').classList.add('active');
  document.getElementById('add-server-form').style.display = 'none';
  renderServerList();
  renderDebugInfo();
  const el = document.getElementById('daily-stats');
  el.innerHTML = '<div style="color:var(--dim);font-size:0.75rem;text-align:center;">Loading...</div>';
  fetch(API + '/api/stats/daily').then(r => r.json()).then(data => {
    let html = '<div style="font-size:0.8rem;font-weight:600;margin-bottom:8px;">Today\'s Tokens</div>';
    html += '<div style="display:flex;justify-content:space-between;font-size:0.85rem;margin-bottom:6px;">';
    html += '<span>amux sessions</span><span style="font-weight:600;">' + fmtTokens(data.amux_tokens) + '</span></div>';
    html += '<div style="display:flex;justify-content:space-between;font-size:0.85rem;margin-bottom:10px;padding-bottom:8px;border-bottom:1px solid var(--border);">';
    html += '<span>All Claude Code</span><span style="font-weight:600;">' + fmtTokens(data.total_tokens) + '</span></div>';
    if (data.sessions && data.sessions.length) {
      html += '<div style="font-size:0.7rem;color:var(--dim);margin-bottom:4px;">Breakdown</div>';
      data.sessions.forEach(s => {
        const bar = s.total / data.total_tokens * 100;
        html += '<div style="margin-bottom:4px;">';
        html += '<div style="display:flex;justify-content:space-between;font-size:0.75rem;">';
        html += '<span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:200px;">' + (s.amux ? '' : '<span style=color:var(--dim)>') + esc(s.name) + (s.amux ? '' : '</span>') + '</span>';
        html += '<span style="flex-shrink:0;margin-left:8px;">' + fmtTokens(s.total) + '</span></div>';
        html += '<div style="height:3px;border-radius:2px;background:var(--border);margin-top:2px;">';
        html += '<div style="height:100%;border-radius:2px;background:' + (s.amux ? 'var(--accent)' : 'var(--dim)') + ';width:' + bar.toFixed(1) + '%;"></div></div></div>';
      });
    } else {
      html += '<div style="color:var(--dim);font-size:0.75rem;text-align:center;">No usage today</div>';
    }
    html += '<div style="text-align:center;margin-top:10px;"><button class="btn" style="font-size:0.7rem;padding:3px 10px;" onclick="resetTokenStats()">Reset counters</button></div>';
    el.innerHTML = html;
  }).catch(() => {
    el.innerHTML = '<div style="color:var(--dim);font-size:0.75rem;text-align:center;">Offline — stats unavailable</div>';
  });
}

async function resetTokenStats() {
  if (!await showConfirm('Reset token counters to zero?', 'Reset', true)) return;
  fetch(API + '/api/stats/reset', { method: 'POST' }).then(r => r.json()).then(() => {
    openAbout();
  }).catch(() => showToast('Reset failed'));
}

// ═══════ SERVER SWITCHER ═══════
function _getSavedServers() {
  try { return JSON.parse(localStorage.getItem('amux_servers') || '[]'); } catch(e) { return []; }
}
function _saveServers(list) { localStorage.setItem('amux_servers', JSON.stringify(list)); }

function renderServerList() {
  const list = document.getElementById('server-list');
  const servers = _getSavedServers();
  const current = location.origin;
  let html = '';
  // Current server always shown first
  html += '<div style="display:flex;align-items:center;justify-content:space-between;padding:6px 8px;border-radius:6px;background:rgba(88,166,255,0.08);margin-bottom:4px;">';
  html += '<div style="min-width:0;flex:1;">';
  html += '<div style="font-size:0.75rem;font-weight:600;color:var(--accent);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">' + esc(location.host) + '</div>';
  html += '<div style="font-size:0.65rem;color:var(--dim);">current</div>';
  html += '</div></div>';
  servers.forEach((s, i) => {
    const isCurrent = s.url.replace(/\/+$/, '') === current;
    if (isCurrent) return;  // skip — already shown above
    html += '<div style="display:flex;align-items:center;justify-content:space-between;padding:6px 8px;border-radius:6px;margin-bottom:4px;cursor:pointer;transition:background 0.12s;" onmouseenter="this.style.background=\'rgba(255,255,255,0.04)\'" onmouseleave="this.style.background=\'none\'" onclick="switchServer(' + i + ')">';
    html += '<div style="min-width:0;flex:1;">';
    html += '<div style="font-size:0.75rem;font-weight:500;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">' + esc(s.name || s.url) + '</div>';
    if (s.name) html += '<div style="font-size:0.65rem;color:var(--dim);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">' + esc(s.url.replace(/^https?:\/\//, '')) + '</div>';
    html += '</div>';
    html += '<button class="btn" style="font-size:0.6rem;padding:1px 6px;flex-shrink:0;margin-left:8px;" onclick="event.stopPropagation();removeServer(' + i + ')">&#x2715;</button>';
    html += '</div>';
  });
  if (!servers.length || servers.every(s => s.url.replace(/\/+$/, '') === current)) {
    html += '<div style="color:var(--dim);font-size:0.7rem;text-align:center;padding:4px 0;">No other servers saved</div>';
  }
  list.innerHTML = html;
}

function toggleAddServer() {
  const form = document.getElementById('add-server-form');
  const visible = form.style.display !== 'none';
  form.style.display = visible ? 'none' : 'block';
  if (!visible) {
    document.getElementById('add-server-name').value = '';
    document.getElementById('add-server-url').value = '';
    setTimeout(() => document.getElementById('add-server-name').focus(), 50);
  }
}

function saveNewServer() {
  const name = document.getElementById('add-server-name').value.trim();
  let url = document.getElementById('add-server-url').value.trim();
  if (!url) { showToast('URL is required'); return; }
  // Normalize: ensure protocol, strip trailing slash
  if (!/^https?:\/\//.test(url)) url = 'https://' + url;
  url = url.replace(/\/+$/, '');
  const servers = _getSavedServers();
  // Dedupe by URL
  if (servers.some(s => s.url.replace(/\/+$/, '') === url)) {
    showToast('Server already saved');
    return;
  }
  servers.push({ name: name || '', url });
  _saveServers(servers);
  document.getElementById('add-server-form').style.display = 'none';
  renderServerList();
  showToast('Server saved');
}

function removeServer(idx) {
  const servers = _getSavedServers();
  servers.splice(idx, 1);
  _saveServers(servers);
  renderServerList();
}

function switchServer(idx) {
  const servers = _getSavedServers();
  const s = servers[idx];
  if (s) location.href = s.url;
}

// ═══════ SETTINGS DROPDOWN ═══════
function toggleSettings() {
  const menu = document.getElementById('settings-menu');
  const open = menu.classList.toggle('open');
  if (open) {
    // Show effective device name and populate override input
    const effective = _getDeviceName();
    const custom = localStorage.getItem('amux_device_name') || '';
    document.getElementById('settings-device-current').textContent = effective;
    const inp = document.getElementById('settings-device-name');
    inp.value = custom;
    // Auto-detected name as placeholder so user knows what they'd override
    const ua = navigator.userAgent;
    let auto = 'Unknown';
    if (/iPhone/.test(ua)) auto = 'iPhone';
    else if (/iPad/.test(ua)) auto = 'iPad';
    else if (/Android/.test(ua)) auto = 'Android';
    else if (/Windows/.test(ua)) auto = 'Windows';
    else if (/Mac/.test(ua)) auto = 'Mac';
    else if (/Linux/.test(ua)) auto = 'Linux';
    inp.placeholder = custom ? 'Override (' + auto + ')' : auto + ' (auto-detected)';
    // Render servers
    renderSettingsServerList();
    // Close add form
    document.getElementById('settings-add-server').style.display = 'none';
  }
}

function closeSettings() {
  document.getElementById('settings-menu').classList.remove('open');
}

function saveDeviceName(val) {
  val = val.trim();
  if (val) {
    localStorage.setItem('amux_device_name', val);
  } else {
    localStorage.removeItem('amux_device_name');
  }
  // Update displayed name immediately
  const el = document.getElementById('settings-device-current');
  if (el) el.textContent = _getDeviceName();
}

function renderSettingsServerList() {
  const el = document.getElementById('settings-server-list');
  const servers = _getSavedServers();
  const current = location.origin;
  let html = '';
  // Current server
  html += '<div class="settings-server-item settings-server-current">';
  html += '<div style="min-width:0;flex:1;">';
  html += '<div class="settings-server-name" style="color:var(--accent);">' + esc(location.host) + '</div>';
  html += '</div>';
  html += '<span class="settings-server-badge">current</span>';
  html += '</div>';
  servers.forEach((s, i) => {
    if (s.url.replace(/\/+$/, '') === current) return;
    html += '<div class="settings-server-item" onclick="switchServer(' + i + ')">';
    html += '<div style="min-width:0;flex:1;">';
    html += '<div class="settings-server-name">' + esc(s.name || s.url.replace(/^https?:\/\//, '')) + '</div>';
    if (s.name) html += '<div class="settings-server-url">' + esc(s.url.replace(/^https?:\/\//, '')) + '</div>';
    html += '</div>';
    html += '<button class="btn" style="font-size:0.55rem;padding:1px 5px;flex-shrink:0;margin-left:6px;" onclick="event.stopPropagation();removeServer(' + i + ');renderSettingsServerList()">&#x2715;</button>';
    html += '</div>';
  });
  if (!servers.length || servers.every(s => s.url.replace(/\/+$/, '') === current)) {
    html += '<div style="color:var(--dim);font-size:0.68rem;text-align:center;padding:4px 0;">No other servers</div>';
  }
  el.innerHTML = html;
}

function toggleSettingsAddServer() {
  const form = document.getElementById('settings-add-server');
  const visible = form.style.display !== 'none';
  form.style.display = visible ? 'none' : 'block';
  if (!visible) {
    document.getElementById('settings-new-server-name').value = '';
    document.getElementById('settings-new-server-url').value = '';
    setTimeout(() => document.getElementById('settings-new-server-name').focus(), 50);
  }
}

function saveSettingsNewServer() {
  const name = document.getElementById('settings-new-server-name').value.trim();
  let url = document.getElementById('settings-new-server-url').value.trim();
  if (!url) { showToast('URL is required'); return; }
  if (!/^https?:\/\//.test(url)) url = 'https://' + url;
  url = url.replace(/\/+$/, '');
  const servers = _getSavedServers();
  if (servers.some(s => s.url.replace(/\/+$/, '') === url)) {
    showToast('Server already saved');
    return;
  }
  servers.push({ name: name || '', url });
  _saveServers(servers);
  document.getElementById('settings-add-server').style.display = 'none';
  renderSettingsServerList();
  showToast('Server saved');
}

// Close settings on outside click
document.addEventListener('click', function(e) {
  const wrap = document.querySelector('.settings-wrap');
  if (wrap && !wrap.contains(e.target)) closeSettings();
});

function forceUpdate() {
  const el = document.getElementById('update-status');
  el.textContent = 'Updating...';
  if (!('serviceWorker' in navigator)) {
    location.reload(true);
    return;
  }
  navigator.serviceWorker.getRegistration().then(reg => {
    if (!reg) { location.reload(true); return; }
    reg.update().then(() => {
      if (reg.waiting) {
        reg.waiting.postMessage({type: 'SKIP_WAITING'});
      }
      caches.keys().then(keys => Promise.all(keys.map(k => caches.delete(k)))).then(() => {
        el.textContent = 'Cache cleared, reloading...';
        setTimeout(() => location.reload(true), 300);
      });
    }).catch(() => {
      el.textContent = 'Update failed, reloading...';
      setTimeout(() => location.reload(true), 300);
    });
  });
}

// ── DevTools Panel ──────────────────────────────────────────────
(function() {
  const MAX_LOG = 500;
  const MAX_NET = 200;
  let _dtLogs = [];
  let _dtNet  = [];
  let _dtOpen = false;
  let _dtTab  = 'console';
  let _dtReplHist = [];
  let _dtReplIdx  = -1;

  // ── Console override ──
  const _origConsole = {};
  ['log','info','warn','error','debug'].forEach(level => {
    _origConsole[level] = console[level].bind(console);
    console[level] = function(...args) {
      _origConsole[level](...args);
      const text = args.map(a => {
        try { return (typeof a === 'object' && a !== null) ? JSON.stringify(a, null, 2) : String(a); }
        catch(e) { return String(a); }
      }).join(' ');
      _dtLogPush(level === 'debug' ? 'log' : level, text);
    };
  });

  // ── Fetch interception ──
  const _origFetch = window.fetch.bind(window);
  window.fetch = async function(input, init) {
    const method = (init && init.method) ? init.method.toUpperCase() : 'GET';
    const url = typeof input === 'string' ? input : (input.url || String(input));
    const t0 = Date.now();
    let entry = { method, url, status: '…', ms: null, t: new Date().toLocaleTimeString() };
    _dtNet.push(entry);
    if (_dtNet.length > MAX_NET) _dtNet = _dtNet.slice(-MAX_NET);
    if (_dtOpen && _dtTab === 'network') _renderDevNetwork();
    try {
      const res = await _origFetch(input, init);
      entry.status = res.status;
      entry.ms = Date.now() - t0;
      if (_dtOpen && _dtTab === 'network') _renderDevNetwork();
      return res;
    } catch(e) {
      entry.status = 'ERR';
      entry.ms = Date.now() - t0;
      if (_dtOpen && _dtTab === 'network') _renderDevNetwork();
      throw e;
    }
  };

  // ── Clipboard event debug ──
  ['copy','cut','paste'].forEach(evtName => {
    document.addEventListener(evtName, e => {
      const ae = document.activeElement;
      const aeDesc = ae ? `${ae.tagName}#${ae.id || ''}${ae.className ? '.'+ae.className.split(' ')[0] : ''}` : 'none';
      let extra = '';
      if (evtName === 'paste' && e.clipboardData) {
        extra = ' types=[' + Array.from(e.clipboardData.types).join(',') + ']';
      }
      _dtLogPush('info', `[clipboard] ${evtName}${extra} — target=${e.target && e.target.tagName} activeEl=${aeDesc}`);
    }, true); // capture phase
  });

  // ── Keyboard debug (Cmd+C/V/X) — capture AND bubble phase ──
  const _watchKeys = new Set(['c','v','x','z']);
  document.addEventListener('keydown', e => {
    if ((e.metaKey || e.ctrlKey) && _watchKeys.has(e.key.toLowerCase())) {
      const ae = document.activeElement;
      const aeDesc = ae ? `${ae.tagName}#${ae.id || ''}` : 'none';
      const isEditable = ae && (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA' || ae.isContentEditable);
      _dtLogPush('log', `[key:capture] ${e.metaKey?'Cmd':'Ctrl'}+${e.key} focus=${aeDesc} editable=${isEditable} defaultPrevented=${e.defaultPrevented}`);
      // schedule a microtask to log after handlers run
      Promise.resolve().then(() => {
        _dtLogPush('log', `[key:after] ${e.metaKey?'Cmd':'Ctrl'}+${e.key} defaultPrevented=${e.defaultPrevented}`);
      });
    }
  }, true);

  // ── Internal log push ──
  function _dtLogPush(level, text) {
    _dtLogs.push({ level, text, t: new Date().toLocaleTimeString() });
    if (_dtLogs.length > MAX_LOG) _dtLogs = _dtLogs.slice(-MAX_LOG);
    if (_dtOpen && _dtTab === 'console') _renderDevLogs();
  }

  // ── Render helpers ──
  function _renderDevLogs() {
    const el = document.getElementById('dt-log-area');
    if (!el) return;
    const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 10;
    el.innerHTML = _dtLogs.map(e => {
      const cls = e.level === 'error' ? 'error' : e.level === 'warn' ? 'warn' : e.level === 'info' ? 'info' : '';
      const escaped = e.text.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
      return `<div class="dt-entry ${cls}"><span class="dt-ts">${e.t}</span> ${escaped}</div>`;
    }).join('');
    if (atBottom) el.scrollTop = el.scrollHeight;
  }

  function _renderDevNetwork() {
    const el = document.getElementById('dt-net-area');
    if (!el) return;
    const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 10;
    el.innerHTML = _dtNet.slice().reverse().map(e => {
      const statusCls = (e.status >= 400 || e.status === 'ERR') ? 'error' : (e.status === '…') ? 'pending' : 'ok';
      const shortUrl = e.url.replace(/^https?:\/\/[^/]+/, '');
      return `<div class="dt-net-entry">` +
        `<span class="dt-net-method">${e.method}</span>` +
        `<span class="dt-net-status ${statusCls}">${e.status}</span>` +
        `<span class="dt-net-url" title="${e.url}">${shortUrl}</span>` +
        `<span class="dt-net-ms">${e.ms !== null ? e.ms+'ms' : ''}</span>` +
        `<span class="dt-ts">${e.t}</span>` +
      `</div>`;
    }).join('');
    if (atBottom) el.scrollTop = el.scrollHeight;
  }

  function _renderDevInfo() {
    const el = document.getElementById('dt-info-area');
    if (!el) return;
    const sw = navigator.serviceWorker;
    const rows = [
      ['Standalone (PWA)', navigator.standalone !== undefined ? String(navigator.standalone) : (window.matchMedia('(display-mode: standalone)').matches ? 'true (display-mode)' : 'false')],
      ['User Agent', navigator.userAgent],
      ['Platform', navigator.platform || navigator.userAgentData?.platform || '?'],
      ['Viewport', `${window.innerWidth}×${window.innerHeight} (dpr=${devicePixelRatio})`],
      ['Online', String(navigator.onLine)],
      ['SW supported', String('serviceWorker' in navigator)],
      ['SW controller', sw && sw.controller ? sw.controller.scriptURL : 'none'],
      ['SW state', sw && sw.controller ? sw.controller.state : 'n/a'],
      ['IndexedDB', String('indexedDB' in window)],
      ['Clipboard API', String('clipboard' in navigator)],
      ['clipboard.read', String(!!(navigator.clipboard && navigator.clipboard.read))],
      ['clipboard.write', String(!!(navigator.clipboard && navigator.clipboard.write))],
      ['Permissions API', String('permissions' in navigator)],
      ['amux version', (document.querySelector('meta[name="amux-version"]') || {content:'?'}).content],
    ];
    el.innerHTML = rows.map(([k,v]) =>
      `<div class="dt-info-row"><span class="dt-info-key">${k}</span><span class="dt-info-val">${v}</span></div>`
    ).join('');
  }

  // ── REPL history ──
  window.dtReplHistUp = function(inp) {
    if (!_dtReplHist.length) return;
    if (_dtReplIdx === -1) _dtReplIdx = _dtReplHist.length - 1;
    else if (_dtReplIdx > 0) _dtReplIdx--;
    inp.value = _dtReplHist[_dtReplIdx];
  };
  window.dtReplHistDown = function(inp) {
    if (_dtReplIdx === -1) return;
    if (_dtReplIdx < _dtReplHist.length - 1) { _dtReplIdx++; inp.value = _dtReplHist[_dtReplIdx]; }
    else { _dtReplIdx = -1; inp.value = ''; }
  };

  // ── Public API ──
  window.openDevtools = function() {
    _dtOpen = true;
    const panel = document.getElementById('devtools-panel');
    if (panel) panel.classList.add('open');
    dtSwitchTab(_dtTab);
  };

  window.closeDevtools = function() {
    _dtOpen = false;
    const panel = document.getElementById('devtools-panel');
    if (panel) panel.classList.remove('open');
  };

  window.dtSwitchTab = function(tab) {
    _dtTab = tab;
    document.querySelectorAll('.dt-tab').forEach(b => b.classList.toggle('active', b.textContent.toLowerCase() === tab));
    document.querySelectorAll('.dt-panel').forEach(p => p.classList.toggle('active', p.id === 'dt-panel-' + tab));
    if (tab === 'console') _renderDevLogs();
    else if (tab === 'network') _renderDevNetwork();
    else if (tab === 'info') _renderDevInfo();
  };

  window.dtClearConsole = function() {
    _dtLogs = [];
    _renderDevLogs();
  };

  window.dtEval = function(code) {
    if (!code.trim()) return;
    _dtReplHist.push(code);
    _dtReplIdx = -1;
    _dtLogPush('log', '> ' + code);
    try {
      // eslint-disable-next-line no-eval
      const result = (0, eval)(code);
      const out = (result === undefined) ? 'undefined' :
                  (typeof result === 'object' && result !== null) ? JSON.stringify(result, null, 2) : String(result);
      _dtLogPush('info', '← ' + out);
    } catch(e) {
      _dtLogPush('error', '✗ ' + e.message);
    }
  };

  // ── Resize handle ──
  document.addEventListener('DOMContentLoaded', () => {
    const handle = document.getElementById('dt-resize-handle');
    const panel  = document.getElementById('devtools-panel');
    if (!handle || !panel) return;
    let dragging = false, startY = 0, startH = 0;
    handle.addEventListener('mousedown', e => {
      dragging = true;
      startY = e.clientY;
      startH = panel.offsetHeight;
      document.body.style.userSelect = 'none';
      e.preventDefault();
    });
    document.addEventListener('mousemove', e => {
      if (!dragging) return;
      const delta = startY - e.clientY;
      const newH = Math.max(120, Math.min(window.innerHeight * 0.8, startH + delta));
      panel.style.height = newH + 'px';
    });
    document.addEventListener('mouseup', () => {
      if (dragging) { dragging = false; document.body.style.userSelect = ''; }
    });
  });

  // log startup
  _dtLogPush('info', '[devtools] Panel initialised — ' + new Date().toLocaleString());
})();
</script>
<script src="https://cdn.jsdelivr.net/npm/sortablejs@1.15.6/Sortable.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/gridstack@7/dist/gridstack-all.js"></script>
<div id="grid-view">
  <div class="grid-toolbar">
    <span class="grid-toolbar-title">Workspace</span>
    <div id="grid-chips"></div>
    <div id="ws-profile-bar" style="display:flex;align-items:center;gap:5px;flex-wrap:wrap;margin-left:4px;"></div>
    <div id="ws-save-row" style="display:flex;align-items:center;gap:4px;flex-shrink:0;">
      <input id="ws-save-input" class="ws-save-input" placeholder="Profile name…" style="display:none;"
        onkeydown="if(event.key==='Enter'){wsSaveProfileConfirm();event.preventDefault();}if(event.key==='Escape'){wsHideSaveInput();}">
      <button id="ws-save-btn" class="btn" onclick="wsShowSaveInput()" style="font-size:0.75rem;padding:4px 10px;" title="Save current layout as a profile">&#x2B; Save</button>
      <button id="ws-save-ok" class="btn" onclick="wsSaveProfileConfirm()" style="display:none;font-size:0.75rem;padding:4px 10px;background:var(--green);color:#fff;border-color:var(--green);">&#x2713;</button>
    </div>
    <button class="btn" onclick="wsClearWorkspace()" style="flex-shrink:0;font-size:0.75rem;padding:4px 10px;color:var(--dim);" title="Remove all panes">Clear</button>
    <button class="btn" onclick="exitGridMode()" style="flex-shrink:0;font-size:0.75rem;padding:4px 10px;">&#x2715; Exit</button>
  </div>
  <div id="gridstack-container">
    <div class="grid-stack" id="gridstack"></div>
  </div>
</div>
<!-- DevTools Panel -->
<div id="devtools-panel">
  <div class="dt-resize-handle" id="dt-resize-handle"></div>
  <div class="dt-header">
    <div class="dt-tabs">
      <button class="dt-tab active" onclick="dtSwitchTab('console')">Console</button>
      <button class="dt-tab" onclick="dtSwitchTab('network')">Network</button>
      <button class="dt-tab" onclick="dtSwitchTab('info')">Info</button>
    </div>
    <div class="dt-toolbar">
      <button class="dt-toolbar-btn" onclick="dtClearConsole()" title="Clear console">&#x2298;</button>
      <button class="dt-toolbar-btn" onclick="closeDevtools()" title="Close">&#x2715;</button>
    </div>
  </div>
  <div class="dt-panel active" id="dt-panel-console">
    <div class="dt-log-area" id="dt-log-area"></div>
    <div class="dt-repl-row">
      <span class="dt-prompt">&gt;</span>
      <input class="dt-repl-input" id="dt-repl-input" placeholder="JavaScript expression... (Enter to evaluate)"
        onkeydown="if(event.key==='Enter'){dtEval(this.value);this.value='';event.preventDefault();}
                   if(event.key==='ArrowUp'){dtReplHistUp(this);event.preventDefault();}
                   if(event.key==='ArrowDown'){dtReplHistDown(this);event.preventDefault();}">
    </div>
  </div>
  <div class="dt-panel" id="dt-panel-network">
    <div class="dt-log-area" id="dt-net-area"></div>
  </div>
  <div class="dt-panel" id="dt-panel-info">
    <div class="dt-log-area" id="dt-info-area"></div>
  </div>
</div>
</body>
</html>"""


# ═══════════════════════════════════════════
# PWA ASSETS
# ═══════════════════════════════════════════

PWA_MANIFEST = json.dumps({
    "name": "amux — Claude Code Multiplexer",
    "short_name": "amux",
    "start_url": "/",
    "display": "standalone",
    "background_color": "#0d1117",
    "theme_color": "#0d1117",
    "icons": [
        {"src": "/icon.svg", "sizes": "any", "type": "image/svg+xml", "purpose": "any"},
        {"src": "/icon.png", "sizes": "180x180", "type": "image/png"},
    ],
})

# Robust service worker: cache-first with localStorage fallback for multi-day offline
SERVICE_WORKER = r"""
const CACHE = 'amux-v0.6.2';
const SHELL_URLS = ['/', '/manifest.json', '/icon.svg', '/icon.png'];

// Install: pre-cache entire app shell
self.addEventListener('install', e => {
  e.waitUntil(
    caches.open(CACHE).then(c => c.addAll(SHELL_URLS))
  );
  self.skipWaiting();
});

// Activate: clean old caches, take control immediately
self.addEventListener('activate', e => {
  e.waitUntil(
    caches.keys().then(keys =>
      Promise.all(keys.filter(k => k !== CACHE).map(k => caches.delete(k)))
    ).then(() => self.clients.claim())
  );
});

self.addEventListener('message', e => {
  if (e.data && e.data.type === 'SKIP_WAITING') self.skipWaiting();
  // Client can push HTML into SW for localStorage-backed fallback
  if (e.data && e.data.type === 'CACHE_HTML') {
    caches.open(CACHE).then(cache => {
      const resp = new Response(e.data.html, {
        headers: { 'Content-Type': 'text/html; charset=utf-8' }
      });
      cache.put('/', resp);
    });
  }
});

self.addEventListener('fetch', e => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET') return;
  // Only handle http/https (skip chrome-extension:// etc.)
  if (!url.protocol.startsWith('http')) return;

  // API requests: network only (app JS handles offline queue)
  if (url.pathname.startsWith('/api/')) return;

  // Main HTML: network-first so updates always reach the client when online
  if (url.pathname === '/') {
    e.respondWith(
      fetch(e.request).then(response => {
        const clone = response.clone();  // clone before any async op
        if (response.ok) {
          caches.open(CACHE).then(c => c.put(e.request, clone));
        }
        return response;
      }).catch(() =>
        caches.open(CACHE).then(c => c.match(e.request)).then(cached =>
          cached || new Response('Offline — please reload when connected', {
            status: 503, headers: { 'Content-Type': 'text/plain' }
          })
        )
      )
    );
    return;
  }

  // Static assets (icons, manifest): cache-first, refresh in background
  e.respondWith(
    caches.open(CACHE).then(cache =>
      cache.match(e.request).then(cached => {
        const networkUpdate = fetch(e.request).then(response => {
          if (response.ok) cache.put(e.request, response.clone());
          return response;
        }).catch(() => null);

        if (cached) return cached;
        return networkUpdate.then(r => r || new Response('Offline — please reload when connected', {
          status: 503, headers: { 'Content-Type': 'text/plain' }
        }));
      })
    )
  );
});

// Background Sync — replay offline queue when connectivity returns
self.addEventListener('sync', e => {
  if (e.tag !== 'replay-queue') return;
  e.waitUntil((async () => {
    // Open IndexedDB directly (SW can't access localStorage)
    const db = await new Promise((resolve, reject) => {
      const req = indexedDB.open('amux', 1);
      req.onupgradeneeded = () => {
        const d = req.result;
        if (!d.objectStoreNames.contains('kv')) d.createObjectStore('kv');
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
    const tx = db.transaction('kv', 'readonly');
    const queue = await new Promise((resolve, reject) => {
      const r = tx.objectStore('kv').get('offline_queue');
      r.onsuccess = () => resolve(r.result || []);
      r.onerror = () => reject(r.error);
    });
    if (!queue.length) return;

    const failures = [];
    let replayed = 0;
    for (const item of queue) {
      try {
        const r = await fetch(item.url, item.options);
        if (r.status >= 500) {
          failures.push(item);  // retry later
        } else {
          replayed++;  // 2xx/3xx/4xx — done (4xx = stale, drop)
        }
      } catch(e) {
        failures.push(item);  // network error — retry later
      }
    }

    // Write remaining failures back to IDB
    const tx2 = db.transaction('kv', 'readwrite');
    tx2.objectStore('kv').put(failures, 'offline_queue');
    await new Promise((resolve, reject) => {
      tx2.oncomplete = resolve;
      tx2.onerror = () => reject(tx2.error);
    });

    // Notify all clients
    const clients = await self.clients.matchAll();
    clients.forEach(c => c.postMessage({ type: 'SYNC_COMPLETE', replayed, remaining: failures.length }));
  })());
});
""".strip()




# ═══════════════════════════════════════════
# HTTP REQUEST HANDLER
# ═══════════════════════════════════════════

class ResilientHTTPSServer(ThreadingHTTPServer):
    """ThreadingHTTPServer with per-connection TLS handshake in worker threads.

    The listening socket stays plain TCP so accept() never blocks on a TLS
    handshake.  Each accepted connection is TLS-wrapped in its own daemon
    thread with a 10-second timeout, so a stalled handshake can never freeze
    the server's accept loop.
    """
    daemon_threads = True
    ssl_ctx = None  # Set after creation; None = plain HTTP

    def process_request_thread(self, request, client_address):
        """Wrap the raw TCP socket with TLS before handing to the handler."""
        if self.ssl_ctx:
            request.settimeout(10)
            try:
                request = self.ssl_ctx.wrap_socket(request, server_side=True)
            except (ssl.SSLError, OSError):
                try:
                    request.close()
                except OSError:
                    pass
                return
            request.settimeout(None)
        try:
            self.finish_request(request, client_address)
        except Exception:
            self.handle_error(request, client_address)
        finally:
            self.shutdown_request(request)


class CCHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        ip = self.client_address[0]
        slog(f"[{ip}] {args[0]}")

    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, PATCH, DELETE, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "SAMEORIGIN")

    def _json(self, data, status=200):
        body = json.dumps(data).encode()
        self.send_response(status)
        self._cors()
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _html(self, html):
        body = html.encode()
        self.send_response(200)
        self._cors()
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _raw(self, body: bytes, content_type: str, cache=False):
        self.send_response(200)
        self._cors()
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        if cache:
            self.send_header("Cache-Control", "public, max-age=86400")
        self.end_headers()
        self.wfile.write(body)

    def _sse_events(self):
        """Server-Sent Events stream for real-time session and board updates."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.send_header("X-Accel-Buffering", "no")
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()

        last_sessions_json = ""
        last_board_json = ""
        heartbeat_counter = 0

        try:
            while True:
                now = time.time()

                # Sessions — use shared cache to avoid redundant subprocess calls
                sc = _sse_cache["sessions"]
                if now - sc["time"] > _SSE_CACHE_TTL:
                    data = list_sessions()
                    sc["data"] = data
                    sc["json"] = json.dumps(data, sort_keys=True)
                    sc["time"] = now
                if sc["json"] != last_sessions_json:
                    last_sessions_json = sc["json"]
                    self.wfile.write(f"data: {json.dumps({'type': 'sessions', 'payload': sc['data']})}\n\n".encode())
                    self.wfile.flush()

                # Board — use shared cache
                bc = _sse_cache["board"]
                if now - bc["time"] > _SSE_CACHE_TTL:
                    data = _load_board()
                    bc["data"] = data
                    bc["json"] = json.dumps(data, sort_keys=True)
                    bc["time"] = now
                if bc["json"] != last_board_json:
                    last_board_json = bc["json"]
                    self.wfile.write(f"data: {json.dumps({'type': 'board', 'payload': bc['data']})}\n\n".encode())
                    self.wfile.flush()

                # Heartbeat every 15s (7-8 iterations at 2s sleep)
                heartbeat_counter += 1
                if heartbeat_counter >= 8:
                    self.wfile.write(b": heartbeat\n\n")
                    self.wfile.flush()
                    heartbeat_counter = 0

                time.sleep(2)
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def _read_body(self) -> dict:
        length = int(self.headers.get("Content-Length", 0))
        if length == 0:
            return {}
        return json.loads(self.rfile.read(length))

    def _route(self, method: str):
        parsed = urlparse(self.path)
        path = parsed.path.rstrip("/") or "/"
        qs = parse_qs(parsed.query)
        try:
            return self._route_inner(method, path, qs)
        except Exception as e:
            import traceback
            slog(f"ERROR {method} {path} — {e}\n{traceback.format_exc()}")
            return self._json({"error": str(e)}, 500)

    def _route_inner(self, method: str, path: str, qs: dict):

        # GET /
        if method == "GET" and path == "/":
            return self._html(DASHBOARD_HTML)

        # GET /clear — unregister SW + wipe caches, then redirect to /
        if method == "GET" and path == "/clear":
            body = b"""<!DOCTYPE html><html><head>
<meta name="viewport" content="width=device-width">
<style>body{font-family:system-ui;background:#0d1117;color:#e6edf3;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;flex-direction:column;gap:12px;font-size:1rem;}</style>
</head><body>
<div>Clearing cache\u2026</div>
<script>
(async () => {
  if ('serviceWorker' in navigator) {
    const regs = await navigator.serviceWorker.getRegistrations();
    await Promise.all(regs.map(r => r.unregister()));
  }
  const keys = await caches.keys();
  await Promise.all(keys.map(k => caches.delete(k)));
  localStorage.clear();
  location.replace('/');
})();
</script>
</body></html>"""
            self.send_response(200)
            self._cors()
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        # PWA assets
        if method == "GET" and path == "/manifest.json":
            return self._raw(PWA_MANIFEST.encode(), "application/manifest+json", cache=True)
        if method == "GET" and path == "/sw.js":
            return self._raw(SERVICE_WORKER.encode(), "application/javascript")
        if method == "GET" and path in ("/icon.svg", "/icon.png"):
            icon_path = Path(__file__).resolve().parent / path.lstrip("/")
            if icon_path.exists():
                ct = "image/svg+xml" if path.endswith(".svg") else "image/png"
                return self._raw(icon_path.read_bytes(), ct, cache=True)
            return self._json({"error": "icon not found"}, 404)

        # GET /ca — serve mkcert root CA for device trust installation
        if method == "GET" and path == "/ca":
            import subprocess as _sp
            try:
                ca_root = _sp.run(["mkcert", "-CAROOT"], capture_output=True, text=True, timeout=5).stdout.strip()
                ca_file = Path(ca_root) / "rootCA.pem"
                if ca_file.exists():
                    body = ca_file.read_bytes()
                    self.send_response(200)
                    self._cors()
                    self.send_header("Content-Type", "application/x-pem-file")
                    self.send_header("Content-Disposition", 'attachment; filename="amux-ca.pem"')
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
            except Exception:
                pass
            return self._json({"error": "CA not found"}, 404)

        # GET /api/events (SSE stream)
        if method == "GET" and path == "/api/events":
            return self._sse_events()

        # GET /api/sessions
        if method == "GET" and path == "/api/sessions":
            return self._json(list_sessions())

        # GET/POST /api/memory/global
        if path == "/api/memory/global":
            if method == "GET":
                content = _GLOBAL_MEM_FILE.read_text(errors="replace") if _GLOBAL_MEM_FILE.exists() else ""
                return self._json({"content": content, "path": str(_GLOBAL_MEM_FILE)})
            if method == "POST":
                body = self._read_body()
                content = body.get("content", "")
                _GLOBAL_MEM_FILE.write_text(content)
                # Recompose Claude memory for all registered sessions
                if CC_SESSIONS.exists():
                    for env_file in CC_SESSIONS.glob("*.env"):
                        sname = env_file.stem
                        wd = _session_work_dir(sname)
                        if wd:
                            _write_claude_memory(sname, wd)
                return self._json({"ok": True})

        # GET /api/skills — list Claude Code commands from ~/.claude/commands/
        if method == "GET" and path == "/api/skills":
            skills = []
            cmd_dir = CLAUDE_HOME / "commands"
            if cmd_dir.is_dir():
                for f in sorted(cmd_dir.glob("*.md")):
                    try:
                        text = f.read_text(errors="replace")
                        desc, hint = "", ""
                        if text.startswith("---"):
                            fm_end = text.find("---", 3)
                            if fm_end > 0:
                                fm = text[3:fm_end]
                                for line in fm.splitlines():
                                    if line.startswith("description:"):
                                        desc = line.split(":", 1)[1].strip()
                                    elif line.startswith("argument-hint:"):
                                        hint = line.split(":", 1)[1].strip()
                        skills.append({"name": f.stem, "description": desc, "hint": hint})
                    except Exception:
                        pass
            return self._json(skills)

        # GET /api/stats/daily
        if method == "GET" and path == "/api/stats/daily":
            return self._json(get_daily_token_stats())

        # POST /api/stats/reset
        if method == "POST" and path == "/api/stats/reset":
            # Save current raw totals as baseline (before subtraction)
            raw_stats = get_daily_token_stats()
            # Need raw totals (add back any existing baseline)
            baseline = _load_token_baseline()
            if baseline:
                raw_stats["total_input"] += baseline.get("total_input", 0)
                raw_stats["total_output"] += baseline.get("total_output", 0)
                bl_sessions = baseline.get("sessions", {})
                for s in raw_stats["sessions"]:
                    bl = bl_sessions.get(s["proj_dir"], bl_sessions.get(s["name"], {}))
                    s["input"] += bl.get("input", 0)
                    s["output"] += bl.get("output", 0)
            save_token_baseline(raw_stats)
            return self._json({"ok": True})

        # GET /api/file?path=...&cwd=...
        if method == "GET" and path == "/api/file":
            fpath = qs.get("path", [""])[0]
            cwd = qs.get("cwd", [""])[0]
            if not fpath:
                return self._json({"error": "missing path"}, 400)
            p = Path(fpath).expanduser()
            if not p.is_absolute() and cwd:
                p = Path(cwd).expanduser() / p
            elif not p.is_absolute():
                return self._json({"error": "relative path without cwd"}, 400)
            if not p.is_file():
                return self._json({"error": "file not found"}, 404)
            try:
                ext = p.suffix.lower()
                IMAGE_MIMES = {
                    ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg",
                    ".gif": "image/gif", ".webp": "image/webp", ".svg": "image/svg+xml",
                    ".bmp": "image/bmp", ".ico": "image/x-icon",
                }
                if ext in IMAGE_MIMES:
                    raw = p.read_bytes()
                    if len(raw) > 5_000_000:
                        return self._json({"error": "Image too large (>5 MB)"}, 400)
                    mime = IMAGE_MIMES[ext]
                    data_url = f"data:{mime};base64,{base64.b64encode(raw).decode()}"
                    return self._json({"path": str(p), "is_image": True, "data_url": data_url, "mime": mime})
                if ext == ".pdf":
                    raw = p.read_bytes()
                    if len(raw) > 10_000_000:
                        return self._json({"error": "PDF too large (>10 MB)"}, 400)
                    data_url = f"data:application/pdf;base64,{base64.b64encode(raw).decode()}"
                    return self._json({"path": str(p), "is_pdf": True, "data_url": data_url})
                content = p.read_text(errors="replace")
                # Limit to 200KB for safety
                if len(content) > 200_000:
                    content = content[:200_000] + "\n\n... (truncated at 200KB)"
                is_md = ext in (".md", ".markdown", ".mdx")
                is_csv = ext == ".csv"
                is_html = ext in (".html", ".htm")
                return self._json({
                    "path": str(p), "content": content,
                    "is_markdown": is_md, "is_csv": is_csv, "is_html": is_html,
                })
            except Exception as e:
                return self._json({"error": str(e)}, 500)

        # GET /api/autocomplete/dir?q=...
        if method == "GET" and path == "/api/autocomplete/dir":
            query = qs.get("q", [""])[0]
            if not query:
                return self._json([])
            p = Path(query).expanduser()
            # If query ends with /, list contents of that dir
            if query.endswith("/") and p.is_dir():
                parent = p
                prefix = ""
            else:
                parent = p.parent
                prefix = p.name.lower()
            if not parent.is_dir():
                return self._json([])
            try:
                results = []
                for item in sorted(parent.iterdir()):
                    if item.name.startswith("."):
                        continue
                    if item.is_dir() and item.name.lower().startswith(prefix):
                        results.append(str(item) + "/")
                        if len(results) >= 10:
                            break
                return self._json(results)
            except PermissionError:
                return self._json([])

        # GET /api/ls?path=...&hidden=0|1
        if method == "GET" and path == "/api/ls":
            ls_path = qs.get("path", [""])[0]
            if not ls_path:
                return self._json({"error": "missing path"}, 400)
            show_hidden = qs.get("hidden", ["0"])[0] == "1"
            p = Path(ls_path).expanduser().resolve()
            if not p.is_dir():
                return self._json({"error": "not a directory"}, 400)
            try:
                entries = []
                for item in sorted(p.iterdir(), key=lambda x: (not x.is_dir(), x.name.lower())):
                    if not show_hidden and item.name.startswith('.'):
                        continue
                    try:
                        st = item.stat()
                        entries.append({
                            "name": item.name,
                            "type": "dir" if item.is_dir() else "file",
                            "size": st.st_size if item.is_file() else None,
                            "modified": int(st.st_mtime),
                        })
                    except (PermissionError, OSError):
                        pass
                return self._json({"path": str(p), "parent": str(p.parent) if p.parent != p else None, "entries": entries})
            except PermissionError:
                return self._json({"error": "permission denied"}, 403)

        # ── File upload ──
        if method == "POST" and path == "/api/upload":
            body = self._read_body()
            filename = re.sub(r'[^a-zA-Z0-9._\-]', '_', body.get("name", "upload"))[:120]
            ext = Path(filename).suffix.lower()
            if ext not in UPLOAD_ALLOWED_EXTS:
                return self._json({"error": f"unsupported file type: {ext}"}, 400)
            raw_b64 = body.get("data", "")
            try:
                data = base64.b64decode(raw_b64)
            except Exception:
                return self._json({"error": "invalid base64"}, 400)
            if len(data) > UPLOAD_MAX_BYTES:
                return self._json({"error": "file too large (max 20 MB)"}, 400)
            # Validate image files are real images (not corrupt/truncated)
            IMAGE_EXTS = {".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"}
            if ext in IMAGE_EXTS:
                if len(data) < 100:
                    return self._json({"error": "image too small — likely corrupt"}, 400)
                # Check magic bytes
                magic_ok = (
                    data[:8] == b'\x89PNG\r\n\x1a\n' or           # PNG
                    data[:2] == b'\xff\xd8' or                      # JPEG
                    data[:6] in (b'GIF87a', b'GIF89a') or          # GIF
                    data[:4] == b'RIFF' and data[8:12] == b'WEBP' or  # WebP
                    data[:2] == b'BM'                               # BMP
                )
                if not magic_ok:
                    return self._json({"error": "file does not appear to be a valid image"}, 400)
            uid = uuid.uuid4().hex[:8]
            save_name = f"{uid}-{filename}"
            save_path = CC_UPLOADS / save_name
            save_path.write_bytes(data)
            # Purge uploads older than 24h
            cutoff = time.time() - 86400
            for old in CC_UPLOADS.iterdir():
                try:
                    if old.stat().st_mtime < cutoff:
                        old.unlink()
                except Exception:
                    pass
            return self._json({"path": str(save_path), "name": filename, "url": f"/api/uploads/{save_name}"})

        # ── Serve uploaded files ──
        if method == "GET" and path.startswith("/api/uploads/"):
            fname = path[len("/api/uploads/"):]
            # Prevent path traversal
            if "/" in fname or "\\" in fname or fname.startswith("."):
                return self._json({"error": "not found"}, 404)
            fpath = CC_UPLOADS / fname
            if not fpath.exists():
                return self._json({"error": "not found"}, 404)
            ext = fpath.suffix.lower()
            ct_map = {
                ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg",
                ".gif": "image/gif", ".webp": "image/webp", ".bmp": "image/bmp",
                ".pdf": "application/pdf", ".txt": "text/plain; charset=utf-8",
                ".md": "text/plain; charset=utf-8", ".csv": "text/csv; charset=utf-8",
                ".json": "application/json", ".log": "text/plain; charset=utf-8",
            }
            ct = ct_map.get(ext, "application/octet-stream")
            data = fpath.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", ct)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "max-age=3600")
            self.end_headers()
            self.wfile.write(data)
            return

        # ── Board API ──
        if path == "/api/board" or path.startswith("/api/board/"):
            db = get_db()

            # GET /api/board — list all non-deleted issues
            if method == "GET" and path == "/api/board":
                return self._json(_load_board())

            # POST /api/board — create issue
            if method == "POST" and path == "/api/board":
                body = self._read_body()
                title = body.get("title", "").strip()
                if not title:
                    return self._json({"error": "missing title"}, 400)
                session = body.get("session", "").strip()
                prefix = _prefix_from_session(session)
                item_id = _next_issue_id(prefix)
                now = int(time.time())
                status = body.get("status", "todo")
                due = body.get("due", "").strip() or None
                creator = body.get("creator", "")
                desc = body.get("desc", "").strip()
                tags = [t for t in body.get("tags", []) if t]
                owner_type = body.get("owner_type", "agent" if session else "human")
                if owner_type not in ("human", "agent"):
                    owner_type = "human"
                db.execute(
                    """INSERT INTO issues (id, title, desc, status, session, creator, due, created, updated, owner_type)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                    (item_id, title, desc, status, session or None, creator, due, now, now, owner_type),
                )
                for tag in tags:
                    db.execute(
                        "INSERT OR IGNORE INTO issue_tags (issue_id, tag) VALUES (?, ?)",
                        (item_id, tag),
                    )
                db.commit()
                _sse_cache["board"]["time"] = 0  # invalidate SSE cache
                item = _item_by_id(item_id)
                return self._json(item, 201)

            # POST /api/board/clear-done — soft-delete all done issues
            if method == "POST" and path == "/api/board/clear-done":
                now = int(time.time())
                db.execute(
                    "UPDATE issues SET deleted = ? WHERE status = 'done' AND deleted IS NULL", (now,)
                )
                db.commit()
                _sse_cache["board"]["time"] = 0
                remaining = db.execute(
                    "SELECT COUNT(*) FROM issues WHERE deleted IS NULL"
                ).fetchone()[0]
                return self._json({"ok": True, "remaining": remaining})

            # GET /api/board/statuses
            if path == "/api/board/statuses":
                if method == "GET":
                    return self._json(_load_board_statuses())
                # POST /api/board/statuses — add custom column
                if method == "POST":
                    body = self._read_body()
                    label = body.get("label", "").strip()
                    if not label:
                        return self._json({"error": "missing label"}, 400)
                    sid = re.sub(r"[^a-z0-9]+", "-", label.lower()).strip("-")[:30]
                    if not sid:
                        return self._json({"error": "invalid label"}, 400)
                    existing = [r["id"] for r in db.execute("SELECT id FROM statuses").fetchall()]
                    if sid in existing:
                        base = sid
                        for i in range(2, 20):
                            candidate = f"{base}-{i}"
                            if candidate not in existing:
                                sid = candidate
                                break
                    max_pos = db.execute("SELECT COALESCE(MAX(position),0) FROM statuses").fetchone()[0]
                    db.execute(
                        "INSERT OR IGNORE INTO statuses (id, label, position, is_builtin) VALUES (?, ?, ?, 0)",
                        (sid, label, max_pos + 1),
                    )
                    db.commit()
                    return self._json({"id": sid, "label": label}, 201)

            # DELETE/PATCH /api/board/statuses/<id>
            status_m = re.match(r"^/api/board/statuses/([a-z0-9-]+)$", path)
            if status_m:
                sid = status_m.group(1)
                if method == "DELETE":
                    if sid in ("backlog", "todo", "doing", "done", "discarded"):
                        return self._json({"error": "cannot delete built-in status"}, 400)
                    db.execute("DELETE FROM statuses WHERE id = ? AND is_builtin = 0", (sid,))
                    db.execute(
                        "UPDATE issues SET status = 'todo' WHERE status = ? AND deleted IS NULL", (sid,)
                    )
                    db.commit()
                    _sse_cache["board"]["time"] = 0
                    return self._json({"ok": True})
                if method == "PATCH":
                    body = self._read_body()
                    label = body.get("label", "").strip()
                    if label:
                        db.execute("UPDATE statuses SET label = ? WHERE id = ?", (label, sid))
                        db.commit()
                    return self._json({"ok": True})

            # POST /api/board/<id>/claim — atomic task claim for multi-agent coordination
            claim_m = re.match(r"^/api/board/([A-Za-z0-9_-]+)/claim$", path)
            if claim_m and method == "POST":
                bid = claim_m.group(1)
                body = self._read_body()
                session_name = body.get("session", "").strip()
                if not session_name:
                    return self._json({"error": "missing session"}, 400)
                row = db.execute(
                    "SELECT id, status, owner_type, session FROM issues WHERE id = ? AND deleted IS NULL",
                    (bid,),
                ).fetchone()
                if not row:
                    return self._json({"error": "item not found"}, 404)
                if dict(row)["owner_type"] != "agent":
                    return self._json({"error": "item is not an agent task"}, 409)
                if dict(row)["status"] not in ("todo", "backlog"):
                    return self._json({"error": f"item not available (status: {dict(row)['status']})"}, 409)
                now = int(time.time())
                # Atomic claim: only succeeds if still todo/backlog
                db.execute(
                    "UPDATE issues SET status='doing', session=?, updated=?"
                    " WHERE id=? AND status IN ('todo','backlog') AND deleted IS NULL",
                    (session_name, now, bid),
                )
                db.commit()
                updated = db.execute("SELECT session FROM issues WHERE id=?", (bid,)).fetchone()
                if not updated or dict(updated)["session"] != session_name:
                    return self._json({"error": "claim failed — taken by another session"}, 409)
                _sse_cache["board"]["time"] = 0
                return self._json(_item_by_id(bid))

            # PATCH/DELETE /api/board/<id>
            board_m = re.match(r"^/api/board/([A-Za-z0-9_-]+)$", path)
            if board_m:
                bid = board_m.group(1)
                exists = db.execute(
                    "SELECT id FROM issues WHERE id = ? AND deleted IS NULL", (bid,)
                ).fetchone()
                if not exists:
                    return self._json({"error": "item not found"}, 404)

                if method == "PATCH":
                    body = self._read_body()
                    now = int(time.time())
                    set_clauses, params = [], []
                    for k in ("title", "desc", "status", "session", "due", "owner_type"):
                        if k in body:
                            set_clauses.append(f"{k} = ?")
                            v = body[k]
                            params.append(None if v == "" and k in ("session", "due") else v)
                    if "creator" in body:
                        set_clauses.append("creator = ?")
                        params.append(body["creator"])
                    set_clauses.append("updated = ?")
                    params.append(now)
                    params.append(bid)
                    if set_clauses:
                        db.execute(
                            f"UPDATE issues SET {', '.join(set_clauses)} WHERE id = ?", params
                        )
                    if "tags" in body:
                        db.execute("DELETE FROM issue_tags WHERE issue_id = ?", (bid,))
                        for tag in (body["tags"] or []):
                            if tag:
                                db.execute(
                                    "INSERT OR IGNORE INTO issue_tags (issue_id, tag) VALUES (?, ?)",
                                    (bid, tag),
                                )
                    db.commit()
                    _sse_cache["board"]["time"] = 0
                    return self._json(_item_by_id(bid))

                if method == "DELETE":
                    now = int(time.time())
                    db.execute("UPDATE issues SET deleted = ? WHERE id = ?", (now, bid))
                    db.commit()
                    _sse_cache["board"]["time"] = 0
                    return self._json({"ok": True, "deleted": bid})

            return self._json({"error": "not found"}, 404)

        # GET /api/cert — download TLS cert for manual trust on mobile
        if method == "GET" and path == "/api/cert":
            cert_path = TLS_DIR / "cert.pem"
            if not cert_path.exists():
                return self._json({"error": "no cert"}, 404)
            body = cert_path.read_bytes()
            self.send_response(200)
            self._cors()
            self.send_header("Content-Type", "application/x-pem-file")
            self.send_header("Content-Disposition", "attachment; filename=\"amux.pem\"")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        # GET /api/calendar.ics — iCal subscription feed
        if method == "GET" and path == "/api/calendar.ics":
            items = [i for i in _load_board() if i.get("due")]
            lines = [
                "BEGIN:VCALENDAR",
                "VERSION:2.0",
                "PRODID:-//amux//amux calendar//EN",
                "CALSCALE:GREGORIAN",
                "METHOD:PUBLISH",
                "X-WR-CALNAME:amux Board",
                "X-WR-CALDESC:amux board items with due dates",
            ]
            status_map = {"todo": "NEEDS-ACTION", "doing": "IN-PROCESS", "done": "COMPLETED"}
            for item in items:
                due = item["due"]  # YYYY-MM-DD
                date_val = due.replace("-", "")
                uid = item["id"] + "@amux"
                summary = item.get("title", "").replace(",", "\\,").replace(";", "\\;").replace("\n", "\\n")
                desc = item.get("desc", "").replace(",", "\\,").replace(";", "\\;").replace("\n", "\\n")
                vstatus = status_map.get(item.get("status", "todo"), "NEEDS-ACTION")
                lines += [
                    "BEGIN:VEVENT",
                    f"UID:{uid}",
                    f"DTSTART;VALUE=DATE:{date_val}",
                    f"DTEND;VALUE=DATE:{date_val}",
                    f"SUMMARY:{summary}",
                    f"DESCRIPTION:{desc}",
                    f"STATUS:{vstatus}",
                    "END:VEVENT",
                ]
            lines.append("END:VCALENDAR")
            ical_text = "\r\n".join(lines) + "\r\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/calendar; charset=utf-8")
            self.send_header("Content-Disposition", 'attachment; filename="amux.ics"')
            self.end_headers()
            self.wfile.write(ical_text.encode("utf-8"))
            return

        # GET /api/sync?since=<unix_seconds> — delta sync for offline clients
        if method == "GET" and path == "/api/sync":
            since = int(qs.get("since", ["0"])[0] or "0")
            db = get_db()
            rows = db.execute(
                """SELECT i.id, i.title, i.desc, i.status, i.session, i.creator,
                          i.due, i.created, i.updated, i.deleted,
                          GROUP_CONCAT(t.tag) AS tags_csv
                   FROM issues i
                   LEFT JOIN issue_tags t ON t.issue_id = i.id
                   WHERE i.updated > ? OR (i.deleted IS NOT NULL AND i.deleted > ?)
                   GROUP BY i.id""",
                (since, since),
            ).fetchall()
            issues = []
            for row in rows:
                item = dict(row)
                tags_csv = item.pop("tags_csv") or ""
                item["tags"] = [t for t in tags_csv.split(",") if t]
                issues.append(item)
            statuses = [
                dict(r)
                for r in db.execute("SELECT id, label, position FROM statuses ORDER BY position").fetchall()
            ]
            return self._json({
                "ts": int(time.time()),
                "issues": issues,
                "statuses": statuses,
            })

        # GET /api/tmux-sessions (unregistered tmux sessions)
        if method == "GET" and path == "/api/tmux-sessions":
            return self._json(list_tmux_sessions())

        # POST /api/sessions/connect (adopt existing tmux session)
        if method == "POST" and path == "/api/sessions/connect":
            body = self._read_body()
            tmux_session = body.get("tmux_name", "").strip()
            cc_name = body.get("name", "").strip()
            if not tmux_session:
                return self._json({"error": "missing tmux_name"}, 400)
            # Derive name: strip amux-/cc- prefix if present, or use as-is
            if not cc_name:
                cc_name = tmux_session.removeprefix("amux-").removeprefix("cc-")
            cc_name = re.sub(r'[^a-zA-Z0-9_-]', '-', cc_name)
            env_file = CC_SESSIONS / f"{cc_name}.env"
            if env_file.exists():
                return self._json({"error": f"session '{cc_name}' already exists"}, 409)
            CC_SESSIONS.mkdir(parents=True, exist_ok=True)
            # Get working dir from tmux
            try:
                r = subprocess.run(
                    ["tmux", "display-message", "-t", tmux_session, "-p", "#{pane_current_path}"],
                    capture_output=True, text=True, timeout=5,
                )
                cwd = r.stdout.strip() if r.returncode == 0 else ""
            except Exception:
                cwd = ""
            cfg = {"CC_DIR": cwd, "CC_FLAGS": ""}
            _write_env(env_file, cfg)
            # Rename the tmux session to match amux convention
            expected_tmux = tmux_name(cc_name)
            if tmux_session != expected_tmux:
                try:
                    subprocess.run(
                        ["tmux", "rename-session", "-t", tmux_session, expected_tmux],
                        capture_output=True, timeout=5,
                    )
                except Exception:
                    pass
            return self._json({"ok": True, "name": cc_name, "message": f"connected {tmux_session} as {cc_name}"})

        # POST /api/sessions (create new session)
        if method == "POST" and path == "/api/sessions":
            body = self._read_body()
            name = body.get("name", "").strip()
            dir_path = body.get("dir", "").strip()
            if not name:
                return self._json({"error": "missing name"}, 400)
            name = re.sub(r'[^a-zA-Z0-9_-]', '-', name)
            env_file = CC_SESSIONS / f"{name}.env"
            if env_file.exists():
                return self._json({"error": f"session '{name}' already exists"}, 409)
            CC_SESSIONS.mkdir(parents=True, exist_ok=True)
            cfg = {}
            if dir_path:
                cfg["CC_DIR"] = dir_path
            desc = body.get("desc", "").strip()
            if desc:
                cfg["CC_DESC"] = desc
            creator = body.get("creator", "").strip()
            if creator:
                cfg["CC_CREATOR"] = creator
            cfg["CC_FLAGS"] = ""
            _write_env(env_file, cfg)
            _save_meta(name, {
                "created_at": int(time.time()),
                "creator": creator,
                "start_count": 0,
            })
            if dir_path:
                _ensure_memory(name, dir_path)
            return self._json({"ok": True, "message": f"created {name}"})

        # Session-specific routes: /api/sessions/<name>/<action>[/<subid>]
        m = re.match(r"^/api/sessions/([^/]+)(/([^/]+)(/([^/]+))?)?$", path)
        if not m:
            return self._json({"error": "not found"}, 404)

        name = m.group(1)
        action = m.group(3) or ""
        action_subid = m.group(5) or ""  # e.g. task ID in /tasks/<id>

        # Validate session exists (except for list)
        env_file = CC_SESSIONS / f"{name}.env"
        if not env_file.exists():
            return self._json({"error": f"session '{name}' not found"}, 404)


        if method == "GET":
            if action == "peek":
                lines = int(qs.get("lines", ["80"])[0])
                output = tmux_capture(name, lines)
                if output:
                    # Also save snapshot while we have it
                    threading.Thread(target=save_session_log, args=(name, output), daemon=True).start()
                    return self._json({"name": name, "output": output})
                # Not running or empty — serve saved log
                saved = load_session_log(name)
                if saved:
                    return self._json({"name": name, "output": saved, "saved": True})
                return self._json({"name": name, "output": "(no output)"})
            if action == "info":
                info = get_session_info(name)
                return self._json(info)
            if action == "meta":
                cfg = parse_env_file(env_file)
                meta = _load_meta(name)
                # Merge static env fields for a complete picture
                meta.setdefault("creator", cfg.get("CC_CREATOR", ""))
                env_mtime = int(env_file.stat().st_mtime)
                mem_file = _session_mem_file(name)
                mem_size = mem_file.stat().st_size if mem_file.exists() else 0
                return self._json({
                    **meta,
                    "name": name,
                    "dir": cfg.get("CC_DIR", ""),
                    "flags": cfg.get("CC_FLAGS", ""),
                    "desc": cfg.get("CC_DESC", ""),
                    "tags": [t.strip() for t in cfg.get("CC_TAGS", "").split(",") if t.strip()],
                    "env_updated": env_mtime,
                    "mem_size": mem_size,
                    "mem_path": str(_session_mem_file(name)),
                })
            if action == "stats":
                cfg = parse_env_file(env_file)
                stats = get_claude_stats(cfg.get("CC_DIR", ""))
                return self._json(stats)
            if action == "memory":
                mem_file = _session_mem_file(name)
                wd = _session_work_dir(name)
                if wd:
                    _ensure_memory(name, wd)
                content = mem_file.read_text(errors="replace") if mem_file.exists() else ""
                return self._json({"content": content, "path": str(mem_file)})
            return self._json({"error": "not found"}, 404)

        if method == "POST":
            if action == "send":
                body = self._read_body()
                text = body.get("text", "")
                if not text:
                    return self._json({"error": "missing 'text'"}, 400)
                wd = _session_work_dir(name)
                if wd:
                    _ensure_memory(name, wd)
                ok, msg = send_text(name, text)
                if ok:
                    _update_meta(name, last_send=int(time.time()))
                return self._json({"ok": ok, "message": msg}, 200 if ok else 500)
            if action == "keys":
                body = self._read_body()
                keys = body.get("keys", "")
                if not keys:
                    return self._json({"error": "missing 'keys'"}, 400)
                ok, msg = send_keys(name, keys)
                if ok:
                    _update_meta(name, last_send=int(time.time()))
                return self._json({"ok": ok, "message": msg}, 200 if ok else 500)
            if action == "memory":
                body = self._read_body()
                content = body.get("content", "")
                mem_file = _session_mem_file(name)
                mem_file.write_text(content)
                wd = _session_work_dir(name)
                if wd:
                    _write_claude_memory(name, wd)
                return self._json({"ok": True})
            if action == "start":
                ok, msg = start_session(name)
                meta = _load_meta(name)
                return self._json({"ok": ok, "message": msg, "resumed": bool(meta.get("cc_conversation_id"))}, 200 if ok else 500)
            if action == "stop":
                ok, msg = stop_session(name)
                return self._json({"ok": ok, "message": msg}, 200 if ok else 500)
            if action == "clear":
                try:
                    subprocess.run(
                        ["tmux", "clear-history", "-t", tmux_name(name)],
                        capture_output=True, timeout=5,
                    )
                    return self._json({"ok": True, "message": "cleared"})
                except Exception as e:
                    return self._json({"ok": False, "message": str(e)}, 500)
            if action == "duplicate":
                body = self._read_body()
                new_name = body.get("new_name", "").strip()
                if not new_name:
                    return self._json({"error": "missing new_name"}, 400)
                new_name = re.sub(r'[^a-zA-Z0-9_-]', '-', new_name)
                new_file = CC_SESSIONS / f"{new_name}.env"
                if new_file.exists():
                    return self._json({"error": f"session '{new_name}' already exists"}, 409)
                import shutil
                shutil.copy2(env_file, new_file)
                return self._json({"ok": True, "message": f"duplicated as {new_name}"})
            if action == "clone":
                body = self._read_body()
                new_name = body.get("new_name", "").strip()
                if not new_name:
                    return self._json({"error": "missing new_name"}, 400)
                new_name = re.sub(r'[^a-zA-Z0-9_-]', '-', new_name)
                new_file = CC_SESSIONS / f"{new_name}.env"
                if new_file.exists():
                    return self._json({"error": f"session '{new_name}' already exists"}, 409)
                # Copy config
                import shutil
                shutil.copy2(env_file, new_file)
                cfg = parse_env_file(env_file)
                work_dir = cfg.get("CC_DIR", str(Path.home()))
                # Use the source session's own conversation ID (not just any recent file)
                source_meta = _load_meta(name)
                session_id = source_meta.get("cc_conversation_id", "") or _find_latest_session_id(work_dir)
                if session_id:
                    # Resume the conversation in a forked session — full history
                    ok, msg = start_session(new_name, f"--resume {session_id} --fork-session", _skip_conv_id=True)
                    method_used = "resume"
                else:
                    # No conversation file found — fall back to scrollback context
                    ok, msg = start_session(new_name)
                    method_used = "scrollback"
                if not ok:
                    return self._json({"ok": False, "message": f"cloned config but failed to start: {msg}"}, 500)
                # For scrollback fallback, capture and send terminal content
                if method_used == "scrollback" and is_running(name):
                    import time as _time
                    _time.sleep(5)
                    scrollback = ""
                    try:
                        r = subprocess.run(
                            ["tmux", "capture-pane", "-t", tmux_name(name), "-p", "-S", "-3000"],
                            capture_output=True, text=True, timeout=10,
                        )
                        raw = r.stdout
                        scrollback = re.sub(r'\x1b\[[0-9;]*[a-zA-Z]|\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07', '', raw)
                        sb_lines = scrollback.splitlines()
                        while sb_lines and not sb_lines[0].strip():
                            sb_lines.pop(0)
                        while sb_lines and not sb_lines[-1].strip():
                            sb_lines.pop()
                        scrollback = "\n".join(sb_lines)
                    except Exception:
                        pass
                    if scrollback:
                        if len(scrollback) > 50000:
                            scrollback = scrollback[-50000:]
                        prompt = (
                            f"This session was cloned from '{name}'. "
                            f"Below is the recent terminal output from that session. "
                            f"Please continue the work from where it left off.\n\n"
                            f"```\n{scrollback}\n```"
                        )
                        t = tmux_name(new_name)
                        subprocess.run(["tmux", "send-keys", "-t", t, "-l", prompt],
                                       capture_output=True, timeout=30)
                        _time.sleep(1)
                        subprocess.run(["tmux", "send-keys", "-t", t, "Enter"],
                                       capture_output=True, timeout=5)
                return self._json({"ok": True, "message": f"cloned as {new_name} (method: {method_used})", "started": ok})
            if action == "delete":
                if is_running(name):
                    stop_session(name)
                env_file.unlink(missing_ok=True)
                (CC_MEMORY / f"{name}.md").unlink(missing_ok=True)
                _meta_path(name).unlink(missing_ok=True)
                return self._json({"ok": True, "message": "deleted"})
            return self._json({"error": "not found"}, 404)

        if method == "PATCH":
            if action == "config":
                body = self._read_body()
                cfg = parse_env_file(env_file)

                # Rename
                if "rename" in body:
                    new_name = re.sub(r'[^a-zA-Z0-9_-]', '-', body["rename"].strip())
                    if not new_name:
                        return self._json({"error": "invalid name"}, 400)
                    new_file = CC_SESSIONS / f"{new_name}.env"
                    if new_file.exists() and new_name != name:
                        return self._json({"error": f"'{new_name}' already exists"}, 409)
                    # Rename tmux session if running
                    if is_running(name):
                        subprocess.run(
                            ["tmux", "rename-session", "-t", tmux_name(name), tmux_name(new_name)],
                            capture_output=True, timeout=5,
                        )
                    env_file.rename(new_file)
                    # Migrate memory file
                    old_mem = CC_MEMORY / f"{name}.md"
                    new_mem = CC_MEMORY / f"{new_name}.md"
                    if old_mem.exists() and not new_mem.exists():
                        old_mem.rename(new_mem)
                    # Repair Claude symlink to point at new memory file
                    work_dir = cfg.get("CC_DIR", "")
                    if work_dir:
                        pname = _project_name(work_dir)
                        claude_link = CLAUDE_HOME / "projects" / pname / "memory" / "MEMORY.md"
                        try:
                            if claude_link.is_symlink():
                                claude_link.unlink()
                            claude_link.symlink_to(new_mem)
                        except Exception:
                            pass
                    # Migrate meta file
                    old_meta = _meta_path(name)
                    new_meta = _meta_path(new_name)
                    if old_meta.exists() and not new_meta.exists():
                        old_meta.rename(new_meta)
                    # Migrate log file
                    old_log = CC_LOGS / f"{name}.log"
                    new_log = CC_LOGS / f"{new_name}.log"
                    if old_log.exists() and not new_log.exists():
                        old_log.rename(new_log)
                    # Update board items referencing old session name
                    try:
                        board_items = _load_board()
                        changed = False
                        for item in board_items:
                            if item.get("session") == name:
                                item["session"] = new_name
                                changed = True
                        if changed:
                            _save_board(board_items)
                    except Exception:
                        pass
                    return self._json({"ok": True, "message": f"renamed to {new_name}"})

                # Change model
                if "model" in body:
                    model_val = body["model"].strip()
                    flags = cfg.get("CC_FLAGS", "")
                    # Remove existing --model flag
                    flags = re.sub(r'--model\s+\S+\s*', '', flags).strip()
                    if model_val:
                        flags = f"--model {model_val} {flags}".strip()
                    cfg["CC_FLAGS"] = flags
                    _write_env(env_file, cfg)
                    # Also send /model to running session so it takes effect immediately
                    if is_running(name) and model_val:
                        try:
                            subprocess.run(
                                ["tmux", "send-keys", "-t", tmux_name(name), f"/model {model_val}", "Enter"],
                                capture_output=True, timeout=5,
                            )
                        except Exception:
                            pass
                    return self._json({"ok": True, "message": f"model set to {model_val}"})

                # Toggle YOLO
                if body.get("toggle_yolo"):
                    flags = cfg.get("CC_FLAGS", "")
                    if "--dangerously-skip-permissions" in flags:
                        flags = flags.replace("--dangerously-skip-permissions", "").strip()
                    else:
                        flags = f"{flags} --dangerously-skip-permissions".strip()
                    cfg["CC_FLAGS"] = flags
                    _write_env(env_file, cfg)
                    return self._json({"ok": True, "message": "yolo toggled"})

                # Change directory
                if "dir" in body:
                    cfg["CC_DIR"] = body["dir"].strip()
                    _write_env(env_file, cfg)
                    return self._json({"ok": True, "message": "directory updated"})

                # Change description
                if "desc" in body:
                    cfg["CC_DESC"] = body["desc"].strip()
                    _write_env(env_file, cfg)
                    return self._json({"ok": True, "message": "description updated"})

                # Toggle pin
                if body.get("toggle_pin"):
                    cfg["CC_PINNED"] = "" if cfg.get("CC_PINNED") == "1" else "1"
                    _write_env(env_file, cfg)
                    return self._json({"ok": True, "message": "pin toggled"})

                # Set tags
                if "tags" in body:
                    cfg["CC_TAGS"] = body["tags"].strip()
                    _write_env(env_file, cfg)
                    return self._json({"ok": True, "message": "tags updated"})

                # Clear conversation history (next start gets a fresh session)
                if body.get("new_conversation"):
                    if is_running(name):
                        return self._json({"error": "stop the session before starting a new conversation"}, 409)
                    meta = _load_meta(name)
                    meta.pop("cc_conversation_id", None)
                    _save_meta(name, meta)
                    return self._json({"ok": True, "message": "conversation reset — next start will be a fresh conversation"})

                return self._json({"error": "nothing to update"}, 400)
            return self._json({"error": "not found"}, 404)

        return self._json({"error": "method not allowed"}, 405)

    def do_OPTIONS(self):
        self.send_response(204)
        self._cors()
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        self._route("GET")

    def do_HEAD(self):
        self._route("GET")

    def do_POST(self):
        self._route("POST")

    def do_PATCH(self):
        self._route("PATCH")

    def do_DELETE(self):
        self._route("DELETE")


# ═══════════════════════════════════════════
# SERVER STARTUP & FILE WATCHER
# ═══════════════════════════════════════════

def _watch_self(server):
    """Watch amux-server.py for changes and restart on modification."""
    script = Path(__file__).resolve()
    mtime = script.stat().st_mtime
    while True:
        time.sleep(1)
        try:
            new_mtime = script.stat().st_mtime
            if new_mtime != mtime:
                print(f"\n\033[33m↻ {script.name} changed — restarting...\033[0m")
                # Shutdown with timeout — don't let stuck threads block restart
                t = threading.Thread(target=server.shutdown, daemon=True)
                t.start()
                t.join(timeout=3)
                os.execv(sys.executable, [sys.executable] + sys.argv)
        except Exception:
            pass


# ── TLS ──

TLS_DIR = CC_HOME / "tls"


def _get_tailscale_hostname() -> str:
    """Get Tailscale MagicDNS hostname if available."""
    for ts_bin in ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "tailscale"]:
        try:
            r = subprocess.run([ts_bin, "status", "--self", "--json"],
                               capture_output=True, text=True, timeout=5)
            if r.returncode == 0:
                import json as _json
                data = _json.loads(r.stdout)
                dns = data.get("Self", {}).get("DNSName", "")
                return dns.rstrip(".")  # e.g. "desktop.tail5ce8f5.ts.net"
        except Exception:
            continue
    return ""


def _get_tailscale_ips() -> list:
    """Get Tailscale IPs (v4 + v6) if available."""
    for ts_bin in ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "tailscale"]:
        try:
            r = subprocess.run([ts_bin, "status", "--self", "--json"],
                               capture_output=True, text=True, timeout=5)
            if r.returncode == 0:
                import json as _json
                data = _json.loads(r.stdout)
                return data.get("Self", {}).get("TailscaleIPs", [])
        except Exception:
            continue
    return []


def _ensure_self_signed(lan_ip: str, extra_ips: list = None):
    """Ensure a self-signed fallback cert exists covering localhost + IPs."""
    cert_file = TLS_DIR / "cert.pem"
    key_file = TLS_DIR / "key.pem"
    if cert_file.exists() and key_file.exists():
        return str(cert_file), str(key_file)
    san_parts = ["DNS:localhost", "IP:127.0.0.1", f"IP:{lan_ip}"]
    for ip in (extra_ips or []):
        entry = f"IP:{ip}" if ":" not in ip else f"IP:{ip}"
        if entry not in san_parts:
            san_parts.append(entry)
    san = ",".join(san_parts)
    print(f"\033[2m  Generating self-signed TLS cert...\033[0m")
    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
         "-keyout", str(key_file), "-out", str(cert_file),
         "-days", "365", "-subj", "/CN=amux",
         "-addext", f"subjectAltName={san}"],
        capture_output=True, check=True,
    )
    return str(cert_file), str(key_file)


def _ensure_tls(lan_ip: str) -> tuple:
    """Ensure TLS cert exists. Returns (cert, key, hostname, fallback_ctx_or_None).

    When a Tailscale cert is used, also generates a self-signed fallback cert
    covering Tailscale IPs so that clients connecting via raw IP get a valid
    cert (with a one-time browser warning) instead of ERR_CERT_COMMON_NAME_INVALID.
    """
    TLS_DIR.mkdir(parents=True, exist_ok=True)

    # 1. Try Tailscale cert (real Let's Encrypt, trusted everywhere, no CA install)
    ts_hostname = _get_tailscale_hostname()
    if ts_hostname:
        ts_cert = TLS_DIR / f"{ts_hostname}.crt"
        ts_key = TLS_DIR / f"{ts_hostname}.key"
        got_ts = ts_cert.exists() and ts_key.exists()
        if not got_ts:
            for ts_bin in ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "tailscale"]:
                try:
                    print(f"\033[2m  Getting Tailscale cert for {ts_hostname}...\033[0m")
                    r = subprocess.run(
                        [ts_bin, "cert", "--cert-file", str(ts_cert), "--key-file", str(ts_key), ts_hostname],
                        capture_output=True, text=True, timeout=30,
                    )
                    if r.returncode == 0 and ts_cert.exists():
                        got_ts = True
                        break
                except Exception:
                    continue
        if got_ts:
            # Build a fallback self-signed ctx for raw-IP connections
            ts_ips = _get_tailscale_ips()
            fb_cert, fb_key = _ensure_self_signed(lan_ip, ts_ips)
            fb_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            fb_ctx.load_cert_chain(fb_cert, fb_key)
            return str(ts_cert), str(ts_key), ts_hostname, fb_ctx

    # 2. Try mkcert (locally-trusted, no browser warnings on same machine)
    cert_file = TLS_DIR / "cert.pem"
    key_file = TLS_DIR / "key.pem"
    if cert_file.exists() and key_file.exists():
        return str(cert_file), str(key_file), "", None

    if subprocess.run(["which", "mkcert"], capture_output=True).returncode == 0:
        print(f"\033[2m  Generating trusted TLS cert with mkcert...\033[0m")
        subprocess.run(
            ["mkcert", "-cert-file", str(cert_file), "-key-file", str(key_file),
             "localhost", "127.0.0.1", lan_ip],
            capture_output=True, check=True,
        )
        return str(cert_file), str(key_file), "", None

    # 3. Fallback: self-signed via openssl
    fb_cert, fb_key = _ensure_self_signed(lan_ip)
    return fb_cert, fb_key, "", None


# ── Main ──

def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8822
    lan_ip = get_lan_ip()
    no_tls = "--no-tls" in sys.argv

    # Initialize SQLite and migrate flat-file data on first run
    _init_db()
    _migrate_flat_to_sqlite()

    server = ResilientHTTPSServer(("0.0.0.0", port), CCHandler)

    scheme = "http"
    ts_hostname = ""
    if not no_tls:
        try:
            cert, key, ts_hostname, fb_ctx = _ensure_tls(lan_ip)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(cert, key)
            # SNI callback: use Tailscale cert for hostname, fallback for IPs
            if ts_hostname and fb_ctx:
                def _sni_cb(sock, server_name, orig_ctx):
                    if server_name != ts_hostname:
                        sock.context = fb_ctx
                ctx.sni_callback = _sni_cb
            server.ssl_ctx = ctx  # per-connection TLS in process_request_thread()
            scheme = "https"
        except Exception as e:
            print(f"\033[33m  TLS setup failed ({e}), falling back to HTTP\033[0m")

    print(f"\033[1m\033[34mamux\033[0m web dashboard running")
    print(f"  Local:   {scheme}://localhost:{port}")
    if ts_hostname:
        print(f"  Tailscale: {scheme}://{ts_hostname}:{port}")
    print(f"  Network: {scheme}://{lan_ip}:{port}")
    if ts_hostname:
        print(f"\n  Open on your phone → \033[1m{scheme}://{ts_hostname}:{port}\033[0m")
    else:
        print(f"\n  Open on your phone → {scheme}://{lan_ip}:{port}")
    if scheme == "https":
        if ts_hostname:
            print(f"\033[32m  ✓ Tailscale HTTPS — trusted cert, no setup needed on phone\033[0m")
        else:
            print(f"\033[32m  ✓ HTTPS enabled — service worker & offline mode will work\033[0m")
    else:
        print(f"\033[33m  ⚠ HTTP only — offline mode requires HTTPS on non-localhost\033[0m")
    print(f"\033[2m  Auto-reload active — editing amux-server.py will restart\033[0m")
    print(f"\n\033[2mPress Ctrl-C to stop\033[0m")

    # Plain HTTP cert server (so phones can fetch cert before trusting it)
    if scheme == "https":
        def _cert_server(port):
            from http.server import HTTPServer, BaseHTTPRequestHandler
            class H(BaseHTTPRequestHandler):
                def do_GET(self):
                    cert_path = TLS_DIR / "cert.pem"
                    if self.path.rstrip("/") == "/api/cert" and cert_path.exists():
                        body = cert_path.read_bytes()
                        self.send_response(200)
                        self.send_header("Content-Type", "application/x-pem-file")
                        self.send_header("Content-Disposition", 'attachment; filename="amux.pem"')
                        self.send_header("Content-Length", str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                    else:
                        self.send_response(301)
                        self.send_header("Location", f"https://{self.headers.get('Host', 'localhost').split(':')[0]}:{port}/")
                        self.end_headers()
                def log_message(self, *a): pass
            class IPv4HTTPServer(HTTPServer):
                address_family = socket.AF_INET
            IPv4HTTPServer(("0.0.0.0", port + 1), H).serve_forever()
        threading.Thread(target=_cert_server, args=(port,), daemon=True).start()
        print(f"  Cert:    http://{lan_ip}:{port + 1}/api/cert")

    # Start file watcher thread
    watcher = threading.Thread(target=_watch_self, args=(server,), daemon=True)
    watcher.start()

    # Start session log snapshot thread
    snapshotter = threading.Thread(target=_snapshot_loop, daemon=True)
    snapshotter.start()
    # Initial snapshot immediately
    threading.Thread(target=_snapshot_all_sessions, daemon=True).start()

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n\033[2mStopped.\033[0m")
        server.server_close()


if __name__ == "__main__":
    main()
