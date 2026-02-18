#!/usr/bin/env python3
"""cmux serve — web dashboard for Claude Code session manager."""

# ═══════════════════════════════════════════
# CONFIGURATION & GLOBALS
# ═══════════════════════════════════════════

import json
import os
import re
import shlex
import socket
import ssl
import subprocess
import sys
import threading
import time
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from pathlib import Path
from urllib.parse import urlparse, parse_qs

# Support both ~/.cmux (new) and ~/.cc (old) for migration
_cmux_home = Path.home() / ".cmux"
_cc_home = Path.home() / ".cc"
if not _cmux_home.exists() and _cc_home.exists():
    _cc_home.rename(_cmux_home)
CC_HOME = Path(os.environ.get("CC_HOME", _cmux_home))
CC_SESSIONS = CC_HOME / "sessions"
CC_LOGS = CC_HOME / "logs"
CC_LOGS.mkdir(parents=True, exist_ok=True)
CLAUDE_HOME = Path.home() / ".claude"
MAX_LOG_BYTES = 10 * 1024 * 1024  # 10MB per session

# SSE shared cache — avoids redundant subprocess calls when multiple tabs connect
_sse_cache = {
    "sessions": {"data": None, "json": "", "time": 0},
    "board": {"data": None, "json": "", "time": 0},
}
_SSE_CACHE_TTL = 2  # seconds

# Per-session token cache — refreshed every 30s, keyed by resolved dir
_token_cache = {"data": {}, "time": 0}
_TOKEN_CACHE_TTL = 30

def _refresh_token_cache():
    """Rebuild per-directory token counts from Claude JSONL files."""
    now = time.time()
    if now - _token_cache["time"] < _TOKEN_CACHE_TTL:
        return
    from datetime import datetime
    result = {}
    projects_dir = Path.home() / ".claude" / "projects"
    if not projects_dir.is_dir():
        _token_cache["data"] = result
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
    _token_cache["data"] = result
    _token_cache["time"] = now

# ═══════════════════════════════════════════
# SESSION FILE HELPERS
# ═══════════════════════════════════════════

def parse_env_file(path: Path) -> dict:
    """Parse a cmux session .env file into a dict."""
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
    """Write a cfg dict back to a cmux .env file."""
    lines = [f'# updated: {__import__("datetime").datetime.now().isoformat()}']
    for k, v in cfg.items():
        lines.append(f'{k}="{v}"')
    path.write_text("\n".join(lines) + "\n")


# ═══════════════════════════════════════════
# TMUX HELPERS
# ═══════════════════════════════════════════

def tmux_name(session: str) -> str:
    # Migrate cc-* → cmux-* if old name exists
    old = f"cc-{session}"
    new = f"cmux-{session}"
    try:
        r = subprocess.run(["tmux", "has-session", "-t", old], capture_output=True)
        if r.returncode == 0:
            subprocess.run(["tmux", "rename-session", "-t", old, new], capture_output=True, timeout=5)
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
# BOARD PERSISTENCE
# ═══════════════════════════════════════════

_BOARD_FILE = CC_HOME / "board.json"


def _load_board() -> list:
    if _BOARD_FILE.exists():
        try:
            return json.loads(_BOARD_FILE.read_text()).get("items", [])
        except Exception:
            pass
    return []


def _save_board(items: list):
    counters = {}
    if _BOARD_FILE.exists():
        try:
            counters = json.loads(_BOARD_FILE.read_text()).get("counters", {})
        except Exception:
            pass
    _BOARD_FILE.write_text(json.dumps({"items": items, "counters": counters}))


