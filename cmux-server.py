#!/usr/bin/env python3
"""cmux serve — web dashboard for Claude Code session manager."""

# ═══════════════════════════════════════════
# CONFIGURATION & GLOBALS
# ═══════════════════════════════════════════

import json
import os
import re
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
CLAUDE_HOME = Path.home() / ".claude"

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


def _detect_claude_status(raw_output: str) -> str:
    """Detect Claude Code status from tmux output.
    Returns: 'active', 'waiting', 'idle', or ''."""
    if not raw_output:
        return ""
    clean = re.sub(r'\x1b\[[0-9;]*[a-zA-Z]|\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07', '', raw_output)
    lines = [l for l in clean.splitlines() if l.strip()]
    if not lines:
        return ""
    tail = "\n".join(lines[-5:])
    tail_lower = tail.lower()
    # Find the status bar line (starts with ⏵⏵ or contains "bypass permissions" / "plan mode")
    status_bar = ""
    for l in reversed(lines[-5:]):
        ls = l.strip().lower()
        if "⏵⏵" in l or "bypass permissions" in ls or "plan mode" in ls:
            status_bar = ls
            break
    # Active: "esc to interru" handles tmux truncation of "esc to interrupt"
    if "esc to interru" in status_bar:
        return "active"
    # Waiting: tool approval pending (N bash, N tool, approve)
    if re.search(r'\d+\s+(bash|tool)', status_bar) or "approve" in status_bar:
        return "waiting"
    # Idle: at the prompt with status bar visible
    if status_bar:
        return "idle"
    # No status bar — check for prompt character
    if "\u276f" in tail or "\u2770" in tail or "$ " in tail:
        return "idle"
    return ""


def _tmux_activity_map() -> dict:
    """Get last activity epoch for all tmux sessions in one call."""
    try:
        r = subprocess.run(
            ["tmux", "list-sessions", "-F", "#{session_name}\t#{session_activity}"],
            capture_output=True, text=True, timeout=5,
        )
        if r.returncode != 0:
            return {}
        result = {}
        for line in r.stdout.strip().splitlines():
            parts = line.split("\t", 1)
            if len(parts) == 2:
                result[parts[0]] = int(parts[1])
        return result
    except Exception:
        return {}