def get_daily_token_stats() -> dict:
    """Get today's token usage across all Claude Code sessions and cmux sessions."""
    from datetime import datetime, timezone
    today = datetime.now().strftime("%Y-%m-%d")
    projects_dir = CLAUDE_HOME / "projects"
    if not projects_dir.is_dir():
        return {"today": today, "total_tokens": 0, "total_input": 0, "total_output": 0, "sessions": []}

    # Get cmux session dirs for labeling (multiple sessions can share a dir)
    cmux_dirs = {}  # resolved_dir -> list of session names
    for f in CC_SESSIONS.glob("*.env"):
        cfg = parse_env_file(f)
        d = cfg.get("CC_DIR", "")
        if d:
            resolved = str(Path(d).expanduser().resolve()).replace("/", "-")
            cmux_dirs.setdefault(resolved, []).append(f.stem)

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
            cmux_names = cmux_dirs.get(proj_name, [])
            if cmux_names:
                label = ", ".join(sorted(cmux_names))
            else:
                # Show short path: ~/Dev/project instead of /Users/ethan/Dev/project
                # proj_name is like "-Users-ethan-Dev" → "/Users/ethan/Dev"
                full = "/" + proj_name.lstrip("-").replace("-", "/")
                home = str(Path.home())
                label = "~" + full[len(home):] if full.startswith(home) else full
            session_stats.append({
                "name": label,
                "proj_dir": proj_name,
                "cmux": bool(cmux_names),
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
    cmux_tokens = sum(s["total"] for s in session_stats if s["cmux"])
    return {
        "today": today,
        "total_tokens": total_in + total_out,
        "total_input": total_in,
        "total_output": total_out,
        "cmux_tokens": cmux_tokens,
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

    # ── 4. Fallback: check for prompt character ──
    tail = "\n".join(lines[-5:])
    if "\u276f" in tail or "$ " in tail:
        return "idle"
    return ""


def _tmux_info_map() -> dict:
    """Get activity, creation time, and pane title for all tmux sessions."""
    result = {}
    try:
        r = subprocess.run(
            ["tmux", "list-panes", "-a", "-F",
             "#{session_name}\t#{session_activity}\t#{session_created}\t#{pane_title}"],
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
        last_activity = tinfo.get("activity", 0)
        # For stopped sessions, fall back to log file mtime, then .env mtime
        if not last_activity:
            lp = _log_path(name)
            try:
                last_activity = int((lp if lp.exists() else f).stat().st_mtime)
            except Exception:
                pass
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
        # Token count from background cache
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
    """Find the most recent Claude Code session ID for a working directory."""
    resolved = str(Path(work_dir).expanduser().resolve())
    project_name = resolved.replace("/", "-")
    project_dir = CLAUDE_HOME / "projects" / project_name
    if not project_dir.is_dir():
        return ""
    jsonl_files = sorted(project_dir.glob("*.jsonl"), key=lambda f: f.stat().st_mtime, reverse=True)
    if not jsonl_files:
        return ""
    return jsonl_files[0].stem  # filename without .jsonl = session UUID


def start_session(name: str, extra_flags: str = "") -> tuple[bool, str]:
    """Start a session headless (no attach). Returns (success, message)."""
    f = CC_SESSIONS / f"{name}.env"
    if not f.exists():
        return False, f"session '{name}' not found"
    if is_running(name):
        return True, "already running"
    cfg = parse_env_file(f)
    work_dir = str(Path(cfg.get("CC_DIR", str(Path.home()))).expanduser().resolve())
    flags = cfg.get("CC_FLAGS", "")

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
    if extra_flags:
        cmd += f" {extra_flags}"

    try:
        tmux_sess = tmux_name(name)
        # Create session with window naming options set upfront so Claude
        # cannot override the window title via terminal escape sequences.
        # Clear CLAUDECODE so nested-session detection doesn't block Claude
        # Source user profile to ensure PATH includes ~/.local/bin (where claude lives)
        # Then cd back to work_dir since the profile may override CWD (e.g. cd ~/Dev)
        shell_rc = ""
        for rc in [Path.home() / ".zprofile", Path.home() / ".bash_profile", Path.home() / ".profile"]:
            if rc.exists():
                shell_rc = f"source {rc} 2>/dev/null; cd {shlex.quote(work_dir)}; "
                break
        if not shell_rc:
            shell_rc = f"cd {shlex.quote(work_dir)}; "
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
    """List all tmux sessions with their working dirs, excluding already-registered cmux sessions."""
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
<title>cmux</title>
<link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' rx='20' fill='%230d1117'/><text x='50' y='68' font-family='system-ui' font-size='40' font-weight='700' fill='%2358a6ff' text-anchor='middle'>cmux</text></svg>">
<link rel="apple-touch-icon" href="/icon-192.png">
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

  /* Session cards */
  .cards { display: grid; grid-template-columns: 1fr; gap: 10px; }
  @media (min-width: 768px) { .cards { grid-template-columns: 1fr 1fr; } }

  /* Tile mode (desktop only) */
  .tile-controls { display: none; }
  @media (min-width: 900px) { .tile-controls { display: flex; gap: 4px; align-items: center; } }
  .tile-btn { width: 28px; height: 28px; border-radius: 6px; border: 1px solid var(--border); background: transparent; color: var(--dim); cursor: pointer; font-size: 0.85rem; display: flex; align-items: center; justify-content: center; }
  .tile-btn:hover { border-color: var(--accent); color: var(--text); }
  .tile-btn.active { background: var(--accent); color: #000; border-color: var(--accent); }
  .cards.tile-mode { display: block; position: relative; min-height: 80vh; }
  .cards.tile-mode .card { position: absolute; width: 340px; }
  .cards.tile-mode .card-header { cursor: grab; }
  .tile-dragging { box-shadow: 0 8px 32px rgba(0,0,0,0.5) !important; z-index: 10 !important; cursor: grabbing !important; }
  .tile-dragging .card-header { cursor: grabbing !important; }

  .card {
    background: var(--card); border: 1px solid var(--border);
    border-radius: 10px; padding: 14px 16px; cursor: pointer;
    transition: border-color 0.15s; overflow: hidden;
    -webkit-tap-highlight-color: transparent; min-width: 0;
  }
  .card:active { border-color: var(--accent); }
  .card-header { display: flex; align-items: center; gap: 10px; position: relative; min-width: 0; }
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
  .edit-box input, .edit-box select {
    width: 100%; font-size: 1rem; padding: 10px 12px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--bg); color: var(--text);
    outline: none; margin-bottom: 14px;
  }
  .edit-box input:focus, .edit-box select:focus { border-color: var(--accent); }
  .edit-box .edit-actions { display: flex; gap: 8px; justify-content: flex-end; }
  .dot {
    width: 10px; height: 10px; border-radius: 50%; flex-shrink: 0;
  }
  .dot.running { background: var(--green); box-shadow: 0 0 6px var(--green); }
  .dot.stopped { background: var(--red); opacity: 0.5; }
  .card-name { font-weight: 600; font-size: 1.05rem; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .card-dir { color: var(--dim); font-size: 0.82rem; margin-top: 4px; margin-left: 20px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
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
  .send-row { display: flex; gap: 8px; }
  .send-input {
    flex: 1; font-size: 1rem; padding: 10px 14px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--bg); color: var(--text);
    outline: none; min-height: 44px; max-height: calc(1.4em * 10 + 28px);
    resize: none; overflow-y: auto; line-height: 1.4;
    font-family: inherit; field-sizing: content;
  }
  .send-input:focus { border-color: var(--accent); }

  /* Peek overlay */
  .overlay {
    position: fixed; top: 0; left: 0; right: 0;
    height: 100%; height: 100dvh;
    background: var(--bg);
    z-index: 100; flex-direction: column;
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
  .file-overlay-header h2 { font-size: 1rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; margin-right: 8px; }
  .file-overlay-body {
    flex: 1; overflow: auto; background: #010409; border: 1px solid var(--border); border-radius: 8px;
    padding: 14px; font-family: "SF Mono", "Fira Code", "Cascadia Code", monospace;
    font-size: 0.78rem; line-height: 1.5; white-space: pre-wrap;
    word-break: break-word; -webkit-overflow-scrolling: touch;
  }
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
  .explore-name { font-size: 0.88rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; }
  .explore-size { font-size: 0.72rem; color: var(--dim); flex-shrink: 0; }

  /* Connect session list */
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
  .search-input:focus { border-color: var(--accent); }
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
  .settings-row input:focus { border-color: var(--accent); }
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
  .peek-cmd-row.open { display: flex; }
  .peek-cmd-row .send-input { font-size: 0.85rem; padding: 8px 12px; min-height: 36px; }
  .peek-cmd-row .btn { min-height: 36px; padding: 6px 12px; font-size: 0.82rem; }

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
  .conn-status.offline {
    color: #f87171; background: rgba(248,113,113,0.15);
  }
  .conn-status.offline::before { background: #f87171; }

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
    display: flex; gap: 12px; overflow-x: auto;
    -webkit-overflow-scrolling: touch; padding-bottom: 16px; align-items: flex-start;
    min-height: 200px;
  }
  .board-columns::-webkit-scrollbar { display: none; }
  .board-col {
    flex: 1; min-width: 200px; max-width: 320px;
    display: flex; flex-direction: column; gap: 6px;
    background: rgba(255,255,255,0.02); border-radius: 10px; padding: 10px 8px;
    min-height: 80px;
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
  }
  .board-card:active { border-color: var(--accent); box-shadow: 0 0 0 1px rgba(88,166,255,0.2); }
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
  .board-card.dragging { opacity: 0.35; }
  .board-col.drag-over { background: rgba(88,166,255,0.06); outline: 1px dashed rgba(88,166,255,0.3); }
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
  .board-session-group { margin-bottom: 8px; }
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
  .tag-group-body { padding: 0 0 4px 0; }
  .board-session-items .board-card .board-status-dot {
    display: inline-block; width: 7px; height: 7px; border-radius: 50%; margin-right: 6px; flex-shrink: 0;
  }
  .board-status-dot.todo { background: var(--dim); }
  .board-status-dot.doing { background: var(--yellow); }
  .board-status-dot.done { background: var(--green); }
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
  .board-detail-status-btn.active-todo { background: rgba(139,148,158,0.15); color: var(--text); border-color: var(--dim); }
  .board-detail-status-btn.active-doing { background: rgba(210,153,34,0.15); color: var(--yellow); border-color: var(--yellow); }
  .board-detail-status-btn.active-done { background: rgba(63,185,80,0.15); color: var(--green); border-color: var(--green); }
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
    position: fixed; inset: 0; z-index: 100;
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
  .board-edit-box input, .board-edit-box textarea {
    width: 100%; padding: 8px 10px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--bg); color: var(--text);
    font-size: 0.85rem; font-family: inherit; margin-bottom: 8px;
  }
  .board-edit-box textarea { resize: vertical; min-height: 60px; }
  .board-edit-box select {
    width: 100%; padding: 8px 10px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--bg); color: var(--text);
    font-size: 0.85rem; margin-bottom: 8px;
  }
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
</style>
</head>
<body>

<div class="header-row">
  <div style="display:flex;gap:8px;align-items:center;">
    <h1 style="margin:0;cursor:pointer;" onclick="openAbout()">cmux</h1>
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
        <div class="settings-section" style="text-align:center;">
          <span style="font-size:0.7rem;color:var(--dim);cursor:pointer;" onclick="openAbout();closeSettings()">About cmux &amp; token stats</span>
        </div>
      </div>
    </div>
  </div>
</div>
<div class="tab-bar">
  <button id="tab-sessions" class="active" onclick="switchView('sessions')">Sessions</button>
  <button id="tab-board" onclick="switchView('board')">Board</button>
</div>
<div id="session-view">
<div style="padding:0 12px;margin-top:4px;display:flex;align-items:center;gap:8px;">
  <div class="search-wrap" id="search-wrap" style="flex:1;">
    <input class="search-input" id="search-input" type="text" placeholder="Search sessions..." autocomplete="off" autocorrect="off"
      oninput="searchQuery=this.value;document.getElementById('search-wrap').classList.toggle('has-value',!!this.value);render()">
    <button class="search-clear" onclick="event.stopPropagation();clearSearch()">&#x2715;</button>
  </div>
  <div class="tile-controls">
    <button class="tile-btn active" id="tile-grid-btn" onclick="setTileMode('grid')" title="Grid layout">&#x25A4;</button>
    <button class="tile-btn" id="tile-free-btn" onclick="setTileMode('tiles')" title="Free tiles">&#x2B1A;</button>
    <button class="tile-btn" id="tile-arrange-btn" onclick="autoArrangeTiles()" title="Auto-arrange" style="display:none;">&#x2725;</button>
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
<!-- Board card "add" small modal -->
<div id="board-edit-overlay" class="board-edit-overlay" onclick="if(event.target===this)closeBoardEdit()">
  <div class="board-edit-box">
    <input id="be-title" type="text" placeholder="Title" autocomplete="off"
      onkeydown="if(event.key==='Enter'){event.preventDefault();document.getElementById('be-desc').focus();}">
    <textarea id="be-desc" placeholder="Description (optional)"></textarea>
    <select id="be-session-add" style="width:100%;padding:8px 10px;border:1px solid var(--border);border-radius:6px;background:var(--card);color:var(--text);font-size:0.85rem;font-family:inherit;margin-bottom:0;"></select>
    <select id="be-status"><option value="todo">To Do</option><option value="doing">In Progress</option><option value="done">Done</option></select>
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
    <div class="board-detail-row">
      <span style="font-size:0.78rem;color:var(--dim);">Session:</span>
      <select id="bd-session" class="board-detail-session-select"></select>
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
    <input id="create-name" type="text" placeholder="Session name" autocomplete="off" autocorrect="off"
      onkeydown="if(event.key==='Enter'){event.preventDefault();document.getElementById('create-dir').focus({preventScroll:true});}">
    <div class="ac-wrap">
      <input id="create-dir" type="text" placeholder="/path/to/project" autocomplete="off" autocorrect="off"
        oninput="acFetch(this.value)" onfocus="acFetch(this.value)"
        onkeydown="acKeydown(event)">
      <div id="ac-list" class="ac-list"></div>
    </div>
    <textarea id="create-prompt" rows="3" placeholder="Initial prompt (optional — sent on start)"
      style="width:100%;font-size:0.85rem;padding:10px;border-radius:8px;border:1px solid var(--border);background:var(--card);color:var(--text);resize:vertical;font-family:inherit;"></textarea>
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
  <div class="overlay-header">
    <div style="display:flex;align-items:center;gap:8px;">
      <h2 id="peek-title">peek</h2>
      <span id="peek-conn-status" class="conn-status online"></span>
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
  <div style="position:relative;flex:1;min-height:0;">
    <div id="peek-body" class="overlay-body" style="position:absolute;inset:0;"></div>
    <button class="peek-copy-btn" id="peek-copy-btn" onclick="copyPeekContent()" title="Copy all">&#x2398; Copy</button>
  </div>
  <div id="peek-status" class="overlay-status"></div>
  <div class="peek-cmd-bar">
    <button class="peek-cmd-toggle" id="peek-cmd-toggle" onclick="togglePeekCmd()">&#x25B2; Send command</button>
    <div class="peek-cmd-row" id="peek-cmd-row" style="flex-wrap:wrap;">
      <div class="chips" style="width:100%;margin:0;">
        <div class="chip" onclick="peekQuickSend('/compact')">/compact</div>
        <div class="chip" onclick="peekQuickSend('/status')">/status</div>
        <div class="chip" onclick="peekQuickSend('/clear')">/clear</div>
        <div class="chip" onclick="peekQuickSend('/cost')">/cost</div>
        <div class="chip" onclick="peekQuickKeys('C-c')">Ctrl-C</div>
        <div class="chip" onclick="peekQuickKeys('Escape')">Esc</div>
        <div class="chip" onclick="peekQuickKeys('Enter')">Enter</div>
        <div class="chip" onclick="peekQuickKeys('Up')">&#x2191;</div>
        <div class="chip" onclick="peekQuickKeys('Down')">&#x2193;</div>
      </div>
      <div class="ac-wrap" style="flex:1;">
        <textarea class="send-input" id="peek-cmd-input" rows="1" placeholder="Type a command..."
          autocomplete="off" autocorrect="on" autocapitalize="sentences" spellcheck="true"
          enterkeyhint="enter" style="width:100%;"
          oninput="autoGrow(this);slashAcUpdate()" onkeydown="slashAcKeydown(event)"></textarea>
        <div id="slash-ac-list" class="ac-list slash-ac"></div>
      </div>
      <button class="btn primary" onclick="sendPeekCmd()">Send</button>
    </div>
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
    <select id="edit-select" style="display:none;width:100%;padding:8px 10px;border:1px solid var(--border);border-radius:6px;background:var(--card);color:var(--text);font-size:0.9rem;-webkit-appearance:menulist;" onchange="submitEdit()">
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
      <h3 style="margin:0 0 4px;">cmux</h3>
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

<!-- File preview overlay (z-index 300 so it stacks above explorer at 250) -->
<div id="file-overlay" class="file-overlay" style="z-index:300;">
  <div class="file-overlay-header">
    <h2 id="file-title">file</h2>
    <button class="btn" onclick="closeFilePreview()">&#x2715;</button>
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
let expanded = null;
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
// Migrate localStorage keys from cc_ to cmux_
['offline_queue','sessions_cache','drafts'].forEach(k => {
  const old = localStorage.getItem('cc_' + k);
  if (old && !localStorage.getItem('cmux_' + k)) {
    localStorage.setItem('cmux_' + k, old);
    localStorage.removeItem('cc_' + k);
  }
});
let offlineQueue = JSON.parse(localStorage.getItem('cmux_offline_queue') || '[]');
function saveQueue() {
  localStorage.setItem('cmux_offline_queue', JSON.stringify(offlineQueue));
  if (typeof _idb !== 'undefined') _idb.set('offline_queue', offlineQueue);
}

// ═══════ DEVICE NAME ═══════
function _getDeviceName() {
  const custom = localStorage.getItem('cmux_device_name');
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
let drafts = JSON.parse(localStorage.getItem('cmux_drafts') || '[]');
// Draft shape: { name, dir, prompt, created_at, syncing }

function saveDrafts() {
  localStorage.setItem('cmux_drafts', JSON.stringify(drafts));
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
  document.querySelectorAll('#conn-status, #peek-conn-status').forEach(el => {
    if (online) {
      el.className = 'conn-status online';
      el.textContent = 'Live';
    } else {
      el.className = 'conn-status offline';
      const total = offlineQueue.length + drafts.length;
      el.textContent = total ? total + ' pending' : 'Offline';
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
  updateConnectionStatus();
  if (!was && val) {
    showToast('Reconnected — syncing...');
    runSyncBanner();
    // Reconnect SSE if not in fallback mode
    if (!_sseFallback && !_sse) { _sseRetries = 0; connectSSE(); }
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
  if (!offlineQueue.length) return;
  setOnline(true);
}

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
    if (!online) setOnline(true);
    const j = JSON.stringify(data);
    if (j !== lastSessionsJSON) {
      lastSessionsJSON = j;
      sessions = data;
      localStorage.setItem('cmux_sessions_cache', j);
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
function render() {
  // Skip render if a menu or edit overlay is open to prevent DOM clobbering
  if (openMenu || editState || document.getElementById('edit-overlay').classList.contains('active')) return;
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
    el.innerHTML = '<div class="empty">No sessions yet.<br>Tap <strong>+</strong> to create one.' +
      (!online ? '<br><span style="color:var(--yellow)">You\'re offline — sessions created now will sync when connected.</span>' : '') + '</div>';
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
    const isExp = expanded === s.name;
    const flags = s.flags || '';
    const isYolo = flags.includes('--dangerously-skip-permissions');
    const modelMatch = flags.match(/--model\s+(\S+)/);
    const flagModel = modelMatch ? modelMatch[1] : null;
    const model = flagModel || s.active_model || null;
    const shortModel = model ? model.replace(/^claude-/, '').replace(/-\d{8}$/, '') : null;
    return `
    <div class="card ${isExp ? 'expanded' : ''}" data-session="${esc(s.name)}" onclick="toggle('${s.name}')">
      <div class="card-header" onclick="headerTap('${s.name}', event)" onmousedown="tileMouseDown(event,'${s.name}')">
        <div class="dot ${s.running ? 'running' : 'stopped'}"></div>
        <div class="card-name">${s.pinned ? '<span class="pin-icon">&#x1F4CC;</span> ' : ''}${esc(s.name)}</div>
        ${s.status === 'active' ? '<span class="status-badge active">working</span>' : ''}
        ${s.status === 'waiting' ? '<span class="status-badge waiting">needs input</span>' : ''}
        ${s.status === 'idle' ? '<span class="status-badge idle">idle</span>' : ''}
        ${s.tokens ? `<span class="token-count">${fmtTokens(s.tokens)}</span>` : ''}
        ${s.last_activity ? `<span class="last-active">${timeAgo(s.last_activity)}</span>` : ''}
        ${!online ? '<span class="cached-badge">cached</span>' : ''}
        <button class="card-menu-btn" onclick="event.stopPropagation();toggleMenu('${s.name}')" title="Options">&#x22EF;</button>
        <div class="card-menu" id="menu-${s.name}">
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
          <div class="card-menu-sep"></div>
          <div class="card-menu-item danger" onclick="event.stopPropagation();deleteSession('${s.name}')"><span class="mi">&#x2716;</span> Delete</div>
        </div>
      </div>
      ${s.dir ? `<div class="card-dir" onclick="event.stopPropagation();openExplore('${s.dir.replace(/'/g,"\\'")}')" style="cursor:pointer;" title="Browse files">${esc(s.dir)}</div>` : ''}
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
            oninput="autoGrow(this);cardSlashAcUpdate('${s.name}')"
            onkeydown="cardSlashAcKeydown('${s.name}',event)"></textarea>
          <button class="btn primary" onclick="sendFromInput('${s.name}')">Send</button>
        </div>` : ''}
      </div>
    </div>`;
  }

  // Group by tag if sessions have tags and no filter is active
  const anyTagged = filtered.some(s => s.tags && s.tags.length);
  if (anyTagged && !activeTag && !q) {
    const tagGroups = {};
    const untagged = [];
    filtered.forEach(s => {
      if (s.tags && s.tags.length) {
        s.tags.forEach(t => {
          if (!tagGroups[t]) tagGroups[t] = [];
          tagGroups[t].push(s);
        });
      } else {
        untagged.push(s);
      }
    });
    const sortedTags = Object.keys(tagGroups).sort((a, b) => tagGroups[b].length - tagGroups[a].length);
    // Default: largest group open, rest closed
    const largestTag = sortedTags[0];
    sortedTags.forEach(t => {
      if (_tagGroupCollapsed[t] === undefined) _tagGroupCollapsed[t] = (t !== largestTag);
    });
    if (untagged.length && _tagGroupCollapsed['__untagged__'] === undefined) _tagGroupCollapsed['__untagged__'] = true;

    let groupHtml = '';
    sortedTags.forEach(tag => {
      const items = tagGroups[tag];
      const col = _tagGroupCollapsed[tag];
      const runC = items.filter(s => s.running).length;
      const stopC = items.length - runC;
      groupHtml += `<div class="board-session-group">
        <div class="board-session-header" onclick="toggleTagGroup('${esc(tag)}')">
          <span class="board-session-chevron${col ? '' : ' open'}">&#x25B6;</span>
          <span class="board-session-name">${esc(tag)}</span>
          <div class="board-session-counts">
            ${runC ? `<span class="board-session-count doing">${runC} running</span>` : ''}
            ${stopC ? `<span class="board-session-count todo">${stopC} stopped</span>` : ''}
          </div>
        </div>
        ${!col ? `<div class="tag-group-body">${items.map(_renderSessionCard).join('')}</div>` : ''}
      </div>`;
    });
    if (untagged.length) {
      const col = _tagGroupCollapsed['__untagged__'];
      const runC = untagged.filter(s => s.running).length;
      const stopC = untagged.length - runC;
      groupHtml += `<div class="board-session-group">
        <div class="board-session-header" onclick="toggleTagGroup('__untagged__')">
          <span class="board-session-chevron${col ? '' : ' open'}">&#x25B6;</span>
          <span class="board-session-name" style="color:var(--dim)">Untagged</span>
          <div class="board-session-counts">
            ${runC ? `<span class="board-session-count doing">${runC} running</span>` : ''}
            ${stopC ? `<span class="board-session-count todo">${stopC} stopped</span>` : ''}
          </div>
        </div>
        ${!col ? `<div class="tag-group-body">${untagged.map(_renderSessionCard).join('')}</div>` : ''}
      </div>`;
    }
    el.innerHTML = draftCards + groupHtml;
  } else {
    el.innerHTML = draftCards + filtered.map(_renderSessionCard).join('');
  }

  // Restore input values and focus after re-rendering
  for (const [id, val] of Object.entries(savedInputs)) {
    const inp = document.getElementById(id);
    if (inp) inp.value = val;
  }
  if (focusedId) {
    const inp = document.getElementById(focusedId);
    if (inp) inp.focus({ preventScroll: true });
  }

  // Apply tile positions after render
  if (tileMode === 'tiles' && window.innerWidth >= 900) {
    requestAnimationFrame(applyTilePositions);
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
  expanded = expanded === name ? null : name;
  closeAllMenus();
  render();
  if (expanded) {
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

async function deleteSession(session) {
  closeAllMenus();
  if (!confirm('Delete session "' + session + '"?')) return;
  await apiCall(API + '/api/sessions/' + session + '/delete', { method: 'POST' });
  if (expanded === session) expanded = null;
  await fetchSessions();
}

async function doStart(name) {
  const r = await apiCall(API + '/api/sessions/' + name + '/start', { method: 'POST' });
  if (!r) return;
  const data = await r.json();
  if (!data.ok) { alert('Failed to start: ' + (data.message || data.error || 'unknown error')); return; }
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
  inp.value = '';
  inp.style.height = 'auto';
  await doSend(name, text);
  inp.style.borderColor = 'var(--green)';
  setTimeout(() => { inp.style.borderColor = ''; }, 400);
}

function openPeek(name) {
  if (peekTimer) { clearInterval(peekTimer); peekTimer = null; }
  peekSession = name;
  peekSessionDir = (sessions.find(s => s.name === name) || {}).dir || '';
  peekSearchQuery = '';
  lastPeekHTML = '';
  const searchInp = document.getElementById('peek-search');
  if (searchInp) searchInp.value = '';
  const draft = _peekDrafts[name] || '';
  const cmdInp = document.getElementById('peek-cmd-input');
  cmdInp.value = draft;
  autoGrow(cmdInp);
  if (draft) {
    peekCmdOpen = true;
    document.getElementById('peek-cmd-row').classList.add('open');
    document.getElementById('peek-cmd-toggle').innerHTML = '&#x25BC; Send command';
  } else {
    peekCmdOpen = false;
    document.getElementById('peek-cmd-row').classList.remove('open');
    document.getElementById('peek-cmd-toggle').innerHTML = '&#x25B2; Send command';
  }
  document.getElementById('peek-title').textContent = name;
  document.getElementById('peek-body').innerHTML = '<span style="color:var(--dim)">Loading...</span>';
  updateConnectionStatus();
  document.getElementById('peek-overlay').classList.add('active');
  document.body.style.overflow = 'hidden';
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
  document.getElementById('peek-overlay').classList.remove('active');
  document.body.style.overflow = '';
  if (peekTimer) { clearInterval(peekTimer); peekTimer = null; }
}

// Swipe right to close peek
(function() {
  const el = document.getElementById('peek-overlay');
  let sx = 0, sy = 0, tracking = false;
  el.addEventListener('touchstart', e => {
    const t = e.touches[0];
    sx = t.clientX; sy = t.clientY; tracking = true;
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
let peekCmdOpen = false;
function togglePeekCmd() {
  peekCmdOpen = !peekCmdOpen;
  const row = document.getElementById('peek-cmd-row');
  const toggle = document.getElementById('peek-cmd-toggle');
  row.classList.toggle('open', peekCmdOpen);
  toggle.innerHTML = peekCmdOpen ? '&#x25BC; Send command' : '&#x25B2; Send command';
  if (peekCmdOpen) setTimeout(() => document.getElementById('peek-cmd-input').focus({ preventScroll: true }), 50);
}
async function sendPeekCmd() {
  if (!peekSession) return;
  const inp = document.getElementById('peek-cmd-input');
  const text = inp.value.trim();
  if (!text) return;
  inp.value = '';
  inp.style.height = 'auto';
  delete _peekDrafts[peekSession];
  await doSend(peekSession, text);
  inp.style.borderColor = 'var(--green)';
  setTimeout(() => { inp.style.borderColor = ''; }, 400);
  // Refresh peek to show result
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
  const el = document.getElementById('slash-ac-list');
  if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); sendPeekCmd(); return; }
  if (!el.classList.contains('open')) {
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
  const el = document.getElementById('card-ac-' + name);
  if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); sendFromInput(name); return; }
  if (!el || !el.classList.contains('open')) {
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
  localStorage.setItem('cmux_tag_collapsed', JSON.stringify(_tagGroupCollapsed));
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
async function openFilePreview(path) {
  document.getElementById('file-title').textContent = path.split('/').pop();
  document.getElementById('file-body').textContent = 'Loading...';
  document.getElementById('file-body').className = 'file-overlay-body';
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
    const body = document.getElementById('file-body');
    if (data.is_markdown) {
      body.className = 'file-overlay-body markdown';
      body.innerHTML = renderMarkdown(data.content);
    } else {
      body.className = 'file-overlay-body';
      body.textContent = data.content;
    }
  } catch(e) {
    document.getElementById('file-body').textContent = 'Failed to load file.';
  }
}

function closeFilePreview() {
  document.getElementById('file-overlay').classList.remove('active');
}

// ═══════ FILE EXPLORER ═══════
let _explorePath = '';
let _exploreShowHidden = false;
function openExplore(startPath) {
  _explorePath = startPath || '/';
  document.getElementById('explore-overlay').classList.add('active');
  document.body.style.overflow = 'hidden';
  loadExplore(_explorePath);
}
function closeExplore() {
  document.getElementById('explore-overlay').classList.remove('active');
  document.body.style.overflow = '';
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
      row.innerHTML = `<span class="explore-icon">${icon}</span><span class="explore-name">${esc(displayName)}</span><span class="explore-size">${esc(_fmtSize(entry.size))}</span>`;
      const entryPath = path.replace(/\/$/, '') + '/' + entry.name;
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
document.addEventListener('mouseup', () => { peekCheckSelection(); });
document.addEventListener('touchend', () => { peekCheckSelection(); });

// Handle Ctrl+C as copy and Ctrl+V as paste in peek (Mac users may use Ctrl instead of Cmd)
document.addEventListener('keydown', (e) => {
  if (document.getElementById('board-detail-overlay').classList.contains('active')) {
    if (e.key === 'Escape') { e.preventDefault(); closeBoardDetail(); return; }
    if ((e.metaKey || e.ctrlKey) && e.key === 's') { e.preventDefault(); boardDetailSave(); return; }
    return;
  }
  if (!document.getElementById('peek-overlay').classList.contains('active')) return;
  if (e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey) {
    if (e.key === 'c') {
      const sel = window.getSelection();
      if (sel && sel.toString().length > 0) {
        navigator.clipboard.writeText(sel.toString());
        e.preventDefault();
      }
    } else if (e.key === 'v') {
      const active = document.activeElement;
      if (active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA')) {
        navigator.clipboard.readText().then(text => {
          const start = active.selectionStart;
          const end = active.selectionEnd;
          active.value = active.value.slice(0, start) + text + active.value.slice(end);
          active.selectionStart = active.selectionEnd = start + text.length;
          active.dispatchEvent(new Event('input'));
        });
        e.preventDefault();
      }
    }
  }
});

// ═══════ DRAGGABLE TILES ═══════
let tileMode = localStorage.getItem('cmux_layout') || 'grid';
let tilePositions = JSON.parse(localStorage.getItem('cmux_tile_positions') || '{}');
let _tileDragging = null; // {name, card, startX, startY, origX, origY}
let _tileJustDragged = false;

function setTileMode(mode) {
  tileMode = mode;
  localStorage.setItem('cmux_layout', mode);
  const cards = document.querySelector('.cards');
  if (!cards) return;
  document.getElementById('tile-grid-btn').classList.toggle('active', mode === 'grid');
  document.getElementById('tile-free-btn').classList.toggle('active', mode === 'tiles');
  document.getElementById('tile-arrange-btn').style.display = mode === 'tiles' ? '' : 'none';
  if (mode === 'tiles') {
    cards.classList.add('tile-mode');
    applyTilePositions();
  } else {
    cards.classList.remove('tile-mode');
    cards.querySelectorAll('.card').forEach(c => { c.style.left = ''; c.style.top = ''; });
  }
}

function applyTilePositions() {
  const cards = document.querySelector('.cards');
  if (!cards) return;
  const allCards = cards.querySelectorAll('.card[data-session]');
  const cols = Math.max(1, Math.floor(cards.clientWidth / 360));
  let maxBottom = 0;
  allCards.forEach((card, i) => {
    const name = card.dataset.session;
    if (tilePositions[name]) {
      card.style.left = tilePositions[name].x + 'px';
      card.style.top = tilePositions[name].y + 'px';
    } else {
      const col = i % cols;
      const row = Math.floor(i / cols);
      card.style.left = col * 360 + 'px';
      card.style.top = row * 200 + 'px';
    }
    const bottom = parseFloat(card.style.top) + card.offsetHeight;
    if (bottom > maxBottom) maxBottom = bottom;
  });
  cards.style.minHeight = (maxBottom + 40) + 'px';
}

function autoArrangeTiles() {
  tilePositions = {};
  localStorage.removeItem('cmux_tile_positions');
  applyTilePositions();
}

function tileMouseDown(e, name) {
  if (tileMode !== 'tiles' || window.innerWidth < 900) return;
  // Only start drag from header, ignore buttons
  if (e.target.closest('.card-menu-btn') || e.target.closest('.btn') || e.target.closest('button')) return;
  if (e.button !== 0) return;
  const card = e.target.closest('.card');
  if (!card) return;
  _tileDragging = {
    name,
    card,
    startX: e.clientX,
    startY: e.clientY,
    origX: parseFloat(card.style.left) || 0,
    origY: parseFloat(card.style.top) || 0,
    moved: false,
  };
  e.preventDefault();
}

document.addEventListener('mousemove', function(e) {
  if (!_tileDragging) return;
  const dx = e.clientX - _tileDragging.startX;
  const dy = e.clientY - _tileDragging.startY;
  if (!_tileDragging.moved && Math.abs(dx) + Math.abs(dy) > 5) {
    _tileDragging.moved = true;
    _tileDragging.card.classList.add('tile-dragging');
  }
  if (_tileDragging.moved) {
    const container = _tileDragging.card.parentElement;
    const newX = Math.max(0, Math.min(_tileDragging.origX + dx, container.clientWidth - _tileDragging.card.offsetWidth));
    const newY = Math.max(0, _tileDragging.origY + dy);
    _tileDragging.card.style.left = newX + 'px';
    _tileDragging.card.style.top = newY + 'px';
  }
});

document.addEventListener('mouseup', function(e) {
  if (!_tileDragging) return;
  _tileDragging.card.classList.remove('tile-dragging');
  if (_tileDragging.moved) {
    _tileJustDragged = true;
    tilePositions[_tileDragging.name] = {
      x: parseFloat(_tileDragging.card.style.left) || 0,
      y: parseFloat(_tileDragging.card.style.top) || 0,
    };
    localStorage.setItem('cmux_tile_positions', JSON.stringify(tilePositions));
    // Update container height
    const cards = _tileDragging.card.parentElement;
    let maxBottom = 0;
    cards.querySelectorAll('.card').forEach(c => {
      const b = parseFloat(c.style.top || 0) + c.offsetHeight;
      if (b > maxBottom) maxBottom = b;
    });
    cards.style.minHeight = (maxBottom + 40) + 'px';
  }
  _tileDragging = null;
});

// Initialize tile mode on load
document.addEventListener('DOMContentLoaded', function() {
  if (tileMode === 'tiles' && window.innerWidth >= 900) {
    setTimeout(() => setTileMode('tiles'), 100);
  } else {
    // Ensure buttons reflect initial state
    const gridBtn = document.getElementById('tile-grid-btn');
    const freeBtn = document.getElementById('tile-free-btn');
    if (gridBtn) gridBtn.classList.add('active');
    if (freeBtn) freeBtn.classList.remove('active');
  }
});

// ═══════ BOARD ═══════
let activeView = 'sessions';
let boardItems = [];
let boardTimer = null;
let boardEditId = null;
let boardEditStatus = 'todo';
let lastBoardJSON = '';
let boardFilterTag = null;
let boardFilterSession = null;
let boardSearchQuery = '';
let _boardDragId = null;
let boardViewMode = localStorage.getItem('cmux_board_view') || 'session';
let _sessionGroupCollapsed = JSON.parse(localStorage.getItem('cmux_board_collapsed') || '{}');
let _tagGroupCollapsed = JSON.parse(localStorage.getItem('cmux_tag_collapsed') || '{}');

function switchView(view) {
  activeView = view;
  document.getElementById('session-view').style.display = view === 'sessions' ? '' : 'none';
  document.getElementById('board-view').style.display = view === 'board' ? '' : 'none';
  document.getElementById('tab-sessions').classList.toggle('active', view === 'sessions');
  document.getElementById('tab-board').classList.toggle('active', view === 'board');
  if (view === 'board') {
    renderBoard();
    fetchBoard();
    // Only poll if SSE is not active (SSE pushes board updates)
    if (_sseFallback && !boardTimer) boardTimer = setInterval(fetchBoard, 5000);
  } else {
    if (boardTimer) { clearInterval(boardTimer); boardTimer = null; }
  }
}

async function fetchBoard() {
  try {
    const r = await fetch(API + '/api/board');
    const data = await r.json();
    consecutiveFailures = 0;
    if (!online) setOnline(true);
    const j = JSON.stringify(data);
    if (j !== lastBoardJSON) {
      lastBoardJSON = j;
      boardItems = data;
      localStorage.setItem('cmux_board_cache', j);
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
    s = s.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
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
  localStorage.setItem('cmux_board_view', mode);
  renderBoard();
}

function toggleSessionGroup(name) {
  _sessionGroupCollapsed[name] = !_sessionGroupCollapsed[name];
  localStorage.setItem('cmux_board_collapsed', JSON.stringify(_sessionGroupCollapsed));
  renderBoard();
}

function _renderBoardCard(item) {
  const tags = item.tags || [];
  const firstLine = (item.desc || '').split('\n')[0].slice(0, 80);
  let h = '<div class="board-card" data-id="' + item.id + '" draggable="true" ondragstart="boardDragStart(event,\'' + item.id + '\')" ondragend="boardDragEnd()" onclick="openBoardDetail(\'' + item.id + '\')">';
  if (item.key) h += '<div class="board-card-key">' + esc(item.key) + '</div>';
  h += '<div class="board-card-title">';
  if (boardViewMode === 'session') h += '<span class="board-status-dot ' + (item.status || 'todo') + '"></span>';
  h += esc(item.title) + '</div>';
  if (firstLine) h += '<div class="board-card-desc">' + esc(firstLine) + ((item.desc || '').length > 80 ? '\u2026' : '') + '</div>';
  h += '<div class="board-card-footer">';
  if (boardViewMode !== 'session' && item.session) h += '<span class="board-card-session" data-session="' + esc(item.session) + '">' + esc(item.session) + '</span>';
  tags.forEach(function(t) { h += '<span class="board-card-tag" data-tag="' + esc(t) + '">' + esc(t) + '</span>'; });
  h += '<span class="board-card-time">' + timeAgo(item.updated || item.created) + '</span>';
  if (item.creator) h += '<span class="board-card-time">' + esc(item.creator) + '</span>';
  h += '</div></div>';
  return h;
}

function _renderBoardBySession(visible, container) {
  // Group by session
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

  let html = '';
  sessionNames.forEach(function(name) {
    const items = name ? groups[name] : noSession;
    const collapsed = _sessionGroupCollapsed[name || '__none__'];
    const todoC = items.filter(function(i) { return i.status === 'todo'; }).length;
    const doingC = items.filter(function(i) { return i.status === 'doing'; }).length;
    const doneC = items.filter(function(i) { return i.status === 'done'; }).length;
    const groupKey = name || '__none__';

    html += '<div class="board-session-group">';
    html += '<div class="board-session-header" onclick="toggleSessionGroup(\'' + esc(groupKey) + '\')">';
    html += '<span class="board-session-chevron' + (collapsed ? '' : ' open') + '">\u25B6</span>';
    html += '<span class="board-session-name">' + (name ? esc(name) : 'No session') + '</span>';
    html += '<div class="board-session-counts">';
    if (todoC) html += '<span class="board-session-count todo">' + todoC + ' todo</span>';
    if (doingC) html += '<span class="board-session-count doing">' + doingC + ' active</span>';
    if (doneC) html += '<span class="board-session-count done">' + doneC + ' done</span>';
    html += '</div></div>';
    if (!collapsed) {
      html += '<div class="board-session-items">';
      items.forEach(function(item) { html += _renderBoardCard(item); });
      html += '</div>';
    }
    html += '</div>';
  });

  if (!sessionNames.length) {
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
      (i.key || '').toLowerCase().includes(q) ||
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

  const cols = { todo: [], doing: [], done: [] };
  visible.forEach(item => {
    const s = item.status || 'todo';
    if (cols[s]) cols[s].push(item);
  });
  const labels = { todo: 'To Do', doing: 'In Progress', done: 'Done' };
  const statuses = ['todo', 'doing', 'done'];

  // FLIP step 1: snapshot current card positions
  const oldRects = {};
  const oldIds = new Set();
  container.querySelectorAll('.board-card[data-id]').forEach(el => {
    const id = el.dataset.id;
    oldIds.add(id);
    const r = el.getBoundingClientRect();
    oldRects[id] = { top: r.top, left: r.left };
  });

  let html = '';
  statuses.forEach(st => {
    const dnd = 'ondragover="boardColDragOver(event,\'' + st + '\')" ondragleave="boardColDragLeave(event)" ondrop="boardColDrop(event,\'' + st + '\')"';
    html += '<div class="board-col" ' + dnd + '>';
    html += '<div class="board-col-header"><span>' + labels[st] + '</span>';
    html += '<span class="col-count" data-col="' + st + '">' + cols[st].length + '</span></div>';
    if (cols[st].length === 0) {
      const hints = { todo: 'Nothing here yet', doing: 'Nothing in progress', done: 'Nothing done yet' };
      html += '<div class="board-empty">' + hints[st] + '</div>';
    }
    cols[st].forEach(item => { html += _renderBoardCard(item); });
    if (st === 'done' && cols[st].length > 0) {
      html += '<button class="board-add-btn" style="color:var(--red);border-color:rgba(248,81,73,0.2);" onclick="clearDone()">Clear done</button>';
    }
    html += '<button class="board-add-btn" onclick="openBoardAdd(\'' + st + '\')">+ Add</button>';
    html += '</div>';
  });
  container.innerHTML = html;

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
  statuses.forEach(st => { _prevCardRects[st] = cols[st].length; });
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

function openBoardAdd(status) {
  boardEditId = null;
  boardEditStatus = status;
  document.getElementById('be-title').value = '';
  document.getElementById('be-desc').value = '';
  document.getElementById('be-status').value = status;
  _populateSessionSelect('be-session-add', '');
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
  closeBoardEdit();
  await addBoardItem(title, desc, status, session, tags);
}

// ── Board detail (full-screen) ──
let boardDetailId = null;
let boardDetailStatus = 'todo';
const _boardDrafts = {};  // item id → { title, desc, session, status }

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
  if (keyEl) keyEl.textContent = item.key || '';
  _populateSessionSelect('bd-session', draft ? draft.session : (item.session || ''));
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
  const statuses = [{v:'todo',l:'To Do'},{v:'doing',l:'In Progress'},{v:'done',l:'Done'}];
  document.getElementById('bd-status-row').innerHTML = statuses.map(s =>
    '<button class="board-detail-status-btn' + (boardDetailStatus === s.v ? ' active-' + s.v : '') + '" onclick="boardDetailSetStatus(\'' + s.v + '\')">' + s.l + '</button>'
  ).join('');
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
      // Only save draft if something actually differs from saved state
      if (t !== (item.title || '') || d !== (item.desc || '') || s !== (item.session || '') || st !== (item.status || 'todo')) {
        _boardDrafts[boardDetailId] = { title: t, desc: d, session: s, status: st };
      } else {
        delete _boardDrafts[boardDetailId];
      }
    }
  }
  document.getElementById('board-detail-overlay').classList.remove('active');
  boardDetailId = null;
}

// Swipe right to close board detail
(function() {
  const el = document.getElementById('board-detail-overlay');
  let sx = 0, sy = 0, tracking = false;
  el.addEventListener('touchstart', e => {
    const t = e.touches[0];
    sx = t.clientX; sy = t.clientY; tracking = true;
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
  const changes = { title, desc, status: boardDetailStatus };
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
  closeBoardDetail();
  await deleteBoardItem(boardDetailId);
}

function saveBoardCache() {
  lastBoardJSON = JSON.stringify(boardItems);
  localStorage.setItem('cmux_board_cache', lastBoardJSON);
}

async function addBoardItem(title, desc, status, session, tags) {
  const tempId = Math.random().toString(16).slice(2, 8);
  const now = Math.floor(Date.now() / 1000);
  const tempItem = { id: tempId, title, desc, status, session: session || '', tags: tags || [], creator: _getDeviceName(), created: now, updated: now, _pending: true };
  boardItems.push(tempItem);
  saveBoardCache();
  renderBoard();
  const r = await apiCall(API + '/api/board', {
    method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ title, desc, status, session: session || '', tags: tags || [], creator: _getDeviceName() })
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


// ═══════ INIT ═══════
// Load cached sessions immediately so offline startup renders content
const _cachedInit = localStorage.getItem('cmux_sessions_cache');
if (_cachedInit) {
  try { sessions = JSON.parse(_cachedInit); } catch(e) {}
}
// Load cached board
const _cachedBoard = localStorage.getItem('cmux_board_cache');
if (_cachedBoard) {
  try { boardItems = JSON.parse(_cachedBoard); lastBoardJSON = _cachedBoard; } catch(e) {}
}
if (sessions.length || drafts.length) render();
updateConnectionStatus();

// ═══════ SSE — real-time push updates ═══════
let _sse = null;
let _sseRetries = 0;
let _sseFallback = false;
let _pollTimer = null;

function connectSSE() {
  if (_sseFallback || _sse) return;
  _sse = new EventSource(API + '/api/events');

  _sse.onmessage = function(e) {
    _sseRetries = 0;
    if (!online) setOnline(true);
    try {
      const msg = JSON.parse(e.data);
      if (msg.type === 'sessions') {
        const j = JSON.stringify(msg.payload);
        if (j !== lastSessionsJSON) {
          lastSessionsJSON = j;
          sessions = msg.payload;
          localStorage.setItem('cmux_sessions_cache', j);
          render();
        }
      } else if (msg.type === 'board') {
        const j = JSON.stringify(msg.payload);
        if (j !== lastBoardJSON) {
          lastBoardJSON = j;
          boardItems = msg.payload;
          localStorage.setItem('cmux_board_cache', j);
          if (activeView === 'board') renderBoard();
        }
      }
    } catch(err) { console.error('SSE parse:', err); }
  };

  _sse.onerror = function() {
    _sseRetries++;
    _sse.close();
    _sse = null;
    if (_sseRetries >= 3) {
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
        localStorage.setItem('cmux_app_html', html);
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
  }).catch(() => {});
}

// IndexedDB for durable draft/queue storage (survives iOS cache purges)
const _idb = (() => {
  let db = null;
  const open = () => new Promise((resolve, reject) => {
    if (db) return resolve(db);
    const req = indexedDB.open('cmux', 1);
    req.onupgradeneeded = () => {
      const d = req.result;
      if (!d.objectStoreNames.contains('kv')) d.createObjectStore('kv');
    };
    req.onsuccess = () => { db = req.result; resolve(db); };
    req.onerror = () => reject(req.error);
  });
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
  };
})();

// Dual-write drafts and queue to both localStorage and IndexedDB
function persistOfflineData() {
  localStorage.setItem('cmux_drafts', JSON.stringify(drafts));
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
});

function fmtTokens(n) {
  if (n >= 1000000) return (n / 1000000).toFixed(1) + 'M';
  if (n >= 1000) return (n / 1000).toFixed(1) + 'k';
  return String(n);
}

function openAbout() {
  document.getElementById('about-overlay').classList.add('active');
  document.getElementById('add-server-form').style.display = 'none';
  renderServerList();
  const el = document.getElementById('daily-stats');
  el.innerHTML = '<div style="color:var(--dim);font-size:0.75rem;text-align:center;">Loading...</div>';
  fetch(API + '/api/stats/daily').then(r => r.json()).then(data => {
    let html = '<div style="font-size:0.8rem;font-weight:600;margin-bottom:8px;">Today\'s Tokens</div>';
    html += '<div style="display:flex;justify-content:space-between;font-size:0.85rem;margin-bottom:6px;">';
    html += '<span>cmux sessions</span><span style="font-weight:600;">' + fmtTokens(data.cmux_tokens) + '</span></div>';
    html += '<div style="display:flex;justify-content:space-between;font-size:0.85rem;margin-bottom:10px;padding-bottom:8px;border-bottom:1px solid var(--border);">';
    html += '<span>All Claude Code</span><span style="font-weight:600;">' + fmtTokens(data.total_tokens) + '</span></div>';
    if (data.sessions && data.sessions.length) {
      html += '<div style="font-size:0.7rem;color:var(--dim);margin-bottom:4px;">Breakdown</div>';
      data.sessions.forEach(s => {
        const bar = s.total / data.total_tokens * 100;
        html += '<div style="margin-bottom:4px;">';
        html += '<div style="display:flex;justify-content:space-between;font-size:0.75rem;">';
        html += '<span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:200px;">' + (s.cmux ? '' : '<span style=color:var(--dim)>') + esc(s.name) + (s.cmux ? '' : '</span>') + '</span>';
        html += '<span style="flex-shrink:0;margin-left:8px;">' + fmtTokens(s.total) + '</span></div>';
        html += '<div style="height:3px;border-radius:2px;background:var(--border);margin-top:2px;">';
        html += '<div style="height:100%;border-radius:2px;background:' + (s.cmux ? 'var(--accent)' : 'var(--dim)') + ';width:' + bar.toFixed(1) + '%;"></div></div></div>';
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

function resetTokenStats() {
  if (!confirm('Reset token counters to zero?')) return;
  fetch(API + '/api/stats/reset', { method: 'POST' }).then(r => r.json()).then(() => {
    openAbout();
  }).catch(() => showToast('Reset failed'));
}

// ═══════ SERVER SWITCHER ═══════
function _getSavedServers() {
  try { return JSON.parse(localStorage.getItem('cmux_servers') || '[]'); } catch(e) { return []; }
}
function _saveServers(list) { localStorage.setItem('cmux_servers', JSON.stringify(list)); }

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
    const custom = localStorage.getItem('cmux_device_name') || '';
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
    localStorage.setItem('cmux_device_name', val);
  } else {
    localStorage.removeItem('cmux_device_name');
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
</script>
</body>
</html>"""


# ═══════════════════════════════════════════
# PWA ASSETS
# ═══════════════════════════════════════════

PWA_MANIFEST = json.dumps({
    "name": "cmux — Claude Code Multiplexer",
    "short_name": "cmux",
    "start_url": "/",
    "display": "standalone",
    "background_color": "#0d1117",
    "theme_color": "#0d1117",
    "icons": [
        {"src": "/icon-192.png", "sizes": "192x192", "type": "image/png"},
        {"src": "/icon-512.png", "sizes": "512x512", "type": "image/png"},
    ],
})

# Robust service worker: cache-first with localStorage fallback for multi-day offline
SERVICE_WORKER = r"""
const CACHE = 'cmux-v0.6.0';
const SHELL_URLS = ['/', '/manifest.json', '/icon-192.png', '/icon-512.png'];

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

  // API requests: network only (app JS handles offline queue)
  if (url.pathname.startsWith('/api/')) return;

  // Cache-first: serve from cache instantly, refresh in background
  e.respondWith(
    caches.open(CACHE).then(cache =>
      cache.match(e.request).then(cached => {
        // Always try to refresh cache in background when online
        const networkUpdate = fetch(e.request).then(response => {
          if (response.ok) cache.put(e.request, response.clone());
          return response;
        }).catch(() => null);

        if (cached) return cached;
        // Cache miss — wait for network
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
      const req = indexedDB.open('cmux', 1);
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


def _generate_icon_png(size):
    """Generate a simple PNG icon for cmux. Returns raw PNG bytes."""
    import struct, zlib
    w = h = size
    rows = []
    bg = (13, 17, 23, 255)       # #0d1117
    fg = (88, 166, 255, 255)     # #58a6ff
    for y in range(h):
        row = bytearray([0])  # filter byte
        for x in range(w):
            cx, cy = x / w, y / h
            # Draw 4 vertical bars (multiplexer symbol) with a > connector
            in_letter = False
            # Four bars at different x positions
            for bx in (0.25, 0.40, 0.55, 0.70):
                if abs(cx - bx) < 0.035 and 0.25 < cy < 0.75:
                    in_letter = True
            # Chevron > on the right side
            mid_y = 0.5
            chev_x = 0.82
            dy = abs(cy - mid_y)
            if 0.15 < dy < 0.25:
                expected_x = chev_x + (0.25 - dy) * 0.4
                if abs(cx - expected_x) < 0.025:
                    in_letter = True
            px = fg if in_letter else bg
            row.extend(px)
        rows.append(bytes(row))
    raw = b''.join(rows)
    # Build PNG
    def chunk(ctype, data):
        c = ctype + data
        return struct.pack('>I', len(data)) + c + struct.pack('>I', zlib.crc32(c) & 0xffffffff)
    ihdr = struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0)
    return (b'\x89PNG\r\n\x1a\n' +
            chunk(b'IHDR', ihdr) +
            chunk(b'IDAT', zlib.compress(raw)) +
            chunk(b'IEND', b''))


# Cache generated icons
_icon_cache = {}


# ═══════════════════════════════════════════
# HTTP REQUEST HANDLER
# ═══════════════════════════════════════════

class CCHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        # Quieter logging: just method + path
        sys.stderr.write(f"  {args[0]}\n")

    def _json(self, data, status=200):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _html(self, html):
        body = html.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _raw(self, body: bytes, content_type: str, cache=False):
        self.send_response(200)
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

        # GET /
        if method == "GET" and path == "/":
            return self._html(DASHBOARD_HTML)

        # PWA assets
        if method == "GET" and path == "/manifest.json":
            return self._raw(PWA_MANIFEST.encode(), "application/manifest+json", cache=True)
        if method == "GET" and path == "/sw.js":
            return self._raw(SERVICE_WORKER.encode(), "application/javascript")
        if method == "GET" and path in ("/icon-192.png", "/icon-512.png"):
            size = 192 if "192" in path else 512
            if size not in _icon_cache:
                _icon_cache[size] = _generate_icon_png(size)
            return self._raw(_icon_cache[size], "image/png", cache=True)

        # GET /api/events (SSE stream)
        if method == "GET" and path == "/api/events":
            return self._sse_events()

        # GET /api/sessions
        if method == "GET" and path == "/api/sessions":
            return self._json(list_sessions())

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
                content = p.read_text(errors="replace")
                # Limit to 100KB for safety
                if len(content) > 100_000:
                    content = content[:100_000] + "\n\n... (truncated at 100KB)"
                is_md = p.suffix.lower() in (".md", ".markdown", ".mdx")
                return self._json({"path": str(p), "content": content, "is_markdown": is_md})
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

        # ── Board API ──
        if path == "/api/board" or path.startswith("/api/board/"):
            if method == "GET" and path == "/api/board":
                return self._json(_load_board())

            if method == "POST" and path == "/api/board":
                body = self._read_body()
                title = body.get("title", "").strip()
                if not title:
                    return self._json({"error": "missing title"}, 400)
                raw = {}
                if _BOARD_FILE.exists():
                    try:
                        raw = json.loads(_BOARD_FILE.read_text())
                    except Exception:
                        pass
                items = raw.get("items", [])
                counters = raw.get("counters", {})
                session = body.get("session", "").strip()
                # Build issue key from session name initials (e.g. "my-project" → "MP", "infra" → "INFRA")
                words = [w for w in re.split(r'[-_\s]+', session) if w] if session else []
                if not words:
                    prefix = "CMUX"
                elif len(words) == 1:
                    prefix = re.sub(r'[^A-Z0-9]', '', words[0].upper())[:5] or "CMUX"
                else:
                    prefix = re.sub(r'[^A-Z0-9]', '', ''.join(w[0] for w in words).upper())[:5] or "CMUX"
                n = counters.get(prefix, 0) + 1
                counters[prefix] = n
                item = {
                    "id": os.urandom(3).hex(),
                    "key": f"{prefix}-{n}",
                    "title": title,
                    "desc": body.get("desc", "").strip(),
                    "status": body.get("status", "todo"),
                    "session": session,
                    "tags": body.get("tags", []),
                    "creator": body.get("creator", ""),
                    "created": int(time.time()),
                    "updated": int(time.time()),
                }
                items.append(item)
                _BOARD_FILE.write_text(json.dumps({"items": items, "counters": counters}))
                return self._json(item, 201)

            if method == "POST" and path == "/api/board/clear-done":
                items = [i for i in _load_board() if i.get("status") != "done"]
                _save_board(items)
                return self._json({"ok": True, "remaining": len(items)})

            # PATCH/DELETE /api/board/<id>
            board_m = re.match(r"^/api/board/([a-f0-9]+)$", path)
            if board_m:
                bid = board_m.group(1)
                items = _load_board()
                idx = next((i for i, it in enumerate(items) if it["id"] == bid), None)
                if idx is None:
                    return self._json({"error": "item not found"}, 404)

                if method == "PATCH":
                    body = self._read_body()
                    for k in ("title", "desc", "status", "session", "tags"):
                        if k in body:
                            items[idx][k] = body[k]
                    items[idx]["updated"] = int(time.time())
                    _save_board(items)
                    return self._json(items[idx])

                if method == "DELETE":
                    removed = items.pop(idx)
                    _save_board(items)
                    return self._json({"ok": True, "deleted": removed["id"]})

            return self._json({"error": "not found"}, 404)

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
            # Derive name: strip cmux-/cc- prefix if present, or use as-is
            if not cc_name:
                cc_name = tmux_session.removeprefix("cmux-").removeprefix("cc-")
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
            # Rename the tmux session to match cmux convention
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
            return self._json({"ok": True, "message": f"created {name}"})

        # Session-specific routes: /api/sessions/<name>/<action>
        m = re.match(r"^/api/sessions/([^/]+)(/([^/]+))?$", path)
        if not m:
            return self._json({"error": "not found"}, 404)

        name = m.group(1)
        action = m.group(3) or ""

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
            if action == "stats":
                cfg = parse_env_file(env_file)
                stats = get_claude_stats(cfg.get("CC_DIR", ""))
                return self._json(stats)
            return self._json({"error": "not found"}, 404)

        if method == "POST":
            if action == "send":
                body = self._read_body()
                text = body.get("text", "")
                if not text:
                    return self._json({"error": "missing 'text'"}, 400)
                ok, msg = send_text(name, text)
                return self._json({"ok": ok, "message": msg}, 200 if ok else 500)
            if action == "keys":
                body = self._read_body()
                keys = body.get("keys", "")
                if not keys:
                    return self._json({"error": "missing 'keys'"}, 400)
                ok, msg = send_keys(name, keys)
                return self._json({"ok": ok, "message": msg}, 200 if ok else 500)
            if action == "start":
                ok, msg = start_session(name)
                return self._json({"ok": ok, "message": msg}, 200 if ok else 500)
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
                # Try to find existing conversation to resume with full context
                session_id = _find_latest_session_id(work_dir)
                if session_id:
                    # Resume the conversation in a forked session — full history
                    ok, msg = start_session(new_name, f"--resume {session_id} --fork-session")
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

                return self._json({"error": "nothing to update"}, 400)
            return self._json({"error": "not found"}, 404)

        return self._json({"error": "method not allowed"}, 405)

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
    """Watch cmux-server.py for changes and restart on modification."""
    script = Path(__file__).resolve()
    mtime = script.stat().st_mtime
    while True:
        time.sleep(1)
        try:
            new_mtime = script.stat().st_mtime
            if new_mtime != mtime:
                print(f"\n\033[33m↻ {script.name} changed — restarting...\033[0m")
                server.shutdown()
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


def _ensure_tls(lan_ip: str) -> tuple:
    """Ensure TLS cert exists. Tries Tailscale → mkcert → self-signed. Returns (cert, key, hostname)."""
    TLS_DIR.mkdir(parents=True, exist_ok=True)

    # 1. Try Tailscale cert (real Let's Encrypt, trusted everywhere, no CA install)
    ts_hostname = _get_tailscale_hostname()
    if ts_hostname:
        ts_cert = TLS_DIR / f"{ts_hostname}.crt"
        ts_key = TLS_DIR / f"{ts_hostname}.key"
        if ts_cert.exists() and ts_key.exists():
            return str(ts_cert), str(ts_key), ts_hostname
        for ts_bin in ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "tailscale"]:
            try:
                print(f"\033[2m  Getting Tailscale cert for {ts_hostname}...\033[0m")
                r = subprocess.run(
                    [ts_bin, "cert", "--cert-file", str(ts_cert), "--key-file", str(ts_key), ts_hostname],
                    capture_output=True, text=True, timeout=30,
                )
                if r.returncode == 0 and ts_cert.exists():
                    return str(ts_cert), str(ts_key), ts_hostname
            except Exception:
                continue

    # 2. Try mkcert (locally-trusted, no browser warnings on same machine)
    cert_file = TLS_DIR / "cert.pem"
    key_file = TLS_DIR / "key.pem"
    if cert_file.exists() and key_file.exists():
        return str(cert_file), str(key_file), ""

    if subprocess.run(["which", "mkcert"], capture_output=True).returncode == 0:
        print(f"\033[2m  Generating trusted TLS cert with mkcert...\033[0m")
        subprocess.run(
            ["mkcert", "-cert-file", str(cert_file), "-key-file", str(key_file),
             "localhost", "127.0.0.1", lan_ip],
            capture_output=True, check=True,
        )
        return str(cert_file), str(key_file), ""

    # 3. Fallback: self-signed via openssl
    print(f"\033[2m  Generating self-signed TLS cert...\033[0m")
    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
         "-keyout", str(key_file), "-out", str(cert_file),
         "-days", "365", "-subj", "/CN=cmux",
         "-addext", f"subjectAltName=DNS:localhost,IP:127.0.0.1,IP:{lan_ip}"],
        capture_output=True, check=True,
    )
    return str(cert_file), str(key_file), ""


# ── Main ──

def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8822
    lan_ip = get_lan_ip()
    no_tls = "--no-tls" in sys.argv

    server = ThreadingHTTPServer(("0.0.0.0", port), CCHandler)

    scheme = "http"
    ts_hostname = ""
    if not no_tls:
        try:
            cert, key, ts_hostname = _ensure_tls(lan_ip)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(cert, key)
            server.socket = ctx.wrap_socket(server.socket, server_side=True)
            scheme = "https"
        except Exception as e:
            print(f"\033[33m  TLS setup failed ({e}), falling back to HTTP\033[0m")

    print(f"\033[1m\033[34mcmux\033[0m web dashboard running")
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
    print(f"\033[2m  Auto-reload active — editing cmux-server.py will restart\033[0m")
    print(f"\n\033[2mPress Ctrl-C to stop\033[0m")

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