def list_sessions() -> list:
    sessions = []
    if not CC_SESSIONS.is_dir():
        return sessions
    activity = _tmux_activity_map()
    for f in sorted(CC_SESSIONS.glob("*.env")):
        name = f.stem
        cfg = parse_env_file(f)
        running = is_running(name)
        preview = ""
        preview_lines = []
        status = ""
        last_activity = activity.get(tmux_name(name), 0)
        if running:
            raw = tmux_capture(name, 8)
            lines = [l for l in raw.splitlines() if l.strip()]
            strip_ansi = lambda t: re.sub(r'\x1b\[[0-9;]*[a-zA-Z]|\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07', '', t)
            preview = strip_ansi(lines[-1][:120]) if lines else ""
            # Last 3 non-empty content lines (skip status bar line)
            content = [strip_ansi(l)[:200] for l in lines[:-1] if l.strip()] if len(lines) > 1 else []
            preview_lines = content[-3:] if content else []
            status = _detect_claude_status(raw)
        sessions.append({
            "name": name,
            "dir": cfg.get("CC_DIR", ""),
            "desc": cfg.get("CC_DESC", ""),
            "pinned": cfg.get("CC_PINNED", "") == "1",
            "tags": [t.strip() for t in cfg.get("CC_TAGS", "").split(",") if t.strip()],
            "flags": cfg.get("CC_FLAGS", ""),
            "running": running,
            "status": status,
            "preview": preview,
            "preview_lines": preview_lines,
            "last_activity": last_activity,
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
    return {
        "name": name,
        "dir": cfg.get("CC_DIR", ""),
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
    work_dir = cfg.get("CC_DIR", str(Path.home()))
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
        shell_rc = ""
        for rc in [Path.home() / ".zprofile", Path.home() / ".bash_profile", Path.home() / ".profile"]:
            if rc.exists():
                shell_rc = f"source {rc} 2>/dev/null; "
                break
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
<meta name="viewport" content="width=device-width, initial-scale=1, user-scalable=no">
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
  @media (min-width: 768px) { .card.expanded { grid-column: 1 / -1; } }
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
    outline: none; min-height: 44px; max-height: calc(1.4em * 3 + 28px);
    resize: none; overflow-y: auto; line-height: 1.4;
    font-family: inherit; field-sizing: content;
  }
  .send-input:focus { border-color: var(--accent); }

  /* Peek overlay */
  .overlay {
    display: none; position: fixed; inset: 0; background: var(--bg);
    z-index: 100; flex-direction: column;
    padding: 12px; padding-top: max(12px, env(safe-area-inset-top));
  }
  .overlay.active { display: flex; }
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
    margin: -16px -16px 16px -16px;
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
  .search-input {
    font-size: 0.85rem; padding: 8px 28px 8px 12px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--card); color: var(--text);
    outline: none; min-height: 40px; width: 140px;
    -webkit-tap-highlight-color: transparent;
  }
  .search-input::placeholder { color: var(--dim); }
  .search-input:focus { border-color: var(--accent); width: 180px; }
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
    .search-input { width: 70px; flex-shrink: 1; min-width: 0; }
    .search-input:focus { width: 110px; }
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

  /* Tags */
  .tag {
    font-size: 0.68rem; padding: 2px 7px; border-radius: 4px;
    font-weight: 500; background: rgba(88,166,255,0.12); color: var(--accent);
    border: 1px solid rgba(88,166,255,0.2); white-space: nowrap; flex-shrink: 0;
  }

  /* Tag filter bar */
  .tag-filters {
    display: flex; gap: 6px; flex-wrap: nowrap; margin-bottom: 10px;
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

  /* Connection status indicator — dot + short label */
  .conn-status {
    display: flex; align-items: center; gap: 5px;
    font-size: 0.7rem; font-weight: 500; white-space: nowrap;
    cursor: pointer; -webkit-tap-highlight-color: transparent;
    flex-shrink: 0; transition: color 0.2s;
  }
  .conn-status::before {
    content: ''; width: 8px; height: 8px; border-radius: 50%;
    flex-shrink: 0; transition: background 0.2s;
  }
  .conn-status.online { color: var(--dim); }
  .conn-status.online::before { background: #4ade80; }
  .conn-status.offline { color: #f87171; }
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
</style>
</head>
<body>

<div class="header-row">
  <div style="display:flex;gap:8px;align-items:center;">
    <h1 style="margin:0;">cmux</h1>
    <span id="conn-status" class="conn-status online" onclick="showQueueModal()"></span>
  </div>
  <div style="display:flex;gap:8px;align-items:center;">
    <div class="search-wrap" id="search-wrap">
      <input class="search-input" id="search-input" type="text" placeholder="Search..." autocomplete="off" autocorrect="off"
        oninput="searchQuery=this.value;document.getElementById('search-wrap').classList.toggle('has-value',!!this.value);render()">
      <button class="search-clear" onclick="event.stopPropagation();clearSearch()">&#x2715;</button>
    </div>
    <button class="btn" onclick="fetchSessions()" title="Refresh" style="padding:8px 10px;font-size:1rem;">&#x21BB;</button>
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

<!-- Create session modal -->
<div id="create-overlay" class="edit-overlay" onclick="if(event.target===this)closeCreate()">
  <div class="edit-box">
    <h3>New session</h3>
    <input id="create-name" type="text" placeholder="Session name" autocomplete="off" autocorrect="off"
      onkeydown="if(event.key==='Enter'){event.preventDefault();document.getElementById('create-dir').focus();}">
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
    <h2 id="peek-title">peek</h2>
    <div style="display:flex;gap:8px;align-items:center;">
      <div class="search-wrap" id="peek-search-wrap">
        <input class="search-input" id="peek-search" type="text" placeholder="Find..." autocomplete="off" autocorrect="off"
          oninput="peekSearchQuery=this.value;document.getElementById('peek-search-wrap').classList.toggle('has-value',!!this.value);applyPeekSearch()">
        <span class="search-count" id="peek-search-count"></span>
        <button class="search-clear" onclick="event.stopPropagation();clearPeekSearch()">&#x2715;</button>
      </div>
      <button class="btn" onclick="closePeek()">Close</button>
    </div>
  </div>
  <div id="peek-body" class="overlay-body"></div>
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
      </div>
      <div class="ac-wrap" style="flex:1;">
        <textarea class="send-input" id="peek-cmd-input" rows="1" placeholder="Type a command..."
          autocomplete="off" autocorrect="on" autocapitalize="sentences" spellcheck="true"
          enterkeyhint="send" style="width:100%;"
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
        oninput="if(editState&&editState.field==='dir')editAcFetch(this.value)"
        onfocus="if(editState&&editState.field==='dir')editAcFetch(this.value)"
        onkeydown="editAcKeydown(event)">
      <div id="edit-ac-list" class="ac-list"></div>
    </div>
    <select id="edit-select" style="display:none;width:100%;padding:8px 10px;border:1px solid var(--border);border-radius:6px;background:var(--card);color:var(--fg);font-size:0.9rem;">
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

<!-- Toast -->
<div id="toast" class="toast"></div>

<!-- File preview overlay -->
<div id="file-overlay" class="file-overlay">
  <div class="file-overlay-header">
    <h2 id="file-title">file</h2>
    <button class="btn" onclick="closeFilePreview()">Close</button>
  </div>
  <div id="file-body" class="file-overlay-body"></div>
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

// Connection & offline state
let online = true;
// Migrate localStorage keys from cc_ to cmux_
['offline_queue','sessions_cache','drafts'].forEach(k => {
  const old = localStorage.getItem('cc_' + k);
  if (old && !localStorage.getItem('cmux_' + k)) {
    localStorage.setItem('cmux_' + k, old);
    localStorage.removeItem('cc_' + k);
  }
});
let offlineQueue = JSON.parse(localStorage.getItem('cmux_offline_queue') || '[]');

// ═══════ DRAFTS — offline-created sessions ═══════
let drafts = JSON.parse(localStorage.getItem('cmux_drafts') || '[]');
// Draft shape: { name, dir, prompt, created_at, syncing }

function saveDrafts() {
  localStorage.setItem('cmux_drafts', JSON.stringify(drafts));
}

function addDraft(name, dir, prompt) {
  drafts.push({ name, dir: dir || '', prompt: prompt || '', created_at: Date.now(), syncing: false });
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
  const el = document.getElementById('conn-status');
  if (!el) return;
  if (online) {
    el.className = 'conn-status online';
    el.textContent = '';
  } else {
    el.className = 'conn-status offline';
    const total = offlineQueue.length + drafts.length;
    el.textContent = total ? total + ' pending' : 'offline';
  }
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
    syncDrafts().then(() => replayQueue());
  } else if (was && !val) {
    showToast('Server unreachable — offline mode');
  }
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
  localStorage.removeItem('cmux_offline_queue');
  updateConnectionStatus();
  closeQueueModal();
  showToast('Queue cleared');
}
async function forceRetry() {
  closeQueueModal();
  if (!offlineQueue.length) return;
  setOnline(true);
}

// apiCall — wraps mutation fetches with offline queuing
async function apiCall(url, options) {
  if (online) {
    try {
      const r = await fetch(url, options);
      consecutiveFailures = 0;
      if (!online) setOnline(true);
      return r;
    } catch(e) {
      consecutiveFailures++;
      if (consecutiveFailures >= 3) setOnline(false);
      // Queue the failed mutation
      offlineQueue.push({ url, options: { method: options.method, headers: options.headers, body: options.body }, timestamp: Date.now() });
      localStorage.setItem('cmux_offline_queue', JSON.stringify(offlineQueue));
      updateConnectionStatus();
      showToast('Queued (' + offlineQueue.length + ' pending)');
      return null;
    }
  } else {
    offlineQueue.push({ url, options: { method: options.method, headers: options.headers, body: options.body }, timestamp: Date.now() });
    localStorage.setItem('cmux_offline_queue', JSON.stringify(offlineQueue));
    updateConnectionStatus();
    showToast('Queued (' + offlineQueue.length + ' pending)');
    return null;
  }
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

// Queue replay on reconnect
async function replayQueue() {
  if (!offlineQueue.length) return;
  const raw = [...offlineQueue];
  offlineQueue = [];
  localStorage.removeItem('cmux_offline_queue');
  // Reconcile before replaying
  const queue = reconcileQueue(raw);
  const skipped = raw.length - queue.length;
  if (skipped) showToast('Skipped ' + skipped + ' superseded ops');
  showToast('Syncing ' + queue.length + ' operation' + (queue.length === 1 ? '' : 's') + '...');
  let synced = 0, failed = 0;
  for (const item of queue) {
    try {
      const r = await fetch(item.url, item.options);
      if (r.status >= 500) {
        offlineQueue.push(item);
        failed++;
      } else {
        synced++;
      }
      // 4xx = stale, discard silently (session gone, etc.)
    } catch(e) {
      offlineQueue.push(item);
      failed++;
    }
  }
  if (failed) {
    localStorage.setItem('cmux_offline_queue', JSON.stringify(offlineQueue));
    showToast(synced + ' synced, ' + failed + ' re-queued');
  } else {
    showToast('All ' + synced + ' operation' + (synced === 1 ? '' : 's') + ' synced');
  }
  updateConnectionStatus();
  fetchSessions();
}

// ═══════ DRAFT SYNC ENGINE ═══════
// On reconnect: create draft sessions → start → send prompts, one by one
async function syncDrafts() {
  if (!drafts.length) return;
  const toSync = [...drafts];
  showToast('Syncing ' + toSync.length + ' draft' + (toSync.length === 1 ? '' : 's') + '...');

  for (const draft of toSync) {
    draft.syncing = true;
    saveDrafts();
    render();

    try {
      // 1. Create session on server
      const createResp = await fetch(API + '/api/sessions', {
        method: 'POST', headers: {'Content-Type':'application/json'},
        body: JSON.stringify({ name: draft.name, dir: draft.dir })
      });

      if (!createResp.ok) {
        const err = await createResp.json().catch(() => ({}));
        // 409 = already exists, treat as success
        if (createResp.status !== 409) {
          showToast('Failed to create ' + draft.name + ': ' + (err.error || 'server error'));
          draft.syncing = false;
          saveDrafts();
          continue;
        }
      }

      // 2. Start the session
      const startResp = await fetch(API + '/api/sessions/' + encodeURIComponent(draft.name) + '/start', {
        method: 'POST'
      });

      // 3. If there's a prompt, wait for Claude to init then send it
      if (draft.prompt && startResp.ok) {
        showToast('Started ' + draft.name + ', sending prompt in 5s...');
        await new Promise(r => setTimeout(r, 5000));
        await fetch(API + '/api/sessions/' + encodeURIComponent(draft.name) + '/send', {
          method: 'POST', headers: {'Content-Type':'application/json'},
          body: JSON.stringify({ text: draft.prompt })
        });
        showToast('Sent prompt to ' + draft.name);
      }

      // Success — remove draft
      removeDraft(draft.name);
      render();
    } catch(e) {
      // Network error — stop syncing, will retry on next reconnect
      draft.syncing = false;
      saveDrafts();
      showToast('Sync interrupted — will retry when connected');
      return;
    }
  }

  if (!drafts.length) {
    showToast('All drafts synced');
  }
  fetchSessions();
}

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
    if (consecutiveFailures >= 3 && online) {
      setOnline(false);
      // Load cached sessions for offline view
      const cached = localStorage.getItem('cmux_sessions_cache');
      if (cached) {
        try {
          sessions = JSON.parse(cached);
          render();
        } catch(ex) {}
      }
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

  el.innerHTML = filtered.map(s => {
    const isExp = expanded === s.name;
    const flags = s.flags || '';
    const isYolo = flags.includes('--dangerously-skip-permissions');
    const modelMatch = flags.match(/--model\s+(\S+)/);
    const model = modelMatch ? modelMatch[1] : null;
    return `
    <div class="card ${isExp ? 'expanded' : ''}" onclick="toggle('${s.name}')">
      <div class="card-header">
        <div class="dot ${s.running ? 'running' : 'stopped'}"></div>
        <div class="card-name">${s.pinned ? '<span class="pin-icon">&#x1F4CC;</span> ' : ''}${esc(s.name)}</div>
        ${s.status === 'active' ? '<span class="status-badge active">working</span>' : ''}
        ${s.status === 'waiting' ? '<span class="status-badge waiting">needs input</span>' : ''}
        ${s.status === 'idle' ? '<span class="status-badge idle">idle</span>' : ''}
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
      ${s.dir ? `<div class="card-dir">${esc(s.dir)}</div>` : ''}
      ${isExp && s.desc ? `<div class="card-desc">${esc(s.desc)}</div>` : ''}
      ${s.preview ? `<div class="card-preview">${esc(s.preview)}</div>` : ''}
      ${(isYolo || model || s.tags.length) ? `<div class="badges">
        ${isYolo ? '<span class="badge yolo">YOLO</span>' : ''}
        ${model ? `<span class="badge model">${esc(model)}</span>` : ''}
        ${s.tags.map(t => `<span class="tag">${esc(t)}</span>`).join('')}
      </div>` : ''}
      <div class="panel" onclick="event.stopPropagation()">
        ${s.preview_lines && s.preview_lines.length ? `<div class="card-preview-lines">${rewriteLocalhostUrls(s.preview_lines.map(l => esc(l)).join('\n'))}</div>` : ''}
        <div class="card-stats" id="stats-${s.name}"></div>
        <div class="panel-actions">
          <button class="btn primary" onclick="openPeek('${s.name}')">Peek</button>
          ${!s.running ? `<button class="btn" id="start-btn-${s.name}" onclick="this.textContent='Starting...';this.disabled=true;doStart('${s.name}')">Start</button>` : ''}
        </div>
        ${s.running ? `
        <div class="chips">
          <div class="chip" onclick="doSend('${s.name}','/compact')">/compact</div>
          <div class="chip" onclick="doSend('${s.name}','/status')">/status</div>
          <div class="chip" onclick="doSend('${s.name}','/clear')">/clear</div>
          <div class="chip" onclick="doSend('${s.name}','/cost')">/cost</div>
          <div class="chip" onclick="doKeys('${s.name}','C-c')">Ctrl-C</div>
          <div class="chip" onclick="doKeys('${s.name}','Escape')">Esc</div>
          <div class="chip" onclick="doKeys('${s.name}','Enter')">Enter</div>
        </div>
        <div class="send-row">
          <textarea class="send-input" id="input-${s.name}" rows="1"
            placeholder="Send to ${esc(s.name)}..." autocomplete="off" autocorrect="on"
            autocapitalize="sentences" spellcheck="true" enterkeyhint="send"
            oninput="autoGrow(this)"
            onkeydown="if(event.key==='Enter'&&!event.shiftKey){event.preventDefault();sendFromInput('${s.name}');}"></textarea>
          <button class="btn primary" onclick="sendFromInput('${s.name}')">Send</button>
        </div>` : ''}
      </div>
    </div>`;
  }).join('');

  // Prepend drafts above server sessions
  el.innerHTML = draftCards + el.innerHTML;

  // Restore input values and focus after re-rendering
  for (const [id, val] of Object.entries(savedInputs)) {
    const inp = document.getElementById(id);
    if (inp) inp.value = val;
  }
  if (focusedId) {
    const inp = document.getElementById(focusedId);
    if (inp) inp.focus();
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

function toggle(name) {
  expanded = expanded === name ? null : name;
  closeAllMenus();
  render();
  if (expanded) {
    const inp = document.getElementById('input-' + name);
    if (inp) setTimeout(() => inp.focus(), 100);
    fetchStats(name);
  }
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
    let left = r.right - 200; // menu min-width is 200, align right edge to button right
    if (left < 8) left = 8;
    if (left + 200 > vw) left = vw - 208;
    el.style.top = (r.bottom + 4) + 'px';
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
  if (field !== 'model') setTimeout(() => { inp.focus(); inp.select(); }, 100);
}
function closeEdit() {
  document.getElementById('edit-overlay').classList.remove('active');
  document.getElementById('edit-ac-list').classList.remove('open');
  document.getElementById('edit-input-wrap').style.display = '';
  document.getElementById('edit-select').style.display = 'none';
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
  if (!el.classList.contains('open')) {
    if (e.key === 'Enter') { e.preventDefault(); submitEdit(); }
    return;
  }
  if (e.key === 'ArrowDown') {
    e.preventDefault();
    editAcSelected = Math.min(editAcSelected + 1, editAcItems.length - 1);
    editAcHighlight();
  } else if (e.key === 'ArrowUp') {
    e.preventDefault();
    editAcSelected = Math.max(editAcSelected - 1, 0);
    editAcHighlight();
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (editAcSelected >= 0) editAcPick(editAcSelected);
    else { el.classList.remove('open'); submitEdit(); }
  } else if (e.key === 'Tab' && editAcItems.length) {
    e.preventDefault();
    editAcPick(editAcSelected >= 0 ? editAcSelected : 0);
  } else if (e.key === 'Escape') {
    el.classList.remove('open');
  }
}
function editAcHighlight() {
  const items = document.getElementById('edit-ac-list').querySelectorAll('.ac-item');
  items.forEach((el, i) => el.classList.toggle('selected', i === editAcSelected));
  if (items[editAcSelected]) items[editAcSelected].scrollIntoView({ block: 'nearest' });
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

async function doSend(name, text) {
  await apiCall(API + '/api/sessions/' + name + '/send', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({text})
  });
}

async function doKeys(name, keys) {
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

async function openPeek(name) {
  peekSession = name;
  peekSessionDir = (sessions.find(s => s.name === name) || {}).dir || '';
  peekSearchQuery = '';
  lastPeekHTML = '';
  const searchInp = document.getElementById('peek-search');
  if (searchInp) searchInp.value = '';
  peekCmdOpen = false;
  document.getElementById('peek-cmd-row').classList.remove('open');
  document.getElementById('peek-cmd-toggle').innerHTML = '&#x25B2; Send command';
  document.getElementById('peek-cmd-input').value = '';
  document.getElementById('peek-title').textContent = name;
  document.getElementById('peek-overlay').classList.add('active');
  document.body.style.overflow = 'hidden';
  await refreshPeek();
  peekTimer = setInterval(refreshPeek, 3000);
}

function closePeek() {
  peekSession = null;
  peekSearchQuery = '';
  lastPeekHTML = '';
  document.getElementById('peek-overlay').classList.remove('active');
  document.body.style.overflow = '';
  if (peekTimer) { clearInterval(peekTimer); peekTimer = null; }
}

// Tailscale URL rewriting
function rewriteLocalhostUrls(html) {
  if (!remoteHostname) return html;
  return html.replace(/http:\/\/(localhost|127\.0\.0\.1)(:\d+)/g,
    (match, host, port) => 'http://' + remoteHostname + port);
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
  try {
    const r = await fetch(API + '/api/sessions/' + peekSession + '/peek?lines=500');
    const data = await r.json();
    const body = document.getElementById('peek-body');
    const atBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 40;
    lastPeekHTML = linkifyOutput(stripAnsi(data.output || '(no output)'));
    applyPeekSearch();
    if (atBottom) body.scrollTop = body.scrollHeight;
    document.getElementById('peek-status').textContent = 'Updated ' + new Date().toLocaleTimeString();
  } catch(e) { console.error('peek:', e); }
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
  if (peekCmdOpen) setTimeout(() => document.getElementById('peek-cmd-input').focus(), 50);
}
async function sendPeekCmd() {
  if (!peekSession) return;
  const inp = document.getElementById('peek-cmd-input');
  const text = inp.value.trim();
  if (!text) return;
  inp.value = '';
  inp.style.height = 'auto';
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
  inp.focus();
}

function slashAcKeydown(e) {
  const el = document.getElementById('slash-ac-list');
  if (!el.classList.contains('open')) {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendPeekCmd(); }
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
    else { el.classList.remove('open'); sendPeekCmd(); }
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

// ── Search clear helpers ──
function toggleTagFilter(tag) {
  activeTag = activeTag === tag ? '' : tag;
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
  setTimeout(() => document.getElementById('create-name').focus(), 100);
}
function closeCreate() {
  document.getElementById('create-overlay').classList.remove('active');
  document.getElementById('ac-list').classList.remove('open');
}
async function submitCreate() {
  const name = document.getElementById('create-name').value.trim();
  const dir = document.getElementById('create-dir').value.trim();
  const prompt = document.getElementById('create-prompt').value.trim();
  if (!name) { document.getElementById('create-name').focus(); return; }
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
    body: JSON.stringify({ name, dir })
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

// ═══════ INIT ═══════
fetchSessions();
setInterval(fetchSessions, 5000);

// Register service worker for offline asset caching
if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js').catch(() => {});
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

# Minimal service worker: cache the app shell so it loads offline
SERVICE_WORKER = r"""
const CACHE = 'cmux-v2';
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

self.addEventListener('fetch', e => {
  const url = new URL(e.request.url);
  if (e.request.method !== 'GET') return;

  // API requests: network only (app JS handles offline queue)
  if (url.pathname.startsWith('/api/')) return;

  // App shell (/, manifest, icons): stale-while-revalidate
  // Serve from cache immediately, fetch update in background
  e.respondWith(
    caches.open(CACHE).then(cache =>
      cache.match(e.request).then(cached => {
        const fetchPromise = fetch(e.request).then(response => {
          if (response.ok) cache.put(e.request, response.clone());
          return response;
        }).catch(() => cached);  // network fail → use cached
        return cached || fetchPromise;  // cache hit → instant, miss → wait for network
      })
    )
  );
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

        # GET /api/sessions
        if method == "GET" and path == "/api/sessions":
            return self._json(list_sessions())

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
                return self._json({"name": name, "output": output})
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

    def do_POST(self):
        self._route("POST")

    def do_PATCH(self):
        self._route("PATCH")


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

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n\033[2mStopped.\033[0m")
        server.server_close()


if __name__ == "__main__":
    main()
