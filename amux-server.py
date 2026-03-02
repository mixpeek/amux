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

# Load ~/.amux/server.env before reading any env vars (persistent server config)
_server_env_file = _amux_home / "server.env"
if _server_env_file.exists():
    for _line in _server_env_file.read_text().splitlines():
        _line = _line.strip()
        if _line and not _line.startswith("#") and "=" in _line:
            _k, _, _v = _line.partition("=")
            os.environ.setdefault(_k.strip(), _v.strip())

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

# S3 iCal public sync (optional — set AMUX_S3_BUCKET to enable)
_S3_BUCKET = os.environ.get("AMUX_S3_BUCKET", "")
_S3_KEY = os.environ.get("AMUX_S3_KEY", "amux/calendar.ics")
_S3_REGION = os.environ.get("AMUX_S3_REGION", "us-east-1")
_S3_CAL_URL = (
    f"https://{_S3_BUCKET}.s3.{_S3_REGION}.amazonaws.com/{_S3_KEY}"
    if _S3_BUCKET else ""
)
MAX_LOG_BYTES = 10 * 1024 * 1024  # 10MB per session
SERVER_LOG = CC_LOGS / "server.log"
_server_log_lock = threading.Lock()

def slog(*args):
    """Append a timestamped line to ~/.amux/logs/server.log (and stderr if TTY)."""
    import datetime
    msg = " ".join(str(a) for a in args)
    line = f"{datetime.datetime.now().strftime('%Y-%m-%d %H:%M:%S')} {msg}\n"
    # Only write to stderr if it's a real terminal — launchd redirects stderr
    # to server.log, so writing to both would double every line.
    if sys.stderr.isatty():
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

# ── Structured event log (in-memory ring buffer, 2 000 events) ─────────────────
import collections as _col
_event_log: "collections.deque[dict]" = _col.deque(maxlen=2000)
_event_log_lock = threading.Lock()
_req_tl = threading.local()  # per-request enrichment (set by handlers, read by _route)

def _emit_event(etype: str, action: str, target: str = "", session: str = "",
                detail: str = "", status: int = 200, ip: str = "") -> None:
    with _event_log_lock:
        _event_log.append({
            "ts": time.time(),
            "type": etype,
            "action": action,
            "target": target,
            "session": session,
            "detail": detail,
            "status": status,
            "ip": ip,
        })

def _classify_request(method: str, path: str) -> tuple:
    """Returns (type, action, target, session) from method+path."""
    # Board
    if path == "/api/board" and method == "POST":
        return ("board", "created", "", "")
    m = re.match(r"^/api/board/([A-Za-z0-9-]+)$", path)
    if m:
        iid = m.group(1)
        if method == "PATCH":   return ("board", "updated",  iid, "")
        if method == "DELETE":  return ("board", "deleted",  iid, "")
    m = re.match(r"^/api/board/([A-Za-z0-9-]+)/claim$", path)
    if m and method == "POST":
        return ("board", "claimed", m.group(1), "")
    if path == "/api/board/clear-done" and method == "POST":
        return ("board", "cleared", "done", "")
    if path == "/api/board/statuses" and method == "POST":
        return ("board", "status-added", "", "")
    m = re.match(r"^/api/board/statuses/([a-z0-9-]+)$", path)
    if m:
        sid = m.group(1)
        if method == "DELETE": return ("board", "status-removed", sid, "")
        if method == "PATCH":  return ("board", "status-renamed", sid, "")
    # Sessions
    if path == "/api/sessions" and method == "POST":
        return ("session", "created", "", "")
    m = re.match(r"^/api/sessions/([^/]+)/([^/]+)$", path)
    if m:
        sname, sub = m.group(1), m.group(2)
        if method == "POST"  and sub == "start":   return ("session", "started",      sname, sname)
        if method == "POST"  and sub == "stop":    return ("session", "stopped",      sname, sname)
        if method == "POST"  and sub == "send":    return ("session", "message-sent", sname, sname)
        if method == "POST"  and sub == "archive": return ("session", "archived",     sname, sname)
        if method == "POST"  and sub == "wake":    return ("session", "woken",        sname, sname)
        if method == "PATCH" and sub == "config":  return ("session", "configured",   sname, sname)
        if method == "DELETE":                     return ("session", "deleted",       sname, sname)
        if method == "POST"  and sub == "memory":  return ("memory",  "updated",      sname, sname)
        if method == "POST"  and sub == "peek":    return ("session", "peeked",       sname, sname)
    m = re.match(r"^/api/sessions/([^/]+)$", path)
    if m:
        sname = m.group(1)
        if method == "DELETE": return ("session", "deleted", sname, sname)
        if method == "PATCH":  return ("session", "updated", sname, sname)
    # Memory
    if path == "/api/memory/global" and method == "POST":
        return ("memory", "updated", "global", "")
    # Uploads
    if path.startswith("/api/uploads") and method == "POST":
        return ("file", "uploaded", "", "")
    # System
    if path == "/api/pull" and method == "POST":
        return ("system", "pull", "repo", "")
    # Generic reads (low-priority)
    if method == "GET":
        return ("http", "get", path, "")
    action_map = {"POST": "post", "PATCH": "patch", "DELETE": "delete"}
    return ("http", action_map.get(method, method.lower()), path, "")

# Auto-recovery state
_sse_alerts: list = []           # ring buffer of alert dicts pushed to all SSE clients
_sse_alert_lock = threading.Lock()
_send_locks: dict = {}          # per-session locks for serializing send_text/send_keys
_send_locks_lock = threading.Lock()  # protects _send_locks dict itself
_session_auto_actions: dict = {} # {name: {"last_compact": ts, "last_restart": ts}}

# ── Remote browser (screenshot-based Playwright child process) ──
_rb_proc: subprocess.Popen | None = None
_rb_lock = threading.Lock()
_rb_profile: str = "default"  # current active profile name
_rb_last_video: str | None = None  # path to last recorded video (mp4 or webm)
_RB_PROFILES_DIR = CC_HOME / "playwright-auth" / "profiles"
_RB_VIDEOS_DIR = CC_HOME / "browser-videos"
_RB_RECORDINGS_DIR = CC_HOME / "recordings"


def _profile_has_state(profile_path: Path) -> bool:
    """Return True if the Chromium profile has saved cookies (i.e. the user has logged in)."""
    cookies = profile_path / "Default" / "Cookies"
    return cookies.is_file() and cookies.stat().st_size > 4096


def _ms_to_srt_ts(ms: int) -> str:
    """Convert milliseconds to SRT timecode HH:MM:SS,mmm."""
    h = ms // 3_600_000; ms %= 3_600_000
    m = ms // 60_000;    ms %= 60_000
    s = ms // 1_000;     ms %= 1_000
    return f"{h:02d}:{m:02d}:{s:02d},{ms:03d}"


def _write_srt(captions: list, srt_path: str) -> None:
    """Write a list of {start, end, text} dicts as an SRT subtitle file."""
    lines = []
    for i, c in enumerate(captions, 1):
        lines.append(f"{i}\n{_ms_to_srt_ts(c['start'])} --> {_ms_to_srt_ts(c['end'])}\n{c['text']}")
    Path(srt_path).write_text("\n\n".join(lines) + "\n", encoding="utf-8")


def _rb_process_video(webm: str, captions: list | None = None,
                      profile: str | None = None, task: str | None = None) -> str:
    """Convert WebM → MP4, burning in SRT captions if provided. Returns final path."""
    global _rb_last_video
    _RB_RECORDINGS_DIR.mkdir(parents=True, exist_ok=True)
    ts = int(time.time())
    mp4 = str(_RB_RECORDINGS_DIR / f"recording-{ts}.mp4")

    ffmpeg_base = ["ffmpeg", "-y", "-i", webm,
                   "-c:v", "libx264", "-crf", "18", "-preset", "fast",
                   "-pix_fmt", "yuv420p", "-movflags", "+faststart"]

    if captions:
        srt_path = str(_RB_RECORDINGS_DIR / f"recording-{ts}.srt")
        _write_srt(captions, srt_path)
        # Escape SRT path for ffmpeg subtitle filter (colons need backslash on macOS)
        srt_esc = srt_path.replace("\\", "\\\\").replace(":", "\\:").replace("'", "\\'")
        sub_filter = (f"subtitles='{srt_esc}':force_style="
                      "'FontSize=24,PrimaryColour=&H00FFFFFF,OutlineColour=&H00000000,"
                      "Outline=2,Shadow=1,Alignment=2,MarginV=40'")
        try:
            r = subprocess.run(
                ffmpeg_base + ["-vf", sub_filter, mp4],
                capture_output=True, timeout=180,
            )
            if r.returncode == 0 and os.path.exists(mp4):
                _rb_last_video = mp4
                _rb_write_recording_meta(mp4, profile, task)
                return mp4
        except Exception:
            pass
        # Fallback: no subtitle burn-in
        mp4 = str(_RB_RECORDINGS_DIR / f"recording-{ts}-nosubs.mp4")

    try:
        r = subprocess.run(
            ffmpeg_base + [mp4],
            capture_output=True, timeout=180,
        )
        if r.returncode == 0 and os.path.exists(mp4):
            _rb_last_video = mp4
            _rb_write_recording_meta(mp4, profile, task)
            return mp4
    except Exception:
        pass

    _rb_last_video = webm
    return webm


def _rb_write_recording_meta(mp4_path: str, profile: str | None, task: str | None) -> None:
    """Write a .json sidecar next to an MP4 recording with profile + task metadata."""
    import json as _json
    meta = {k: v for k, v in {"profile": profile or "default", "task": task}.items() if v}
    try:
        Path(mp4_path).with_suffix(".json").write_text(
            _json.dumps(meta, ensure_ascii=False), encoding="utf-8"
        )
    except Exception:
        pass

# ── Browser agent state ──
_rb_agent_state: dict = {
    "running": False, "task": "", "step": 0, "total_steps": 0,
    "action": "", "done": False, "error": None, "video_ready": False,
}
_rb_agent_lock = threading.Lock()

def _rb_agent_update(**kwargs):
    with _rb_agent_lock:
        _rb_agent_state.update(kwargs)

def _run_browser_agent(task: str, start_url: str = "", max_steps: int = 25):
    """Drive the browser with Claude tool-use, record everything, produce MP4."""
    global _rb_last_video
    try:
        import anthropic
    except ImportError as _e:
        _rb_agent_update(running=False, error=f"anthropic not installed: {_e}", action="Error")
        return

    api_key = os.environ.get("ANTHROPIC_API_KEY", "")
    if not api_key:
        _rb_agent_update(running=False, error="ANTHROPIC_API_KEY not set", action="Error")
        return

    client = anthropic.Anthropic(api_key=api_key)
    tools = [
        {"name": "navigate", "description": "Navigate to a URL.",
         "input_schema": {"type": "object", "properties": {"url": {"type": "string"}}, "required": ["url"]}},
        {"name": "click", "description": "Click at pixel coordinates (x, y). The viewport is 1280×800.",
         "input_schema": {"type": "object", "properties": {"x": {"type": "number"}, "y": {"type": "number"}}, "required": ["x", "y"]}},
        {"name": "type", "description": "Type text into the currently focused element.",
         "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
        {"name": "key", "description": "Press a keyboard key, e.g. Enter, Tab, Escape, ArrowDown.",
         "input_schema": {"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}},
        {"name": "scroll", "description": "Scroll the page. dy > 0 scrolls down. Provide x,y to scroll over a specific area (e.g. x=580,y=400 for the email list in Gmail).",
         "input_schema": {"type": "object", "properties": {"dy": {"type": "number"}, "x": {"type": "number"}, "y": {"type": "number"}}, "required": ["dy"]}},
        {"name": "wait", "description": "Pause for ms milliseconds while content loads (max 5000).",
         "input_schema": {"type": "object", "properties": {"ms": {"type": "number"}}, "required": ["ms"]}},
        {"name": "done", "description": "Task complete — stop the browser and save the recording.",
         "input_schema": {"type": "object", "properties": {"summary": {"type": "string"}}, "required": ["summary"]}},
    ]

    messages: list = []
    try:
        _rb_agent_update(action="Starting browser…", total_steps=max_steps)
        start_cmd: dict = {"action": "start", "url": start_url or "about:blank", "record": True}
        with _rb_agent_lock:
            prof = _rb_agent_state.get("_profile", _rb_profile)
        if prof and prof != "default":
            start_cmd["profile"] = prof
        result = _rb_send(start_cmd, timeout=30.0)
        if not result.get("ok"):
            _rb_agent_update(running=False, error=result.get("error", "Browser start failed"))
            return
        for step in range(max_steps):
            with _rb_agent_lock:
                if not _rb_agent_state["running"]:
                    break

            _rb_agent_update(step=step + 1, action=f"Step {step+1}/{max_steps} — screenshot")
            ss = _rb_send({"action": "screenshot"}, timeout=15.0)
            if not ss.get("ok"):
                time.sleep(1)
                continue

            _rb_agent_update(action=f"Step {step+1}/{max_steps} — thinking…")
            messages.append({"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": ss["data"]}},
                {"type": "text", "text":
                    f"URL: {ss.get('url','')}\nTitle: {ss.get('title','')}\n\n"
                    f"Task: {task}\n\n"
                    f"Step {step+1} of up to {max_steps}. "
                    "Continue working on the task. Call done() when finished."},
            ]})

            response = client.messages.create(
                model="claude-sonnet-4-6", max_tokens=1024,
                tools=tools, messages=messages,
            )
            messages.append({"role": "assistant", "content": response.content})

            tool_results = []
            done = False
            for block in response.content:
                if block.type != "tool_use":
                    continue
                name, inp, bid = block.name, block.input, block.id
                label = f"{name}({', '.join(f'{k}={repr(v)}' for k,v in inp.items())})"
                _rb_agent_update(action=f"Step {step+1} — {label}")

                if name == "done":
                    done = True
                    tool_results.append({"type": "tool_result", "tool_use_id": bid, "content": "Done."})
                    break
                elif name == "navigate":
                    r = _rb_send({"action": "navigate", "url": inp["url"]}, timeout=20.0)
                elif name == "click":
                    r = _rb_send({"action": "click", "x": inp["x"], "y": inp["y"]})
                elif name == "type":
                    r = _rb_send({"action": "type", "text": inp["text"]})
                elif name == "key":
                    r = _rb_send({"action": "key", "key": inp["key"]})
                elif name == "scroll":
                    r = _rb_send({"action": "scroll", "dy": inp["dy"],
                                  "x": inp.get("x", 640), "y": inp.get("y", 400)})
                elif name == "wait":
                    time.sleep(min(float(inp.get("ms", 1000)), 5000) / 1000)
                    r = {"ok": True}
                else:
                    r = {"ok": False, "error": f"unknown tool: {name}"}
                tool_results.append({"type": "tool_result", "tool_use_id": bid, "content": json.dumps(r)})

            if tool_results:
                messages.append({"role": "user", "content": tool_results})
            if done or response.stop_reason == "end_turn":
                break

    except Exception as e:
        _rb_agent_update(error=str(e))
    finally:
        _rb_agent_update(action="Converting video…")
        stop_result = _rb_send({"action": "stop"}, timeout=30.0)
        _rb_last_video = None
        webm = stop_result.get("videoPath")
        captions = stop_result.get("captions") or []
        if webm and os.path.exists(webm):
            with _rb_agent_lock:
                agent_prof = _rb_agent_state.get("_profile") or _rb_profile
                agent_task = _rb_agent_state.get("task") or ""
            _rb_process_video(webm, captions or None, profile=agent_prof, task=agent_task or None)
        video_ready = bool(_rb_last_video)
        _rb_agent_update(
            running=False, done=True, video_ready=video_ready,
            action="Video ready — click Download." if video_ready else "Done (no video).",
        )

_RB_AGENT_SCRIPT = r"""
const { chromium } = require('PLAYWRIGHT_PATH');
const { homedir } = require('os');
const readline = require('readline');
const path = require('path');

let ctx = null, page = null;

// ── Caption system ─────────────────────────────────────────────────────────
// Captions are stored with ms-offset from recording start, returned on stop.
let _captions = [];
let _recordingStartMs = 0;
function _addCaption(text, durationMs) {
  if (!_recordingStartMs) return;
  const startMs = Date.now() - _recordingStartMs;
  _captions.push({ start: startMs, end: startMs + (durationMs || 3500), text });
}

// ── Cursor + click-ripple injection script ─────────────────────────────────
// Injected via ctx.addInitScript() — runs on every page in the context.
// Both elements are DOM nodes so they appear in Playwright's recordVideo output.
const _CURSOR_RIPPLE_SCRIPT = `
(function() {
  'use strict';
  var CURSOR_ID = '__amux_cursor__';
  function _attachCursor() {
    if (document.getElementById(CURSOR_ID)) return;
    var el = document.createElement('div');
    el.id = CURSOR_ID;
    el.style.cssText = [
      'position:fixed','pointer-events:none','z-index:2147483647',
      'width:20px','height:20px','border-radius:50%',
      'border:2.5px solid rgba(255,80,0,0.92)',
      'background:rgba(255,80,0,0.18)',
      'transform:translate(-50%,-50%)',
      'transition:left 0.06s linear,top 0.06s linear',
      'box-shadow:0 0 0 3px rgba(255,80,0,0.22)',
      'left:-100px','top:-100px',
    ].join(';');
    document.body.appendChild(el);
    return el;
  }
  var _cursorEl = null;
  document.addEventListener('mousemove', function(e) {
    if (!_cursorEl) _cursorEl = _attachCursor();
    if (_cursorEl) { _cursorEl.style.left = e.clientX+'px'; _cursorEl.style.top = e.clientY+'px'; }
  }, {capture:true, passive:true});
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function() { _cursorEl = _attachCursor(); });
  } else { _cursorEl = _attachCursor(); }

  var _styleEl = document.createElement('style');
  _styleEl.textContent = '@keyframes __amux_ripple{0%{transform:translate(-50%,-50%) scale(0.2);opacity:1}100%{transform:translate(-50%,-50%) scale(2.8);opacity:0}}';
  (document.head || document.documentElement).appendChild(_styleEl);
  document.addEventListener('click', function(e) {
    var r = document.createElement('div');
    r.style.cssText = [
      'position:fixed','pointer-events:none','z-index:2147483646',
      'width:44px','height:44px','border-radius:50%',
      'background:rgba(255,140,0,0.50)',
      'border:3px solid rgba(255,140,0,0.88)',
      'left:'+e.clientX+'px','top:'+e.clientY+'px',
      'animation:__amux_ripple 0.65s ease-out forwards',
    ].join(';');
    document.body.appendChild(r);
    setTimeout(function(){r.remove();}, 750);
  }, {capture:true, passive:true});
})();
`;

// ── Ghost cursor (smooth Bezier mouse movement) ────────────────────────────
let _ghostCursor = null;
try {
  const gcPath = path.resolve(__dirname, 'node_modules/ghost-cursor-playwright');
  const { createCursor } = require(gcPath);
  _ghostCursor = createCursor;
} catch(e) { /* not installed — fall back to linear interpolation */ }

async function _smoothMove(targetPage, x, y) {
  if (_ghostCursor && targetPage) {
    try {
      const cur = await _ghostCursor(targetPage);
      await cur.actions.move({ x, y });
      return;
    } catch(e) { /* fallback */ }
  }
  // Linear interpolation fallback
  const steps = 18;
  for (let i = 1; i <= steps; i++) {
    await targetPage.mouse.move(640 + (x - 640) * (i / steps), 400 + (y - 400) * (i / steps));
    await targetPage.waitForTimeout(16);
  }
}

function attachPageListeners(p) {
  p.on('close', () => {
    // If active page closed, switch to another open page
    if (page === p && ctx) {
      const pages = ctx.pages().filter(pg => !pg.isClosed());
      if (pages.length > 0) page = pages[pages.length - 1];
      else page = null;
    }
  });
}

function switchToPage(p) {
  page = p;
  attachPageListeners(p);
}
const PROFILES_DIR = homedir() + '/.amux/playwright-auth/profiles';
const DEFAULT_PROFILE = homedir() + '/.amux/playwright-auth/profile';
const VIDEOS_DIR = homedir() + '/.amux/browser-videos';
const fs = require('fs');

const rl = readline.createInterface({ input: process.stdin, terminal: false });
rl.on('line', async (line) => {
  let cmd;
  try { cmd = JSON.parse(line); } catch(e) { respond({ok:false,error:'bad json'}); return; }
  try {
    switch (cmd.action) {
      case 'start': {
        if (ctx) { try { await ctx.close(); } catch(e) {} }
        // Use named profile or default
        let profilePath = DEFAULT_PROFILE;
        if (cmd.profile) {
          profilePath = PROFILES_DIR + '/' + cmd.profile.replace(/[^a-zA-Z0-9_-]/g, '');
          fs.mkdirSync(profilePath, { recursive: true });
        }
        const headed = !!cmd.headed;
        const record = !!cmd.record;
        const ctxOpts = {
          headless: !headed,
          viewport: headed ? null : { width: 1280, height: 800 },
          ignoreHTTPSErrors: true,
          args: ['--no-first-run', '--disable-blink-features=AutomationControlled'],
        };
        if (record) {
          fs.mkdirSync(VIDEOS_DIR, { recursive: true });
          ctxOpts.recordVideo = { dir: VIDEOS_DIR, size: { width: 1280, height: 800 } };
        }
        ctx = await chromium.launchPersistentContext(profilePath, ctxOpts);
        // Inject visible cursor + click-ripple into every page
        if (record) {
          await ctx.addInitScript(_CURSOR_RIPPLE_SCRIPT);
          _captions = [];
          _recordingStartMs = Date.now();
        }
        page = ctx.pages()[0] || await ctx.newPage();
        attachPageListeners(page);
        // Auto-switch to new tabs when they open
        ctx.on('page', async (newPage) => {
          switchToPage(newPage);
          await newPage.waitForLoadState('domcontentloaded', { timeout: 10000 }).catch(() => {});
        });
        if (cmd.url) {
          await page.goto(cmd.url, { waitUntil: 'domcontentloaded', timeout: 15000 }).catch(()=>{});
        }
        respond({ ok: true, url: headed ? '' : page.url(), title: headed ? '' : await page.title(), profile: cmd.profile || 'default', headed });
        break;
      }
      case 'navigate': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        await page.goto(cmd.url, { waitUntil: 'domcontentloaded', timeout: 15000 }).catch(()=>{});
        await page.waitForTimeout(300);
        respond({ ok: true, url: page.url(), title: await page.title() });
        break;
      }
      case 'click': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        const pagesBefore = ctx ? ctx.pages().length : 0;
        await page.mouse.move(cmd.x, cmd.y);
        await page.waitForTimeout(150);
        await page.mouse.click(cmd.x, cmd.y, { button: cmd.button || 'left' });
        await page.waitForTimeout(400);
        // If a new tab opened, ctx.on('page') already switched page; just wait for it to load
        const pagesAfter = ctx ? ctx.pages().length : 0;
        if (pagesAfter > pagesBefore) {
          await page.waitForLoadState('domcontentloaded', {timeout: 8000}).catch(()=>{});
          await page.waitForTimeout(500);
        } else {
          await page.waitForLoadState('domcontentloaded', {timeout: 5000}).catch(()=>{});
          await page.waitForTimeout(800);
        }
        const tabs = ctx ? ctx.pages().filter(p => !p.isClosed()).map((p,i) => ({ i, url: p.url(), title: p.title() })) : [];
        respond({ ok: true, url: page.url(), title: await page.title(), tabs });
        break;
      }
      case 'type': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        await page.keyboard.type(cmd.text, { delay: 30 });
        await page.waitForTimeout(200);
        respond({ ok: true });
        break;
      }
      case 'key': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        await page.keyboard.press(cmd.key);
        await page.waitForTimeout(200);
        respond({ ok: true });
        break;
      }
      case 'scroll': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        await page.mouse.move(cmd.x || 640, cmd.y || 400);
        await page.mouse.wheel(0, cmd.dy || 300);
        await page.waitForTimeout(400);
        respond({ ok: true });
        break;
      }
      case 'smooth_move': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        await _smoothMove(page, cmd.x, cmd.y);
        respond({ ok: true });
        break;
      }
      case 'caption': {
        // Add a timed caption to the recording (stored with ms offset from start)
        _addCaption(cmd.text, cmd.duration_ms);
        respond({ ok: true, count: _captions.length });
        break;
      }
      case 'back': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        await page.goBack({ waitUntil: 'domcontentloaded', timeout: 10000 }).catch(()=>{});
        await page.waitForTimeout(300);
        respond({ ok: true, url: page.url(), title: await page.title() });
        break;
      }
      case 'forward': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        await page.goForward({ waitUntil: 'domcontentloaded', timeout: 10000 }).catch(()=>{});
        await page.waitForTimeout(300);
        respond({ ok: true, url: page.url(), title: await page.title() });
        break;
      }
      case 'reload': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        await page.reload({ waitUntil: 'domcontentloaded', timeout: 15000 }).catch(()=>{});
        await page.waitForTimeout(300);
        respond({ ok: true, url: page.url(), title: await page.title() });
        break;
      }
      case 'screenshot': {
        if (!page) { respond({ok:false,error:'no page'}); break; }
        const buf = await page.screenshot({ type: 'jpeg', quality: 70 });
        const allPages = ctx ? ctx.pages().filter(p => !p.isClosed()) : [];
        const tabIdx = allPages.indexOf(page);
        const tabs = await Promise.all(allPages.map(async (p, i) => ({ i, url: p.url(), title: await p.title() })));
        respond({ ok: true, data: buf.toString('base64'), url: page.url(), title: await page.title(), tabs, activeTab: tabIdx });
        break;
      }
      case 'tabs': {
        if (!ctx) { respond({ok:false,error:'no context'}); break; }
        const allPages = ctx.pages().filter(p => !p.isClosed());
        const activeIdx = allPages.indexOf(page);
        const tabs = await Promise.all(allPages.map(async (p, i) => ({ i, url: p.url(), title: await p.title() })));
        respond({ ok: true, tabs, activeTab: activeIdx });
        break;
      }
      case 'switch_tab': {
        if (!ctx) { respond({ok:false,error:'no context'}); break; }
        const allPages = ctx.pages().filter(p => !p.isClosed());
        const idx = cmd.index;
        if (idx < 0 || idx >= allPages.length) { respond({ok:false,error:'tab index out of range'}); break; }
        switchToPage(allPages[idx]);
        await page.bringToFront().catch(()=>{});
        const buf = await page.screenshot({ type: 'jpeg', quality: 70 });
        const tabs = await Promise.all(allPages.map(async (p, i) => ({ i, url: p.url(), title: await p.title() })));
        respond({ ok: true, data: buf.toString('base64'), url: page.url(), title: await page.title(), tabs, activeTab: idx });
        break;
      }
      case 'close_tab': {
        if (!ctx) { respond({ok:false,error:'no context'}); break; }
        const allPages = ctx.pages().filter(p => !p.isClosed());
        const idx = cmd.index !== undefined ? cmd.index : allPages.indexOf(page);
        if (idx >= 0 && idx < allPages.length) {
          const toClose = allPages[idx];
          if (toClose === page) {
            // Switch to adjacent tab before closing
            const next = allPages[idx + 1] || allPages[idx - 1];
            if (next) switchToPage(next);
          }
          await toClose.close().catch(()=>{});
        }
        const remaining = ctx.pages().filter(p => !p.isClosed());
        const tabs = await Promise.all(remaining.map(async (p, i) => ({ i, url: p.url(), title: await p.title() })));
        const activeIdx = page ? remaining.indexOf(page) : 0;
        if (page && !page.isClosed()) {
          const buf = await page.screenshot({ type: 'jpeg', quality: 70 });
          respond({ ok: true, data: buf.toString('base64'), url: page.url(), title: await page.title(), tabs, activeTab: activeIdx });
        } else {
          respond({ ok: true, tabs, activeTab: activeIdx });
        }
        break;
      }
      case 'stop': {
        let videoPath = null;
        const capturesCopy = [..._captions];
        if (ctx) {
          try {
            const vid = page && page.video ? page.video() : null;
            await ctx.close();
            if (vid) { try { videoPath = await vid.path(); } catch(e) {} }
          } catch(e) {}
          ctx = null; page = null;
        }
        _captions = []; _recordingStartMs = 0;
        respond({ ok: true, videoPath, captions: capturesCopy });
        process.exit(0);
        break;
      }
      default:
        respond({ ok: false, error: 'unknown action: ' + cmd.action });
    }
  } catch(e) {
    respond({ ok: false, error: e.message });
  }
});

function respond(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}
process.on('SIGTERM', () => { if (ctx) ctx.close().finally(() => process.exit(0)); });
"""

def _rb_send(cmd: dict, timeout: float = 20.0) -> dict:
    """Send a command to the remote browser child process and return the JSON response."""
    global _rb_proc
    with _rb_lock:
        if _rb_proc is None or _rb_proc.poll() is not None:
            if cmd.get("action") != "start":
                return {"ok": False, "error": "browser not running"}
            # Start the child process
            pw_path = str(Path(__file__).resolve().parent / "node_modules" / "playwright")
            script = _RB_AGENT_SCRIPT.replace("PLAYWRIGHT_PATH", pw_path.replace("\\", "/"))
            _rb_proc = subprocess.Popen(
                ["node", "-e", script],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, bufsize=1,
            )
        proc = _rb_proc
        try:
            proc.stdin.write(json.dumps(cmd) + "\n")
            proc.stdin.flush()
            # Read one line of response (with timeout)
            import select
            ready, _, _ = select.select([proc.stdout], [], [], timeout)
            if not ready:
                return {"ok": False, "error": "timeout"}
            line = proc.stdout.readline()
            if not line:
                _rb_proc = None
                return {"ok": False, "error": "process exited"}
            return json.loads(line)
        except Exception as e:
            _rb_proc = None
            return {"ok": False, "error": str(e)}

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

_tmux_name_migrated = set()  # Sessions already checked for legacy name migration

def tmux_name(session: str) -> str:
    new = f"amux-{session}"
    # Only check for legacy cmux-*/cc-* names once per session per process lifetime
    if session not in _tmux_name_migrated:
        _tmux_name_migrated.add(session)
        for old in [f"cmux-{session}", f"cc-{session}"]:
            try:
                r = subprocess.run(["tmux", "has-session", "-t", old], capture_output=True, timeout=3)
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


def _tmux_capture_batch(sessions: list, lines: int = 30) -> dict:
    """Capture pane output for multiple sessions in parallel using threads.
    Returns {session_name: output_str}."""
    if not sessions:
        return {}
    from concurrent.futures import ThreadPoolExecutor
    def _cap(name):
        return name, tmux_capture(name, lines)
    with ThreadPoolExecutor(max_workers=min(len(sessions), 16)) as pool:
        return dict(pool.map(_cap, sessions))


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


# ── Yolo auto-responder ──────────────────────────────────────────────────────
# When a session runs with --dangerously-skip-permissions, some Claude Code
# internal safety prompts still require a keypress. We detect and answer them.
_YOLO_PROMPTS = [
    # Claude Code internal safety prompts always show "Esc to cancel" UI chrome.
    # Model-level direction questions never do. We require that marker so we never
    # auto-answer open-ended questions where the model is asking for user guidance.
    #
    # Shell command substitution: "Command contains $() command substitution … Esc to cancel"
    (re.compile(r'command contains.*command substitution.*esc to cancel', re.IGNORECASE | re.DOTALL), '1'),
    # Generic tool proceed prompt: "Do you want to proceed? ❯ 1. Yes … Esc to cancel"
    (re.compile(r'do you want to proceed.*esc to cancel', re.IGNORECASE | re.DOTALL), '1'),
    # Leaked permission prompt: "Yes, and don't ask again … Esc to cancel"
    (re.compile(r'yes.*and don.t ask again.*esc to cancel', re.IGNORECASE | re.DOTALL), '1'),
]
_YOLO_COOLDOWN = 6  # seconds between auto-responses per session
_yolo_last_responded: dict = {}


def _yolo_auto_respond():
    """Check yolo sessions for known blocking prompts and auto-answer them."""
    now = time.time()
    for f in CC_SESSIONS.glob("*.env"):
        name = f.stem
        try:
            if not is_running(name):
                continue
            if now - _yolo_last_responded.get(name, 0) < _YOLO_COOLDOWN:
                continue
            cfg = parse_env_file(f)
            if '--dangerously-skip-permissions' not in cfg.get('CC_FLAGS', ''):
                continue
            raw = tmux_capture(name, 20)
            if not raw:
                continue
            clean = re.sub(r'\x1b\[[0-9;]*[a-zA-Z]|\x1b[^a-zA-Z]*[a-zA-Z]', '', raw)
            for pattern, response in _YOLO_PROMPTS:
                if pattern.search(clean):
                    send_text(name, response)
                    _yolo_last_responded[name] = now
                    break
        except Exception:
            pass


def _yolo_loop():
    try:
        _yolo_auto_respond()
    except Exception:
        pass


def _push_alert(alert_type: str, session: str, message: str):
    """Enqueue an alert to be streamed to all SSE clients."""
    global _sse_alerts
    with _sse_alert_lock:
        _sse_alerts.append({"type": alert_type, "session": session, "message": message, "ts": int(time.time())})
        if len(_sse_alerts) > 50:
            _sse_alerts = _sse_alerts[-50:]


# ── Event log (persistent, streamed via SSE) ─────────────────────────
_log_ring: list = []   # in-memory ring buffer for SSE push
_log_ring_lock = threading.Lock()

def _log_event(category: str, action: str, *, session: str = None,
               actor: str = None, detail: str = None, level: str = "info"):
    """Write an event to the logs table and push to SSE clients."""
    ts = int(time.time())
    try:
        db = get_db()
        db.execute(
            "INSERT INTO logs (ts, category, action, session, actor, detail, level) "
            "VALUES (?,?,?,?,?,?,?)",
            (ts, category, action, session, actor, detail, level))
        db.commit()
    except Exception:
        pass
    evt = {"ts": ts, "category": category, "action": action,
           "type": category, "target": detail,  # aliases for _event_log schema compat
           "session": session, "actor": actor, "detail": detail, "level": level}
    with _log_ring_lock:
        _log_ring.append(evt)
        if len(_log_ring) > 100:
            _log_ring[:] = _log_ring[-100:]
    # Also push to the SSE event log ring so the Logs tab receives it live
    with _event_log_lock:
        _event_log.append(evt)


def _last_meaningful_user_message(work_dir: str) -> str:
    """Extract the last meaningful user message (>20 chars) from the session's JSONL history."""
    if not work_dir:
        return ""
    resolved = str(Path(work_dir).expanduser().resolve())
    project_dir = CLAUDE_HOME / "projects" / resolved.replace("/", "-")
    if not project_dir.is_dir():
        return ""
    jsonl_files = sorted(project_dir.glob("*.jsonl"), key=lambda f: f.stat().st_mtime, reverse=True)
    if not jsonl_files:
        return ""
    last_msg = ""
    try:
        for line in jsonl_files[0].read_text(errors="replace").splitlines():
            try:
                entry = json.loads(line)
                msg = entry.get("message", {})
                if msg.get("role") == "user":
                    content = msg.get("content", [])
                    text = ""
                    if isinstance(content, list):
                        for block in content:
                            if isinstance(block, dict) and block.get("type") == "text":
                                text = block.get("text", "")
                    elif isinstance(content, str):
                        text = content
                    if len(text) > 20:
                        last_msg = text
            except (json.JSONDecodeError, AttributeError):
                continue
    except Exception:
        pass
    return last_msg


_STRIP_ANSI = re.compile(
    r'\x1b\[[0-9;]*[a-zA-Z]|\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07'
    r'|\x1b\][^\x1b]*\x1b\\|\x1b[()][A-Z0-9]|\x1b[\x20-\x2f]*[\x40-\x7e]'
)


def _snapshot_all_sessions():
    """Capture scrollback for all running sessions and save to disk.

    Also runs health checks on each session's output:
    1. Proactive: if context < 20% remaining before auto-compact, send /compact
    2. Reactive: if thinking-block corruption error detected, restart conversation
       and replay the last meaningful user message automatically.
    3. Auto-continue: if CC_AUTO_CONTINUE=1 and session is stuck waiting for user
       input for 2+ consecutive snapshots (~60s), auto-respond to unblock it.
    """
    for f in CC_SESSIONS.glob("*.env"):
        name = f.stem
        if not is_running(name):
            continue
        try:
            output = tmux_capture(name, 5000)
            if not output:
                continue
            save_session_log(name, output)

            # Strip ANSI codes for pattern matching
            clean = _STRIP_ANSI.sub("", output)
            now = time.time()
            actions = _session_auto_actions.setdefault(name, {})

            # ── 1. Proactive: auto-compact when context is low ──────────────
            ctx_match = re.search(r'context left until auto-compact[:\s]+(\d+)%', clean, re.IGNORECASE)
            if ctx_match:
                pct = int(ctx_match.group(1))
                _ac_row = get_db().execute("SELECT value FROM prefs WHERE key='auto_compact_enabled'").fetchone()
                _ac_enabled = (_ac_row is None) or (_ac_row[0] != "0")  # default ON
                if _ac_enabled and pct < 20 and now - actions.get("last_compact", 0) > 300:
                    actions["last_compact"] = now
                    actions["post_compact_continue"] = True  # send continuation when compact finishes
                    send_text(name, "/compact")
                    _push_alert("auto_compact", name,
                                f"Auto-compacted '{name}' — context was at {pct}%")

            # ── 2. Reactive: thinking-block corruption → restart + replay ───
            if ("redacted_thinking" in clean and
                    "cannot be modified" in clean and
                    now - actions.get("last_restart", 0) > 120):
                actions["last_restart"] = now
                wd = _session_work_dir(name)
                last_msg = _last_meaningful_user_message(wd)
                stop_session(name)
                meta = _load_meta(name)
                meta.pop("cc_conversation_id", None)
                _save_meta(name, meta)
                start_session(name)
                if last_msg:
                    def _replay(sname=name, msg=last_msg):
                        time.sleep(6)
                        send_text(sname, msg)
                    threading.Thread(target=_replay, daemon=True).start()
                _push_alert("thinking_reset", name,
                            f"Session '{name}' auto-restarted: thinking block corruption"
                            + (" — last message replayed" if last_msg else ""))

            # ── 3. Auto-continue: unblock waiting sessions ───────────────────
            status = _detect_claude_status(clean)

            # ── 3a. Post-compact continuation ──────────────────────────────────
            # After we trigger auto-compact, the session goes idle at the ❯ prompt.
            # status == 'idle' doesn't trigger normal auto-continue, so we handle
            # it explicitly: wait ≥30s for compact to finish, then send a continue msg.
            if actions.get("post_compact_continue") and status in ("idle", "waiting"):
                elapsed_since_compact = now - actions.get("last_compact", 0)
                if elapsed_since_compact > 30 and now - actions.get("last_auto_continue", 0) > 60:
                    cfg_ac = parse_env_file(f)
                    cont_msg = cfg_ac.get("CC_AUTO_CONTINUE_MSG", "continue")
                    send_text(name, cont_msg)
                    actions["last_auto_continue"] = now
                    actions.pop("post_compact_continue", None)
                    _push_alert("auto_continue", name,
                                f"Post-compact auto-continue sent to '{name}'")

            if status == "waiting":
                if "ac_waiting_since" not in actions:
                    # First snapshot seeing this session waiting — remember it
                    actions["ac_waiting_since"] = now
                else:
                    # Still waiting on a subsequent snapshot — check opt-in flag
                    cfg_ac = parse_env_file(f)
                    if cfg_ac.get("CC_AUTO_CONTINUE") in ("1", "true", "yes"):
                        if now - actions.get("last_auto_continue", 0) > 300:
                            # Determine what kind of waiting and what to send
                            lines_ac = [l.strip() for l in clean.splitlines() if l.strip()]
                            response = None
                            label = ""
                            for l in reversed(lines_ac[-15:]):
                                sl = l.lower()
                                if "enter to select" in sl:
                                    response = ""   # bare Enter → select current option
                                    label = "Enter (select option)"
                                    break
                                if re.match(r".*\u276f\s*\d+\.", l):  # ❯ 1. Yes selector
                                    response = "1"
                                    label = "1 (first option)"
                                    break
                                if "do you want to proceed" in sl:
                                    response = "1"
                                    label = "1 (proceed)"
                                    break
                                if "interrupted" in sl and "what should claude do" in sl:
                                    custom = cfg_ac.get("CC_AUTO_CONTINUE_MSG", "continue")
                                    response = custom
                                    label = repr(custom)
                                    break
                            if response is not None:
                                send_text(name, response)
                                actions["last_auto_continue"] = now
                                actions.pop("ac_waiting_since", None)
                                _push_alert("auto_continue", name,
                                            f"Auto-continued '{name}': sent {label}")
            else:
                # Session no longer waiting — reset tracking
                actions.pop("ac_waiting_since", None)
        except Exception:
            pass


def _snapshot_loop():
    try:
        _snapshot_all_sessions()
    except Exception:
        pass


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


_model_cache = {}  # {work_dir: (model, mtime, timestamp)}
_MODEL_CACHE_TTL = 15  # seconds

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
    f = jsonl_files[0]
    try:
        mtime = f.stat().st_mtime
    except OSError:
        return ""
    # Cache hit: same file mtime and within TTL
    cached = _model_cache.get(work_dir)
    if cached:
        c_model, c_mtime, c_ts = cached
        if c_mtime == mtime or (time.time() - c_ts < _MODEL_CACHE_TTL):
            return c_model
    try:
        # Search from end in increasing chunks until we find a model
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
                        _model_cache[work_dir] = (model, mtime, time.time())
                        return model
                except (json.JSONDecodeError, AttributeError):
                    continue
            if chunk_size >= size:
                break
    except Exception:
        pass
    _model_cache[work_dir] = ("", mtime, time.time())
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
CREATE TABLE IF NOT EXISTS schedules (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    session     TEXT NOT NULL,
    command     TEXT NOT NULL,
    sched_type  TEXT NOT NULL DEFAULT 'once',
    recurrence  TEXT,
    run_at      TEXT,
    next_run    TEXT,
    last_run    TEXT,
    enabled     INTEGER NOT NULL DEFAULT 1,
    created     INTEGER NOT NULL,
    updated     INTEGER NOT NULL,
    deleted     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_schedules_next ON schedules(next_run) WHERE deleted IS NULL AND enabled=1;
CREATE TABLE IF NOT EXISTS reports (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    type         TEXT NOT NULL DEFAULT 'infra-spend',
    config       TEXT NOT NULL DEFAULT '{}',
    position     INTEGER NOT NULL DEFAULT 0,
    created      INTEGER NOT NULL,
    last_refresh INTEGER,
    cached_data  TEXT
);
CREATE TABLE IF NOT EXISTS prefs (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS logs (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    ts       INTEGER NOT NULL,
    category TEXT NOT NULL DEFAULT 'system',
    action   TEXT NOT NULL,
    session  TEXT,
    actor    TEXT,
    detail   TEXT,
    level    TEXT NOT NULL DEFAULT 'info'
);
CREATE INDEX IF NOT EXISTS idx_logs_ts       ON logs(ts);
CREATE INDEX IF NOT EXISTS idx_logs_category ON logs(category);
CREATE INDEX IF NOT EXISTS idx_logs_session  ON logs(session) WHERE session IS NOT NULL;
CREATE TABLE IF NOT EXISTS email_accounts (
    id           TEXT PRIMARY KEY,
    email        TEXT NOT NULL UNIQUE,
    access_token TEXT,
    refresh_token TEXT,
    token_expiry INTEGER,
    calendar_id  TEXT NOT NULL DEFAULT 'primary',
    last_synced  INTEGER,
    created      INTEGER NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS email_events (
    id               TEXT PRIMARY KEY,
    account_id       TEXT NOT NULL,
    gmail_message_id TEXT NOT NULL,
    gmail_thread_id  TEXT,
    email_subject    TEXT,
    email_from       TEXT,
    email_date       TEXT,
    event_title      TEXT,
    event_start      TEXT,
    event_end        TEXT,
    event_location   TEXT,
    event_description TEXT,
    calendar_event_id TEXT,
    status           TEXT NOT NULL DEFAULT 'pending',
    raw_extract      TEXT,
    created          INTEGER NOT NULL,
    UNIQUE(account_id, gmail_message_id)
);
CREATE INDEX IF NOT EXISTS idx_email_events_account ON email_events(account_id);
CREATE INDEX IF NOT EXISTS idx_email_events_status  ON email_events(status);

CREATE TABLE IF NOT EXISTS waitlist (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    email    TEXT NOT NULL UNIQUE,
    note     TEXT,
    ts       INTEGER NOT NULL
);
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
        "ALTER TABLE issues ADD COLUMN due_time TEXT",
        "ALTER TABLE issues ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
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


# ── Report type registry ─────────────────────────────────────────────────────
# Each entry: { label, description, vendors: { id: { label, color, env_vars, fetch } } }
# To add a new report type, append an entry here — no other code changes needed.

def _report_fetch_anyscale(cfg):
    import json as _j, urllib.request as _ur
    key = os.environ.get("AMUX_ANYSCALE_API_KEY", "")
    if not key:
        return {"name": "Anyscale", "error": "AMUX_ANYSCALE_API_KEY not set", "daily": [], "monthly": []}
    try:
        req = _ur.Request("https://console.anyscale.com/api/v2/billing_invoices?limit=12",
                          headers={"Authorization": f"Bearer {key}"})
        with _ur.urlopen(req, timeout=10) as r:
            inv = _j.loads(r.read())
        monthly = [{"month": i.get("period_start", "")[:7],
                    "amount": float(i.get("amount_due", 0)) / 100}
                   for i in inv.get("results", [])]
        return {"name": "Anyscale", "daily": [], "monthly": monthly, "currency": "USD", "error": None}
    except Exception as e:
        return {"name": "Anyscale", "error": str(e), "daily": [], "monthly": []}


def _report_fetch_render(cfg):
    import json as _j, urllib.request as _ur
    key = os.environ.get("AMUX_RENDER_API_KEY", "")
    if not key:
        return {"name": "Render", "error": "AMUX_RENDER_API_KEY not set", "daily": [], "monthly": []}
    try:
        req = _ur.Request("https://api.render.com/v1/billing/summary",
                          headers={"Authorization": f"Bearer {key}", "Accept": "application/json"})
        with _ur.urlopen(req, timeout=10) as r:
            data = _j.loads(r.read())
        monthly = [{"month": m.get("month", ""), "amount": float(m.get("total", 0))}
                   for m in data.get("monthlyCharges", [])]
        return {"name": "Render", "daily": [], "monthly": monthly, "currency": "USD", "error": None}
    except Exception as e:
        return {"name": "Render", "error": str(e), "daily": [], "monthly": []}


def _report_fetch_mongo(cfg):
    import json as _j, urllib.request as _ur, urllib.parse as _up
    import hashlib as _hl, http.client as _hc, ssl as _ssl, time as _t
    pub  = os.environ.get("AMUX_MONGO_PUBLIC_KEY", "")
    priv = os.environ.get("AMUX_MONGO_PRIVATE_KEY", "")
    org  = os.environ.get("AMUX_MONGO_ORG_ID", "")
    if not (pub and priv and org):
        return {"name": "MongoDB", "error": "AMUX_MONGO_PUBLIC_KEY / PRIVATE_KEY / ORG_ID not set",
                "daily": [], "monthly": []}
    try:
        url = f"https://cloud.mongodb.com/api/atlas/v2/orgs/{org}/invoices?includePartialInvoices=true"
        parsed = _up.urlparse(url)
        # First pass to get Digest challenge
        conn = _hc.HTTPSConnection(parsed.netloc, context=_ssl.create_default_context())
        conn.request("GET", parsed.path + "?" + parsed.query,
                     headers={"Accept": "application/vnd.atlas.2023-01-01+json"})
        resp = conn.getresponse(); resp.read()
        if resp.status == 401:
            www = resp.getheader("WWW-Authenticate", "")
            realm_m = re.search(r'realm="([^"]+)"', www)
            nonce_m = re.search(r'nonce="([^"]+)"', www)
            if realm_m and nonce_m:
                realm, nonce = realm_m.group(1), nonce_m.group(1)
                ha1  = _hl.md5(f"{pub}:{realm}:{priv}".encode()).hexdigest()
                ha2  = _hl.md5(f"GET:{parsed.path}".encode()).hexdigest()
                nc = "00000001"; cnonce = _hl.md5(str(_t.time()).encode()).hexdigest()[:8]
                rh   = _hl.md5(f"{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}".encode()).hexdigest()
                auth = (f'Digest username="{pub}",realm="{realm}",nonce="{nonce}"'
                        f',uri="{parsed.path}",qop=auth,nc={nc},cnonce="{cnonce}",response="{rh}"')
                conn2 = _hc.HTTPSConnection(parsed.netloc, context=_ssl.create_default_context())
                conn2.request("GET", parsed.path + "?" + parsed.query,
                              headers={"Accept": "application/vnd.atlas.2023-01-01+json", "Authorization": auth})
                resp = conn2.getresponse()
        body_b = resp.read()
        body_d = _j.loads(body_b)
        monthly = [{"month": inv.get("startDate", "")[:7],
                    "amount": float(inv.get("amountBilledCents", 0)) / 100}
                   for inv in body_d.get("results", [])]
        return {"name": "MongoDB", "daily": [], "monthly": monthly, "currency": "USD", "error": None}
    except Exception as e:
        return {"name": "MongoDB", "error": str(e), "daily": [], "monthly": []}


def _report_fetch_gcp(cfg):
    import json as _j, urllib.request as _ur, urllib.parse as _up
    import base64 as _b64
    from datetime import date, timedelta
    sa_path = os.environ.get("AMUX_GCP_SA_KEY_PATH", "")
    billing_acct = os.environ.get("AMUX_GCP_BILLING_ACCOUNT", "")
    if not (sa_path and billing_acct):
        return {"name": "GCP", "error": "AMUX_GCP_SA_KEY_PATH and AMUX_GCP_BILLING_ACCOUNT not set",
                "daily": [], "monthly": []}
    try:
        import time as _t
        with open(sa_path) as f:
            sa = _j.load(f)
        now = int(_t.time())
        header  = _b64.urlsafe_b64encode(_j.dumps({"alg": "RS256", "typ": "JWT"}).encode()).rstrip(b"=")
        payload = _b64.urlsafe_b64encode(_j.dumps({
            "iss": sa["client_email"],
            "scope": "https://www.googleapis.com/auth/cloud-billing.readonly",
            "aud": "https://oauth2.googleapis.com/token", "iat": now, "exp": now + 3600
        }).encode()).rstrip(b"=")
        try:
            from cryptography.hazmat.primitives import serialization, hashes
            from cryptography.hazmat.primitives.asymmetric import padding as _pad
            pk = serialization.load_pem_private_key(sa["private_key"].encode(), password=None)
            sig = _b64.urlsafe_b64encode(pk.sign(header + b"." + payload, _pad.PKCS1v15(), hashes.SHA256())).rstrip(b"=")
        except ImportError:
            return {"name": "GCP", "error": "cryptography package required: pip install cryptography",
                    "daily": [], "monthly": []}
        jwt = (header + b"." + payload + b"." + sig).decode()
        tok_req = _ur.Request("https://oauth2.googleapis.com/token",
            data=_up.urlencode({"grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer", "assertion": jwt}).encode(),
            headers={"Content-Type": "application/x-www-form-urlencoded"})
        with _ur.urlopen(tok_req, timeout=10) as r:
            access_token = _j.loads(r.read())["access_token"]
        start = (date.today().replace(day=1) - timedelta(days=365)).isoformat()
        url = (f"https://cloudbilling.googleapis.com/v1/{billing_acct}/reports"
               f"?interval.startDate={start}")
        req = _ur.Request(url, headers={"Authorization": f"Bearer {access_token}"})
        with _ur.urlopen(req, timeout=15) as r:
            data = _j.loads(r.read())
        monthly = [{"month": row.get("month", ""), "amount": float(row.get("costAmount", 0))}
                   for row in data.get("reports", [])]
        return {"name": "GCP", "daily": [], "monthly": monthly, "currency": "USD", "error": None}
    except Exception as e:
        return {"name": "GCP", "error": str(e), "daily": [], "monthly": []}


def _report_fetch_qdrant(cfg):
    import json as _j, urllib.request as _ur
    key = os.environ.get("AMUX_QDRANT_CLOUD_API_KEY", "")
    if not key:
        return {"name": "Qdrant", "error": "AMUX_QDRANT_CLOUD_API_KEY not set", "daily": [], "monthly": []}
    try:
        req = _ur.Request("https://cloud.qdrant.io/public/v1/billing/invoices",
                          headers={"api-key": key, "Accept": "application/json"})
        with _ur.urlopen(req, timeout=10) as r:
            data = _j.loads(r.read())
        monthly = [{"month": inv.get("period", "")[:7], "amount": float(inv.get("total", 0))}
                   for inv in data.get("invoices", [])]
        return {"name": "Qdrant", "daily": [], "monthly": monthly, "currency": "USD", "error": None}
    except Exception as e:
        return {"name": "Qdrant", "error": str(e), "daily": [], "monthly": []}


def _report_fetch_mixpeek_ops_all(cfg):
    """Fetch all vendor spend from the Mixpeek ops server in a single HTTP call.

    Returns {vendor_id: {name, monthly, daily, error}} — the same structure the
    ops server's /dashboard/spend endpoint already returns.

    Config keys (all optional):
      ops_url   — override AMUX_MIXPEEK_OPS_URL env var
      ops_token — override AMUX_MIXPEEK_OPS_TOKEN env var
      months    — months of history (default 12)
    """
    import json as _j, urllib.request as _ur, ssl as _ssl
    _VENDORS = ("render", "gcp_cloud_run", "mongodb_atlas", "gke", "qdrant_cloud")
    url    = cfg.get("ops_url")   or os.environ.get("AMUX_MIXPEEK_OPS_URL", "")
    token  = cfg.get("ops_token") or os.environ.get("AMUX_MIXPEEK_OPS_TOKEN", "")
    months = int(cfg.get("months", 12))
    if not url:
        err = "AMUX_MIXPEEK_OPS_URL not set"
        return {v: {"name": v, "error": err, "daily": [], "monthly": []} for v in _VENDORS}
    try:
        req = _ur.Request(
            f"{url.rstrip('/')}/api/dashboard/spend?months={months}",
            headers={"Authorization": f"Bearer {token}", "Accept": "application/json"},
        )
        ctx = _ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = _ssl.CERT_NONE
        with _ur.urlopen(req, timeout=30, context=ctx) as resp:
            return _j.loads(resp.read())
    except Exception as e:
        err = str(e)
        return {v: {"name": v, "error": err, "daily": [], "monthly": []} for v in _VENDORS}


def _report_fetch_posthog_all(cfg):
    """Fetch PostHog product analytics from the Mixpeek ops server.

    Returns {metric_id: {name, monthly, weekly, daily, error}} where each
    metric contains user/event counts (not dollar amounts).

    Config keys (all optional):
      ops_url   — override AMUX_MIXPEEK_OPS_URL env var
      ops_token — override AMUX_MIXPEEK_OPS_TOKEN env var
      days      — days of history (default 90)
    """
    import json as _j, urllib.request as _ur, ssl as _ssl
    _METRICS = ("active_users", "new_users", "total_events")
    url   = cfg.get("ops_url")   or os.environ.get("AMUX_MIXPEEK_OPS_URL", "")
    token = cfg.get("ops_token") or os.environ.get("AMUX_MIXPEEK_OPS_TOKEN", "")
    days  = int(cfg.get("days", 90))
    if not url:
        err = "AMUX_MIXPEEK_OPS_URL not set"
        return {m: {"name": m, "error": err, "daily": [], "weekly": [], "monthly": []} for m in _METRICS}
    try:
        req = _ur.Request(
            f"{url.rstrip('/')}/api/dashboard/posthog?days={days}",
            headers={"Authorization": f"Bearer {token}", "Accept": "application/json"},
        )
        ctx = _ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = _ssl.CERT_NONE
        with _ur.urlopen(req, timeout=60, context=ctx) as resp:
            return _j.loads(resp.read())
    except Exception as e:
        err = str(e)
        return {m: {"name": m, "error": err, "daily": [], "weekly": [], "monthly": []} for m in _METRICS}


# Registry maps type_id → metadata + vendor fetchers.
# To add a new report type: add an entry here. The UI reads /api/reports/types dynamically.
_REPORT_TYPE_REGISTRY = {
    "infra-spend": {
        "label": "Infrastructure Spend",
        "description": "Aggregate cloud & infrastructure spend across vendors (daily/weekly/monthly)",
        "vendors": {
            "gcp":      {"label": "GCP",       "color": "#4285F4",
                         "env_vars": ["AMUX_GCP_SA_KEY_PATH", "AMUX_GCP_BILLING_ACCOUNT"],
                         "fetch": _report_fetch_gcp},
            "anyscale": {"label": "Anyscale",  "color": "#FF6B35",
                         "env_vars": ["AMUX_ANYSCALE_API_KEY"],
                         "fetch": _report_fetch_anyscale},
            "render":   {"label": "Render",    "color": "#46E3B7",
                         "env_vars": ["AMUX_RENDER_API_KEY"],
                         "fetch": _report_fetch_render},
            "mongo":    {"label": "MongoDB",   "color": "#47A248",
                         "env_vars": ["AMUX_MONGO_PUBLIC_KEY", "AMUX_MONGO_PRIVATE_KEY", "AMUX_MONGO_ORG_ID"],
                         "fetch": _report_fetch_mongo},
            "qdrant":   {"label": "Qdrant",    "color": "#DC244C",
                         "env_vars": ["AMUX_QDRANT_CLOUD_API_KEY"],
                         "fetch": _report_fetch_qdrant},
        },
    },
    "mixpeek-vendor-spend": {
        "label": "Mixpeek Vendor Spend",
        "description": "Infrastructure vendor spend via Mixpeek ops server (Render, GCP Cloud Run, MongoDB Atlas, GKE Autopilot, Qdrant Cloud). Config: months (default 12), ops_url, ops_token.",
        "report_fetch": _report_fetch_mixpeek_ops_all,
        "vendors": {
            "render":        {"label": "Render",        "color": "#46E3B7",
                              "env_vars": ["AMUX_MIXPEEK_OPS_URL", "AMUX_MIXPEEK_OPS_TOKEN"]},
            "gcp_cloud_run": {"label": "GCP Cloud Run", "color": "#4285F4", "env_vars": []},
            "mongodb_atlas": {"label": "MongoDB Atlas", "color": "#47A248", "env_vars": []},
            "gke":           {"label": "GKE Autopilot", "color": "#FF6B35", "env_vars": []},
            "qdrant_cloud":  {"label": "Qdrant Cloud",  "color": "#DC244C", "env_vars": []},
        },
    },
    "posthog-analytics": {
        "label": "PostHog Analytics",
        "description": "Product analytics — PostHog active/new users, total events, plus auth signups and new orgs from MongoDB (daily/weekly/monthly). Config: days (default 90), ops_url, ops_token.",
        "report_fetch": _report_fetch_posthog_all,
        "display": "count",
        "vendors": {
            "active_users":  {"label": "Active Users",  "color": "#F64E0F", "env_vars": []},
            "new_users":     {"label": "New Users",     "color": "#1D4ED8", "env_vars": []},
            "total_events":  {"label": "Total Events",  "color": "#059669", "env_vars": []},
            "auth_signups":  {"label": "Auth Signups",  "color": "#7C3AED", "env_vars": []},
            "auth_new_orgs": {"label": "New Orgs",      "color": "#D97706", "env_vars": []},
        },
    },
    # Add more report types here, e.g.:
    # "custom-http": {
    #     "label": "Custom HTTP",
    #     "description": "Fetch JSON from any URL and display as a chart",
    #     "vendors": { ... },
    # },
}


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
                  i.due, i.due_time, i.created, i.updated, i.owner_type,
                  COALESCE(i.pinned, 0) AS pinned,
                  GROUP_CONCAT(t.tag) AS tags_csv
           FROM issues i
           LEFT JOIN issue_tags t ON t.issue_id = i.id
           WHERE i.deleted IS NULL
           GROUP BY i.id
           ORDER BY COALESCE(i.pinned, 0) DESC, i.updated DESC"""
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
                  i.due, i.due_time, i.created, i.updated, i.owner_type,
                  COALESCE(i.pinned, 0) AS pinned,
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


def _generate_ical() -> str:
    """Generate iCal text from board items that have due dates."""
    items = [i for i in _load_board() if i.get("due")]
    lines = [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//amux//amux calendar//EN",
        "CALSCALE:GREGORIAN",
        "METHOD:PUBLISH",
        "X-WR-CALNAME:amux Board",
        "X-WR-CALDESC:amux board items with due dates",
        "REFRESH-INTERVAL;VALUE=DURATION:PT15M",
        "X-PUBLISHED-TTL:PT15M",
    ]
    status_map = {"todo": "NEEDS-ACTION", "doing": "IN-PROCESS", "done": "COMPLETED"}
    for item in items:
        due = item["due"]
        due_time = (item.get("due_time") or "").strip()
        date_val = due.replace("-", "")
        uid = item["id"] + "@amux"
        summary = item.get("title", "").replace(",", "\\,").replace(";", "\\;").replace("\n", "\\n")
        idesc = item.get("desc", "").replace(",", "\\,").replace(";", "\\;").replace("\n", "\\n")
        vstatus = status_map.get(item.get("status", "todo"), "NEEDS-ACTION")
        if due_time and re.match(r"^\d{2}:\d{2}$", due_time):
            hh, mm = due_time.split(":")
            dt_start = f"{date_val}T{hh}{mm}00"
            ev_lines = [
                "BEGIN:VEVENT",
                f"UID:{uid}",
                f"DTSTART:{dt_start}",
                "DURATION:PT1H",
                f"SUMMARY:{summary}",
                f"DESCRIPTION:{idesc}",
                f"STATUS:{vstatus}",
                "END:VEVENT",
            ]
        else:
            ev_lines = [
                "BEGIN:VEVENT",
                f"UID:{uid}",
                f"DTSTART;VALUE=DATE:{date_val}",
                f"DTEND;VALUE=DATE:{date_val}",
                f"SUMMARY:{summary}",
                f"DESCRIPTION:{idesc}",
                f"STATUS:{vstatus}",
                "END:VEVENT",
            ]
        lines += ev_lines
    lines.append("END:VCALENDAR")
    return "\r\n".join(lines) + "\r\n"


def _upload_ical_to_s3():
    """Upload the iCal feed to a public S3 bucket (if configured)."""
    if not _S3_BUCKET:
        return
    try:
        import boto3, hashlib, base64
        ical = _generate_ical()
        body = ical.encode("utf-8")
        # ContentMD5 lets S3 verify the upload and sets the ETag to the content MD5,
        # enabling efficient conditional GETs (If-None-Match) by Google Calendar.
        content_md5 = base64.b64encode(hashlib.md5(body).digest()).decode()
        s3 = boto3.client("s3", region_name=_S3_REGION)
        s3.put_object(
            Bucket=_S3_BUCKET,
            Key=_S3_KEY,
            Body=body,
            ContentType="text/calendar; charset=utf-8",
            ContentMD5=content_md5,
            # Allow 15-min caching; ETag + Last-Modified enable conditional revalidation
            CacheControl="public, max-age=900, must-revalidate",
        )
        slog(f"iCal uploaded to s3://{_S3_BUCKET}/{_S3_KEY}")
    except Exception as e:
        slog(f"S3 iCal upload failed: {e}")



def _next_run_dt(sched):
    """Compute next run datetime for a schedule. Returns ISO string or None."""
    from datetime import datetime, timedelta
    now = datetime.now()
    stype = sched.get("sched_type", "once")
    if stype == "once":
        return sched.get("run_at")
    rec = sched.get("recurrence", "daily")
    run_at = sched.get("run_at") or now.strftime("%H:%M")
    # parse time portion
    try:
        t = datetime.strptime(run_at[-5:], "%H:%M")
        hour, minute = t.hour, t.minute
    except Exception:
        hour, minute = 9, 0
    base = now.replace(hour=hour, minute=minute, second=0, microsecond=0)
    if rec == "hourly":
        base = now.replace(minute=minute, second=0, microsecond=0)
        if base <= now:
            base += timedelta(hours=1)
    elif rec == "daily":
        if base <= now:
            base += timedelta(days=1)
    elif rec == "weekly":
        # run_at encodes weekday as first char: "1:09:00" = Monday 09:00
        try:
            parts = run_at.split(":", 1)
            wd = int(parts[0])  # 0=Mon, 6=Sun
            time_str = parts[1] if len(parts) > 1 else "09:00"
            tt = datetime.strptime(time_str, "%H:%M")
            base = now.replace(hour=tt.hour, minute=tt.minute, second=0, microsecond=0)
            days_ahead = (wd - now.weekday()) % 7
            if days_ahead == 0 and base <= now:
                days_ahead = 7
            base += timedelta(days=days_ahead)
        except Exception:
            if base <= now:
                base += timedelta(weeks=1)
    elif rec == "monthly":
        # run_at encodes day: "15:09:00" = 15th at 09:00
        try:
            parts = run_at.split(":", 1)
            mday = int(parts[0])
            time_str = parts[1] if len(parts) > 1 else "09:00"
            tt = datetime.strptime(time_str, "%H:%M")
            base = now.replace(day=min(mday, 28), hour=tt.hour, minute=tt.minute, second=0, microsecond=0)
            if base <= now:
                # next month
                m = (now.month % 12) + 1
                y = now.year + (1 if now.month == 12 else 0)
                base = base.replace(year=y, month=m)
        except Exception:
            if base <= now:
                import calendar as _cal
                _, last = _cal.monthrange(now.year, now.month)
                base += timedelta(days=last)
    return base.strftime("%Y-%m-%dT%H:%M")


def _run_schedule(sched):
    """Execute a schedule entry — send command to tmux session."""
    import subprocess
    session = sched["session"]
    command = sched["command"]
    slog(f"[sched] running '{sched['title']}' on session '{session}'")
    try:
        subprocess.run(["tmux", "send-keys", "-t", session, command, "Enter"],
                       capture_output=True, timeout=5)
    except Exception as e:
        slog(f"[sched] error running '{sched['title']}': {e}")


def _scheduler_loop():
    """Unified background scheduler — 1s tick drives both:
    1. _JOB_REGISTRY: internal recurring Python functions (email sync, snapshots, etc.)
    2. DB schedules table: user-configured tmux commands (cron-style, once, etc.)
    """
    from datetime import datetime
    _DB_CHECK_INTERVAL = 30  # only hit SQLite every 30s for DB schedules
    _last_db_check = 0.0
    while True:
        time.sleep(1)
        now = time.time()

        # ── 1. Internal registered jobs ──────────────────────────────────────
        for job in _JOB_REGISTRY:
            if now >= job["next_run"]:
                job["next_run"] = now + job["interval"]
                threading.Thread(target=job["func"], daemon=True, name=job["name"]).start()

        # ── 2. DB-backed user schedules (tmux commands) ───────────────────────
        if now - _last_db_check < _DB_CHECK_INTERVAL:
            continue
        _last_db_check = now
        try:
            now_str = datetime.now().strftime("%Y-%m-%dT%H:%M")
            db = get_db()
            due = db.execute(
                "SELECT * FROM schedules WHERE deleted IS NULL AND enabled=1 AND next_run <= ?",
                (now_str,)
            ).fetchall()
            cols = [d[0] for d in db.execute("SELECT * FROM schedules LIMIT 0").description]
            for row in due:
                sched = dict(zip(cols, row))
                _run_schedule(sched)
                now_ts = int(time.time())
                if sched["sched_type"] == "once":
                    db.execute("UPDATE schedules SET enabled=0, last_run=?, updated=? WHERE id=?",
                               (now_str, now_ts, sched["id"]))
                else:
                    next_r = _next_run_dt(sched)
                    db.execute("UPDATE schedules SET last_run=?, next_run=?, updated=? WHERE id=?",
                               (now_str, next_r, now_ts, sched["id"]))
            if due:
                db.commit()
        except Exception as e:
            slog(f"[sched] scheduler loop error: {e}")

def _push_ical_bg():
    """Trigger S3 iCal upload in a background thread."""
    if _S3_BUCKET:
        threading.Thread(target=_upload_ical_to_s3, daemon=True).start()


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
    # Pre-compute which sessions are running and batch-capture their panes
    env_files = sorted(CC_SESSIONS.glob("*.env"))
    running_names = [f.stem for f in env_files if tmux_name(f.stem) in tmux_info]
    captures = _tmux_capture_batch(running_names, 30) if running_names else {}
    # Refresh token cache once (not per session)
    _refresh_token_cache()
    # Batch-load "doing" board tasks per session for task_name display
    try:
        _doing_tasks = {
            row["session"]: row["title"]
            for row in get_db().execute(
                "SELECT session, title FROM issues WHERE status='doing' AND deleted IS NULL AND session IS NOT NULL"
            ).fetchall()
        }
    except Exception:
        _doing_tasks = {}
    for f in env_files:
        name = f.stem
        cfg = parse_env_file(f)
        running = tmux_name(name) in tmux_info
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
            raw = captures.get(name, "")
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
        # Detect active model from JSONL
        raw_dir = cfg.get("CC_DIR", "")
        resolved_dir = str(Path(raw_dir).expanduser().resolve()) if raw_dir else ""
        active_model = detect_active_model(raw_dir)
        # Parse task time from spinner line
        task_time = _parse_task_time(raw) if raw else ""
        # Token count from JSONL cache (refreshed once above the loop)
        proj_key = resolved_dir.replace("/", "-") if resolved_dir else ""
        tokens = _token_cache["data"].get(proj_key, 0)
        sessions.append({
            "name": name,
            "dir": resolved_dir,
            "desc": cfg.get("CC_DESC", ""),
            "pinned": cfg.get("CC_PINNED", "") == "1",
            "archived": cfg.get("CC_ARCHIVED", "") == "1",
            "auto_continue": cfg.get("CC_AUTO_CONTINUE") in ("1", "true", "yes"),
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
            "task_name": _doing_tasks.get(name) or pane_title,
            "tokens": tokens,
        })
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

GLOBAL_MEMORY_DEFAULT = """\
# Shared Context

<!-- Add shared context for all sessions here -->

## amux inter-session API

You are session **$AMUX_SESSION** (env var). API base: **$AMUX_URL** (self-signed, use `curl -sk`).

### Discover sessions
```bash
curl -sk $AMUX_URL/api/sessions | python3 -c "import json,sys; [print(s['name'], s.get('status',''), '-', s.get('desc','')) for s in json.load(sys.stdin)]"
```

### Peek at another session's output
```bash
curl -sk "$AMUX_URL/api/sessions/OTHER/peek?lines=100" | python3 -c "import json,sys; print(json.load(sys.stdin).get('output',''))"
```

### Send a message to another session
```bash
curl -sk -X POST -H 'Content-Type: application/json' \\
  -d '{"text":"<your message>"}' \\
  $AMUX_URL/api/sessions/OTHER/send
```

### Task delegation via board (recommended for orchestration)
```bash
# Post task for a specific session
curl -sk -X POST -H 'Content-Type: application/json' \\
  -d '{"title":"Do X","session":"worker-1","owner_type":"agent","status":"todo"}' \\
  $AMUX_URL/api/board

# Check tasks assigned to this session
curl -sk $AMUX_URL/api/board | python3 -c "
import json,sys,os
s=os.getenv('AMUX_SESSION','')
[print(i['id'],i['title']) for i in json.load(sys.stdin) if i.get('session')==s and i['status'] in ('todo','doing')]
"

# Claim a task atomically (prevents two sessions taking same task)
curl -sk -X POST -H 'Content-Type: application/json' \\
  -d '{"session":"'"$AMUX_SESSION"'"}' \\
  $AMUX_URL/api/board/TASK-ID/claim

# Mark task done
curl -sk -X PATCH -H 'Content-Type: application/json' \\
  -d '{"status":"done","desc":"Result: ..."}' \\
  $AMUX_URL/api/board/TASK-ID
```
"""


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


def _git_info(work_dir: str) -> dict:
    """Return {branch, repo} for a directory. Returns empty strings if not a git repo."""
    if not work_dir:
        return {"branch": "", "repo": ""}
    try:
        rb = subprocess.run(
            ["git", "-C", work_dir, "branch", "--show-current"],
            capture_output=True, text=True, timeout=1,
        )
        branch = rb.stdout.strip() if rb.returncode == 0 else ""
        repo = ""
        if branch:
            rr = subprocess.run(
                ["git", "-C", work_dir, "rev-parse", "--show-toplevel"],
                capture_output=True, text=True, timeout=1,
            )
            repo = rr.stdout.strip() if rr.returncode == 0 else ""
        return {"branch": branch, "repo": repo}
    except Exception:
        return {"branch": "", "repo": ""}


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
             "-e", "TMUX_SESSION_NAME=" + name,
             "-e", "AMUX_SESSION=" + name,
             "-e", "AMUX_URL=https://localhost:8822",
             shell_rc + cmd],
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


def archive_session(name: str) -> tuple[bool, str]:
    """Save full tmux scrollback to log, mark CC_ARCHIVED=1, and kill the tmux pane."""
    f = CC_SESSIONS / f"{name}.env"
    if not f.exists():
        return False, f"session '{name}' not found"
    # Capture full scrollback while still running (50k line history)
    if is_running(name):
        try:
            r = subprocess.run(
                ["tmux", "capture-pane", "-t", tmux_name(name), "-p", "-S", "-"],
                capture_output=True, text=True, timeout=30,
            )
            if r.stdout.strip():
                lp = _log_path(name)
                data = r.stdout.encode("utf-8", errors="replace")
                if len(data) > MAX_LOG_BYTES:
                    data = data[-MAX_LOG_BYTES:]
                lp.write_bytes(data)
        except Exception:
            pass
        stop_session(name)
    cfg = parse_env_file(f)
    cfg["CC_ARCHIVED"] = "1"
    _write_env(f, cfg)
    return True, "archived"


def wake_session(name: str) -> tuple[bool, str]:
    """Remove CC_ARCHIVED flag and start the session (resumes conversation via --resume)."""
    f = CC_SESSIONS / f"{name}.env"
    if not f.exists():
        return False, f"session '{name}' not found"
    cfg = parse_env_file(f)
    cfg.pop("CC_ARCHIVED", None)
    _write_env(f, cfg)
    return start_session(name)


def _get_send_lock(name: str) -> threading.Lock:
    """Get or create a per-session lock for serializing send operations."""
    with _send_locks_lock:
        if name not in _send_locks:
            _send_locks[name] = threading.Lock()
        return _send_locks[name]


def send_text(name: str, text: str) -> tuple[bool, str]:
    if not is_running(name):
        return False, "not running"
    lock = _get_send_lock(name)
    with lock:
        try:
            t = tmux_name(name)
            # tmux send-keys -l has a ~500 char buffer limit; use load-buffer+paste-buffer for longer text
            if len(text) > 400:
                import tempfile, os as _os
                with tempfile.NamedTemporaryFile(mode='w', delete=False, suffix='.txt', encoding='utf-8') as f:
                    f.write(text)
                    tmp = f.name
                try:
                    subprocess.run(["tmux", "load-buffer", tmp], check=True, capture_output=True, timeout=5)
                    subprocess.run(["tmux", "paste-buffer", "-t", t], check=True, capture_output=True, timeout=5)
                finally:
                    _os.unlink(tmp)
            else:
                # Send text literally (-l) then Enter separately
                subprocess.run(
                    ["tmux", "send-keys", "-t", t, "-l", text],
                    check=True, capture_output=True, timeout=5,
                )
            # Give readline time to process all queued characters before Enter arrives
            time.sleep(0.1)
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
    lock = _get_send_lock(name)
    with lock:
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
# EMAIL → CALENDAR SYNC (macOS Mail.app + Calendar.app via AppleScript)
# No OAuth required — uses the accounts already signed in to Mail.app.
# ═══════════════════════════════════════════

_EMAIL_SYNC_INTERVAL = int(os.environ.get("AMUX_EMAIL_SYNC_INTERVAL", "900"))  # 15 min
_EMAIL_LOOKBACK_DAYS = int(os.environ.get("AMUX_EMAIL_LOOKBACK_DAYS",  "60"))
_EMAIL_CALENDAR_NAME = os.environ.get("AMUX_CALENDAR_NAME", "")  # empty = first writable calendar
_email_sync_lock     = threading.Lock()

# ═══════════════════════════════════════════
# UNIFIED JOB SCHEDULER
# All internal recurring jobs register here. _scheduler_loop() drives them
# alongside the DB-backed user schedules (tmux commands).
# ═══════════════════════════════════════════

_JOB_REGISTRY: list[dict] = []


def schedule_job(func, interval: float, *, name: str | None = None, initial_delay: float | None = None):
    """Register a recurring background job.
    func            — callable, run in a daemon thread each tick
    interval        — seconds between runs
    initial_delay   — seconds before first run (default: interval)
    """
    _JOB_REGISTRY.append({
        "name":     name or func.__name__,
        "func":     func,
        "interval": interval,
        "next_run": time.time() + (initial_delay if initial_delay is not None else interval),
    })


def _email_last_synced() -> int:
    """Return unix timestamp of last successful email sync (0 = never)."""
    row = get_db().execute(
        "SELECT value FROM prefs WHERE key='email_last_synced'"
    ).fetchone()
    return int(row["value"]) if row else 0


def _email_set_synced(ts: int) -> None:
    get_db().execute(
        "INSERT OR REPLACE INTO prefs (key, value) VALUES ('email_last_synced', ?)",
        (str(ts),),
    )
    get_db().commit()


def _mail_fetch_messages(lookback_seconds: int) -> list:
    """
    Read event-candidate emails from Mail.app inboxes via AppleScript.
    lookback_seconds: how far back to scan (small = fast; first run uses 7 days).
    Messages are newest-first, so we exit the loop as soon as we hit the cutoff.
    Only fetches message body for keyword-matching emails.
    """
    # Convert seconds to fractional days for AppleScript
    lookback_days_frac = max(lookback_seconds / 86400, 0.1)
    # Cap max messages per inbox to keep things fast
    max_msgs = 200 if lookback_days_frac > 3 else 80

    script = f"""
set NL to ASCII character 10
set output to ""
set cutoff to (current date) - ({lookback_days_frac} * days)
tell application "Mail"
    repeat with acct in accounts
        set acctName to name of acct
        try
            repeat with mb in mailboxes of acct
                if name of mb is "INBOX" then
                    -- Use indexed access to avoid loading all 21k+ message refs
                    try
                        set recentMsgs to messages 1 through {max_msgs} of mb
                    on error
                        set recentMsgs to messages of mb
                    end try
                    repeat with msg in recentMsgs
                        try
                            if date received of msg < cutoff then exit repeat
                        end try
                        try
                            set subj to subject of msg
                            set sndr to sender of msg
                            if (subj contains "ticket" or subj contains "confirmation" or subj contains "reservation" or subj contains "booking" or subj contains "receipt" or subj contains "invitation" or subj contains "event reminder" or subj contains "your tickets" or subj contains "order confirm" or subj contains "seating" or sndr contains "ticketmaster" or sndr contains "eventbrite" or sndr contains "axs.com" or sndr contains "dice.fm" or sndr contains "livenation" or sndr contains "stubhub" or sndr contains "songkick" or sndr contains "bandsintown" or sndr contains "etix" or sndr contains "tixr" or sndr contains "seetickets") then
                                set rcvd to date received of msg as string
                                set msgId to message id of msg
                                set msgBody to content of msg
                                if length of msgBody > 5000 then set msgBody to text 1 thru 5000 of msgBody
                                set output to output & "MSG_START" & NL & acctName & NL & sndr & NL & rcvd & NL & subj & NL & msgId & NL & msgBody & "MSG_END" & NL
                            end if
                        end try
                    end repeat
                end if
            end repeat
        end try
    end repeat
end tell
return output
"""
    try:
        result = subprocess.run(
            ["osascript", "-e", script],
            capture_output=True, text=True, timeout=120,
        )
        if result.returncode != 0:
            slog(f"[email] AppleScript error: {result.stderr[:300]}")
            return []
        messages = []
        for block in result.stdout.split("MSG_START\n"):
            if "MSG_END" not in block:
                continue
            content = block[: block.rfind("MSG_END")].strip()
            lines = content.split("\n", 5)
            if len(lines) < 5:
                continue
            messages.append({
                "account": lines[0],
                "sender":  lines[1],
                "date":    lines[2],
                "subject": lines[3],
                "msg_id":  lines[4],
                "body":    lines[5] if len(lines) > 5 else "",
            })
        slog(f"[email] Mail.app: {len(messages)} candidates (lookback {lookback_days_frac:.1f}d)")
        return messages
    except subprocess.TimeoutExpired:
        slog("[email] Mail.app AppleScript timed out (>120s)")
        return None  # None = timeout/failure; [] = success with no matches
    except Exception as e:
        slog(f"[email] mail fetch error: {e}")
        return None


def _ai_extract_event(subject: str, sender: str, date_str: str, body: str) -> dict | None:
    """Use Claude Haiku to extract event details from an email. Returns dict or None."""
    try:
        import anthropic
        client = anthropic.Anthropic()
        prompt = f"""Analyze this email and determine if it is a confirmation or reminder about a real-world event the recipient will attend (concert, show, sports game, conference, workshop, class, appointment, etc).

Email subject: {subject}
From: {sender}
Date received: {date_str}
Body:
{body[:3000]}

Respond with JSON only (no markdown fences):
If this IS an event the recipient will attend:
{{"is_event": true, "title": "Event name", "start": "YYYY-MM-DDTHH:MM:SS or YYYY-MM-DD", "end": "YYYY-MM-DDTHH:MM:SS or null", "location": "Venue name and/or address or null", "description": "Short summary with key info (ticket #, seat, etc)", "confidence": 0.0-1.0}}

If NOT an event email: {{"is_event": false}}"""
        msg = client.messages.create(
            model="claude-haiku-4-5-20251001",
            max_tokens=400,
            messages=[{"role": "user", "content": prompt}],
        )
        text = msg.content[0].text.strip()
        if text.startswith("```"):
            text = "\n".join(text.split("\n")[1:]).rstrip("`").strip()
        # Strip trailing content after JSON object in case model adds explanation
        brace = text.rfind("}")
        if brace != -1:
            text = text[:brace + 1]
        result = json.loads(text)
        if result.get("is_event") and float(result.get("confidence", 0)) >= 0.65:
            return result
        return None
    except Exception as e:
        slog(f"[email] AI extract error: {e}")
        return None


def _cal_create_event(event: dict) -> bool:
    """Create a Calendar.app event via AppleScript. Returns True on success."""
    from datetime import datetime, timedelta
    title       = (event.get("title") or "Event").replace("\\", "").replace('"', "'")
    location    = (event.get("location") or "").replace("\\", "").replace('"', "'")
    description = (event.get("description") or "").replace("\\", "").replace('"', "'")[:500]
    start_str   = event.get("start")
    if not start_str:
        return False
    try:
        if "T" in str(start_str):
            start_dt = datetime.fromisoformat(start_str)
        else:
            start_dt = datetime.strptime(str(start_str)[:10], "%Y-%m-%d").replace(hour=0, minute=0)
    except Exception:
        return False
    end_str = event.get("end")
    try:
        if end_str and "T" in str(end_str):
            end_dt = datetime.fromisoformat(end_str)
        elif end_str:
            end_dt = datetime.strptime(str(end_str)[:10], "%Y-%m-%d").replace(hour=23, minute=59)
        else:
            end_dt = start_dt + timedelta(hours=2) if "T" in str(start_str) else start_dt.replace(hour=23, minute=59)
    except Exception:
        end_dt = start_dt + timedelta(hours=2)
    all_day = "T" not in str(start_str)
    if _EMAIL_CALENDAR_NAME:
        cal_selector = f'first calendar whose name is "{_EMAIL_CALENDAR_NAME}"'
    else:
        cal_selector = "first calendar whose writable is true"
    allday_prop = ", allday event:true" if all_day else ""
    script = f"""
tell application "Calendar"
    set targetCal to {cal_selector}
    set startD to current date
    set year of startD to {start_dt.year}
    set month of startD to {start_dt.month}
    set day of startD to {start_dt.day}
    set hours of startD to {start_dt.hour}
    set minutes of startD to {start_dt.minute}
    set seconds of startD to 0
    set endD to current date
    set year of endD to {end_dt.year}
    set month of endD to {end_dt.month}
    set day of endD to {end_dt.day}
    set hours of endD to {end_dt.hour}
    set minutes of endD to {end_dt.minute}
    set seconds of endD to 0
    tell targetCal
        make new event with properties {{summary:"{title}", start date:startD, end date:endD, location:"{location}", description:"{description}"{allday_prop}}}
    end tell
end tell
return "ok"
"""
    try:
        result = subprocess.run(
            ["osascript", "-e", script],
            capture_output=True, text=True, timeout=30,
        )
        if result.returncode == 0 and "ok" in result.stdout:
            return True
        slog(f"[email] Calendar.app error: {result.stderr[:300]}")
        return False
    except Exception as e:
        slog(f"[email] cal create error: {e}")
        return False


def _email_sync() -> None:
    """Read Mail.app → AI extract → Calendar.app. No accounts to manage."""
    db = get_db()
    last_ts = _email_last_synced()
    now_ts  = int(time.time())
    if last_ts == 0:
        lookback_seconds = 7 * 86400  # first run: 7 days
    else:
        elapsed = now_ts - last_ts
        lookback_seconds = elapsed + 300  # add 5 min buffer to avoid gaps
    slog(f"[email] syncing lookback={lookback_seconds//60}min")
    messages = _mail_fetch_messages(lookback_seconds)
    if messages is None:
        return  # timed out — don't update last_synced so next run retries with same window
    for msg in messages:
        # Use message-id as dedup key; fall back to subject+date hash
        msg_id = msg["msg_id"] or f"{msg['subject']}|{msg['date']}"
        existing = db.execute(
            "SELECT id, status FROM email_events WHERE gmail_message_id=?", (msg_id,)
        ).fetchone()
        if existing and existing["status"] in ("synced", "dismissed", "not_event"):
            continue
        if existing:
            continue  # pending — don't re-extract
        event_data = _ai_extract_event(msg["subject"], msg["sender"], msg["date"], msg["body"])
        now   = int(time.time())
        ev_id = str(uuid.uuid4())
        if event_data:
            db.execute(
                "INSERT OR IGNORE INTO email_events "
                "(id, account_id, gmail_message_id, email_subject, email_from, email_date, "
                "event_title, event_start, event_end, event_location, event_description, "
                "status, raw_extract, created) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (ev_id, msg["account"], msg_id, msg["subject"], msg["sender"], msg["date"],
                 event_data.get("title"), event_data.get("start"), event_data.get("end"),
                 event_data.get("location"), event_data.get("description"),
                 "pending", json.dumps(event_data), now),
            )
            db.commit()
            slog(f"[email] detected: {event_data.get('title')}")
            if _cal_create_event(event_data):
                db.execute(
                    "UPDATE email_events SET status='synced', calendar_event_id='applescript' WHERE id=?",
                    (ev_id,),
                )
                db.commit()
                slog(f"[email] \u2192 Calendar.app: {event_data.get('title')}")
        else:
            db.execute(
                "INSERT OR IGNORE INTO email_events "
                "(id, account_id, gmail_message_id, email_subject, email_from, email_date, "
                "status, created) VALUES (?,?,?,?,?,?,?,?)",
                (ev_id, msg["account"], msg_id, msg["subject"], msg["sender"], msg["date"],
                 "not_event", now),
            )
            db.commit()
    _email_set_synced(now_ts)


def _email_sync_job() -> None:
    """Scheduled job: sync Mail.app → Calendar.app (called by _scheduler_loop)."""
    try:
        with _email_sync_lock:
            _email_sync()
    except Exception as e:
        slog(f"[email] sync error: {e}")


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
  body.light {
    --bg: #ffffff; --card: #f6f8fa; --border: #d0d7de;
    --text: #1f2328; --dim: #656d76; --accent: #0969da;
    --green: #1a7f37; --red: #cf222e; --yellow: #9a6700;
    --cyan: #0550ae;
  }
  body.light .board-sortable-ghost { background: rgba(9,105,218,0.08) !important; }
  body.light .log-line { filter: none; }
  /* ── Light mode contrast fixes ── */
  /* Terminal output always dark regardless of theme — ANSI colors need dark bg */
  body.light .overlay-body {
    background: #1c2128 !important; color: #cdd9e5 !important;
  }
  body.light .peek-copy-btn {
    background: rgba(28,33,40,0.85); border-color: rgba(255,255,255,0.2); color: #cdd9e5;
  }
  /* Connection status — hardcoded neon colors are invisible on white */
  body.light .conn-status.online  { color: #1a7f37; background: rgba(26,127,55,0.1); }
  body.light .conn-status.online::before  { background: #1a7f37; }
  body.light .conn-status.polling { color: #9a6700; background: rgba(154,103,0,0.1); }
  body.light .conn-status.polling::before { background: #9a6700; }
  body.light .conn-status.offline { color: #cf222e; background: rgba(207,34,46,0.1); }
  body.light .conn-status.offline::before { background: #cf222e; }
  /* Board empty placeholder — 50% transparent is invisible on white */
  body.light .board-empty { color: var(--dim) !important; }
  /* Board tag chips — very transparent bg needs a real border */
  body.light .board-card-tag { border: 1px solid var(--border); }
  /* Session card terminal preview — semi-transparent dark bg becomes gray in light mode */
  body.light .card-preview-lines {
    background: #1c2128; color: #cdd9e5;
  }
  /* Peek search highlight — white text on yellow bg fine in dark; fix for light terminal */
  body.light .peek-highlight { color: #fff; }
  /* Ac section headers */
  body.light .ac-section { background: var(--bg); }
  /* Workspace overlay + pane header/send area — hardcoded dark bg needs light override */
  body.light #grid-view { background: var(--bg); }
  body.light .gp-header { background: var(--card); }
  body.light .gp-send { background: var(--card); }
  body.light .gp-close, body.light .gp-peek-btn { color: var(--dim); }
  /* File/explore overlay — hardcoded near-black backdrop + body need light overrides */
  body.light .file-overlay { background: rgba(240,242,245,0.97); }
  body.light .file-overlay-header h2 { color: var(--text); }
  /* Directory listing body — light bg with readable text */
  body.light #explore-overlay .file-overlay-body { background: var(--bg); color: var(--text); }
  body.light .explore-name { color: var(--text); }
  body.light .explore-row:hover { background: var(--card); }
  /* File viewer body — keep dark for ANSI/code readability; use card bg for non-raw views */
  body.light #file-overlay .file-overlay-body { background: #1c2128; color: #cdd9e5; }
  body.light #file-overlay .file-overlay-body.file-image,
  body.light #file-overlay .file-overlay-body.file-video,
  body.light #file-overlay .file-overlay-body.file-pdf,
  body.light #file-overlay .file-overlay-body.markdown { background: var(--bg); color: var(--text); }
  body.light #file-overlay .file-overlay-body.file-video { background: #000; }
  /* ── Reports ── */
  .report-card { background: var(--card); border: 1px solid var(--border); border-radius: 8px; overflow: hidden; }
  .report-card-header { display:flex; align-items:center; gap:8px; padding:10px 14px; border-bottom:1px solid var(--border); background:var(--bg); }
  .report-card-title { font-weight:600; font-size:0.85rem; flex:1; }
  .report-refresh-btn { background:none; border:none; cursor:pointer; color:var(--dim); font-size:0.85rem; padding:3px 7px; border-radius:4px; }
  .report-refresh-btn:hover { background:var(--card); color:var(--text); }
  .report-refresh-btn.spinning { animation: spin 1s linear infinite; }
  .report-del-btn { background:none; border:none; cursor:pointer; color:var(--dim); font-size:0.8rem; padding:3px 7px; border-radius:4px; }
  .report-del-btn:hover { background:rgba(248,81,73,0.1); color:#f85149; }
  .report-period-tabs { display:flex; gap:4px; }
  .report-period-tab { padding:3px 9px; border-radius:5px; border:1px solid var(--border); background:none; color:var(--dim); font-size:0.75rem; cursor:pointer; }
  .report-period-tab.active { background:var(--accent); color:#fff; border-color:var(--accent); }
  .report-body { padding:14px; }
  .report-chart-wrap { position:relative; height:200px; margin-bottom:12px; }
  .report-table { width:100%; border-collapse:collapse; font-size:0.78rem; }
  .report-table th { text-align:left; color:var(--dim); font-weight:500; padding:4px 8px; border-bottom:1px solid var(--border); }
  .report-table td { padding:5px 8px; border-bottom:1px solid var(--border); }
  .report-table tr:last-child td { border-bottom:none; }
  .report-vendor-dot { display:inline-block; width:8px; height:8px; border-radius:50%; margin-right:6px; }
  .report-error { color:var(--dim); font-size:0.78rem; padding:4px 0; }
  .report-last-refresh { font-size:0.72rem; color:var(--dim); }
  .report-total { font-weight:600; }
  @keyframes spin { to { transform: rotate(360deg); } }
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
  body.session-dragging, body.session-dragging * { user-select: none !important; -webkit-user-select: none !important; cursor: grabbing !important; }
  body.session-dragging .card { transition: none !important; }
  .sortable-ghost { opacity: 0.35; background: rgba(88,166,255,0.07) !important; border: 2px dashed var(--accent) !important; border-radius: 10px; }
  .sortable-chosen { opacity: 0.7; box-shadow: none; }
  .sortable-drag { opacity: 0.96; box-shadow: 0 14px 36px rgba(0,0,0,0.45); transform: rotate(1deg) scale(1.02); border-radius: 10px; }
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
    cursor: default; color: var(--dim); opacity: 0; transition: opacity 0.15s; border-radius: 3px;
    user-select: none; -webkit-user-select: none; touch-action: none;
  }
  .card:hover .card-drag-handle { opacity: 0.45; cursor: grab; }
  .card-drag-handle:hover { opacity: 1 !important; cursor: grab; color: var(--fg); background: rgba(139,148,158,0.1); }
  .card-drag-handle:active { cursor: grabbing; }
  @media (hover: none) { .card-drag-handle { opacity: 0.35; cursor: grab; } }
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

  /* Archived sessions */
  #archived-section { padding: 0 12px 12px; }
  .archived-footer {
    display: flex; align-items: center; gap: 8px;
    padding: 8px 10px; border-radius: 8px; cursor: pointer;
    border: 1px dashed var(--border); color: var(--dim);
    font-size: 0.8rem; user-select: none; margin-top: 4px;
  }
  .archived-footer:hover { border-color: var(--accent); color: var(--text); }
  .archived-chevron { font-size: 0.65rem; transition: transform 0.15s; }
  .archived-chevron.open { transform: rotate(90deg); }
  .archived-body { margin-top: 6px; display: flex; flex-direction: column; gap: 4px; }
  .archived-card {
    display: flex; align-items: center; gap: 8px;
    padding: 7px 10px; border-radius: 8px; border: 1px solid var(--border);
    background: var(--card); font-size: 0.8rem;
  }
  .archived-card-name { font-weight: 600; color: var(--dim); flex-shrink: 0; min-width: 80px; }
  .archived-card-meta { color: var(--dim); font-size: 0.72rem; flex-shrink: 0; }
  .archived-card-preview { flex: 1; color: var(--dim); font-size: 0.73rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
  .archived-card-actions { display: flex; gap: 4px; flex-shrink: 0; }
  .archived-wake-btn {
    padding: 3px 10px; border-radius: 5px; font-size: 0.75rem; border: 1px solid var(--accent);
    color: var(--accent); background: transparent; cursor: pointer; white-space: nowrap;
  }
  .archived-wake-btn:hover { background: var(--accent); color: #000; }
  .archived-del-btn {
    padding: 3px 7px; border-radius: 5px; font-size: 0.75rem; border: 1px solid var(--border);
    color: var(--dim); background: transparent; cursor: pointer;
  }
  .archived-del-btn:hover { border-color: var(--red); color: var(--red); }

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
  .branch-badge {
    display: inline-flex; align-items: center; gap: 3px; font-size: 0.75rem;
    color: var(--dim); cursor: pointer; padding: 1px 6px; border-radius: 3px;
    border: 1px solid transparent; transition: border-color 0.15s, color 0.15s;
    flex-shrink: 0; white-space: nowrap; user-select: none;
  }
  .branch-badge:hover { border-color: var(--border); color: var(--fg); }
  .branch-badge.on-main { color: var(--yellow); }
  .branch-badge.on-branch { color: var(--green); }
  .branch-badge.conflict { color: var(--red) !important; }
  .branch-popover {
    position: fixed; z-index: 9999; width: 240px;
    background: var(--surface); border: 1px solid var(--border); border-radius: 8px;
    padding: 12px; box-shadow: 0 8px 24px rgba(0,0,0,0.5);
  }
  .branch-popover input { width: 100%; box-sizing: border-box; margin-bottom: 8px; }
  .branch-popover-actions { display: flex; gap: 6px; }
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
  .badge.auto-continue { background: rgba(98,160,234,0.2); color: #62a0ea; }
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
  .send-row { display: flex; gap: 8px; min-width: 0; overflow: visible; position: relative; }
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
    position: fixed; top: 0; left: 0; right: 0; bottom: 0;
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
  .overlay-body a { color: var(--accent); text-decoration: underline; text-underline-offset: 2px; cursor: pointer; }
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
  .file-overlay-body.file-video { display:flex;flex-direction:column;align-items:center;justify-content:center;background:#000;padding:0;white-space:normal; }
  .file-video-player { max-width:100%;max-height:calc(100% - 36px);outline:none;display:block; }
  .file-video-meta { width:100%;box-sizing:border-box;padding:6px 14px;display:flex;align-items:center;gap:12px;font-family:-apple-system,BlinkMacSystemFont,sans-serif;font-size:0.72rem;color:var(--dim);flex-shrink:0;background:rgba(0,0,0,0.7); }
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
  .file-overlay-body.markdown .table-scroll { overflow-x: auto; -webkit-overflow-scrolling: touch; margin: 12px 0; border-radius: 6px; border: 1px solid var(--border); }
  .file-overlay-body.markdown table { border-collapse: collapse; min-width: 100%; margin: 0; font-size: 0.84rem; }
  .file-overlay-body.markdown thead { background: var(--card); }
  .file-overlay-body.markdown th { font-weight: 600; text-align: left; padding: 8px 12px; border: 1px solid var(--border); white-space: nowrap; }
  .file-overlay-body.markdown td { padding: 6px 12px; border: 1px solid var(--border); }
  .file-overlay-body.markdown tbody tr:nth-child(even) { background: rgba(88,166,255,0.04); }
  .file-overlay-body.markdown tbody tr:hover { background: rgba(88,166,255,0.08); }
  @media (max-width: 600px) {
    .file-overlay-body.markdown th, .file-overlay-body.markdown td { padding: 5px 8px; font-size: 0.78rem; }
    .board-detail-preview th, .peek-memory-editor th, .board-detail-preview td, .peek-memory-editor td { padding: 4px 8px; font-size: 0.78rem; }
  }
  .file-overlay-body.markdown img { max-width: 100%; border-radius: 6px; margin: 8px 0; }
  .file-overlay-body.markdown input[type="checkbox"] { margin-right: 6px; pointer-events: none; }
  .file-overlay-body.markdown del { color: var(--dim); }
  /* Board/memory markdown preview tables */
  .board-detail-preview .table-scroll, .peek-memory-editor .table-scroll { overflow-x: auto; -webkit-overflow-scrolling: touch; margin: 10px 0; border-radius: 6px; border: 1px solid var(--border); }
  .board-detail-preview table, .peek-memory-editor table { border-collapse: collapse; min-width: 100%; margin: 0; font-size: 0.84rem; }
  .board-detail-preview th, .peek-memory-editor th { font-weight: 600; text-align: left; padding: 6px 10px; border: 1px solid var(--border); background: var(--card); }
  .board-detail-preview td, .peek-memory-editor td { padding: 5px 10px; border: 1px solid var(--border); }
  .board-detail-preview tbody tr:nth-child(even), .peek-memory-editor tbody tr:nth-child(even) { background: rgba(88,166,255,0.04); }

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
  .explore-mtime { font-size: 0.68rem; color: var(--dim); flex-shrink: 0; }
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
  .cal-chip.sched-chip { background: rgba(163,113,247,0.18); color: #c084fc; border-left: 2px solid #c084fc; }
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
  .ac-section { padding: 4px 12px; font-size: 0.68rem; font-family: -apple-system, sans-serif;
    color: var(--dim); font-weight: 700; text-transform: uppercase; letter-spacing: 0.06em;
    border-bottom: 1px solid var(--border); background: var(--bg); pointer-events: none; }
  .at-item .at-at { color: var(--accent); }

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
  .card-log-hit {
    font-size: 0.72rem; padding: 2px 0; cursor: pointer; line-height: 1.4;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .card-log-hit:hover { opacity: 0.8; }
  .log-hit-loc { color: var(--accent); font-weight: 600; font-family: monospace; flex-shrink: 0; margin-right: 6px; }
  .log-hit-text { color: var(--dim); }
  .log-search-btn { width: auto; padding: 0 8px; gap: 4px; font-size: 0.75rem; white-space: nowrap; }
  .log-search-btn .log-search-icon { flex-shrink: 0; }
  .log-search-btn .log-search-label { display: inline; }
  @media (max-width: 480px) {
    .log-search-btn { width: 28px; padding: 0; }
    .log-search-btn .log-search-label { display: none; }
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
    box-shadow: 0 8px 24px rgba(0,0,0,0.4); overflow-x: hidden; overflow-y: auto;
    max-height: calc(100dvh - 80px);
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
  /* Theme toggle switch */
  .theme-toggle { position: relative; display: inline-flex; align-items: center; cursor: pointer; }
  .theme-toggle input { position: absolute; opacity: 0; width: 0; height: 0; }
  .theme-track {
    width: 36px; height: 20px; background: var(--border); border-radius: 10px;
    transition: background 0.2s; display: flex; align-items: center; padding: 2px;
  }
  .theme-toggle input:checked + .theme-track { background: var(--accent); }
  .theme-thumb {
    width: 16px; height: 16px; background: #fff; border-radius: 50%;
    transition: transform 0.2s; box-shadow: 0 1px 3px rgba(0,0,0,0.3);
  }
  .theme-toggle input:checked + .theme-track .theme-thumb { transform: translateX(16px); }
  .settings-server-item {
    display: flex; align-items: center; justify-content: space-between;
    padding: 6px 8px; border-radius: 6px; cursor: pointer; margin-bottom: 2px;
    transition: background 0.12s; -webkit-tap-highlight-color: transparent;
    touch-action: manipulation;
  }
  .settings-server-item:active { background: rgba(88,166,255,0.1); }
  .settings-server-current { background: rgba(88,166,255,0.08); cursor: default; }
  .settings-server-name { font-size: 0.78rem; font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .settings-server-url { font-size: 0.65rem; color: var(--dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .settings-server-badge { font-size: 0.6rem; color: var(--accent); flex-shrink: 0; margin-left: 6px; }

  /* Logs tab */
  .logs-toolbar { display:flex;justify-content:space-between;align-items:center;gap:8px;padding:6px 12px;border-bottom:1px solid var(--border);flex-shrink:0;flex-wrap:wrap; }
  .lf-btn { font-size:0.68rem;padding:2px 8px;border-radius:10px;border:1px solid var(--border);background:none;color:var(--dim);cursor:pointer;transition:all 0.15s; }
  .lf-btn:hover { color:var(--fg);border-color:var(--fg); }
  .lf-btn.active { background:var(--accent);color:#000;border-color:var(--accent); }
  .logs-subtabs { display:flex;gap:0;border-bottom:1px solid var(--border);flex-shrink:0; }
  .lst-btn { flex:1;text-align:center;padding:6px 0;font-size:0.72rem;border:none;background:none;color:var(--dim);cursor:pointer;border-bottom:2px solid transparent;transition:all 0.15s; }
  .lst-btn:hover { color:var(--fg); }
  .lst-btn.active { color:var(--accent);border-bottom-color:var(--accent); }
  .log-evt { padding:8px 0;border-bottom:1px solid var(--border-subtle,rgba(255,255,255,0.04)); display:flex;gap:10px;align-items:flex-start; }
  .log-evt-icon { width:28px;height:28px;border-radius:6px;display:flex;align-items:center;justify-content:center;font-size:0.8rem;flex-shrink:0; }
  .log-evt-body { flex:1;min-width:0; }
  .log-evt-title { font-size:0.78rem;font-weight:500; }
  .log-evt-detail { font-size:0.68rem;color:var(--dim);margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap; }
  .log-evt-time { font-size:0.6rem;color:var(--dim);flex-shrink:0;white-space:nowrap; }
  .log-evt-cat { font-size:0.55rem;padding:1px 5px;border-radius:6px;border:1px solid var(--border);color:var(--dim);flex-shrink:0; }
  .stat-card { background:var(--card-bg);border:1px solid var(--border);border-radius:8px;padding:12px;margin-bottom:8px; }
  .stat-card-label { font-size:0.7rem;color:var(--dim);margin-bottom:4px; }
  .stat-card-value { font-size:1.4rem;font-weight:600; }
  .stat-row { display:flex;justify-content:space-between;align-items:center;padding:4px 0;font-size:0.72rem; }
  .stat-row-bar { height:4px;border-radius:2px;background:var(--accent);margin-top:2px; }

  /* Peek find bar */
  .peek-find-wrap { position: relative; display: flex; align-items: center; gap: 0; }
  .peek-find-wrap .search-input { padding-right: 120px; min-width: 180px; }
  .peek-find-count { position: absolute; right: 60px; font-size: 0.65rem; color: var(--dim); white-space: nowrap; pointer-events: none; }
  .peek-nav-btn { width: 22px; height: 22px; border: none; background: transparent; color: var(--dim); cursor: pointer; font-size: 0.75rem; display: flex; align-items: center; justify-content: center; position: absolute; }
  .peek-nav-btn:first-of-type { right: 36px; }
  .peek-nav-btn:last-of-type { right: 16px; }
  .peek-nav-btn:hover { color: var(--text); }
  .peek-find-wrap .search-clear { position: absolute; right: 2px; }
  .peek-find-wrap.has-value .search-clear { display: flex; }
  /* Peek search highlight */
  .peek-highlight { background: rgba(210,153,34,0.35); color: #fff; border-radius: 2px; }
  .peek-highlight.current { background: rgba(210,153,34,0.85); color: #000; }

  /* Peek compact mode — when visual viewport is constrained (pinch zoom / keyboard) */
  .overlay.vv-compact .peek-cmd-row .chips { display: none !important; }
  .overlay.vv-compact .peek-dir-bar { display: none; }
  .overlay.vv-compact .peek-tabs { padding: 0 8px; }
  .overlay.vv-compact .peek-tab { padding: 4px 10px; font-size: 0.75rem; }
  .overlay.vv-compact .overlay-header { padding-bottom: 4px !important; gap: 2px !important; }
  .overlay.vv-compact .peek-cmd-toggle { padding: 3px; }
  .overlay.vv-compact .peek-cmd-row .send-input { min-height: 30px; padding: 4px 8px; font-size: 0.8rem; }
  .overlay.vv-compact .peek-cmd-row .btn { min-height: 30px; padding: 4px 10px; }
  .overlay.vv-compact .peek-attach-btn { min-height: 30px; }

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
  .peek-cmd-row.open { display: flex; min-width: 0; overflow: visible; position: relative; }
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
  .peek-attach-btn.mic-active { color: var(--red); border-color: var(--red); animation: mic-pulse 1s ease-in-out infinite; }
  @keyframes mic-pulse { 0%,100% { opacity: 1; } 50% { opacity: 0.5; } }
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
    display: flex; gap: 0; margin: 0 -16px 12px -16px; padding: 0 0 0 16px;
    border-bottom: 1px solid var(--border);
    position: sticky; top: 60px; z-index: 39; background: var(--bg);
    overflow-x: auto; -webkit-overflow-scrolling: touch; scroll-behavior: smooth;
  }
  .tab-bar::-webkit-scrollbar { display: none; }
  .tab-bar button {
    flex: none; padding: 10px 14px; font-size: 0.85rem; font-weight: 600;
    background: none; border: none; border-bottom: 2px solid transparent;
    border-right: 1px solid var(--border);
    color: var(--dim); cursor: pointer; transition: color 0.15s, border-color 0.15s;
    -webkit-tap-highlight-color: transparent; white-space: nowrap;
  }
  .tab-bar button:last-child { border-right: none; }
  .tab-bar button.active { color: var(--accent); border-bottom-color: var(--accent); }
  .tab-bar button:active { opacity: 0.7; }

  /* Logs view */
  .logs-toolbar {
    display: flex; align-items: center; gap: 8px; padding: 7px 12px;
    border-bottom: 1px solid var(--border); flex-shrink: 0; flex-wrap: wrap;
    background: var(--bg); position: sticky; top: 0; z-index: 10;
  }
  .logs-subtabs { display: flex; gap: 2px; flex-shrink: 0; }
  .logs-subtab {
    background: transparent; border: none; padding: 4px 11px; font-size: 0.78rem;
    cursor: pointer; color: var(--dim); border-radius: 6px; font-weight: 500;
    transition: background 0.12s, color 0.12s;
  }
  .logs-subtab.active { background: var(--card); color: var(--text); font-weight: 600; }
  .logs-subtab:hover:not(.active) { color: var(--text); }
  .logs-filter-bar { display: flex; gap: 3px; flex: 1; flex-wrap: wrap; min-width: 0; }
  .lf-btn {
    background: transparent; border: 1px solid transparent; padding: 2px 8px;
    font-size: 0.71rem; cursor: pointer; border-radius: 4px; color: var(--dim);
    transition: background 0.1s, border-color 0.1s, color 0.1s;
  }
  .lf-btn.active { border-color: var(--border); background: var(--card); color: var(--text); font-weight: 600; }
  .lf-btn:hover:not(.active) { color: var(--text); }
  .logs-search { width: 120px; font-size: 0.78rem; padding: 3px 8px; }
  .logs-panel { display: flex; flex-direction: column; }
  .logs-activity-body { padding: 6px 10px; display: flex; flex-direction: column; gap: 1px; }
  .log-evt {
    display: flex; align-items: flex-start; gap: 10px; padding: 6px 8px;
    border-radius: 6px; font-size: 0.8rem; cursor: default;
    transition: background 0.1s;
  }
  .log-evt:hover { background: var(--card); }
  .log-evt-icon { font-size: 0.88rem; width: 22px; text-align: center; flex-shrink: 0; padding-top: 1px; }
  .log-evt-body { flex: 1; min-width: 0; }
  .log-evt-title { font-weight: 500; color: var(--text); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .log-evt-meta { font-size: 0.69rem; color: var(--dim); margin-top: 2px; display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
  .log-evt-badge {
    display: inline-block; padding: 1px 6px; border-radius: 3px; font-size: 0.64rem;
    font-weight: 700; text-transform: uppercase; letter-spacing: 0.04em;
  }
  .log-evt-ts { color: var(--dim); font-size: 0.69rem; min-width: 52px; text-align: right; flex-shrink: 0; }
  .logs-day-divider {
    padding: 8px 8px 3px; font-size: 0.68rem; font-weight: 700; color: var(--dim);
    letter-spacing: 0.06em; text-transform: uppercase;
  }
  .logs-raw-body {
    padding: 10px 14px; font-family: 'SF Mono', 'Fira Code', monospace; font-size: 0.71rem;
    line-height: 1.55; color: var(--text); white-space: pre-wrap; word-break: break-all; margin: 0;
  }
  .stats-grid {
    display: grid; grid-template-columns: repeat(auto-fill, minmax(130px, 1fr)); gap: 10px;
  }
  .stat-card {
    background: var(--card); border: 1px solid var(--border); border-radius: 8px; padding: 13px 14px;
  }
  .stat-card-label { font-size: 0.69rem; color: var(--dim); margin-bottom: 5px; }
  .stat-card-value { font-size: 1.5rem; font-weight: 700; color: var(--text); }

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
    cursor: pointer; transition: border-color 0.15s, box-shadow 0.15s;
    -webkit-tap-highlight-color: transparent;
    will-change: transform;
    position: relative;
  }
  /* Kill all transitions while dragging so Sortable's JS animation is the sole driver */
  body.board-dragging .board-card { transition: none !important; }
  .board-card:active { border-color: var(--accent); box-shadow: 0 0 0 1px rgba(88,166,255,0.2); }
  .board-card-pinned { border-color: rgba(88,166,255,0.35); box-shadow: 0 0 0 1px rgba(88,166,255,0.12); }
  .board-pin-btn {
    position: absolute; top: 6px; right: 30px;
    width: 22px; height: 22px; padding: 0; border: none; background: none;
    font-size: 0.72rem; cursor: pointer; opacity: 0;
    transition: opacity 0.15s; border-radius: 4px;
    color: var(--dim); display: flex; align-items: center; justify-content: center;
  }
  .board-card:hover .board-pin-btn { opacity: 0.5; }
  .board-pin-btn:hover { opacity: 1 !important; background: rgba(139,148,158,0.12); }
  .board-pin-btn.active { opacity: 0.8 !important; color: var(--accent); }
  .board-card:hover .board-pin-btn.active { opacity: 1 !important; }
  .board-drag-handle {
    position: absolute; top: 6px; right: 6px;
    width: 24px; height: 24px;
    display: flex; align-items: center; justify-content: center;
    cursor: default; color: var(--dim); opacity: 0;
    transition: opacity 0.15s;
    border-radius: 4px;
    touch-action: none;
  }
  .board-card:hover .board-drag-handle { opacity: 0.55; cursor: grab; }
  .board-drag-handle:hover { opacity: 1 !important; cursor: grab; color: var(--fg); background: rgba(139,148,158,0.12); }
  .board-drag-handle:active { cursor: grabbing; }
  @media (hover: none) { .board-drag-handle { opacity: 0.5; width: 32px; height: 32px; cursor: grab; } }
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
  /* Tag input widget (used in add-issue modal and detail view) */
  .be-tag-wrap {
    display: flex; flex-wrap: wrap; gap: 4px; align-items: center;
    padding: 5px 8px; border: 1px solid var(--border); border-radius: 8px;
    min-height: 34px; cursor: text; background: var(--input-bg, var(--bg));
    transition: border-color 0.15s;
  }
  .be-tag-wrap:focus-within { border-color: var(--accent); }
  .be-tag-chip {
    display: inline-flex; align-items: center; gap: 3px;
    background: rgba(88,166,255,0.12); color: var(--accent);
    border: 1px solid rgba(88,166,255,0.25); border-radius: 4px;
    padding: 2px 6px; font-size: 0.7rem; white-space: nowrap;
  }
  .be-tag-chip-remove { cursor: pointer; opacity: 0.55; line-height: 1; padding: 0 1px; }
  .be-tag-chip-remove:hover { opacity: 1; }
  .be-tag-input {
    border: none; outline: none; background: none;
    color: var(--text); font-size: 0.82rem; min-width: 80px; flex: 1; padding: 0;
    font-family: inherit;
  }
  .be-tag-suggestions { display: flex; flex-wrap: wrap; gap: 4px; margin-top: 6px; }
  .be-tag-suggestion {
    font-size: 0.7rem; padding: 2px 7px; border-radius: 4px; cursor: pointer;
    background: rgba(139,148,158,0.08); color: var(--dim);
    border: 1px solid rgba(139,148,158,0.12); transition: background 0.12s, color 0.12s;
  }
  .be-tag-suggestion:hover { background: rgba(88,166,255,0.1); color: var(--accent); border-color: rgba(88,166,255,0.2); }
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
  /* Suppress all selection + set grab cursor while a board drag is in progress */
  body.board-dragging, body.board-dragging * { user-select: none !important; -webkit-user-select: none !important; cursor: grabbing !important; }
  /* Ghost = placeholder showing drop target */
  .board-sortable-ghost { opacity: 0.4; background: rgba(88,166,255,0.07) !important; border: 2px dashed var(--accent) !important; border-radius: 8px; }
  /* Chosen = original card while being held */
  .board-sortable-chosen { opacity: 0.7; box-shadow: none; }
  /* Drag = the floating clone following the pointer */
  .board-sortable-drag { opacity: 0.96; box-shadow: 0 14px 36px rgba(0,0,0,0.45); transform: rotate(1.2deg) scale(1.03); border-radius: 8px; }
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
  .board-filters { display: flex; gap: 6px; flex-wrap: nowrap; padding: 6px 0 8px; align-items: center; overflow-x: auto; -webkit-overflow-scrolling: touch; scrollbar-width: none; }
  .board-filters::-webkit-scrollbar { display: none; }
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
  .gp-close, .gp-peek-btn {
    background: none; border: none; color: var(--dim); cursor: pointer;
    font-size: 0.82rem; padding: 2px 5px; border-radius: 3px; line-height: 1;
    flex-shrink: 0; -webkit-tap-highlight-color: transparent;
  }
  .gp-close:hover { background: rgba(248,81,73,0.15); color: #f85149; }
  .gp-peek-btn:hover { background: rgba(88,166,255,0.12); color: #58a6ff; }
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
    <button id="notif-btn" onclick="toggleNotifications()" title="Session notifications" style="background:none;border:none;cursor:pointer;padding:2px 4px;font-size:1rem;opacity:0.5;line-height:1;" aria-label="Toggle notifications">&#x1F514;</button>
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
        <div class="settings-section">
          <div class="settings-section-label">Automation</div>
          <div class="settings-row" style="justify-content:space-between;align-items:center;">
            <span style="font-size:0.85rem;">Auto-compact</span>
            <label class="theme-toggle">
              <input type="checkbox" id="auto-compact-checkbox" onchange="toggleAutoCompact(this.checked)" checked>
              <span class="theme-track"><span class="theme-thumb"></span></span>
            </label>
          </div>
          <div style="font-size:0.68rem;color:var(--dim);margin-top:3px;">Send /compact when context &lt; 20%</div>
        </div>
        <div class="settings-sep"></div>
        <div class="settings-section">
          <div class="settings-section-label">Appearance</div>
          <div class="settings-row" style="justify-content:space-between;align-items:center;">
            <span style="font-size:0.85rem;" id="theme-label">Dark mode</span>
            <label class="theme-toggle">
              <input type="checkbox" id="theme-checkbox" onchange="toggleTheme(this.checked)">
              <span class="theme-track"><span class="theme-thumb"></span></span>
            </label>
          </div>
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
  <button id="tab-reports" onclick="switchView('reports')">Reports</button>
  <button id="tab-notifications" onclick="switchView('notifications')" style="display:none;">Notifications</button>
  <button id="tab-files" onclick="switchView('files')">Files</button>
  <button id="tab-logs" onclick="switchView('logs')">Logs</button>
  <button id="tab-browser" onclick="switchView('browser')">Browser</button>
  <button id="tab-grid" onclick="enterGridMode()">Workspace</button>
  <button id="tab-email" onclick="switchView('email')">Email</button>
</div>
<div id="session-view">
<div style="padding:0 12px;margin-top:4px;display:flex;align-items:center;gap:8px;">
  <div class="search-wrap" id="search-wrap" style="flex:1;">
    <input class="search-input" id="search-input" type="text" placeholder="Search sessions..." autocomplete="off" autocorrect="off"
      oninput="searchQuery=this.value;document.getElementById('search-wrap').classList.toggle('has-value',!!this.value);onSearchInput()">
    <button class="search-clear" onclick="event.stopPropagation();clearSearch()">&#x2715;</button>
  </div>
  <button id="log-search-btn" class="tile-btn log-search-btn" onclick="toggleLogSearch()" title="Search inside session logs">
    <svg class="log-search-icon" xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/><line x1="8" y1="11" x2="14" y2="11"/><line x1="8" y1="8" x2="14" y2="8"/><line x1="8" y1="14" x2="11" y2="14"/></svg>
    <span class="log-search-label">Logs</span>
  </button>
  <div class="tile-controls">
    <button class="tile-btn" id="tile-list-btn" onclick="setLayoutMode('list')" title="List view">&#x2630;</button>
    <button class="tile-btn" id="tile-group-btn" onclick="setLayoutMode('group')" title="Group by status" style="font-size:0.75rem;font-weight:700;">#</button>
    <button class="tile-btn tile-grid-only" id="tile-grid-btn" onclick="setLayoutMode('grid')" title="Grid view">&#x268F;</button>
    <button class="tile-btn" id="tile-reset-btn" onclick="resetCardOrder()" title="Reset to default order (pinned → last active)" style="display:none;font-size:0.8rem;">&#x21BA;</button>
    <button class="tile-btn" id="tile-collapse-btn" onclick="collapseAll()" title="Collapse all sessions" style="display:none;font-size:0.75rem;">&#x2B06;</button>
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
<div id="archived-section"></div>
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
      <button class="btn" onclick="openSchedModal()" title="New scheduled task" style="font-size:0.8rem;">&#x23F0; Schedule</button>
    </div>
  </div>
  <div id="cal-body"></div>
</div>

<!-- Email events view -->
<div id="email-view" style="display:none;">
  <div style="padding:10px 12px 6px;display:flex;align-items:center;gap:8px;flex-wrap:wrap;">
    <span style="font-weight:600;font-size:0.85rem;color:var(--text);">Email Events</span>
    <div style="flex:1;"></div>
    <span id="email-last-sync" style="font-size:0.72rem;color:var(--dim);"></span>
    <button class="btn" id="email-sync-btn" onclick="_emailSync()" style="font-size:0.78rem;padding:4px 10px;">&#x21BB; Sync Now</button>
  </div>
  <div style="margin:0 12px 8px;padding:8px 12px;background:var(--card);border:1px solid var(--border);border-radius:6px;font-size:0.78rem;color:var(--dim);line-height:1.6;">
    Reads all accounts in <strong style="color:var(--text);">Mail.app</strong>, detects event/ticket emails with AI, and adds them to <strong style="color:var(--text);">Calendar.app</strong> (which syncs to Google Calendar automatically). No OAuth setup needed.
    <span style="display:block;margin-top:4px;">Optional: set <code style="color:var(--accent);">AMUX_CALENDAR_NAME</code> in <code>~/.amux/server.env</code> to target a specific calendar. Default: first writable calendar.</span>
  </div>
  <div style="padding:0 12px 4px;display:flex;align-items:center;gap:8px;">
    <span style="font-size:0.78rem;font-weight:600;color:var(--dim);">Detected Events</span>
    <div style="flex:1;"></div>
    <select id="email-filter" onchange="_emailRender()" style="font-size:0.75rem;padding:3px 7px;border-radius:5px;border:1px solid var(--border);background:var(--bg);color:var(--text);">
      <option value="">All</option>
      <option value="synced">Synced</option>
      <option value="pending">Pending</option>
      <option value="dismissed">Dismissed</option>
    </select>
  </div>
  <div id="email-events-list" style="padding:0 12px 60px;display:flex;flex-direction:column;gap:8px;"></div>
</div>
<!-- Reports view -->
<div id="reports-view" style="display:none;">
  <div style="padding:10px 12px 6px;display:flex;align-items:center;gap:8px;">
    <span style="font-weight:600;font-size:0.85rem;color:var(--text);">Reports</span>
    <div style="flex:1;"></div>
    <button class="btn" onclick="openAddReport()" style="font-size:0.78rem;padding:4px 10px;">+ Add Report</button>
  </div>
  <div id="reports-list" style="padding:0 12px 12px;display:flex;flex-direction:column;gap:12px;"></div>
  <!-- Add report modal -->
  <div id="add-report-overlay" class="board-edit-overlay" onclick="if(event.target===this)closeAddReport()" style="display:none;">
    <div class="board-edit-box" style="max-width:400px;">
      <div style="font-weight:600;font-size:0.9rem;margin-bottom:12px;">Add Report</div>
      <div class="field-group">
        <label class="field-label">Report Name</label>
        <input id="add-report-name" type="text" placeholder="Ops Spend" autocomplete="off" style="width:100%;box-sizing:border-box;">
      </div>
      <div class="field-group">
        <label class="field-label">Type</label>
        <select id="add-report-type" onchange="_updateAddReportVendors()" style="width:100%;padding:7px 10px;border-radius:6px;border:1px solid var(--border);background:var(--bg);color:var(--text);font-size:0.85rem;">
          <option value="infra-spend">Infrastructure Spend</option>
        </select>
      </div>
      <div class="field-group" id="add-report-vendors-group">
        <label class="field-label">Vendors</label>
        <div style="display:flex;flex-wrap:wrap;gap:8px;margin-top:4px;"></div>
      </div>
      <div style="display:flex;gap:8px;margin-top:14px;">
        <button class="btn" onclick="closeAddReport()" style="flex:1;">Cancel</button>
        <button class="btn primary" onclick="submitAddReport()" style="flex:1;">Add</button>
      </div>
    </div>
  </div>
</div>

<!-- Notifications view -->
<div id="notifications-view" style="display:none;">
  <div class="empty-state" style="padding:48px 16px;text-align:center;">
    <div style="font-size:2rem;margin-bottom:12px;">🔔</div>
    <div style="font-weight:600;margin-bottom:6px;color:var(--fg);">Notifications</div>
    <div style="color:var(--dim);font-size:0.85rem;">Due date reminders, spend alerts, and agent updates will appear here.</div>
  </div>
</div>

<div id="files-view" style="display:none;flex-direction:column;flex:1;min-height:0;">
  <div style="padding:8px 12px;display:flex;align-items:center;gap:8px;border-bottom:1px solid var(--border);flex-shrink:0;">
    <div id="files-breadcrumb" style="flex:1;font-size:0.82rem;font-family:'SF Mono','Fira Code',monospace;overflow-x:auto;white-space:nowrap;"></div>
    <button class="btn" id="files-home-btn" onclick="loadFiles(_filesCwd)" style="font-size:0.7rem;padding:2px 8px;" title="Go to working directory">&#x1F3E0;</button>
    <button class="btn" id="files-setcwd-btn" onclick="setFilesCwd()" style="font-size:0.7rem;padding:2px 8px;" title="Set current directory as working directory">&#x1F4CC; Set CWD</button>
    <button class="btn" id="files-hidden-btn" onclick="toggleFilesHidden()" style="font-size:0.7rem;padding:2px 8px;" title="Show hidden files">.*</button>
    <button class="btn" id="files-cache-btn" onclick="cacheFilesDir(_filesPath)" style="font-size:0.7rem;padding:2px 8px;" title="Cache directory for offline viewing">&#x2601; Cache</button>
    <span id="files-cache-status" style="font-size:0.7rem;color:var(--dim);white-space:nowrap;"></span>
  </div>
  <div id="files-body" style="flex:1;overflow-y:auto;padding:0;"></div>
</div>

<div id="logs-view" style="display:none;flex-direction:column;flex:1;min-height:0;">
  <div class="logs-toolbar">
    <div style="display:flex;gap:4px;flex-wrap:wrap;align-items:center;">
      <button class="lf-btn active" data-cat="" onclick="logsSetFilter(this,'')">All</button>
      <button class="lf-btn" data-cat="board" onclick="logsSetFilter(this,'board')">Board</button>
      <button class="lf-btn" data-cat="session" onclick="logsSetFilter(this,'session')">Sessions</button>
      <button class="lf-btn" data-cat="memory" onclick="logsSetFilter(this,'memory')">Memory</button>
      <button class="lf-btn" data-cat="files" onclick="logsSetFilter(this,'files')">Files</button>
      <button class="lf-btn" data-cat="http" onclick="logsSetFilter(this,'http')">HTTP</button>
    </div>
    <div style="display:flex;gap:6px;align-items:center;">
      <input id="logs-search" class="search-input" type="text" placeholder="Search..." style="width:140px;font-size:0.72rem;padding:4px 8px;" oninput="logsSearchQuery=this.value;renderActivity()">
      <button class="btn" title="Refresh" onclick="fetchLogs()" style="font-size:0.7rem;padding:2px 6px;">&#x21BB;</button>
    </div>
  </div>
  <div class="logs-subtabs">
    <button id="lst-activity" class="lst-btn active" onclick="logsSetTab('activity')">Activity</button>
    <button id="lst-raw" class="lst-btn" onclick="logsSetTab('raw')">Raw Logs</button>
    <button id="lst-stats" class="lst-btn" onclick="logsSetTab('stats')">Stats</button>
  </div>
  <div id="logs-activity" style="flex:1;overflow-y:auto;padding:0 12px 12px;">
    <div id="logs-activity-body"></div>
  </div>
  <div id="logs-raw" style="display:none;flex:1;overflow-y:auto;padding:0 12px 12px;">
    <div style="display:flex;align-items:center;gap:8px;padding:6px 0;">
      <input id="raw-filter" class="search-input" type="text" placeholder="Filter lines..." style="flex:1;font-size:0.72rem;padding:4px 8px;" oninput="renderRawLogs()">
      <span id="raw-line-count" style="font-size:0.65rem;color:var(--dim);white-space:nowrap;"></span>
    </div>
    <pre id="logs-raw-body" style="font-size:0.68rem;line-height:1.5;margin:0;white-space:pre-wrap;word-break:break-all;color:var(--fg);"></pre>
  </div>
  <div id="logs-stats" style="display:none;flex:1;overflow-y:auto;padding:8px 12px;">
    <div id="logs-stats-body"></div>
  </div>
</div>

<div id="browser-view" style="display:none;flex-direction:column;flex:1;min-height:0;">
  <!-- Profile bar -->
  <div style="padding:4px 12px;display:flex;align-items:center;gap:6px;border-bottom:1px solid var(--border);flex-shrink:0;font-size:0.72rem;">
    <span style="color:var(--dim);">Profile:</span>
    <select id="rb-profile" onchange="_rbSwitchProfile(this.value)" style="font-size:0.72rem;padding:2px 6px;background:var(--card-bg);color:var(--fg);border:1px solid var(--border);border-radius:4px;">
      <option value="default">default</option>
    </select>
    <button class="btn" id="rb-new-btn" onclick="_rbShowNewProfile()" style="font-size:0.6rem;padding:1px 6px;">+ New</button>
    <span id="rb-new-profile-form" style="display:none;align-items:center;gap:4px;">
      <input id="rb-new-profile-name" type="text" placeholder="profile name" autocomplete="off"
        style="font-size:0.7rem;padding:2px 6px;width:110px;background:var(--card-bg);color:var(--fg);border:1px solid var(--border);border-radius:4px;"
        onkeydown="if(event.key==='Enter')_rbCreateProfile();if(event.key==='Escape')_rbHideNewProfile()">
      <button class="btn primary" onclick="_rbCreateProfile()" style="font-size:0.6rem;padding:1px 7px;">Create</button>
      <button class="btn" onclick="_rbHideNewProfile()" style="font-size:0.6rem;padding:1px 6px;">&#x2715;</button>
    </span>
    <button class="btn" id="rb-del-profile" onclick="_rbDeleteProfile()" style="font-size:0.6rem;padding:1px 6px;color:var(--red);display:none;" title="Delete profile">&#x2715;</button>
    <span id="rb-profile-state" style="font-size:0.72rem;line-height:1;" title="Profile has saved login state"></span>
    <button class="btn" id="rb-login-btn" onclick="_rbShowLoginUrl()" style="font-size:0.6rem;padding:1px 8px;" title="Log in and save credentials to this profile">&#x1F511; Login</button>
    <span id="rb-login-url-form" style="display:none;align-items:center;gap:4px;">
      <input id="rb-login-url" type="text" placeholder="https://site.com/login" autocomplete="off"
        style="font-size:0.7rem;padding:2px 6px;width:160px;background:var(--card-bg);color:var(--fg);border:1px solid var(--border);border-radius:4px;"
        onkeydown="if(event.key==='Enter')_rbDoLogin();if(event.key==='Escape')_rbHideLoginUrl()">
      <button class="btn primary" onclick="_rbDoLogin()" style="font-size:0.6rem;padding:1px 7px;">Open</button>
      <button class="btn" onclick="_rbHideLoginUrl()" style="font-size:0.6rem;padding:1px 6px;">&#x2715;</button>
    </span>
    <button class="btn" id="rb-rec-btn" onclick="_rbToggleRecord()" style="font-size:0.6rem;padding:1px 8px;" title="Toggle video recording (takes effect on next launch)">&#x23FA; Rec</button>
    <button class="btn" onclick="openRecordingsDir()" style="font-size:0.6rem;padding:1px 8px;" title="Browse saved recordings">&#x1F3A5; Recordings</button>
    <span id="rb-profile-status" style="color:var(--dim);margin-left:auto;"></span>
  </div>
  <!-- Recording download banner -->
  <div id="rb-video-banner" style="display:none;padding:5px 12px;background:var(--card-bg);border-bottom:1px solid var(--border);font-size:0.72rem;display:none;align-items:center;gap:8px;">
    <span>&#x1F3A5; Recording ready —</span>
    <a id="rb-video-link" href="/api/browser/video" download style="color:var(--accent);text-decoration:none;font-weight:600;">Download MP4</a>
    <button class="btn" onclick="document.getElementById('rb-video-banner').style.display='none'" style="font-size:0.6rem;padding:1px 6px;margin-left:auto;">&#x2715;</button>
  </div>
  <!-- Tab bar (hidden when only 1 tab) -->
  <div id="rb-tab-bar" style="display:none;padding:0 12px;border-bottom:1px solid var(--border);flex-shrink:0;overflow-x:auto;white-space:nowrap;background:var(--bg);">
  </div>
  <!-- URL bar -->
  <div style="padding:6px 12px;display:flex;align-items:center;gap:6px;border-bottom:1px solid var(--border);flex-shrink:0;">
    <button class="btn" onclick="_rbCmd('back')" style="font-size:0.8rem;padding:2px 6px;" title="Back">&#x25C0;</button>
    <button class="btn" onclick="_rbCmd('forward')" style="font-size:0.8rem;padding:2px 6px;" title="Forward">&#x25B6;</button>
    <button class="btn" onclick="_rbCmd('reload')" style="font-size:0.8rem;padding:2px 6px;" title="Reload">&#x21BB;</button>
    <input id="rb-url" type="text" placeholder="https://example.com" autocomplete="off"
      style="flex:1;font-size:0.8rem;font-family:monospace;padding:5px 8px;"
      onkeydown="if(event.key==='Enter'){_rbNavigate(this.value);}">
    <button class="btn" onclick="_rbNavigate(document.getElementById('rb-url').value)" style="font-size:0.75rem;padding:3px 10px;">Go</button>
    <span id="rb-status" style="font-size:0.7rem;color:var(--dim);min-width:50px;text-align:right;"></span>
  </div>
  <!-- Agent bar -->
  <div style="padding:5px 12px;display:flex;align-items:center;gap:6px;border-bottom:1px solid var(--border);flex-shrink:0;background:color-mix(in srgb,var(--card-bg) 60%,transparent);">
    <span style="font-size:0.65rem;color:var(--dim);white-space:nowrap;">&#x1F916; Agent:</span>
    <input id="rb-agent-input" type="text" placeholder='e.g. "search YouTube for lo-fi music and play the first result"'
      style="flex:1;font-size:0.72rem;padding:3px 8px;"
      onkeydown="if(event.key==='Enter')_rbRunAgent()">
    <button class="btn primary" id="rb-agent-run" onclick="_rbRunAgent()" style="font-size:0.68rem;padding:3px 10px;white-space:nowrap;">&#x25B6; Run</button>
    <button class="btn" id="rb-agent-stop" onclick="_rbStopAgent()" style="font-size:0.68rem;padding:3px 8px;color:var(--red);display:none;">&#x23F9; Stop</button>
    <span id="rb-agent-status" style="font-size:0.65rem;color:var(--dim);white-space:nowrap;max-width:180px;overflow:hidden;text-overflow:ellipsis;"></span>
  </div>
  <!-- Viewport -->
  <div id="rb-viewport-wrap" style="flex:1;min-height:0;overflow:hidden;position:relative;background:#1a1a2e;display:flex;align-items:center;justify-content:center;">
    <div id="rb-placeholder" style="color:var(--dim);font-size:0.9rem;text-align:center;padding:40px;">
      <div style="font-size:2rem;margin-bottom:12px;">&#x1F310;</div>
      <div>Remote Browser</div>
      <div style="font-size:0.75rem;margin-top:8px;">Enter a URL above or</div>
      <button class="btn primary" onclick="_rbStart()" style="margin-top:12px;font-size:0.8rem;padding:6px 16px;">Launch Browser</button>
    </div>
    <div id="rb-login-panel" style="display:none;text-align:center;padding:40px;color:var(--fg);">
      <div style="font-size:2.5rem;margin-bottom:16px;">&#x1F511;</div>
      <div style="font-weight:600;font-size:1rem;margin-bottom:8px;">Browser window open on your desktop</div>
      <div style="font-size:0.8rem;color:var(--dim);margin-bottom:24px;max-width:320px;margin-left:auto;margin-right:auto;">Log in to your accounts, then click Done to save your credentials to this profile.</div>
      <button class="btn primary" onclick="_rbLoginDone()" style="font-size:0.85rem;padding:8px 24px;">Done — Save & Close</button>
    </div>
    <img id="rb-screen" style="display:none;max-width:100%;max-height:100%;cursor:crosshair;image-rendering:auto;user-select:none;-webkit-user-select:none;"
      onclick="_rbClick(event)" oncontextmenu="event.preventDefault();_rbClick(event,true)">
  </div>
  <!-- Input bar -->
  <div style="padding:6px 12px;display:flex;align-items:center;gap:6px;border-top:1px solid var(--border);flex-shrink:0;">
    <input id="rb-type-input" type="text" placeholder="Type text and press Enter to send keystrokes..."
      style="flex:1;font-size:0.8rem;padding:5px 8px;"
      onkeydown="_rbTypeKey(event)">
    <button class="btn" onclick="_rbCmd('screenshot')" style="font-size:0.7rem;padding:3px 8px;" title="Refresh screenshot">&#x1F4F7;</button>
    <button class="btn" onclick="_rbStop()" style="font-size:0.7rem;padding:3px 8px;color:var(--red);" title="Close browser">&#x2716;</button>
  </div>
</div>

<!-- Schedule modal -->
<div id="sched-overlay" class="board-edit-overlay" onclick="if(event.target===this)closeSchedModal()" style="display:none;">
  <div class="board-edit-box" style="max-width:420px;">
    <div style="font-weight:600;font-size:0.9rem;margin-bottom:10px;">&#x23F0; Scheduled Task</div>
    <div class="field-group">
      <label class="field-label">Title</label>
      <input id="sched-title" type="text" placeholder="What should run?" autocomplete="off">
    </div>
    <div class="field-group">
      <label class="field-label">Session</label>
      <select id="sched-session" class="board-detail-session-select" style="width:100%;"></select>
    </div>
    <div class="field-group">
      <label class="field-label">Command</label>
      <input id="sched-command" type="text" placeholder="e.g. /status or npm run build" autocomplete="off">
    </div>
    <div class="field-group">
      <label class="field-label">Schedule</label>
      <select id="sched-type" class="board-detail-session-select" style="width:100%;" onchange="updateSchedTypeUI()">
        <option value="once">Once</option>
        <option value="recurring">Recurring</option>
      </select>
    </div>
    <div id="sched-once-fields" class="field-group">
      <label class="field-label">Run at</label>
      <input id="sched-run-at" type="datetime-local" class="board-detail-session-select" style="width:100%;">
    </div>
    <div id="sched-rec-fields" style="display:none;">
      <div class="field-group">
        <label class="field-label">Repeat</label>
        <select id="sched-recurrence" class="board-detail-session-select" style="width:100%;" onchange="updateSchedRecUI()">
          <option value="hourly">Hourly</option>
          <option value="daily" selected>Daily</option>
          <option value="weekly">Weekly</option>
          <option value="monthly">Monthly</option>
        </select>
      </div>
      <div class="field-group" id="sched-time-field">
        <label class="field-label">Time</label>
        <input id="sched-time" type="time" class="board-detail-session-select" style="width:100%;" value="09:00">
      </div>
      <div class="field-group" id="sched-weekday-field" style="display:none;">
        <label class="field-label">Day of week</label>
        <select id="sched-weekday" class="board-detail-session-select" style="width:100%;">
          <option value="0">Monday</option><option value="1">Tuesday</option>
          <option value="2">Wednesday</option><option value="3">Thursday</option>
          <option value="4">Friday</option><option value="5">Saturday</option><option value="6">Sunday</option>
        </select>
      </div>
      <div class="field-group" id="sched-monthday-field" style="display:none;">
        <label class="field-label">Day of month</label>
        <input id="sched-monthday" type="number" min="1" max="28" value="1" class="board-detail-session-select" style="width:100%;">
      </div>
    </div>
    <div style="display:flex;gap:8px;justify-content:flex-end;margin-top:12px;">
      <button class="btn" onclick="closeSchedModal()">Cancel</button>
      <button class="btn btn-primary" onclick="saveSchedModal()" id="sched-save-btn">Save</button>
    </div>
  </div>
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
    <div class="field-group">
      <label class="field-label">Tags <span class="field-optional">(optional)</span></label>
      <div class="be-tag-wrap" id="be-tag-wrap" onclick="document.getElementById('be-tag-input').focus()">
        <input id="be-tag-input" class="be-tag-input" type="text" placeholder="Add tag…"
          autocomplete="off" autocorrect="off" autocapitalize="none"
          oninput="_beTagInputUpdate('be')"
          onkeydown="_beTagKeydown(event,'be')">
      </div>
      <div id="be-tag-suggestions" class="be-tag-suggestions"></div>
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
      <div style="display:flex;gap:0.5rem;">
        <input id="be-due" type="date" style="flex:1;">
        <input id="be-due-time" type="time" style="width:110px;" title="Time (optional — leave blank for all-day)">
      </div>
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
      <input type="time" id="bd-due-time" class="board-detail-session-select" style="width:110px;cursor:pointer;" title="Time (optional — leave blank for all-day)">
    </div>
    <div class="board-detail-row" style="align-items:flex-start;">
      <span style="font-size:0.78rem;color:var(--dim);padding-top:7px;">Tags:</span>
      <div style="flex:1;">
        <div class="be-tag-wrap" id="bd-tag-wrap" onclick="document.getElementById('bd-tag-input').focus()">
          <input id="bd-tag-input" class="be-tag-input" type="text" placeholder="Add tag…"
            autocomplete="off" autocorrect="off" autocapitalize="none"
            oninput="_beTagInputUpdate('bd')"
            onkeydown="_beTagKeydown(event,'bd')">
        </div>
        <div id="bd-tag-suggestions" class="be-tag-suggestions"></div>
      </div>
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
        oninput="_createNameChanged(this.value)"
        onkeydown="if(event.key==='Enter'){event.preventDefault();document.getElementById('create-dir').focus({preventScroll:true});}">
    </div>
    <div class="field-group">
      <label class="field-label">Working directory</label>
      <div class="ac-wrap">
        <input id="create-dir" type="text" placeholder="/path/to/project" autocomplete="off" autocorrect="off"
          oninput="acFetch(this.value)" onfocus="acFetch(this.value)"
          onpaste="_acSuppressNext=true" onkeydown="acKeydown(event)">
        <div id="ac-list" class="ac-list"></div>
      </div>
    </div>
    <div class="field-group">
      <label class="field-label">Initial prompt <span class="field-optional">(optional)</span></label>
      <textarea id="create-prompt" rows="3" placeholder="What should Claude work on first?"></textarea>
    </div>
    <div class="field-group">
      <label class="field-label" style="display:flex;align-items:center;gap:6px;cursor:pointer;">
        <input type="checkbox" id="create-branch-enabled" onchange="_toggleCreateBranch(this.checked)" style="width:auto;margin:0;">
        Git branch <span class="field-optional">(optional)</span>
      </label>
      <div id="create-branch-wrap" style="display:none;margin-top:8px;">
        <div style="display:flex;gap:6px;align-items:center;">
          <input id="create-branch" type="text" placeholder="session/my-project" autocomplete="off" autocorrect="off" style="flex:1;">
          <button class="btn" id="create-branch-suggest-btn" onclick="_suggestBranch()" title="Ask Claude to suggest branch names" style="flex-shrink:0;font-size:0.9rem;">✨</button>
        </div>
        <div id="create-branch-suggestions" style="display:none;flex-wrap:wrap;gap:6px;margin-top:8px;"></div>
      </div>
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
      <div class="peek-find-wrap" id="peek-search-wrap">
        <input class="search-input" id="peek-search" type="text" placeholder="Find..." autocomplete="off" autocorrect="off"
          oninput="peekSearchQuery=this.value;document.getElementById('peek-search-wrap').classList.toggle('has-value',!!this.value);applyPeekSearch()"
          onkeydown="if(event.key==='Enter'){event.preventDefault();event.shiftKey?peekSearchPrev():peekSearchNext();}">
        <span class="peek-find-count" id="peek-search-count"></span>
        <button class="peek-nav-btn" onclick="peekSearchPrev()" title="Previous match (Shift+Enter)">&#x2191;</button>
        <button class="peek-nav-btn" onclick="peekSearchNext()" title="Next match (Enter)">&#x2193;</button>
        <button class="search-clear" onclick="event.stopPropagation();clearPeekSearch()">&#x2715;</button>
      </div>
      <button class="btn" id="peek-explore-btn" onclick="openExplore(peekSessionDir,peekSession)" title="Browse files">&#x1F4C2;</button>
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
    <span id="peek-dir-text" onclick="peekSessionDir&&openExplore(peekSessionDir,peekSession)" title="Browse directory" style="cursor:pointer;text-decoration:underline;text-decoration-color:var(--dim);text-underline-offset:2px;"></span>
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
          <div class="chip" onclick="peekQuickSend('continue')">continue</div>
          <div class="chip danger" onclick="peekQuickSend('/compact')">/compact</div>
          <div class="chip danger" onclick="peekQuickSend('/clear')">/clear</div>
        </div>
        <!-- Attachment chips -->
        <div class="peek-attach-bar" id="peek-attach-bar"></div>
        <!-- Input row -->
        <div class="ac-wrap" style="flex:1;min-width:0;position:relative;">
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
        <button id="peek-mic-btn" class="peek-attach-btn" title="Dictate" onclick="toggleMic()" style="display:none;">&#127908;</button>
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
      <button class="btn" onclick="pullFromRemote(this)" style="margin-top:6px;font-size:0.72rem;padding:4px 12px;">&#x2B07; Pull from remote</button>
      <div id="pull-status" style="color:var(--dim);font-size:0.7rem;font-family:monospace;margin-top:4px;min-height:1.2em;white-space:pre-wrap;max-height:60px;overflow-y:auto;"></div>
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
  <div style="display:flex;flex-direction:column;height:100%;max-width:600px;margin:0 auto;width:100%;">
    <div class="overlay-header">
      <h2>&#x26A1; Skills library</h2>
      <div style="display:flex;gap:8px;">
        <button class="btn primary" onclick="editSkill()" style="font-size:0.8rem;">+ New skill</button>
        <button class="btn" onclick="closeSkills()">&#x2715;</button>
      </div>
    </div>
    <p style="font-size:0.8rem;color:var(--dim);margin:0 0 14px;">Shared skills for all sessions. Use with <code style="background:var(--card);padding:1px 5px;border-radius:4px;font-size:0.78rem;">/command</code> in Claude Code.</p>
    <div id="skills-list" style="flex:1;overflow-y:auto;display:flex;flex-direction:column;gap:8px;"></div>
  </div>
</div>
<!-- Skill editor modal -->
<div id="skill-edit-modal" class="overlay" style="z-index:210;" onclick="if(event.target===this)closeSkillEdit()">
  <div style="display:flex;flex-direction:column;height:100%;max-width:700px;margin:0 auto;width:100%;">
    <div class="overlay-header">
      <h2 id="skill-edit-title">New skill</h2>
      <div style="display:flex;gap:8px;">
        <button class="btn" id="skill-delete-btn" onclick="deleteSkill()" style="color:var(--red);display:none;">Delete</button>
        <button class="btn primary" onclick="saveSkill()">Save</button>
        <button class="btn" onclick="closeSkillEdit()">&#x2715;</button>
      </div>
    </div>
    <div style="display:flex;gap:8px;margin-bottom:10px;">
      <label style="font-size:0.8rem;color:var(--dim);align-self:center;flex-shrink:0;">Name:</label>
      <input id="skill-edit-name" class="send-input" style="min-height:36px;font-size:0.85rem;padding:6px 10px;font-family:'SF Mono','Fira Code',monospace;"
        placeholder="my-skill" autocomplete="off">
    </div>
    <textarea id="skill-edit-content" class="peek-memory-textarea" style="flex:1;"
      placeholder="---&#10;description: What this skill does&#10;allowed-tools: Bash, Read, Edit&#10;argument-hint: [args]&#10;---&#10;&#10;# Skill instructions...&#10;&#10;$ARGUMENTS"></textarea>
  </div>
</div>

<!-- File preview overlay (z-index 300 so it stacks above explorer at 250) -->
<div id="file-overlay" class="file-overlay" style="z-index:300;">
  <div class="file-overlay-header">
    <h2 id="file-title">file</h2>
    <div class="file-view-tabs" id="file-view-tabs" style="display:none;">
      <button class="file-view-tab active" id="file-tab-preview" onclick="setFileViewMode('preview')">Preview</button>
      <button class="file-view-tab" id="file-tab-raw" onclick="setFileViewMode('raw')">Raw</button>
      <button class="file-view-tab" id="file-tab-copy" onclick="copyFileContent()" title="Copy to clipboard">Copy</button>
    </div>
    <a id="file-download-btn" style="display:none;font-size:0.72rem;padding:3px 10px;border:1px solid var(--border);border-radius:6px;color:var(--accent);text-decoration:none;white-space:nowrap;flex-shrink:0;">&#x2B07; Download</a>
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
// ── Theme ──
function _applyTheme(light) {
  document.body.classList.toggle('light', light);
  const cb = document.getElementById('theme-checkbox');
  if (cb) cb.checked = light;
  const lbl = document.getElementById('theme-label');
  if (lbl) lbl.textContent = light ? 'Light mode' : 'Dark mode';
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.content = light ? '#ffffff' : '#0d1117';
}
function toggleTheme(checked) {
  const isLight = checked !== undefined ? checked : !document.body.classList.contains('light');
  localStorage.setItem('amux_theme', isLight ? 'light' : 'dark');
  _applyTheme(isLight);
}
(function initTheme() {
  const saved = localStorage.getItem('amux_theme');
  const preferLight = saved ? saved === 'light' : window.matchMedia('(prefers-color-scheme: light)').matches;
  _applyTheme(preferLight);
})();

// ── Auto-compact toggle ──
async function toggleAutoCompact(checked) {
  await fetch('/api/prefs', {
    method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ key: 'auto_compact_enabled', value: checked ? '1' : '0' })
  });
}
(async function initAutoCompact() {
  try {
    const r = await fetch('/api/prefs?key=auto_compact_enabled');
    const d = await r.json();
    const enabled = d.value !== '0';  // default ON
    const cb = document.getElementById('auto-compact-checkbox');
    if (cb) cb.checked = enabled;
  } catch(e) {}
})();

// ═══════ STATE & GLOBALS ═══════
const API = '';
let sessions = [];
let archivedExpanded = false;
let gitInfo = {};  // {sessionName: {branch, repo, _conflict}}
let _initialLoad = true;   // true until first data arrives from server
let _lastDataTime = null;  // timestamp of last successful data
let _debugLog = [];        // recent connection events (capped at 12)
let _liveSSE = false;      // true only when SSE is actively receiving messages
let expanded = new Set();
let searchQuery = '';
let activeTag = '';
let logSearchMode = false;
let _logMatches = {};       // name -> matched snippet string
let _logSearchTimer = null;
let _logSearchAbort = null;
let peekSession = null;
let peekTimer = null;
let peekSessionDir = '';
let peekSearchQuery = '';
let peekSearchIndex = 0;
let _peekMatches = [];
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

// ── Session notifications ──
const _prevSessionState = {};  // name → {status, running}
let _notifsEnabled = localStorage.getItem('amux_notifs') === '1';

function _updateNotifBtn() {
  const btn = document.getElementById('notif-btn');
  if (!btn) return;
  const granted = Notification.permission === 'granted';
  btn.textContent = _notifsEnabled && granted ? '\uD83D\uDD14' : '\uD83D\uDD15';
  btn.style.opacity = _notifsEnabled && granted ? '1' : '0.4';
  btn.title = _notifsEnabled ? 'Notifications on — click to disable' : 'Notifications off — click to enable';
}

async function toggleNotifications() {
  if (!_notifsEnabled) {
    if (Notification.permission === 'denied') {
      showToast('Blocked — go to browser Site Settings and allow Notifications for this site');
      return;
    }
    const perm = Notification.permission === 'granted'
      ? 'granted'
      : await Notification.requestPermission();
    if (perm !== 'granted') { showToast('Notification permission not granted'); return; }
    _notifsEnabled = true;
    localStorage.setItem('amux_notifs', '1');
    showToast('Session notifications enabled');
  } else {
    _notifsEnabled = false;
    localStorage.setItem('amux_notifs', '0');
    showToast('Session notifications disabled');
  }
  _updateNotifBtn();
}

function _fireSessionNotif(name, title, body) {
  if (!_notifsEnabled || Notification.permission !== 'granted') return;
  const n = new Notification(title, { body, tag: 'amux-' + name, renotify: true, silent: false });
  n.onclick = () => { window.focus(); openPeek(name); n.close(); };
}

function _checkSessionTransitions(newData) {
  if (_initialLoad) return;  // skip initial load to avoid flood
  for (const s of newData) {
    const prev = _prevSessionState[s.name];
    if (!prev) { _prevSessionState[s.name] = { status: s.status, running: s.running }; continue; }
    const statusChanged = s.status !== prev.status;
    const stoppedNow = prev.running && !s.running;
    if (statusChanged) {
      if (s.status === 'waiting') {
        _fireSessionNotif(s.name, s.name + ' needs input', s.task_name || 'Waiting for a response');
      } else if (s.status === 'active' && prev.status !== 'active') {
        _fireSessionNotif(s.name, s.name + ' started working', s.task_name || '');
      }
    }
    if (stoppedNow && prev.status !== '') {
      _fireSessionNotif(s.name, s.name + ' stopped', '');
    }
    _prevSessionState[s.name] = { status: s.status, running: s.running };
  }
}

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
      _checkSessionTransitions(data);
      lastSessionsJSON = j;
      sessions = data;
      localStorage.setItem('amux_sessions_cache', j);
      render();
      _fetchGitBranches(sessions);
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
  // Save focused element before ANY DOM changes — captures search-input, send-input, or anything else
  const focusedId = document.activeElement && document.activeElement.id ? document.activeElement.id : null;
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
  const _nonArchivedCount = sessions.filter(s => !s.archived).length;
  if (!_nonArchivedCount && !drafts.length) {
    if (_initialLoad) {
      el.innerHTML = '<div class="empty"><span class="loading-spinner"></span>Connecting to server…</div>';
    } else {
      el.innerHTML = '<div class="empty">No sessions yet.<br>Tap <strong>+</strong> to create one.' +
        (!online ? '<br><span style="color:var(--yellow)">You\'re offline — sessions created now will sync when connected.</span>' : '') + '</div>';
    }
    _renderArchivedSection();
    if (focusedId) { const f = document.getElementById(focusedId); if (f) f.focus({ preventScroll: true }); }
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

  // Filter by tag (exclude archived from main view)
  let list = (activeTag ? sessions.filter(s => (s.tags || []).includes(activeTag)) : sessions).filter(s => !s.archived);
  // Filter by search query
  const q = searchQuery.toLowerCase().trim();
  const filtered = q ? list.filter(s =>
    s.name.toLowerCase().includes(q) ||
    (s.dir || '').toLowerCase().includes(q) ||
    (s.desc || '').toLowerCase().includes(q) ||
    (s.tags || []).some(t => t.toLowerCase().includes(q)) ||
    (logSearchMode && s.name in _logMatches)
  ) : list;
  if ((q || activeTag) && !filtered.length) {
    el.innerHTML = '<div class="empty">No matching sessions.</div>';
    _renderArchivedSection();
    if (focusedId) { const f = document.getElementById(focusedId); if (f) f.focus({ preventScroll: true }); }
    return;
  }
  // Save input values before re-rendering (focusedId already captured at top)
  const savedInputs = {};
  el.querySelectorAll('.send-input').forEach(inp => {
    if (inp.value) savedInputs[inp.id] = inp.value;
  });

  function _renderSessionCard(s) {
    const isExp = expanded.has(s.name);
    const flags = s.flags || '';
    const isYolo = flags.includes('--dangerously-skip-permissions');
  const isAutoContinue = !!s.auto_continue;
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
          <div class="card-menu-item" onclick="event.stopPropagation();togglePin('${s.name}')"><span class="mi">${s.pinned?'&#x1F4CC;':'&#x1F4CC;'}</span> ${s.pinned ? 'Unpin' : 'Pin to top'}</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','name','${esc(s.name)}')"><span class="mi">&#x270E;</span> Rename</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','model','${esc(model||"")}')"><span class="mi">&#x2699;</span> Model${model ? ': '+esc(model) : ''}</div>
          <div class="card-menu-item" onclick="event.stopPropagation();toggleYolo('${s.name}')"><span class="mi">${isYolo?'&#x2611;':'&#x2610;'}</span> YOLO mode</div>
          <div class="card-menu-item" onclick="event.stopPropagation();toggleAutoContinue('${s.name}')" title="Auto-respond when Claude is waiting for user input (~60s delay)"><span class="mi">${isAutoContinue?'&#x2611;':'&#x2610;'}</span> Auto-continue</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','desc','${esc(s.desc||"")}')"><span class="mi">&#x1F4DD;</span> Description</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','tags','${esc(s.tags.join(", "))}')"><span class="mi">&#x1F3F7;</span> Tags</div>
          <div class="card-menu-item" onclick="event.stopPropagation();editField('${s.name}','dir','${esc(s.dir)}')"><span class="mi">&#x1F4C1;</span> Directory</div>
          ${s.running ? `<div class="card-menu-item" onclick="event.stopPropagation();clearScrollback('${s.name}')"><span class="mi">&#x239A;</span> Clear scrollback</div>` : ''}
          <div class="card-menu-item" onclick="event.stopPropagation();duplicateSession('${s.name}')"><span class="mi">&#x2398;</span> Duplicate</div>
          ${s.running ? `<div class="card-menu-item" onclick="event.stopPropagation();cloneSession('${s.name}')"><span class="mi">&#x1F504;</span> Clone &amp; continue</div>` : ''}
          ${!s.running ? `<div class="card-menu-item" onclick="event.stopPropagation();newConversation('${s.name}')"><span class="mi">&#x1F195;</span> New conversation</div>` : ''}
          <div class="card-menu-item" onclick="event.stopPropagation();archiveSession('${s.name}')"><span class="mi">&#x1F4E6;</span> Archive</div>
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
      ${s.dir ? `<div class="card-dir"><span class="card-dir-path" onclick="event.stopPropagation();openExplore('${s.dir.replace(/'/g,"\\'")}','${s.name.replace(/'/g,"\\'")}')" style="cursor:pointer;" title="Browse files">${esc(s.dir)}</span></div>` : ''}
      ${s.creator ? `<div class="card-dir" style="font-size:0.72rem;">${esc(s.creator)}</div>` : ''}
      ${s.dir ? _renderBranchBadge(s.name) : ''}
      ${isExp && s.desc ? `<div class="card-desc">${esc(s.desc)}</div>` : ''}
      ${!isExp && s.task_name ? `<div class="card-preview">${esc(s.task_name)}</div>` : ''}
      ${isExp && s.preview ? `<div class="card-preview">${esc(s.preview)}</div>` : ''}
      ${logSearchMode && _logMatches[s.name] ? (() => {
        const hits = _logMatches[s.name];
        const sq = searchQuery.replace(/'/g,"\\'");
        return hits.slice(0, 2).map((h, hi) =>
          `<div class="card-log-hit" onclick="event.stopPropagation();openPeek('${s.name}',{query:'${sq}',hitIdx:${hi}})"><span class="log-hit-loc">${esc(s.name)}:${h.line}</span> <span class="log-hit-text">${esc(h.text.slice(0, 80))}</span></div>`
        ).join('') + (hits.length > 2 ? `<div class="card-log-hit" style="color:var(--dim);font-style:italic;" onclick="event.stopPropagation();openPeek('${s.name}',{query:'${sq}'})">+${hits.length - 2} more matches</div>` : '');
      })() : ''}
      ${(isYolo || isAutoContinue || model || s.tags.length) ? `<div class="badges">
        ${isYolo ? '<span class="badge yolo">YOLO</span>' : ''}
        ${isAutoContinue ? '<span class="badge auto-continue" title="Auto-continue enabled">AUTO</span>' : ''}
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
        ${s.running ? `
        <div class="chips">
          <div class="chip" onclick="chipToInput('${s.name}','/compact')">/compact</div>
          <div class="chip" onclick="chipToInput('${s.name}','/status')">/status</div>
          <div class="chip" onclick="chipToInput('${s.name}','/clear')">/clear</div>
          <div class="chip" onclick="chipToInput('${s.name}','/cost')">/cost</div>
          <div class="chip" onclick="doKeys('${s.name}','C-c')">Ctrl-C</div>
          <div class="chip" onclick="doKeys('${s.name}','Escape')">Esc</div>
          <div class="chip" onclick="doKeys('${s.name}','Enter')">Enter</div>
          <div class="chip" onclick="doSend('${s.name}','continue')">continue</div>
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
    if (layoutMode === 'list' && !activeTag && !q) {
      if (cardOrder.length) {
        const orderMap = {};
        cardOrder.forEach((n, i) => { orderMap[n] = i; });
        flatList = [...filtered].sort((a, b) => {
          const ai = orderMap[a.name];
          const bi = orderMap[b.name];
          if (ai !== undefined && bi !== undefined) return ai - bi;
          if (ai !== undefined) return -1; // ordered before unordered
          if (bi !== undefined) return 1;
          return _naturalSortSessions(a, b); // new sessions: natural order
        });
      } else {
        flatList = [...filtered].sort(_naturalSortSessions);
      }
    }
    el.innerHTML = draftCards + flatList.map(_renderSessionCard).join('');
    if (layoutMode === 'list') requestAnimationFrame(initSortable);
  }
  _updateResetBtn();

  // Restore input values and focus after re-rendering
  for (const [id, val] of Object.entries(savedInputs)) {
    const inp = document.getElementById(id);
    if (inp) { inp.value = val; autoGrow(inp); }
  }
  if (focusedId) {
    const inp = document.getElementById(focusedId);
    if (inp) inp.focus({ preventScroll: true });
  }

  _renderArchivedSection();
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

// ═══════ GIT BRANCH AWARENESS ═══════
function _isBranchMain(b) { return !b || b === 'main' || b === 'master' || b === 'dev' || b === 'develop'; }

function _renderBranchBadge(name) {
  const gi = gitInfo[name];
  if (!gi || !gi.branch) return '';
  const isMain = _isBranchMain(gi.branch);
  const cls = gi._conflict ? 'conflict' : isMain ? 'on-main' : 'on-branch';
  const tip = gi._conflict ? 'Another session shares this branch — risk of conflicts' : isMain ? 'On main — click to create a session branch' : 'On feature branch';
  const conflictWarn = gi._conflict ? ' ⚠' : '';
  return `<div class="card-dir"><span class="branch-badge ${cls}" onclick="event.stopPropagation();showBranchPopover('${name}',event)" title="${tip}">⎇ ${esc(gi.branch)}${conflictWarn}</span></div>`;
}

async function _fetchGitBranches(sess) {
  const withDir = (sess || []).filter(s => s.dir);
  if (!withDir.length) return;
  const results = await Promise.allSettled(
    withDir.map(s =>
      fetch(API + '/api/sessions/' + encodeURIComponent(s.name) + '/git')
        .then(r => r.json()).then(d => ({name: s.name, ...d}))
    )
  );
  const newInfo = {};
  for (const r of results) {
    if (r.status === 'fulfilled' && r.value.name) newInfo[r.value.name] = r.value;
  }
  // Detect conflicts: two sessions on same branch in same repo
  const byKey = {};
  for (const [n, gi] of Object.entries(newInfo)) {
    if (!gi.branch || !gi.repo) continue;
    const key = gi.repo + '::' + gi.branch;
    (byKey[key] = byKey[key] || []).push(n);
  }
  for (const names of Object.values(byKey)) {
    if (names.length > 1) names.forEach(n => { if (newInfo[n]) newInfo[n]._conflict = true; });
  }
  if (JSON.stringify(newInfo) !== JSON.stringify(gitInfo)) {
    gitInfo = newInfo;
    render();
  }
}

function showBranchPopover(name, e) {
  e.stopPropagation();
  document.querySelectorAll('.branch-popover').forEach(p => p.remove());
  const gi = gitInfo[name] || {};
  const isMain = _isBranchMain(gi.branch);
  const suggested = 'session/' + name;
  const pop = document.createElement('div');
  pop.className = 'branch-popover';
  pop.onclick = ev => ev.stopPropagation();
  if (isMain) {
    pop.innerHTML = `
      <div style="font-size:0.75rem;color:var(--dim);margin-bottom:8px;font-weight:600;">⎇ Create session branch</div>
      <div style="font-size:0.78rem;color:var(--dim);margin-bottom:8px;">Isolate changes from other sessions on <strong>${esc(gi.branch || 'main')}</strong></div>
      <input class="search-input" id="bp-input-${name}" value="${esc(suggested)}" style="font-size:0.82rem;margin-bottom:8px;">
      <div class="branch-popover-actions">
        <button class="btn primary" style="flex:1;" onclick="doCreateBranch('${name}')">Create &amp; checkout</button>
        <button class="btn" onclick="document.querySelectorAll('.branch-popover').forEach(p=>p.remove())">✕</button>
      </div>`;
  } else {
    pop.innerHTML = `
      <div style="font-size:0.75rem;color:var(--dim);margin-bottom:6px;font-weight:600;">⎇ ${esc(gi.branch)}</div>
      ${gi._conflict ? '<div style="font-size:0.78rem;color:var(--red);margin-bottom:6px;">⚠ Another session shares this branch — conflicts possible</div>' : '<div style="font-size:0.78rem;color:var(--green);margin-bottom:6px;">✓ Isolated on feature branch</div>'}
      <button class="btn" style="width:100%;" onclick="document.querySelectorAll('.branch-popover').forEach(p=>p.remove())">Close</button>`;
  }
  // Append to body to escape card's overflow:hidden
  document.body.appendChild(pop);
  const rect = e.target.getBoundingClientRect();
  const vw = window.innerWidth;
  let left = rect.left;
  if (left + 240 > vw - 8) left = vw - 248;
  pop.style.top = (rect.bottom + 6) + 'px';
  pop.style.left = Math.max(8, left) + 'px';
  setTimeout(() => {
    document.addEventListener('click', function closer() {
      document.querySelectorAll('.branch-popover').forEach(p => p.remove());
      document.removeEventListener('click', closer);
    }, {once: true});
  }, 10);
}

async function doCreateBranch(name) {
  const input = document.getElementById('bp-input-' + name);
  const branch = (input ? input.value : '').trim() || ('session/' + name);
  document.querySelectorAll('.branch-popover').forEach(p => p.remove());
  try {
    const r = await fetch(API + '/api/sessions/' + encodeURIComponent(name) + '/git', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({branch, create: true}),
    });
    const d = await r.json();
    if (d.ok) {
      gitInfo[name] = {...(gitInfo[name] || {}), branch, _conflict: false};
      render();
    } else {
      alert('Branch creation failed: ' + (d.error || 'unknown error'));
    }
  } catch(ex) { alert('Error: ' + ex.message); }
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

function collapseAll() {
  expanded.clear();
  render();
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

function toggleArchived() {
  archivedExpanded = !archivedExpanded;
  _renderArchivedSection();
}

function _renderArchivedSection() {
  const el = document.getElementById('archived-section');
  if (!el) return;
  const archived = sessions.filter(s => s.archived);
  if (!archived.length) { el.innerHTML = ''; return; }
  const chevron = `<span class="archived-chevron${archivedExpanded ? ' open' : ''}">&#x25B6;</span>`;
  let html = `<div class="archived-footer" onclick="toggleArchived()">${chevron} ${archived.length} archived</div>`;
  if (archivedExpanded) {
    html += '<div class="archived-body">';
    archived.forEach(s => {
      const ago = s.last_activity ? timeAgo(s.last_activity) : '';
      const preview = esc(s.preview || s.desc || '');
      html += `<div class="archived-card">
        <div class="archived-card-name">${esc(s.name)}</div>
        ${ago ? `<div class="archived-card-meta">${ago}</div>` : ''}
        ${preview ? `<div class="archived-card-preview">${preview}</div>` : ''}
        <div class="archived-card-actions">
          <button class="archived-wake-btn" onclick="wakeSession('${esc(s.name)}')">Wake</button>
          <button class="archived-del-btn" onclick="deleteSession('${esc(s.name)}')">&#x2715;</button>
        </div>
      </div>`;
    });
    html += '</div>';
  }
  el.innerHTML = html;
}

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
  const r = await apiCall(API + '/api/sessions/' + session + '/config', {
    method: 'PATCH', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ toggle_yolo: true })
  });
  if (r) {
    // Optimistic update: flip the flag locally so render() shows the change immediately
    const s = sessions.find(s => s.name === session);
    if (s) {
      const flag = '--dangerously-skip-permissions';
      s.flags = (s.flags || '').includes(flag)
        ? s.flags.replace(flag, '').trim()
        : ((s.flags || '') + ' ' + flag).trim();
      lastSessionsJSON = '';  // force fetchSessions to re-render
      render();
    }
  }
  fetchSessions();
}

async function toggleAutoContinue(session) {
  closeAllMenus();
  const r = await apiCall(API + '/api/sessions/' + session + '/config', {
    method: 'PATCH', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ toggle_auto_continue: true })
  });
  if (r) {
    // Optimistic update
    const s = sessions.find(s => s.name === session);
    if (s) {
      s.auto_continue = !s.auto_continue;
      lastSessionsJSON = '';  // force fetchSessions to re-render
      render();
    }
  }
  fetchSessions();
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

async function archiveSession(session) {
  closeAllMenus();
  const r = await apiCall(API + '/api/sessions/' + session + '/archive', { method: 'POST' });
  if (r) showToast(session + ' archived');
  await fetchSessions();
}

async function wakeSession(session) {
  const r = await apiCall(API + '/api/sessions/' + session + '/wake', { method: 'POST' });
  if (r) showToast('Waking ' + session + '…');
  // Poll until running
  for (let i = 0; i < 14; i++) {
    await new Promise(res => setTimeout(res, 500));
    await fetchSessions();
    if (sessions.find(s => s.name === session && s.running)) break;
  }
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
  const now = new Date();
  const ts = now.toLocaleTimeString([], {hour: '2-digit', minute: '2-digit', hour12: true});
  const stamped = `[${ts}] ${text}`;
  await apiCall(API + '/api/sessions/' + name + '/send', {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({text: stamped})
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
  const routed = _atRoute(text);
  if (routed) {
    await doSend(routed.target, routed.message);
    showToast('Sent to @' + routed.target);
  } else {
    await doSend(name, _expandAtMentions(text));
  }
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

function openPeek(name, opts) {
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
  const prefillQuery = opts && opts.query ? opts.query : '';
  peekSearchQuery = prefillQuery;
  peekSearchIndex = 0;
  _peekMatches = [];
  lastPeekHTML = '';
  const searchInp = document.getElementById('peek-search');
  if (searchInp) {
    searchInp.value = prefillQuery;
    document.getElementById('peek-search-wrap').classList.toggle('has-value', !!prefillQuery);
  }
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
  _syncPeekOverlayToVisualViewport();
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
  const ov = document.getElementById('peek-overlay');
  ov.classList.remove('active', 'vv-compact');
  ov.style.height = '';
  ov.style.top = '';
  if (peekTimer) { clearInterval(peekTimer); peekTimer = null; }
}

// Keep peek overlay fitted to the visual viewport so it stays visible
// when the user pinches to zoom or the on-screen keyboard appears.
// Only apply inline sizing when visual viewport differs from layout viewport
// (pinch zoom or virtual keyboard). Desktop browser zoom (Cmd+/-) keeps them
// equal and is handled by CSS position:fixed + inset:0.
function _syncPeekOverlayToVisualViewport() {
  const ov = document.getElementById('peek-overlay');
  if (!window.visualViewport || !ov) return;
  const vv = window.visualViewport;
  const constrained = vv.height < window.innerHeight - 1 || vv.offsetTop > 1;
  if (constrained) {
    ov.style.height = vv.height + 'px';
    ov.style.top = vv.offsetTop + 'px';
  } else {
    ov.style.height = '';
    ov.style.top = '';
  }
  // Compact mode: hide chips, shrink padding when viewport is tight
  ov.classList.toggle('vv-compact', constrained && vv.height < window.innerHeight * 0.7);
}
(function() {
  if (!window.visualViewport) return;
  window.visualViewport.addEventListener('resize', () => {
    if (document.getElementById('peek-overlay')?.classList.contains('active')) {
      _syncPeekOverlayToVisualViewport();
    }
  });
  window.visualViewport.addEventListener('scroll', () => {
    if (document.getElementById('peek-overlay')?.classList.contains('active')) {
      _syncPeekOverlayToVisualViewport();
    }
  });
})();

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
      html += `<a href="${esc(match.value)}" target="_blank" rel="noopener noreferrer">${esc(match.value)}</a>`;
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

function applyPeekSearch(keepIndex) {
  const body = document.getElementById('peek-body');
  const countEl = document.getElementById('peek-search-count');
  if (!body) return;
  const q = peekSearchQuery.trim();
  if (!q) {
    body.innerHTML = lastPeekHTML;
    _peekMatches = [];
    peekSearchIndex = 0;
    if (countEl) countEl.textContent = '';
    return;
  }
  // Highlight all matches in text nodes only (not inside tags)
  const escaped = q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const re = new RegExp('(' + escaped + ')', 'gi');
  const parts = lastPeekHTML.split(/(<[^>]+>)/);
  let idx = 0;
  body.innerHTML = parts.map(p => {
    if (p.startsWith('<')) return p;
    return p.replace(re, (match) => `<span class="peek-highlight" data-idx="${idx++}">${esc(match)}</span>`);
  }).join('');
  _peekMatches = Array.from(body.querySelectorAll('.peek-highlight'));
  if (!keepIndex || peekSearchIndex >= _peekMatches.length) peekSearchIndex = 0;
  _peekScrollTo(peekSearchIndex);
  if (countEl) countEl.textContent = _peekMatches.length > 0 ? (peekSearchIndex + 1) + '/' + _peekMatches.length : 'no matches';
}
function _peekScrollTo(i) {
  _peekMatches.forEach((m, j) => m.classList.toggle('current', j === i));
  const cur = _peekMatches[i];
  if (cur) cur.scrollIntoView({ block: 'center', behavior: 'smooth' });
  const countEl = document.getElementById('peek-search-count');
  if (countEl && _peekMatches.length) countEl.textContent = (i + 1) + '/' + _peekMatches.length;
}
function peekSearchNext() {
  if (!_peekMatches.length) return;
  peekSearchIndex = (peekSearchIndex + 1) % _peekMatches.length;
  _peekScrollTo(peekSearchIndex);
}
function peekSearchPrev() {
  if (!_peekMatches.length) return;
  peekSearchIndex = (peekSearchIndex - 1 + _peekMatches.length) % _peekMatches.length;
  _peekScrollTo(peekSearchIndex);
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

// ── Microphone / transcription ──
let _micRecorder = null;
let _micChunks = [];
let _micActive = false;

async function _checkTranscription() {
  if (!window.isSecureContext || !navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) return;
  try {
    const r = await fetch(API + '/api/transcribe');
    const d = await r.json();
    if (d.available) document.getElementById('peek-mic-btn').style.display = '';
  } catch(e) {}
}

function _bufToB64(buf) {
  const bytes = new Uint8Array(buf);
  let bin = '';
  const chunk = 8192;
  for (let i = 0; i < bytes.length; i += chunk)
    bin += String.fromCharCode(...bytes.subarray(i, i + chunk));
  return btoa(bin);
}

async function toggleMic() {
  const btn = document.getElementById('peek-mic-btn');
  if (_micActive) {
    _micRecorder && _micRecorder.stop();
    return;
  }
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    _micChunks = [];
    _micActive = true;
    btn.textContent = '\u23F9';
    btn.classList.add('mic-active');
    _micRecorder = new MediaRecorder(stream);
    _micRecorder.ondataavailable = e => { if (e.data.size > 0) _micChunks.push(e.data); };
    _micRecorder.onstop = async () => {
      _micActive = false;
      btn.classList.remove('mic-active');
      stream.getTracks().forEach(t => t.stop());
      btn.textContent = '\u23F3';
      try {
        const blob = new Blob(_micChunks, { type: _micRecorder.mimeType || 'audio/webm' });
        const b64 = _bufToB64(await blob.arrayBuffer());
        const r = await fetch(API + '/api/transcribe', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ data: b64, mime: blob.type }),
        });
        const d = await r.json();
        if (d.text) {
          const inp = document.getElementById('peek-cmd-input');
          inp.value = (inp.value ? inp.value + ' ' : '') + d.text;
          autoGrow(inp);
          inp.focus();
        } else {
          const msg = 'Transcription failed: ' + (d.error || 'unknown');
          showToast(msg);
        }
      } catch(e) {
        showToast('Transcription error: ' + e.message);
      } finally {
        btn.textContent = '\u{1F3A4}';
      }
    };
    _micRecorder.start();
  } catch(e) {
    _micActive = false;
    btn.classList.remove('mic-active');
    btn.textContent = '\u{1F3A4}';
    if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
      const isPwa = window.matchMedia('(display-mode: standalone)').matches || navigator.standalone;
      if (isPwa) {
        showToast('Mic unavailable in PWA — open in Safari and try again, or update to iOS 16.4+');
      } else {
        showToast('Mic unavailable — requires HTTPS or localhost');
      }
    } else if (e.name === 'NotAllowedError' || e.name === 'PermissionDeniedError') {
      showToast('Microphone blocked — System Settings → Privacy & Security → Microphone → allow Safari');
    } else if (e.name === 'NotFoundError' || e.name === 'DevicesNotFoundError') {
      showToast('No microphone found');
    } else {
      showToast('Mic error: ' + (e.message || e.name || 'unknown'));
    }
  }
}

let _slashAcSuppressNext = false;
function handlePeekPaste(e) {
  _slashAcSuppressNext = true;  // suppress slash dropdown for pasted text
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

  const routed = files.length === 0 && _atRoute(message);
  if (routed) {
    await doSend(routed.target, routed.message);
    showToast('Sent to @' + routed.target);
  } else {
    message = _expandAtMentions(message);
    await doSend(peekSession, message);
  }
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

// ── @mention routing ──
// If a message starts with @sessionname, auto-route to that session.
// Returns {target, message} where target may differ from the sending session.
function _atRoute(text) {
  const known = (sessions || []).map(s => s.name);
  const m = text.match(/^@([\w][\w.-]*)([\s\S]*)$/);
  if (m && known.includes(m[1])) {
    const target = m[1];
    const rest = m[2].trim();
    return { target, message: rest || text };
  }
  return null;
}

// ── @mention → HTTP API hint ──
// When a message contains @session-name mentions, append a compact API hint
// so Claude knows to use the HTTP API for delegation (not tmux directly).
function _expandAtMentions(text) {
  const known = (window._sessions || []).map(s => s.name);
  // find all @word tokens that match a known session name
  const mentioned = [];
  const re = /@([\w][\w.-]*)/g;
  let m;
  while ((m = re.exec(text)) !== null) {
    const n = m[1];
    if (known.includes(n) && !mentioned.includes(n)) mentioned.push(n);
  }
  if (mentioned.length === 0) return text;
  const base = '$AMUX_URL';
  const hints = mentioned.map(n =>
    `  @${n} → POST ${base}/api/sessions/${n}/send  {"text":"<msg>"}`
  ).join('\n');
  return text + '\n\n[amux: use HTTP API to reach @-mentioned sessions — never tmux directly]\n' + hints;
}

// ── @mention helpers ──
// Returns {q, idx} when cursor is inside an @word, else null
function _atQuery(inp) {
  const before = inp.value.slice(0, inp.selectionStart);
  const atIdx = before.lastIndexOf('@');
  if (atIdx === -1) return null;
  const fragment = before.slice(atIdx + 1);
  if (/\s/.test(fragment)) return null; // space after @ means mention is complete
  return { q: fragment.toLowerCase(), idx: atIdx };
}

// Populate dropdown with @session matches; returns true if @ mode active
function _atRender(inp, el, pickCall) {
  const at = _atQuery(inp);
  if (at === null) return false;
  const matches = (sessions || []).filter(s => s.name.toLowerCase().startsWith(at.q)).slice(0, 8);
  if (!matches.length) { el.classList.remove('open'); return true; }
  el.innerHTML = matches.map((s, i) =>
    `<div class="ac-item at-item" onmousedown="${pickCall}(${i})">` +
    `<span class="at-at">@</span>${esc(s.name)}` +
    `<span class="ac-desc">${s.running ? '● running' : '○ stopped'}</span></div>`
  ).join('');
  el._atItems = matches;
  el.classList.add('open');
  return true;
}

// Insert @name at the trigger position
function _atInsert(inp, el) {
  const at = _atQuery(inp);
  const items = el._atItems;
  const sel = el._atSel >= 0 ? el._atSel : 0;
  if (!at || !items || !items[sel]) return;
  const name = items[sel].name;
  const val = inp.value;
  inp.value = val.slice(0, at.idx) + '@' + name + ' ' + val.slice(inp.selectionStart);
  const newPos = at.idx + name.length + 2;
  inp.selectionStart = inp.selectionEnd = newPos;
  el._atItems = null; el._atSel = -1;
  el.classList.remove('open');
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
  { cmd: '/amux', desc: 'Interact with amux — board, memory, sessions' },
  { cmd: '/amux-board', desc: 'Add a task or note to the board' },
  { cmd: '/pw-test', desc: 'Run Playwright UI tests or investigate issues' },
  { cmd: '/playwright-auth', desc: 'Capture/sync browser auth profiles' },
];
let slashAcItems = [];
let slashAcSelected = -1;

function slashAcUpdate() {
  const inp = document.getElementById('peek-cmd-input');
  const el = document.getElementById('slash-ac-list');
  if (_slashAcSuppressNext) { _slashAcSuppressNext = false; el.classList.remove('open'); slashAcItems = []; return; }
  const val = inp.value;
  // @ mention takes priority (cursor-aware)
  if (_atRender(inp, el, 'slashAcPick')) { slashAcItems = []; slashAcSelected = -1; return; }
  el._atItems = null; el._atSel = -1;
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
  const el = document.getElementById('slash-ac-list');
  if (el._atItems) {
    el._atSel = i;
    _atInsert(inp, el);
    // Auto-send if the message has content beyond just the @mention itself
    const val = inp.value.trim();
    const onlyMention = /^@[\w][\w.-]*\s*$/.test(val);
    if (!onlyMention) { setTimeout(sendPeekCmd, 0); return; }
  } else {
    inp.value = slashAcItems[i].cmd;
    el.classList.remove('open');
    slashAcItems = [];
  }
  inp.focus({ preventScroll: true });
}

function slashAcKeydown(e) {
  const inp = document.getElementById('peek-cmd-input');
  const el = document.getElementById('slash-ac-list');
  if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); sendPeekCmd(); return; }
  if (!el.classList.contains('open')) {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendPeekCmd(); return; }
    if (e.key === 'ArrowUp' && inp.selectionStart === 0) { e.preventDefault(); cmdHistoryUp(inp); return; }
    if (e.key === 'ArrowDown' && _cmdHistoryIdx !== -1) { e.preventDefault(); cmdHistoryDown(inp); return; }
    return;
  }
  const atMode = !!el._atItems;
  const itemLen = atMode ? el._atItems.length : slashAcItems.length;
  const getSel = () => atMode ? (el._atSel >= 0 ? el._atSel : -1) : slashAcSelected;
  const setSel = v => { if (atMode) el._atSel = v; else slashAcSelected = v; };
  if (e.key === 'ArrowUp') {
    e.preventDefault();
    setSel(getSel() <= 0 ? itemLen - 1 : getSel() - 1);
    slashAcHighlight();
  } else if (e.key === 'ArrowDown') {
    e.preventDefault();
    setSel(getSel() >= itemLen - 1 ? 0 : getSel() + 1);
    slashAcHighlight();
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (getSel() >= 0) slashAcPick(getSel());
    else if (atMode) slashAcPick(0);
    else el.classList.remove('open');
  } else if (e.key === 'Tab') {
    e.preventDefault();
    slashAcPick(getSel() >= 0 ? getSel() : 0);
  } else if (e.key === 'Escape') {
    el.classList.remove('open'); el._atItems = null; el._atSel = -1;
  }
}

function slashAcHighlight() {
  const el = document.getElementById('slash-ac-list');
  const sel = el._atItems ? el._atSel : slashAcSelected;
  const items = el.querySelectorAll('.ac-item');
  items.forEach((item, i) => item.classList.toggle('selected', i === sel));
  if (items[sel]) items[sel].scrollIntoView({ block: 'nearest' });
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
    if (prev) { prev.classList.remove('open'); prev._atItems = null; }
  }
  _cardAcName = name;
  const val = inp.value;
  // @ mention takes priority (cursor-aware)
  if (_atRender(inp, el, 'cardAtPick')) { _cardAcItems = []; _cardAcSelected = -1; return; }
  el._atItems = null; el._atSel = -1;
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

function cardAtPick(i) {
  const name = _cardAcName;
  const inp = document.getElementById('input-' + name);
  const el = document.getElementById('card-ac-' + name);
  if (!inp || !el) return;
  el._atSel = i;
  _atInsert(inp, el);
  inp.focus({ preventScroll: true });
}

function cardSlashAcPick(name, i) {
  const inp = document.getElementById('input-' + name);
  const el = document.getElementById('card-ac-' + name);
  if (el && el._atItems) {
    el._atSel = i;
    _atInsert(inp, el);
  } else {
    inp.value = _cardAcItems[i].cmd;
    if (el) el.classList.remove('open');
    _cardAcItems = [];
  }
  inp.focus({ preventScroll: true });
}

function cardSlashAcKeydown(name, e) {
  const inp = document.getElementById('input-' + name);
  const el = document.getElementById('card-ac-' + name);
  if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) { e.preventDefault(); sendFromInput(name); return; }
  if (!el || !el.classList.contains('open')) {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); sendFromInput(name); return; }
    if (e.key === 'ArrowUp' && inp && inp.selectionStart === 0) { e.preventDefault(); cmdHistoryUp(inp); return; }
    if (e.key === 'ArrowDown' && _cmdHistoryIdx !== -1) { e.preventDefault(); if (inp) cmdHistoryDown(inp); return; }
    return;
  }
  const atMode = !!el._atItems;
  const itemLen = atMode ? el._atItems.length : _cardAcItems.length;
  const getSel = () => atMode ? (el._atSel >= 0 ? el._atSel : -1) : _cardAcSelected;
  const setSel = v => { if (atMode) el._atSel = v; else _cardAcSelected = v; };
  if (e.key === 'ArrowUp') {
    e.preventDefault();
    setSel(getSel() <= 0 ? itemLen - 1 : getSel() - 1);
    cardSlashAcHighlight(name);
  } else if (e.key === 'ArrowDown') {
    e.preventDefault();
    setSel(getSel() >= itemLen - 1 ? 0 : getSel() + 1);
    cardSlashAcHighlight(name);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (getSel() >= 0) cardSlashAcPick(name, getSel());
    else if (atMode) cardSlashAcPick(name, 0);
    else el.classList.remove('open');
  } else if (e.key === 'Tab') {
    e.preventDefault();
    cardSlashAcPick(name, getSel() >= 0 ? getSel() : 0);
  } else if (e.key === 'Escape') {
    el.classList.remove('open'); el._atItems = null; el._atSel = -1;
  }
}

function cardSlashAcHighlight(name) {
  const el = document.getElementById('card-ac-' + name);
  const sel = el && el._atItems ? el._atSel : _cardAcSelected;
  const items = el ? el.querySelectorAll('.ac-item') : [];
  items.forEach((item, i) => item.classList.toggle('selected', i === sel));
  if (items[sel]) items[sel].scrollIntoView({ block: 'nearest' });
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
  _logMatches = {};
  document.getElementById('search-wrap').classList.remove('has-value');
  render();
}

function onSearchInput() {
  searchQuery = document.getElementById('search-input').value;
  document.getElementById('search-wrap').classList.toggle('has-value', !!searchQuery);
  render();
  if (logSearchMode) {
    clearTimeout(_logSearchTimer);
    _logSearchTimer = setTimeout(_runLogSearch, 400);
  }
}

function toggleLogSearch() {
  logSearchMode = !logSearchMode;
  const btn = document.getElementById('log-search-btn');
  btn.classList.toggle('active', logSearchMode);
  document.getElementById('search-input').placeholder = logSearchMode
    ? 'Search session logs...' : 'Search sessions...';
  _logMatches = {};
  if (logSearchMode && searchQuery) {
    clearTimeout(_logSearchTimer);
    _logSearchTimer = setTimeout(_runLogSearch, 100);
  }
  render();
}

async function _runLogSearch() {
  const q = searchQuery.toLowerCase().trim();
  if (!q || !logSearchMode) { _logMatches = {}; render(); return; }
  // Cancel any in-flight search
  if (_logSearchAbort) _logSearchAbort.abort();
  _logSearchAbort = new AbortController();
  const sig = _logSearchAbort.signal;
  const sessionList = sessions || [];
  const results = await Promise.allSettled(
    sessionList.map(s =>
      fetch(API + '/api/sessions/' + encodeURIComponent(s.name) + '/peek?lines=500', { signal: sig })
        .then(r => r.json())
        .then(data => {
          const output = data.output || '';
          const lines = output.split('\n');
          const hits = [];
          lines.forEach((l, i) => { if (l.toLowerCase().includes(q)) hits.push({ line: i + 1, text: l.replace(/\x1b\[[0-9;]*m/g, '').trim() }); });
          if (!hits.length) return null;
          return { name: s.name, hits };
        })
        .catch(() => null)
    )
  );
  if (sig.aborted) return;
  _logMatches = {};
  results.forEach(r => { if (r.status === 'fulfilled' && r.value) _logMatches[r.value.name] = r.value.hits; });
  render();
}
function clearPeekSearch() {
  const inp = document.getElementById('peek-search');
  inp.value = '';
  peekSearchQuery = '';
  peekSearchIndex = 0;
  _peekMatches = [];
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
  if (data.is_video) {
    body.className = 'file-overlay-body file-video';
    const rawUrl = API + '/api/file/raw?path=' + encodeURIComponent(data.path);
    const fname = data.path.split('/').pop();
    const sizeMB = data.size ? (data.size / 1048576).toFixed(1) + ' MB' : '';
    const dateStr = data.modified ? new Date(data.modified * 1000).toLocaleString() : '';
    const trackHtml = data.srt
      ? `<track kind="captions" label="Captions" src="${API}/api/file/vtt?path=${encodeURIComponent(data.srt)}" default>`
      : '';
    body.innerHTML = `
      <video class="file-video-player" controls src="${rawUrl}">${trackHtml}</video>
      <div class="file-video-meta">
        ${sizeMB ? `<span>${sizeMB}</span>` : ''}
        ${dateStr ? `<span>${dateStr}</span>` : ''}
        ${data.srt ? '<span>📝 Captions</span>' : ''}
        ${data.profile && data.profile !== 'default' ? `<span>👤 ${esc(data.profile)}</span>` : ''}
        ${data.task ? `<span title="${esc(data.task)}" style="max-width:220px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">🎯 ${esc(data.task)}</span>` : ''}
        <a href="${rawUrl}" download="${fname}" style="margin-left:auto;color:var(--accent);text-decoration:none;">⬇ Download</a>
      </div>`;
    return;
  }
  if (data.is_audio) {
    body.className = 'file-overlay-body file-image';
    const rawUrl = API + '/api/file/raw?path=' + encodeURIComponent(data.path) + (peekSessionDir ? '&cwd=' + encodeURIComponent(peekSessionDir) : '');
    const fname = data.path.split('/').pop();
    const sizeMB = data.size ? (data.size / 1048576).toFixed(1) + ' MB' : '';
    body.innerHTML = `<div style="display:flex;flex-direction:column;align-items:center;gap:16px;padding:32px;"><div style="font-size:2.5rem;">🎵</div><div style="font-size:0.95rem;color:var(--muted);">${fname}${sizeMB ? ' · ' + sizeMB : ''}</div><audio controls style="width:100%;max-width:480px;" src="${rawUrl}"></audio></div>`;
    return;
  }
  if (data.is_binary) {
    body.className = 'file-overlay-body file-image';
    const fname = data.path.split('/').pop();
    const sizeMB = data.size ? (data.size / 1048576).toFixed(1) + ' MB' : '';
    const rawUrl = API + '/api/file/raw?path=' + encodeURIComponent(data.path) + (peekSessionDir ? '&cwd=' + encodeURIComponent(peekSessionDir) : '');
    body.innerHTML = `<div style="display:flex;flex-direction:column;align-items:center;gap:12px;padding:32px;"><div style="font-size:2.5rem;">📦</div><div style="font-size:0.95rem;color:var(--muted);">${fname}${sizeMB ? ' · ' + sizeMB : ''}${data.ext ? ' · ' + data.ext : ''}</div><a href="${rawUrl}" download="${fname}" style="padding:8px 18px;background:var(--green);color:#fff;border-radius:6px;font-size:0.85rem;font-weight:600;text-decoration:none;">Download</a></div>`;
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
  document.getElementById('file-download-btn').style.display = 'none';
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
    const isTextFile = !data.is_image && !data.is_pdf && !data.is_video && !data.is_audio && !data.is_binary;
    if (isTextFile) {
      document.getElementById('file-view-tabs').style.display = '';
    }
    // Show download button for binary/audio/video/image (not text — they use copy)
    if (!isTextFile) {
      const dlBtn = document.getElementById('file-download-btn');
      const rawUrl = API + '/api/file/raw?path=' + encodeURIComponent(data.path);
      dlBtn.href = rawUrl;
      dlBtn.download = data.path.split('/').pop();
      dlBtn.style.display = '';
    }
    _renderFileBody(data, _fileViewMode);
    // Update cache
    _idb.setFile(path, { type: 'file', data });
  } catch(e) {
    // Offline: try IDB cache
    const cached = await _idb.getFile(path);
    if (cached && cached.type === 'file') {
      _fileData = cached.data;
      const age = Math.round((Date.now() - cached.ts) / 60000);
      const ageStr = age < 60 ? age + 'm ago' : Math.round(age/60) + 'h ago';
      document.getElementById('file-title').textContent = path.split('/').pop() + ' \u2022 cached ' + ageStr;
      const cachedIsText = !cached.data.is_image && !cached.data.is_pdf && !cached.data.is_video && !cached.data.is_audio && !cached.data.is_binary;
      if (cachedIsText) {
        document.getElementById('file-view-tabs').style.display = '';
      }
      _renderFileBody(cached.data, _fileViewMode);
      return;
    }
    document.getElementById('file-body').textContent = 'Failed to load file.';
  }
}

function closeFilePreview() {
  const v = document.querySelector('#file-body video');
  if (v) { v.pause(); v.src = ''; }
  document.getElementById('file-overlay').classList.remove('active');
  document.getElementById('file-download-btn').style.display = 'none';
  _fileData = null;
}

async function copyFileContent() {
  if (!_fileData || !_fileData.content) return;
  await navigator.clipboard.writeText(_fileData.content);
  const btn = document.getElementById('file-tab-copy');
  btn.textContent = 'Copied!';
  setTimeout(() => { btn.textContent = 'Copy'; }, 1500);
}

// ═══════ REMOTE BROWSER ═══════
let _rbActive = false;
let _rbLoading = false;
let _rbCurrentProfile = 'default';
let _rbLoginMode = false;
let _rbRecordEnabled = false;  // whether to record on next launch

async function _rbLoadProfiles() {
  try {
    const r = await fetch(API + '/api/browser/profiles');
    const d = await r.json();
    const sel = document.getElementById('rb-profile');
    sel.innerHTML = '';
    for (const p of (d.profiles || [])) {
      const opt = document.createElement('option');
      opt.value = p.name;
      opt.textContent = p.name;
      opt.dataset.hasState = p.has_state ? '1' : '';
      sel.appendChild(opt);
    }
    // Preserve an already-selected profile; only fall back to server's active on initial load
    const serverActive = d.active || 'default';
    const validNames = (d.profiles || []).map(p => p.name);
    if (!_rbCurrentProfile || !validNames.includes(_rbCurrentProfile)) {
      _rbCurrentProfile = serverActive;
    }
    sel.value = _rbCurrentProfile;
    document.getElementById('rb-del-profile').style.display = _rbCurrentProfile !== 'default' ? '' : 'none';
    _rbUpdateProfileState(d.profiles || []);
  } catch(e) {}
}

function _rbUpdateProfileState(profiles) {
  const current = profiles.find(p => p.name === _rbCurrentProfile);
  const dot = document.getElementById('rb-profile-state');
  if (dot) {
    dot.textContent = current?.has_state ? '🟢' : '⚪';
    dot.title = current?.has_state ? 'Logged in — credentials saved' : 'No saved login for this profile';
  }
}

function _rbShowNewProfile() {
  document.getElementById('rb-new-btn').style.display = 'none';
  const form = document.getElementById('rb-new-profile-form');
  form.style.display = 'flex';
  document.getElementById('rb-new-profile-name').value = '';
  document.getElementById('rb-new-profile-name').focus();
}

function _rbHideNewProfile() {
  document.getElementById('rb-new-btn').style.display = '';
  document.getElementById('rb-new-profile-form').style.display = 'none';
}

async function _rbCreateProfile() {
  const inp = document.getElementById('rb-new-profile-name');
  const clean = inp.value.trim().replace(/[^a-zA-Z0-9_-]/g, '-');
  if (!clean || clean === 'default') return;
  _rbHideNewProfile();
  try {
    const r = await fetch(API + '/api/browser/profiles', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ name: clean }),
    });
    if (r.ok) {
      await _rbLoadProfiles();
      document.getElementById('rb-profile').value = clean;
      _rbSwitchProfile(clean);
    }
  } catch(e) {}
}

async function _rbDeleteProfile() {
  const sel = document.getElementById('rb-profile');
  const name = sel.value;
  if (name === 'default') return;
  if (!confirm('Delete profile "' + name + '"? All saved cookies and sessions will be lost.')) return;
  try {
    await fetch(API + '/api/browser/profiles/' + encodeURIComponent(name), { method: 'DELETE' });
    if (_rbActive) await _rbStop();
    await _rbLoadProfiles();
  } catch(e) {}
}

async function _rbSwitchProfile(name) {
  _rbCurrentProfile = name;
  document.getElementById('rb-del-profile').style.display = name !== 'default' ? '' : 'none';
  document.getElementById('rb-profile-status').textContent = '';
  await _rbLoadProfiles();
  // If browser is running, restart with new profile
  if (_rbActive) {
    await _rbStop();
    await _rbStart();
  }
}

async function _rbStart(url) {
  const status = document.getElementById('rb-status');
  status.textContent = 'Starting...';
  _rbLoading = true;
  try {
    const body = { url: url || '' };
    if (_rbCurrentProfile && _rbCurrentProfile !== 'default') body.profile = _rbCurrentProfile;
    if (_rbRecordEnabled) body.record = true;
    const r = await fetch(API + '/api/browser/start', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(body),
    });
    const d = await r.json();
    if (!d.ok) { status.textContent = d.error || 'Failed'; _rbLoading = false; return; }
    _rbActive = true;
    if (d.profile) {
      _rbCurrentProfile = d.profile;
      document.getElementById('rb-profile').value = d.profile;
    }
    const recLabel = _rbRecordEnabled ? ' \u25CF REC' : '';
    document.getElementById('rb-profile-status').textContent = 'Active: ' + _rbCurrentProfile + recLabel;
    if (_rbRecordEnabled) document.getElementById('rb-profile-status').style.color = 'var(--red)';
    status.textContent = '';
    document.getElementById('rb-video-banner').style.display = 'none';
    document.getElementById('rb-placeholder').style.display = 'none';
    document.getElementById('rb-screen').style.display = 'block';
    await _rbRefresh();
  } catch(e) {
    status.textContent = 'Error: ' + e.message;
  }
  _rbLoading = false;
}

async function _rbStop() {
  let videoReady = false;
  try {
    const r = await fetch(API + '/api/browser/stop', { method: 'POST' });
    const d = await r.json().catch(() => ({}));
    videoReady = !!d.videoReady;
  } catch(e) {}
  _rbActive = false;
  _rbLoginMode = false;
  document.getElementById('rb-screen').style.display = 'none';
  document.getElementById('rb-screen').src = '';
  document.getElementById('rb-login-panel').style.display = 'none';
  document.getElementById('rb-placeholder').style.display = '';
  document.getElementById('rb-url').value = '';
  document.getElementById('rb-status').textContent = '';
  document.getElementById('rb-tab-bar').style.display = 'none';
  document.getElementById('rb-tab-bar').innerHTML = '';
  const ps = document.getElementById('rb-profile-status');
  ps.textContent = '';
  ps.style.color = '';
  _rbSetRecord(_rbRecordEnabled); // re-sync button state
  if (videoReady) {
    const banner = document.getElementById('rb-video-banner');
    document.getElementById('rb-video-link').href = API + '/api/browser/video?t=' + Date.now();
    banner.style.display = 'flex';
  }
}

function _rbToggleRecord() {
  _rbSetRecord(!_rbRecordEnabled);
}

function _rbSetRecord(enabled) {
  _rbRecordEnabled = enabled;
  const btn = document.getElementById('rb-rec-btn');
  if (!btn) return;
  if (enabled) {
    btn.style.color = 'var(--red)';
    btn.style.fontWeight = '700';
    btn.title = 'Recording ON — will record on next launch (click to disable)';
  } else {
    btn.style.color = '';
    btn.style.fontWeight = '';
    btn.title = 'Toggle video recording (takes effect on next launch)';
  }
}

function _rbShowLoginUrl() {
  document.getElementById('rb-login-btn').style.display = 'none';
  const form = document.getElementById('rb-login-url-form');
  form.style.display = 'flex';
  const inp = document.getElementById('rb-login-url');
  inp.value = '';
  inp.focus();
}

function _rbHideLoginUrl() {
  document.getElementById('rb-login-btn').style.display = '';
  document.getElementById('rb-login-url-form').style.display = 'none';
}

async function _rbDoLogin() {
  const url = document.getElementById('rb-login-url').value.trim();
  _rbHideLoginUrl();
  if (_rbActive) await _rbStop();
  const status = document.getElementById('rb-status');
  status.textContent = 'Opening...';
  try {
    const body = { headed: true };
    if (_rbCurrentProfile && _rbCurrentProfile !== 'default') body.profile = _rbCurrentProfile;
    if (url) body.url = url;
    const r = await fetch(API + '/api/browser/start', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(body),
    });
    const d = await r.json();
    if (!d.ok) { status.textContent = d.error || 'Failed'; return; }
    _rbActive = true;
    _rbLoginMode = true;
    if (d.profile) {
      _rbCurrentProfile = d.profile;
      document.getElementById('rb-profile').value = d.profile;
    }
    document.getElementById('rb-placeholder').style.display = 'none';
    document.getElementById('rb-screen').style.display = 'none';
    document.getElementById('rb-login-panel').style.display = '';
    document.getElementById('rb-profile-status').textContent = 'Login window open — navigate and log in, then click Done';
    status.textContent = '';
  } catch(e) {
    status.textContent = 'Error: ' + e.message;
  }
}

async function _rbLoginDone() {
  await _rbStop();
  await _rbLoadProfiles();  // refresh state dot
  const ps = document.getElementById('rb-profile-status');
  ps.textContent = 'Credentials saved for ' + _rbCurrentProfile;
  setTimeout(() => { if (ps.textContent.startsWith('Credentials')) ps.textContent = ''; }, 3000);
}

async function openRecordingsDir() {
  try {
    const r = await fetch(API + '/api/recordings');
    const d = await r.json();
    openExplore(d.dir || null, null);
  } catch(e) { openExplore(null, null); }
}

// ── Browser agent ──
let _rbAgentRunning = false;
let _rbAgentPollTimer = null;

async function _rbRunAgent() {
  const input = document.getElementById('rb-agent-input');
  const task = input.value.trim();
  if (!task) { input.focus(); return; }
  if (_rbAgentRunning) return;

  // Stop any active manual browser first
  if (_rbActive) await _rbStop();

  _rbAgentRunning = true;
  document.getElementById('rb-agent-run').style.display = 'none';
  document.getElementById('rb-agent-stop').style.display = '';
  document.getElementById('rb-agent-status').textContent = 'Starting…';
  document.getElementById('rb-video-banner').style.display = 'none';
  document.getElementById('rb-placeholder').style.display = 'none';
  document.getElementById('rb-screen').style.display = 'block';

  try {
    const body = { task };
    if (_rbCurrentProfile && _rbCurrentProfile !== 'default') body.profile = _rbCurrentProfile;
    const r = await fetch(API + '/api/browser/agent', {
      method: 'POST', headers: {'Content-Type':'application/json'},
      body: JSON.stringify(body),
    });
    const d = await r.json();
    if (!d.ok) { _rbAgentDone(d.error || 'Failed to start'); return; }
    // Start polling
    _rbAgentPollTimer = setInterval(_rbPollAgent, 1500);
  } catch(e) {
    _rbAgentDone('Error: ' + e.message);
  }
}

async function _rbPollAgent() {
  try {
    const r = await fetch(API + '/api/browser/agent/status');
    const d = await r.json();
    document.getElementById('rb-agent-status').textContent = d.action || '';
    // Refresh screenshot while agent is doing things
    if (d.running) {
      await _rbRefresh().catch(() => {});
    }
    if (!d.running) {
      clearInterval(_rbAgentPollTimer);
      _rbAgentPollTimer = null;
      // Final screenshot
      await _rbRefresh().catch(() => {});
      if (d.video_ready) {
        const banner = document.getElementById('rb-video-banner');
        document.getElementById('rb-video-link').href = API + '/api/browser/video?t=' + Date.now();
        banner.style.display = 'flex';
      }
      _rbAgentDone(d.error || (d.video_ready ? 'Done — video ready' : 'Done'));
    }
  } catch(e) {}
}

function _rbAgentDone(msg) {
  _rbAgentRunning = false;
  document.getElementById('rb-agent-run').style.display = '';
  document.getElementById('rb-agent-stop').style.display = 'none';
  document.getElementById('rb-agent-status').textContent = msg || '';
  // Browser is already stopped by server — reset local state
  _rbActive = false;
}

async function _rbStopAgent() {
  try { await fetch(API + '/api/browser/agent/stop', { method: 'POST' }); } catch(e) {}
  clearInterval(_rbAgentPollTimer);
  _rbAgentPollTimer = null;
  _rbAgentDone('Stopped');
  document.getElementById('rb-placeholder').style.display = '';
  document.getElementById('rb-screen').style.display = 'none';
  document.getElementById('rb-screen').src = '';
}

function _rbRenderTabs(tabs, activeTab) {
  const bar = document.getElementById('rb-tab-bar');
  if (!bar) return;
  if (!tabs || tabs.length <= 1) { bar.style.display = 'none'; return; }
  bar.style.display = 'flex';
  bar.style.alignItems = 'stretch';
  bar.style.gap = '2px';
  bar.style.padding = '4px 8px 0';
  bar.innerHTML = tabs.map((t, i) => {
    const active = i === activeTab;
    const label = t.title || t.url || ('Tab ' + (i+1));
    const short = label.length > 22 ? label.substring(0, 20) + '…' : label;
    return `<div style="display:inline-flex;align-items:center;gap:4px;padding:4px 8px 4px 10px;font-size:0.7rem;cursor:pointer;border-radius:6px 6px 0 0;border:1px solid ${active ? 'var(--border)' : 'transparent'};border-bottom:${active ? '1px solid var(--bg)' : 'none'};background:${active ? 'var(--bg)' : 'var(--card-bg)'};color:${active ? 'var(--fg)' : 'var(--dim)'};white-space:nowrap;margin-bottom:-1px;"
      onclick="_rbSwitchTab(${i})" title="${t.url || ''}">
      <span>${short}</span>
      <span onclick="event.stopPropagation();_rbCloseTab(${i})" style="margin-left:2px;opacity:0.5;font-size:0.65rem;padding:0 2px;border-radius:3px;" onmouseenter="this.style.opacity='1'" onmouseleave="this.style.opacity='0.5'">✕</span>
    </div>`;
  }).join('');
}

async function _rbSwitchTab(index) {
  if (!_rbActive) return;
  const status = document.getElementById('rb-status');
  status.textContent = 'Switching...';
  try {
    const r = await fetch(API + '/api/browser/action', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ action: 'switch_tab', index }),
    });
    const d = await r.json();
    if (d.url) document.getElementById('rb-url').value = d.url;
    if (d.title) status.textContent = d.title;
    if (d.data) {
      const img = document.getElementById('rb-screen');
      const blob = new Blob([Uint8Array.from(atob(d.data), c => c.charCodeAt(0))], {type:'image/jpeg'});
      const old = img.src;
      img.src = URL.createObjectURL(blob);
      if (old && old.startsWith('blob:')) URL.revokeObjectURL(old);
    }
    _rbRenderTabs(d.tabs, d.activeTab);
  } catch(e) { status.textContent = 'Error'; }
}

async function _rbCloseTab(index) {
  if (!_rbActive) return;
  try {
    const r = await fetch(API + '/api/browser/action', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ action: 'close_tab', index }),
    });
    const d = await r.json();
    if (d.url) document.getElementById('rb-url').value = d.url;
    if (d.title) document.getElementById('rb-status').textContent = d.title;
    if (d.data) {
      const img = document.getElementById('rb-screen');
      const blob = new Blob([Uint8Array.from(atob(d.data), c => c.charCodeAt(0))], {type:'image/jpeg'});
      const old = img.src;
      img.src = URL.createObjectURL(blob);
      if (old && old.startsWith('blob:')) URL.revokeObjectURL(old);
    }
    _rbRenderTabs(d.tabs, d.activeTab);
  } catch(e) {}
}

async function _rbRefresh() {
  if (!_rbActive) return;
  const img = document.getElementById('rb-screen');
  const status = document.getElementById('rb-status');
  try {
    const r = await fetch(API + '/api/browser/screenshot?t=' + Date.now());
    if (!r.ok) { status.textContent = 'Screenshot failed'; return; }
    const blob = await r.blob();
    const old = img.src;
    img.src = URL.createObjectURL(blob);
    if (old && old.startsWith('blob:')) URL.revokeObjectURL(old);
    // Update URL bar with current page URL
    const info = r.headers.get('X-Page-Url');
    if (info) document.getElementById('rb-url').value = info;
    const title = r.headers.get('X-Page-Title');
    if (title) status.textContent = decodeURIComponent(title);
    // Render tab bar
    const tabsHdr = r.headers.get('X-Tabs');
    const activeHdr = r.headers.get('X-Active-Tab');
    if (tabsHdr) {
      try { _rbRenderTabs(JSON.parse(tabsHdr), parseInt(activeHdr || '0', 10)); } catch(e) {}
    }
  } catch(e) {
    status.textContent = 'Offline';
  }
}

async function _rbNavigate(url) {
  if (!url) return;
  if (!/^https?:\/\//i.test(url)) url = 'https://' + url;
  document.getElementById('rb-url').value = url;
  if (!_rbActive) { await _rbStart(url); return; }
  const status = document.getElementById('rb-status');
  status.textContent = 'Loading...';
  try {
    const r = await fetch(API + '/api/browser/navigate', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ url }),
    });
    const d = await r.json();
    if (d.url) document.getElementById('rb-url').value = d.url;
    if (d.title) status.textContent = d.title;
  } catch(e) { status.textContent = 'Error'; }
  await _rbRefresh();
}

async function _rbCmd(action) {
  if (!_rbActive) return;
  const status = document.getElementById('rb-status');
  try {
    await fetch(API + '/api/browser/action', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ action }),
    });
  } catch(e) {}
  await _rbRefresh();
}

function _rbClick(event, rightClick) {
  if (!_rbActive) return;
  const img = event.target;
  const rect = img.getBoundingClientRect();
  // Scale click coordinates from displayed size to actual viewport size (1280x800)
  const scaleX = img.naturalWidth / rect.width;
  const scaleY = img.naturalHeight / rect.height;
  const x = Math.round((event.clientX - rect.left) * scaleX);
  const y = Math.round((event.clientY - rect.top) * scaleY);
  const status = document.getElementById('rb-status');
  status.textContent = 'Click ' + x + ',' + y + '...';
  fetch(API + '/api/browser/action', {
    method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ action: 'click', x, y, button: rightClick ? 'right' : 'left' }),
  }).then(() => _rbRefresh()).catch(() => { status.textContent = 'Error'; });
}

function _rbTypeKey(event) {
  if (!_rbActive) return;
  if (event.key === 'Enter') {
    event.preventDefault();
    const input = event.target;
    const text = input.value;
    input.value = '';
    if (!text) {
      // Just press Enter
      fetch(API + '/api/browser/action', {
        method: 'POST', headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({ action: 'key', key: 'Enter' }),
      }).then(() => _rbRefresh());
      return;
    }
    // Type text then press Enter
    fetch(API + '/api/browser/action', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ action: 'type', text }),
    }).then(() => _rbRefresh());
  } else if (event.key === 'Escape') {
    fetch(API + '/api/browser/action', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ action: 'key', key: 'Escape' }),
    }).then(() => _rbRefresh());
  } else if (event.key === 'Tab') {
    event.preventDefault();
    fetch(API + '/api/browser/action', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ action: 'key', key: 'Tab' }),
    }).then(() => _rbRefresh());
  } else if (event.key === 'Backspace' && !event.target.value) {
    // If input is empty, forward backspace to remote browser
    fetch(API + '/api/browser/action', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({ action: 'key', key: 'Backspace' }),
    }).then(() => _rbRefresh());
  }
}

// ═══════ FILE EXPLORER ═══════
let _explorePath = '';
let _exploreShowHidden = false;
// ═══════ FILES TAB (inline directory browser) ═══════
let _filesPath = '/';
let _filesCwd = '/';   // saved working directory (persisted on server)
let _filesShowHidden = false;

let _exploreSession = null;  // set when explore overlay is opened from a session
// Load saved working dir from server prefs
(async () => {
  try {
    const r = await fetch(API + '/api/prefs?key=files_cwd');
    const d = await r.json();
    if (d.value) { _filesPath = d.value; _filesCwd = d.value; }
  } catch(e) {}
})();
function setFilesCwd() {
  _filesCwd = _filesPath;
  fetch(API + '/api/prefs', {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({key:'files_cwd', value:_filesCwd})}).catch(()=>{});
  _updateFilesCwdBtn();
}
function _updateFilesCwdBtn() {
  const btn = document.getElementById('files-setcwd-btn');
  if (!btn) return;
  const isHome = _filesPath === _filesCwd;
  btn.style.background = isHome ? 'var(--accent)' : '';
  btn.style.color = isHome ? '#000' : '';
  btn.title = isHome ? 'Working directory: ' + _filesCwd : 'Set ' + _filesPath + ' as working directory';
}
function toggleFilesHidden() {
  _filesShowHidden = !_filesShowHidden;
  const btn = document.getElementById('files-hidden-btn');
  btn.style.background = _filesShowHidden ? 'var(--accent)' : '';
  btn.style.color = _filesShowHidden ? '#000' : '';
  loadFiles(_filesPath);
}
async function loadFiles(path) {
  const body = document.getElementById('files-body');
  body.innerHTML = '<div style="padding:16px;color:var(--dim)">Loading...</div>';
  _filesPath = path;
  _updateFilesCwdBtn();
  // Breadcrumb
  const parts = path.split('/').filter(Boolean);
  let crumbHtml = '<span class="explore-crumb" onclick="loadFiles(\'/\')">/</span>';
  let cum = '';
  for (const part of parts) {
    cum += '/' + part;
    const cp = cum;
    crumbHtml += '<span class="explore-crumb" onclick="loadFiles(\'' + cp.replace(/'/g, "\\'") + '\')"> ' + esc(part) + '</span><span style="color:var(--dim)">/</span>';
  }
  document.getElementById('files-breadcrumb').innerHTML = crumbHtml;
  try {
    const r = await fetch(API + '/api/ls?path=' + encodeURIComponent(path) + (_filesShowHidden ? '&hidden=1' : ''));
    const data = await r.json();
    if (data.error) { body.innerHTML = '<div style="padding:16px;color:var(--dim)">' + esc(data.error) + '</div>'; return; }
    _renderFilesEntries(body, path, data, false);
    _idb.setFile(path, { type: 'dir', data });
  } catch(e) {
    // Offline: try IDB cache
    const cached = await _idb.getFile(path);
    if (cached && cached.type === 'dir') {
      _renderFilesEntries(body, path, cached.data, cached.ts);
    } else {
      body.innerHTML = '<div style="padding:16px;color:var(--dim)">Failed to load directory.</div>';
    }
  }
}
function _renderFilesEntries(body, path, data, cacheTs) {
  body.innerHTML = '';
  if (cacheTs) {
    const age = Math.round((Date.now() - cacheTs) / 60000);
    const ageStr = age < 60 ? age + 'm ago' : Math.round(age/60) + 'h ago';
    body.innerHTML = '<div style="padding:4px 12px;font-size:0.7rem;color:var(--dim);background:var(--card);border-bottom:1px solid var(--border);">&#x1F4F5; Offline cache &middot; ' + ageStr + '</div>';
  }
  if (data.parent && data.parent !== data.path) {
    const back = document.createElement('div');
    back.className = 'explore-row';
    back.innerHTML = '<span class="explore-icon">&#x2B05;</span><span class="explore-name" style="color:var(--dim)">.. (up)</span>';
    back.onclick = () => loadFiles(data.parent);
    body.appendChild(back);
  }
  if (!data.entries.length) {
    body.innerHTML += '<div style="padding:16px;color:var(--dim)">Empty directory</div>';
    return;
  }
  for (const entry of data.entries) {
    const entryPath = path.replace(/\/$/, '') + '/' + entry.name;
    const row = document.createElement('div');
    row.className = 'explore-row';
    const icon = entry.type === 'dir' ? '&#x1F4C2;' : '&#x1F4C4;';
    const displayName = entry.name + (entry.type === 'dir' ? '/' : '');
    const menuBtn = '<button class="explore-menu-btn" title="Options" onclick="event.stopPropagation();_showFilesMenu(\'' + entryPath.replace(/'/g, "\\'") + '\',this,\'' + entry.type + '\')">⋯</button>';
    const mtime = entry.modified ? '<span class="explore-mtime">' + timeAgo(entry.modified) + '</span>' : '';
    row.innerHTML = '<span class="explore-icon">' + icon + '</span><span class="explore-name">' + esc(displayName) + '</span><span class="explore-size">' + esc(_fmtSize(entry.size)) + '</span>' + mtime + menuBtn;
    if (entry.type === 'dir') {
      row.onclick = () => loadFiles(entryPath);
    } else {
      row.onclick = () => openFilePreview(entryPath);
    }
    body.appendChild(row);
  }
}

// ═══════ OFFLINE FILE CACHE ═══════
const _CACHEABLE_TEXT_EXTS = new Set([
  '.md','.markdown','.mdx','.txt','.py','.js','.ts','.jsx','.tsx','.mjs','.cjs',
  '.html','.htm','.css','.json','.yaml','.yml','.toml','.sh','.bash','.zsh',
  '.env','.gitignore','.gitattributes','.go','.rs','.java','.rb','.php','.swift',
  '.kt','.c','.cpp','.h','.cs','.sql','.graphql','.proto','.tf','.xml','.csv',
  '.ini','.cfg','.conf','.lock','.mod','.sum','.dockerfile','.makefile',
]);
const _CACHEABLE_IMG_EXTS = new Set(['.png','.jpg','.jpeg','.gif','.webp','.svg','.bmp','.ico']);
const _SKIP_CACHE_DIRS = new Set(['.git','node_modules','__pycache__','.next','.nuxt','dist','build','.venv','venv','.tox','.mypy_cache','target','vendor']);

async function cacheFilesDir(rootPath, maxDepth = 2) {
  const statusEl = document.getElementById('files-cache-status');
  const btn = document.getElementById('files-cache-btn');
  let count = 0;
  const setStatus = msg => { if (statusEl) statusEl.textContent = msg; };
  setStatus('Scanning\u2026');
  if (btn) { btn.disabled = true; btn.textContent = '\u23F3 Caching'; }

  async function walkDir(path, depth) {
    try {
      const r = await fetch(API + '/api/ls?path=' + encodeURIComponent(path));
      if (!r.ok) return;
      const data = await r.json();
      if (data.error) return;
      await _idb.setFile(path, { type: 'dir', data });
      count++;
      setStatus('Cached ' + count + ' items\u2026');
      for (const entry of (data.entries || [])) {
        const entryPath = path.replace(/\/$/, '') + '/' + entry.name;
        if (entry.type === 'dir') {
          if (depth < maxDepth && !_SKIP_CACHE_DIRS.has(entry.name)) {
            await walkDir(entryPath, depth + 1);
          }
        } else {
          const dot = entry.name.lastIndexOf('.');
          const ext = dot >= 0 ? entry.name.slice(dot).toLowerCase() : '';
          const isImg = _CACHEABLE_IMG_EXTS.has(ext);
          const isText = _CACHEABLE_TEXT_EXTS.has(ext) || (!ext && (entry.size || 0) < 50000);
          if (!isText && !isImg) continue;
          if (isImg && (entry.size || 0) > 200000) continue;
          if (!isImg && (entry.size || 0) > 200000) continue;
          try {
            const fr = await fetch(API + '/api/file?path=' + encodeURIComponent(entryPath));
            if (fr.ok) {
              const fd = await fr.json();
              if (!fd.error) { await _idb.setFile(entryPath, { type: 'file', data: fd }); count++; setStatus('Cached ' + count + ' items\u2026'); }
            }
          } catch(e) {}
        }
      }
    } catch(e) {}
  }

  await walkDir(rootPath, 0);
  if (btn) { btn.disabled = false; btn.textContent = '\u2601 Cache'; }
  setStatus('\u2713 ' + count + ' cached');
  setTimeout(() => setStatus(''), 5000);
}

function openExplore(startPath, session) {
  _explorePath = startPath || '/';
  _exploreSession = session || null;
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
function _showExploreMenu(path, btn, type) {
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
function _showFilesMenu(path, btn, type) {
  document.querySelectorAll('.explore-menu-popup').forEach(el => el.remove());
  const popup = document.createElement('div');
  popup.className = 'explore-menu-popup';
  // Copy path
  const copyItem = document.createElement('button');
  copyItem.className = 'explore-menu-item';
  copyItem.textContent = 'Copy path';
  copyItem.onclick = () => { popup.remove(); _copyExplorePath(path); };
  popup.appendChild(copyItem);
  document.body.appendChild(popup);
  const r = btn.getBoundingClientRect();
  const pw = popup.offsetWidth || 160;
  let left = r.right - pw;
  if (left < 8) left = 8;
  let top = r.bottom + 4;
  if (top + 100 > window.innerHeight) top = r.top - 100;
  popup.style.left = left + 'px';
  popup.style.top = top + 'px';
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
    _idb.setFile(path, { type: 'dir', data });
    _renderExploreEntries(body, path, data, false);
  } catch(e) {
    const cached = await _idb.getFile(path);
    if (cached && cached.type === 'dir') {
      _renderExploreEntries(body, path, cached.data, cached.ts);
    } else {
      body.innerHTML = '<div style="padding:16px;color:var(--dim)">Offline — no cached data for this directory.</div>';
    }
  }
}
function _renderExploreEntries(body, path, data, cacheTs) {
  body.innerHTML = '';
  if (cacheTs) {
    const age = Math.round((Date.now() - cacheTs) / 60000);
    const ageStr = age < 60 ? age + 'm ago' : Math.round(age/60) + 'h ago';
    body.innerHTML = '<div style="padding:4px 12px;font-size:0.7rem;color:var(--dim);background:var(--card);border-bottom:1px solid var(--border);">&#x1F4F5; Offline cache &middot; ' + ageStr + '</div>';
  }
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
    const entryPath = path.replace(/\/$/, '') + '/' + entry.name;
    const row = document.createElement('div');
    row.className = 'explore-row';
    const icon = entry.type === 'dir' ? '&#x1F4C2;' : '&#x1F4C4;';
    const displayName = entry.name + (entry.type === 'dir' ? '/' : '');
    const menuBtn = `<button class="explore-menu-btn" title="Options" onclick="event.stopPropagation();_showExploreMenu('${entryPath.replace(/'/g,"\\'")}',this,'${entry.type}')">⋯</button>`;
    const mtime = entry.modified ? `<span class="explore-mtime">${timeAgo(entry.modified)}</span>` : '';
    row.innerHTML = `<span class="explore-icon">${icon}</span><span class="explore-name">${esc(displayName)}</span><span class="explore-size">${esc(_fmtSize(entry.size))}</span>${mtime}${menuBtn}`;
    if (entry.type === 'dir') {
      row.onclick = () => loadExplore(entryPath);
    } else {
      row.onclick = () => openFilePreview(entryPath);
    }
    body.appendChild(row);
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

// renderMarkdown() — defined once below (board detail section), uses marked.js with fallback

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
let _createBranchEdited = false;  // track if user manually changed branch name

function openCreate() {
  document.getElementById('create-name').value = '';
  document.getElementById('create-dir').value = (_filesCwd && _filesCwd !== '/') ? _filesCwd : '';
  document.getElementById('create-prompt').value = '';
  document.getElementById('create-branch').value = '';
  document.getElementById('create-branch-enabled').checked = false;
  document.getElementById('create-branch-wrap').style.display = 'none';
  document.getElementById('create-branch-suggestions').style.display = 'none';
  document.getElementById('create-branch-suggestions').innerHTML = '';
  document.getElementById('ac-list').innerHTML = '';
  document.getElementById('ac-list').classList.remove('open');
  _createBranchEdited = false;
  document.getElementById('create-overlay').classList.add('active');
  setTimeout(() => document.getElementById('create-name').focus({ preventScroll: true }), 100);
}
function closeCreate() {
  document.getElementById('create-overlay').classList.remove('active');
  document.getElementById('ac-list').classList.remove('open');
}
function _createNameChanged(val) {
  // Auto-update branch name if user hasn't manually edited it
  if (!_createBranchEdited) {
    const slug = val.trim().toLowerCase().replace(/[^a-z0-9-]/g, '-').replace(/-+/g, '-').replace(/^-|-$/g, '');
    document.getElementById('create-branch').value = slug ? 'session/' + slug : '';
  }
}
function _toggleCreateBranch(on) {
  document.getElementById('create-branch-wrap').style.display = on ? '' : 'none';
  if (on) {
    // Pre-fill if empty
    const inp = document.getElementById('create-branch');
    if (!inp.value) {
      const name = document.getElementById('create-name').value.trim();
      const slug = name.toLowerCase().replace(/[^a-z0-9-]/g, '-').replace(/-+/g, '-').replace(/^-|-$/g, '');
      inp.value = slug ? 'session/' + slug : '';
    }
    inp.addEventListener('input', () => { _createBranchEdited = true; }, {once: true});
    setTimeout(() => inp.focus({preventScroll: true}), 50);
  }
}
async function _suggestBranch() {
  const name = document.getElementById('create-name').value.trim();
  const dir = document.getElementById('create-dir').value.trim();
  const prompt = document.getElementById('create-prompt').value.trim();
  const btn = document.getElementById('create-branch-suggest-btn');
  const sugg = document.getElementById('create-branch-suggestions');
  btn.textContent = '…'; btn.disabled = true;
  sugg.style.display = 'none'; sugg.innerHTML = '';
  try {
    const r = await fetch(API + '/api/suggest-branch', {
      method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({name, dir, prompt}),
    });
    const d = await r.json();
    if (d.suggestions && d.suggestions.length) {
      sugg.innerHTML = d.suggestions.map(s =>
        `<span class="chip" style="cursor:pointer;" onclick="document.getElementById('create-branch').value='${esc(s)}';_createBranchEdited=true;document.getElementById('create-branch-suggestions').style.display='none';">${esc(s)}</span>`
      ).join('');
      sugg.style.display = 'flex';
    }
  } catch(e) {}
  btn.textContent = '✨'; btn.disabled = false;
}
async function submitCreate() {
  const name = document.getElementById('create-name').value.trim();
  const dir = document.getElementById('create-dir').value.trim();
  const prompt = document.getElementById('create-prompt').value.trim();
  const branchEnabled = document.getElementById('create-branch-enabled').checked;
  const branch = branchEnabled ? document.getElementById('create-branch').value.trim() : '';
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
  if (r && r.ok) {
    if (dir) _addRecentDir(dir);
    // Create branch if requested
    if (branch && dir) {
      await fetch(API + '/api/sessions/' + encodeURIComponent(name) + '/git', {
        method: 'POST', headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({branch, create: true}),
      }).catch(() => {});
    }
    // Always start the session immediately
    await apiCall(API + '/api/sessions/' + encodeURIComponent(name) + '/start', { method: 'POST' });
    if (prompt) {
      setTimeout(async () => {
        await apiCall(API + '/api/sessions/' + encodeURIComponent(name) + '/send', {
          method: 'POST', headers: {'Content-Type':'application/json'},
          body: JSON.stringify({ text: prompt })
        });
      }, 5000);
    }
  }
  await fetchSessions();
  // Scroll to the newly created session card
  const newCard = document.querySelector('[data-session="' + CSS.escape(name) + '"]');
  if (newCard) newCard.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
}

// ── Directory autocomplete + recent dirs ──
let acTimer = null;
let acItems = [];
let acSelected = -1;
let _acSuppressNext = false;  // set true on paste — skip dropdown for that one input event

function _getRecentDirs() {
  try { return JSON.parse(localStorage.getItem('amux_recent_dirs') || '[]'); } catch(e) { return []; }
}
function _addRecentDir(dir) {
  if (!dir) return;
  let recents = _getRecentDirs().filter(d => d !== dir);
  recents.unshift(dir);
  recents = recents.slice(0, 12);
  localStorage.setItem('amux_recent_dirs', JSON.stringify(recents));
}
function _buildSuggestedDirs() {
  // Combine recent dirs + unique dirs from loaded sessions, deduped
  const recents = _getRecentDirs();
  const sessionDirs = [...new Set(sessions.map(s => s.dir).filter(Boolean))];
  const combined = [...recents];
  for (const d of sessionDirs) {
    if (!combined.includes(d)) combined.push(d);
  }
  return combined.slice(0, 15);
}

function _acShowSuggested() {
  const el = document.getElementById('ac-list');
  const recents = _getRecentDirs();
  const sessionDirs = [...new Set(sessions.map(s => s.dir).filter(Boolean))].filter(d => !recents.includes(d));
  if (!recents.length && !sessionDirs.length) { el.classList.remove('open'); return; }
  acItems = [...recents, ...sessionDirs].slice(0, 15);
  acSelected = -1;
  let html = '';
  if (recents.length) {
    html += `<div class="ac-section">Recent</div>`;
    html += recents.slice(0, 8).map((item, i) =>
      `<div class="ac-item" onmousedown="acPick(${i})">${esc(item)}</div>`
    ).join('');
  }
  if (sessionDirs.length) {
    const offset = recents.length;
    html += `<div class="ac-section">Sessions</div>`;
    html += sessionDirs.slice(0, 7).map((item, i) =>
      `<div class="ac-item" onmousedown="acPick(${offset + i})">${esc(item)}</div>`
    ).join('');
  }
  el.innerHTML = html;
  el.classList.add('open');
}

function acFetch(query) {
  clearTimeout(acTimer);
  const el = document.getElementById('ac-list');
  if (_acSuppressNext) {
    _acSuppressNext = false;
    el.classList.remove('open');
    return;
  }
  if (!query || query.length < 2) {
    // Show recent/session dirs when field is empty or very short
    _acShowSuggested();
    return;
  }
  el.classList.remove('open');
  acTimer = setTimeout(async () => {
    try {
      const r = await fetch(API + '/api/autocomplete/dir?q=' + encodeURIComponent(query));
      acItems = await r.json();
      acSelected = -1;
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
  // Close card slash/@ autocomplete
  if (!e.target.closest('.send-row') && _cardAcName) {
    const el = document.getElementById('card-ac-' + _cardAcName);
    if (el) { el.classList.remove('open'); el._atItems = null; el._atSel = -1; }
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
    // Use a synthetic <a> click — more reliable than window.open in desktop PWA
    // standalone mode where window.open can be silently blocked by the popup blocker.
    const tmp = document.createElement('a');
    tmp.href = href;
    tmp.target = '_blank';
    tmp.rel = 'noopener noreferrer';
    document.body.appendChild(tmp);
    tmp.click();
    document.body.removeChild(tmp);
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
  const peekOpen = document.getElementById('peek-overlay')?.classList.contains('active');
  const boardOpen = document.getElementById('board-detail-overlay')?.classList.contains('active');
  // Check for image/file paste — route to uploadAndAttach when peek is open
  if (peekOpen && e.clipboardData?.items) {
    for (const item of e.clipboardData.items) {
      if (item.kind === 'file') {
        e.preventDefault();
        uploadAndAttach(item.getAsFile());
        return;
      }
    }
  }
  const text = e.clipboardData?.getData('text/plain');
  if (!text) return;
  const inp = peekOpen ? document.getElementById('peek-cmd-input')
    : boardOpen ? document.querySelector('#board-detail-overlay textarea, #board-detail-overlay input')
    : document.querySelector('.card.open .send-input') || document.getElementById('search');
  if (!inp) return;
  e.preventDefault();
  if (inp.id === 'peek-cmd-input') _slashAcSuppressNext = true;
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
function _pasteTextInto(target) {
  navigator.clipboard.readText().then(text => {
    if (!text) return;
    if (target.id === 'create-dir') _acSuppressNext = true;
    if (target.id === 'peek-cmd-input') _slashAcSuppressNext = true;
    target.focus({ preventScroll: true });
    const s = target.selectionStart ?? target.value.length;
    const en = target.selectionEnd ?? target.value.length;
    target.value = target.value.slice(0, s) + text + target.value.slice(en);
    target.selectionStart = target.selectionEnd = s + text.length;
    target.dispatchEvent(new Event('input', { bubbles: true }));
    if (typeof autoGrow === 'function') autoGrow(target);
  }).catch(() => {});
}
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
      // Try clipboard.read() first for image/file paste support
      if (peekOpen && navigator.clipboard.read) {
        navigator.clipboard.read().then(items => {
          for (const item of items) {
            const imgType = item.types.find(t => t.startsWith('image/'));
            if (imgType) {
              item.getType(imgType).then(blob => {
                const ext = imgType.split('/')[1] || 'png';
                const file = new File([blob], 'pasted-image.' + ext, { type: imgType });
                uploadAndAttach(file);
              });
              return;
            }
          }
          // No image — fall through to text paste
          _pasteTextInto(target);
        }).catch(() => _pasteTextInto(target));
      } else {
        _pasteTextInto(target);
      }
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

// Natural sort: pinned first, then most recently active
function _naturalSortSessions(a, b) {
  if (a.pinned !== b.pinned) return a.pinned ? -1 : 1;
  return (b.last_activity || 0) - (a.last_activity || 0);
}

function resetCardOrder() {
  cardOrder = [];
  localStorage.removeItem('amux_card_order');
  _updateResetBtn();
  render();
}

function _updateResetBtn() {
  const btn = document.getElementById('tile-reset-btn');
  if (btn) btn.style.display = cardOrder.length > 0 ? '' : 'none';
  const cb = document.getElementById('tile-collapse-btn');
  if (cb) cb.style.display = expanded.size > 0 ? '' : 'none';
}

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
  _updateResetBtn();
}

function initSortable() {
  if (typeof Sortable === 'undefined') return;
  destroySortable();
  const cards = document.querySelector('.cards');
  if (!cards) return;
  _sortable = Sortable.create(cards, {
    handle: '.card-drag-handle',
    animation: 120,
    easing: 'cubic-bezier(0.2, 0, 0, 1)',
    ghostClass: 'sortable-ghost',
    chosenClass: 'sortable-chosen',
    dragClass: 'sortable-drag',
    delay: 120,
    delayOnTouchOnly: true,
    touchStartThreshold: 3,
    swapThreshold: 0.6,
    onStart: function(evt) {
      _tileJustDragged = false;
      document.body.classList.add('session-dragging');
    },
    onEnd: function(evt) {
      document.body.classList.remove('session-dragging');
      _tileJustDragged = evt.oldIndex !== evt.newIndex;
      const allCards = cards.querySelectorAll('.card[data-session]');
      cardOrder = Array.from(allCards).map(c => c.dataset.session);
      localStorage.setItem('amux_card_order', JSON.stringify(cardOrder));
      _updateResetBtn();
    }
  });
}

function destroySortable() {
  if (_sortable) { try { _sortable.destroy(); } catch(e) {} _sortable = null; }
}

// Safety net: if a drag ends abnormally (Escape, window blur, cancelled by browser),
// onEnd may not fire — clean up the dragging body classes so cursor resets.
function _clearDragClasses() {
  document.body.classList.remove('board-dragging', 'session-dragging');
}
document.addEventListener('mouseup', _clearDragClasses);
document.addEventListener('pointerup', _clearDragClasses);
document.addEventListener('dragend', _clearDragClasses);
document.addEventListener('keydown', function(e) { if (e.key === 'Escape') _clearDragClasses(); });

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

// ═══════ REPORTS ═══════
let _reportTypes = {};  // loaded from /api/reports/types
let _reportCharts = {};  // chartId → Chart instance

function _vendorColor(typeId, vid) {
  return (_reportTypes[typeId]?.vendors?.[vid]?.color) || '#888';
}
function _vendorLabel(typeId, vid) {
  return (_reportTypes[typeId]?.vendors?.[vid]?.label) || vid;
}

async function fetchReports() {
  const [rTypes, rReports] = await Promise.all([
    fetch(API + '/api/reports/types').then(r=>r.json()).catch(()=>({})),
    fetch(API + '/api/reports').then(r=>r.json()).catch(()=>[]),
  ]);
  _reportTypes = rTypes;
  renderReports(rReports);
}

function renderReports(reports) {
  const list = document.getElementById('reports-list');
  if (!reports.length) {
    list.innerHTML = '<div style="text-align:center;padding:40px 0;color:var(--dim);font-size:0.85rem;">No reports yet. Click <b>+ Add Report</b> to get started.</div>';
    return;
  }
  // Destroy old chart instances
  Object.values(_reportCharts).forEach(c => { try { c.destroy(); } catch(e){} });
  _reportCharts = {};
  list.innerHTML = reports.map(r => `
    <div class="report-card" data-report-id="${r.id}" data-report-type="${r.type||'infra-spend'}">
      <div class="report-card-header">
        <span class="report-card-title">${esc(r.name)}</span>
        <span class="report-last-refresh" id="rpt-refresh-ts-${r.id}">${r.last_refresh ? 'Updated ' + _fmtRelTime(r.last_refresh) : 'Never refreshed'}</span>
        <button class="report-refresh-btn" id="rpt-refresh-btn-${r.id}" onclick="refreshReport('${r.id}')" title="Refresh">&#x21BB;</button>
        <button class="report-del-btn" onclick="deleteReport('${r.id}')" title="Delete report">&times;</button>
      </div>
      <div class="report-body" id="rpt-body-${r.id}">
        <div style="color:var(--dim);font-size:0.82rem;padding:20px 0;text-align:center;">
          Click \u21bb to load data
        </div>
      </div>
    </div>
  `).join('');
  // Show cached data immediately, then auto-refresh in background
  reports.forEach(r => {
    loadReportData(r.id, null);
    refreshReport(r.id);
  });
}

async function loadReportData(id, data) {
  if (!data) {
    try {
      const r = await fetch(API + '/api/reports/' + id + '/data');
      const d = await r.json();
      data = d.data;
      if (d.refreshed_at) {
        const el = document.getElementById('rpt-refresh-ts-' + id);
        if (el) el.textContent = 'Updated ' + _fmtRelTime(d.refreshed_at);
      }
    } catch(e) { return; }
  }
  if (!data || !Object.keys(data).length) return;
  const body = document.getElementById('rpt-body-' + id);
  if (!body) return;
  const _rCard = body.closest('[data-report-id]');
  const _rtype = _rCard ? (_rCard.dataset.reportType || 'infra-spend') : 'infra-spend';
  const _isCount = (_reportTypes[_rtype]?.display === 'count');
  const _col0 = _isCount ? 'Metric' : 'Vendor';
  const _col4 = _isCount ? 'Total (90d)' : 'Total (12mo)';
  body.innerHTML = `
    <div class="report-period-tabs" style="margin-bottom:10px;">
      <button class="report-period-tab active" onclick="setReportPeriod('${id}','monthly',this)">Monthly</button>
      <button class="report-period-tab" onclick="setReportPeriod('${id}','weekly',this)">Weekly</button>
      <button class="report-period-tab" onclick="setReportPeriod('${id}','daily',this)">Daily</button>
    </div>
    <div class="report-chart-wrap"><canvas id="rpt-chart-${id}"></canvas></div>
    <table class="report-table">
      <thead><tr><th>${_col0}</th><th style="text-align:right">This Month</th><th style="text-align:right">Last Month</th><th style="text-align:right">${_col4}</th></tr></thead>
      <tbody id="rpt-table-${id}"></tbody>
    </table>
  `;
  body._data = data;
  body._period = 'monthly';
  body._rtype = _rtype;
  _renderReportChart(id, data, 'monthly');
}

function setReportPeriod(id, period, btn) {
  const body = document.getElementById('rpt-body-' + id);
  if (!body || !body._data) return;
  body._period = period;
  body.querySelectorAll('.report-period-tab').forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
  _renderReportChart(id, body._data, period);
}

function _renderReportChart(id, data, period) {
  const canvas = document.getElementById('rpt-chart-' + id);
  const tbody = document.getElementById('rpt-table-' + id);
  if (!canvas) return;

  const body = document.getElementById('rpt-body-' + id);
  const rtype = body?._rtype || 'infra-spend';
  const vendors = Object.keys(data);
  // Collect all period labels
  const labelSet = new Set();
  vendors.forEach(v => {
    (data[v][period] || []).forEach(p => labelSet.add(p.month || p.week || p.date || ''));
  });
  const labels = [...labelSet].sort().slice(-12);

  // Build datasets
  const datasets = vendors.map(v => {
    const vdata = data[v];
    const pts = {};
    (vdata[period] || []).forEach(p => { pts[p.month || p.week || p.date || ''] = p.amount || 0; });
    return {
      label: _vendorLabel(rtype, v),
      data: labels.map(l => pts[l] || 0),
      backgroundColor: _vendorColor(rtype, v),
      borderRadius: 3,
    };
  });

  // Destroy old chart
  if (_reportCharts[id]) { try { _reportCharts[id].destroy(); } catch(e){} }

  const _isCount = (_reportTypes[rtype]?.display === 'count');
  const _yFmt = _isCount ? (v => v.toLocaleString()) : (v => '$' + v.toFixed(0));
  if (typeof Chart !== 'undefined' && labels.length) {
    _reportCharts[id] = new Chart(canvas, {
      type: 'bar',
      data: { labels, datasets },
      options: {
        responsive: true, maintainAspectRatio: false,
        plugins: { legend: { position: 'bottom', labels: { font: { size: 11 }, boxWidth: 12 } } },
        scales: {
          x: { stacked: !_isCount, ticks: { font: { size: 10 }, maxRotation: 45 } },
          y: { stacked: !_isCount, ticks: { font: { size: 10 }, callback: _yFmt } }
        }
      }
    });
  } else if (!labels.length) {
    canvas.parentElement.innerHTML = '<div style="text-align:center;padding:30px 0;color:var(--dim);font-size:0.82rem;">No data available \u2014 click \u21bb to fetch</div>';
  }

  // Table: show per-vendor current month, last month, 12mo total
  if (tbody) {
    const now = new Date();
    const thisMo = now.toISOString().slice(0,7);
    const lastMo = new Date(now.getFullYear(), now.getMonth()-1, 1).toISOString().slice(0,7);
    let rows = '';
    let totThis=0, totLast=0, tot12=0;
    vendors.forEach(v => {
      const vd = data[v];
      const vcolor = _vendorColor(rtype, v);
      const vlabel = _vendorLabel(rtype, v);
      if (vd.error) {
        rows += `<tr><td><span class="report-vendor-dot" style="background:${vcolor}"></span>${vlabel}</td><td colspan="3" class="report-error">${esc(vd.error)}</td></tr>`;
        return;
      }
      const monthly = vd.monthly || [];
      const thisAmt = monthly.find(m=>m.month===thisMo)?.amount||0;
      const lastAmt = monthly.find(m=>m.month===lastMo)?.amount||0;
      const total12 = monthly.slice(-12).reduce((a,m)=>a+m.amount,0);
      totThis+=thisAmt; totLast+=lastAmt; tot12+=total12;
      const _fmt = _isCount ? (v => v.toLocaleString()) : (v => '$' + v.toFixed(2));
      rows += `<tr>
        <td><span class="report-vendor-dot" style="background:${vcolor}"></span>${vlabel}</td>
        <td style="text-align:right">${_fmt(thisAmt)}</td>
        <td style="text-align:right">${_fmt(lastAmt)}</td>
        <td style="text-align:right">${_fmt(total12)}</td>
      </tr>`;
    });
    const _fmtTot = _isCount ? (v => v.toLocaleString()) : (v => '$' + v.toFixed(2));
    rows += `<tr class="report-total">
      <td>Total</td>
      <td style="text-align:right">${_fmtTot(totThis)}</td>
      <td style="text-align:right">${_fmtTot(totLast)}</td>
      <td style="text-align:right">${_fmtTot(tot12)}</td>
    </tr>`;
    tbody.innerHTML = rows;
  }
}

async function refreshReport(id) {
  const btn = document.getElementById('rpt-refresh-btn-' + id);
  if (btn) { btn.classList.add('spinning'); btn.disabled = true; }
  try {
    const r = await fetch(API + '/api/reports/' + id + '/refresh', { method: 'POST' });
    const d = await r.json();
    if (d.data) {
      await loadReportData(id, d.data);
      const ts = document.getElementById('rpt-refresh-ts-' + id);
      if (ts && d.refreshed_at) ts.textContent = 'Updated ' + _fmtRelTime(d.refreshed_at);
    }
  } finally {
    if (btn) { btn.classList.remove('spinning'); btn.disabled = false; }
  }
}

async function deleteReport(id) {
  if (!await showConfirm('Delete this report?', 'Delete', true)) return;
  await fetch(API + '/api/reports/' + id, { method: 'DELETE' });
  fetchReports();
}

function _populateAddReportModal() {
  const typeSelect = document.getElementById('add-report-type');
  typeSelect.innerHTML = Object.entries(_reportTypes).map(([id, t]) =>
    `<option value="${id}">${esc(t.label)}</option>`
  ).join('') || '<option value="infra-spend">Infrastructure Spend</option>';
  _updateAddReportVendors();
}
function _updateAddReportVendors() {
  const typeId = document.getElementById('add-report-type').value;
  const vendors = _reportTypes[typeId]?.vendors || {};
  const grp = document.getElementById('add-report-vendors-group');
  const entries = Object.entries(vendors);
  if (!entries.length) { grp.style.display = 'none'; return; }
  grp.style.display = '';
  grp.querySelector('div').innerHTML = entries.map(([vid, vm]) =>
    `<label style="display:flex;align-items:center;gap:5px;font-size:0.82rem;">` +
    `<input type="checkbox" value="${vid}" checked> ${esc(vm.label)}</label>`
  ).join('');
}
function openAddReport() {
  _populateAddReportModal();
  document.getElementById('add-report-overlay').style.display = 'flex';
  document.getElementById('add-report-name').focus();
}
function closeAddReport() {
  document.getElementById('add-report-overlay').style.display = 'none';
}
async function submitAddReport() {
  const name = document.getElementById('add-report-name').value.trim() || 'New Report';
  const type = document.getElementById('add-report-type').value;
  const vendors = [...document.querySelectorAll('#add-report-vendors-group input[type=checkbox]:checked')].map(c=>c.value);
  closeAddReport();
  await fetch(API + '/api/reports', {
    method: 'POST', headers:{'Content-Type':'application/json'},
    body: JSON.stringify({name, type, config:{vendors}})
  });
  fetchReports();
}

function _fmtRelTime(ts) {
  const diff = Math.floor(Date.now()/1000) - ts;
  if (diff < 60) return 'just now';
  if (diff < 3600) return Math.floor(diff/60) + 'm ago';
  if (diff < 86400) return Math.floor(diff/3600) + 'h ago';
  return Math.floor(diff/86400) + 'd ago';
}

// ═══════ BOARD ═══════
let activeView = 'sessions';
let boardItems = [];
let boardStatuses = [{id:'backlog',label:'Backlog'},{id:'todo',label:'To Do'},{id:'doing',label:'In Progress'},{id:'done',label:'Done'},{id:'discarded',label:'Discarded'}];
let _boardSortables = [];
let boardTimer = null;
let schedules = [];
let _schedEditId = null;
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
// Light-mode versions of the same statuses — opaque/dark enough on white
const _BUILT_IN_STATUS_STYLE_LIGHT = {
  'backlog':   {bg:'rgba(9,105,218,0.1)',color:'#0550ae',border:'rgba(9,105,218,0.3)',dot:'#0550ae'},
  'todo':      {bg:'rgba(101,109,118,0.1)',color:'#57606a',border:'rgba(101,109,118,0.3)',dot:'#57606a'},
  'doing':     {bg:'rgba(154,103,0,0.1)',color:'#7d4e00',border:'rgba(154,103,0,0.35)',dot:'#7d4e00'},
  'done':      {bg:'rgba(26,127,55,0.1)',color:'#1a7f37',border:'rgba(26,127,55,0.35)',dot:'#1a7f37'},
  'discarded': {bg:'rgba(101,109,118,0.07)',color:'#57606a',border:'rgba(101,109,118,0.2)',dot:'#57606a'},
};
const _CUSTOM_STATUS_PALETTE = [
  {bg:'rgba(88,166,255,0.15)',color:'var(--accent)',border:'rgba(88,166,255,0.4)',dot:'var(--accent)'},
  {bg:'rgba(188,140,255,0.15)',color:'#bc8cff',border:'rgba(188,140,255,0.4)',dot:'#bc8cff'},
  {bg:'rgba(255,140,80,0.15)',color:'#ff8c50',border:'rgba(255,140,80,0.4)',dot:'#ff8c50'},
  {bg:'rgba(248,81,73,0.15)',color:'var(--red)',border:'rgba(248,81,73,0.4)',dot:'var(--red)'},
  {bg:'rgba(57,210,192,0.15)',color:'var(--cyan)',border:'rgba(57,210,192,0.4)',dot:'var(--cyan)'},
  {bg:'rgba(255,100,180,0.15)',color:'#ff64b4',border:'rgba(255,100,180,0.4)',dot:'#ff64b4'},
];
const _CUSTOM_STATUS_PALETTE_LIGHT = [
  {bg:'rgba(9,105,218,0.1)',color:'#0550ae',border:'rgba(9,105,218,0.3)',dot:'#0550ae'},
  {bg:'rgba(130,80,255,0.1)',color:'#6639ba',border:'rgba(130,80,255,0.3)',dot:'#6639ba'},
  {bg:'rgba(200,80,20,0.1)',color:'#bc4c00',border:'rgba(200,80,20,0.3)',dot:'#bc4c00'},
  {bg:'rgba(207,34,46,0.1)',color:'#cf222e',border:'rgba(207,34,46,0.3)',dot:'#cf222e'},
  {bg:'rgba(5,80,174,0.1)',color:'#0550ae',border:'rgba(5,80,174,0.3)',dot:'#0550ae'},
  {bg:'rgba(180,30,120,0.1)',color:'#99286e',border:'rgba(180,30,120,0.3)',dot:'#99286e'},
];
function statusStyle(id) {
  const light = document.body.classList.contains('light');
  const builtIn = light ? _BUILT_IN_STATUS_STYLE_LIGHT[id] : _BUILT_IN_STATUS_STYLE[id];
  if (builtIn) return builtIn;
  const customs = boardStatuses.filter(s => !_BUILT_IN_STATUS_STYLE[s.id]);
  const idx = customs.findIndex(s => s.id === id);
  const palette = light ? _CUSTOM_STATUS_PALETTE_LIGHT : _CUSTOM_STATUS_PALETTE;
  return palette[Math.max(0, idx) % palette.length];
}

function switchView(view) {
  if (document.getElementById('grid-view').classList.contains('active')) exitGridMode();
  activeView = view;
  document.getElementById('session-view').style.display = view === 'sessions' ? '' : 'none';
  document.getElementById('board-view').style.display = view === 'board' ? '' : 'none';
  document.getElementById('calendar-view').style.display = view === 'calendar' ? '' : 'none';
  document.getElementById('reports-view').style.display = view === 'reports' ? '' : 'none';
  document.getElementById('notifications-view').style.display = view === 'notifications' ? '' : 'none';
  document.getElementById('files-view').style.display = view === 'files' ? 'flex' : 'none';
  document.getElementById('browser-view').style.display = view === 'browser' ? 'flex' : 'none';
  document.getElementById('logs-view').style.display = view === 'logs' ? 'flex' : 'none';
  document.getElementById('email-view').style.display = view === 'email' ? '' : 'none';
  document.getElementById('tab-sessions').classList.toggle('active', view === 'sessions');
  document.getElementById('tab-board').classList.toggle('active', view === 'board');
  document.getElementById('tab-calendar').classList.toggle('active', view === 'calendar');
  document.getElementById('tab-reports').classList.toggle('active', view === 'reports');
  document.getElementById('tab-notifications').classList.toggle('active', view === 'notifications');
  document.getElementById('tab-files').classList.toggle('active', view === 'files');
  document.getElementById('tab-browser').classList.toggle('active', view === 'browser');
  document.getElementById('tab-logs').classList.toggle('active', view === 'logs');
  document.getElementById('tab-email').classList.toggle('active', view === 'email');
  if (view === 'files') loadFiles(_filesPath);
  if (view === 'reports') fetchReports();
  if (view === 'browser') _rbLoadProfiles();
  if (view === 'email') _emailLoad();
  if (view === 'logs') { fetchLogs(); _startLogsTimer(); } else { _stopLogsTimer(); }
  if (view === 'board') {
    renderBoard();
    fetchBoard();
    // Only poll if SSE is not active (SSE pushes board updates)
    if (_sseFallback && !boardTimer) boardTimer = setInterval(fetchBoard, 5000);
  } else if (view === 'calendar') {
    Promise.all([fetchBoard(), fetchSchedules()]).then(() => renderCalendar());
  } else {
    if (boardTimer) { clearInterval(boardTimer); boardTimer = null; }
  }
}

// ── Logs tab ──────────────────────────────────────────────────────────────────
function escHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}
let _logsEvents = [];
let _logsRaw = '';
let _logsRawLines = [];
let _logsFilter = '';
let _logsSubtab = 'activity';
let logsSearchQuery = '';
let _logsTimer = null;

const _LOG_TYPE_CFG = {
  board:   {icon:'&#x1F4CB;', color:'#1a7f37',        label:'Board'},
  session: {icon:'&#x26A1;',  color:'var(--accent)',   label:'Session'},
  memory:  {icon:'&#x1F9E0;', color:'#bc8cff',         label:'Memory'},
  file:    {icon:'&#x1F4CE;', color:'#ff8c50',         label:'File'},
  system:  {icon:'&#x2699;',  color:'var(--dim)',       label:'System'},
  http:    {icon:'&#x1F517;', color:'var(--dim)',       label:'HTTP'},
};
const _LOG_ACTION_COLOR = {
  created:'#1a7f37', started:'#1a7f37', 'status-added':'#1a7f37',
  updated:'var(--accent)', configured:'var(--accent)', 'message-sent':'var(--accent)', claimed:'var(--accent)',
  deleted:'var(--red)', 'status-removed':'var(--red)',
  stopped:'#e07000', cleared:'#7d4e00',
  uploaded:'#bc8cff', pull:'var(--cyan)',
};

function _relTime(ts) {
  const d = Date.now()/1000 - ts;
  if (d < 5)    return 'now';
  if (d < 60)   return Math.round(d) + 's ago';
  if (d < 3600) return Math.round(d/60) + 'm ago';
  if (d < 86400)return Math.round(d/3600) + 'h ago';
  return Math.round(d/86400) + 'd ago';
}

function _evtTitle(evt) {
  if (evt.detail) return evt.detail;
  const tc = _LOG_TYPE_CFG[evt.type] || {label: evt.type};
  if (evt.target && evt.target !== evt.action && evt.target !== evt.type)
    return tc.label + ': ' + evt.action + ' \u2014 ' + evt.target;
  return tc.label + ': ' + evt.action;
}

async function fetchLogs() {
  const params = ['limit=500'];
  if (_logsFilter) params.push('category=' + encodeURIComponent(_logsFilter));
  try {
    const r = await fetch(API + '/api/logs?' + params.join('&'));
    if (!r.ok) return;
    const d = await r.json();
    const fetched = (d.events || []).map(e => ({
      ...e, type: e.type || e.category, target: e.target || e.detail, ip: e.ip || e.actor,
      status: e.level === 'error' ? 500 : (e.status || 200),
    }));
    // Merge: keep SSE-accumulated events that are newer than what the API returned,
    // then append fetched history. Deduplicate by (ts, action, type).
    const seen = new Set();
    const merged = [];
    for (const e of fetched) {
      const key = `${e.ts}|${e.action}|${e.type||''}`;
      if (!seen.has(key)) { seen.add(key); merged.push(e); }
    }
    for (const e of _logsEvents) {
      const key = `${e.ts}|${e.action}|${e.type||''}`;
      if (!seen.has(key)) { seen.add(key); merged.push(e); }
    }
    merged.sort((a, b) => (b.ts || 0) - (a.ts || 0));
    _logsEvents = merged.slice(0, 2000);
    renderActivity();
    if (_logsSubtab === 'stats') renderStats();
  } catch(e) {}
}

async function fetchRawLogs() {
  try {
    const r = await fetch(API + '/api/logs/raw?lines=300');
    if (!r.ok) return;
    const d = await r.json();
    _logsRawLines = d.lines || [];
    const cnt = document.getElementById('raw-line-count');
    if (cnt) cnt.textContent = (d.total||0).toLocaleString() + ' total lines';
    renderRawLogs();
  } catch(e) {}
}

function _stripAnsi(s) {
  return s.replace(/\x1b\[[0-9;]*[A-Za-z]/g, '').replace(/\x1b\][^\x07]*(\x07|$)/g, '');
}

function renderRawLogs() {
  const el = document.getElementById('logs-raw-body');
  if (!el) return;
  const filt = (document.getElementById('raw-filter')?.value || '').toLowerCase();
  const lines = filt
    ? _logsRawLines.filter(l => _stripAnsi(l).toLowerCase().includes(filt))
    : _logsRawLines;
  el.innerHTML = lines.map(l => {
    const clean = _stripAnsi(l);
    const isErr = /error|exception|traceback/i.test(clean);
    const isWarn = /warn/i.test(clean);
    const esc = clean.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
    if (isErr)  return '<span style="color:var(--red)">'  + esc + '</span>';
    if (isWarn) return '<span style="color:#e07000">' + esc + '</span>';
    if (/^\d{4}-\d{2}-\d{2}/.test(clean)) {
      const tEnd = clean.indexOf(' ', 20);
      if (tEnd > 0) {
        return '<span style="color:var(--dim)">' + escHtml(clean.slice(0,tEnd)) + '</span>' + escHtml(clean.slice(tEnd));
      }
    }
    return esc;
  }).join('\n');
}

function renderActivity() {
  const el = document.getElementById('logs-activity-body');
  if (!el) return;
  let evts = _logsEvents.slice();
  // Hide pure GET reads unless http filter is active
  if (_logsFilter !== 'http') evts = evts.filter(e => !(e.type === 'http' && e.action === 'get'));
  if (logsSearchQuery) {
    const q = logsSearchQuery.toLowerCase();
    evts = evts.filter(e =>
      (e.detail||'').toLowerCase().includes(q) ||
      (e.target||'').toLowerCase().includes(q) ||
      (e.session||'').toLowerCase().includes(q) ||
      e.action.toLowerCase().includes(q) ||
      e.type.toLowerCase().includes(q)
    );
  }
  if (!evts.length) {
    el.innerHTML = '<div style="padding:48px 16px;text-align:center;color:var(--dim);font-size:0.85rem;">' +
      (_logsFilter || logsSearchQuery ? 'No matching events' : 'No events yet \u2014 activity will appear as you use amux') + '</div>';
    return;
  }
  // Group by calendar day (most recent first, evts are already newest-first)
  const groups = new Map();
  for (const e of evts) {
    const day = new Date(e.ts*1000).toLocaleDateString([], {weekday:'short',month:'short',day:'numeric'});
    if (!groups.has(day)) groups.set(day, []);
    groups.get(day).push(e);
  }
  let html = '';
  for (const [day, dayEvts] of groups) {
    html += '<div class="logs-day-divider">' + escHtml(day) + '</div>';
    for (const evt of dayEvts) {
      const tc = _LOG_TYPE_CFG[evt.type] || {icon:'&#x2022;', color:'var(--dim)', label:evt.type};
      const isErr = evt.status >= 400;
      const ac = isErr ? 'var(--red)' : (_LOG_ACTION_COLOR[evt.action] || tc.color);
      const title = _evtTitle(evt);
      const meta = [];
      if (evt.session) meta.push('<span style="color:var(--accent)">' + escHtml(evt.session) + '</span>');
      if (evt.ip && evt.ip !== '127.0.0.1' && evt.ip !== '::1') meta.push(escHtml(evt.ip));
      if (isErr) meta.push('<span style="color:var(--red)">HTTP ' + evt.status + '</span>');
      html += '<div class="log-evt">' +
        '<div class="log-evt-icon">' + tc.icon + '</div>' +
        '<div class="log-evt-body">' +
          '<div class="log-evt-title">' + escHtml(title) + '</div>' +
          '<div class="log-evt-meta">' +
            '<span class="log-evt-badge" style="background:' + ac + '1a;color:' + ac + ';border:1px solid ' + ac + '55">' + escHtml(evt.action) + '</span>' +
            meta.join('') +
          '</div>' +
        '</div>' +
        '<div class="log-evt-ts" title="' + escHtml(new Date(evt.ts*1000).toLocaleString()) + '">' + _relTime(evt.ts) + '</div>' +
      '</div>';
    }
  }
  el.innerHTML = html;
}

function renderStats() {
  const el = document.getElementById('logs-stats-body');
  if (!el) return;
  if (!_logsEvents.length) {
    el.innerHTML = '<div style="color:var(--dim);font-size:0.85rem;padding:20px 0;">No event data yet.</div>';
    return;
  }
  const now = Date.now()/1000;
  const h24 = _logsEvents.filter(e => now - e.ts < 86400);
  const typeCounts = {}, sessionActivity = {};
  let errCount = 0, boardCreates = 0, sessionStarts = 0, msgSent = 0;
  for (const e of h24) {
    typeCounts[e.type] = (typeCounts[e.type]||0) + 1;
    if (e.session) sessionActivity[e.session] = (sessionActivity[e.session]||0) + 1;
    if (e.status >= 400) errCount++;
    if (e.type === 'board'   && e.action === 'created')      boardCreates++;
    if (e.type === 'session' && e.action === 'started')      sessionStarts++;
    if (e.type === 'session' && e.action === 'message-sent') msgSent++;
  }
  const cards = [
    {label:'Events (24h)',    value:h24.length,                icon:'&#x26A1;'},
    {label:'Total tracked',   value:_logsEvents.length,        icon:'&#x1F4CA;'},
    {label:'Errors (24h)',    value:errCount,                  icon:'&#x274C;', hi: errCount>0},
    {label:'Board created',   value:boardCreates,              icon:'&#x1F4CB;'},
    {label:'Sessions started',value:sessionStarts,             icon:'&#x25B6;'},
    {label:'Messages sent',   value:msgSent,                   icon:'&#x1F4AC;'},
  ];
  let html = '<div class="stats-grid">';
  for (const c of cards) {
    const color = c.hi ? 'var(--red)' : 'var(--text)';
    html += '<div class="stat-card"><div class="stat-card-label">' + c.icon + ' ' + escHtml(c.label) + '</div>' +
      '<div class="stat-card-value" style="color:' + color + '">' + c.value + '</div></div>';
  }
  html += '</div>';
  const topSessions = Object.entries(sessionActivity).sort((a,b)=>b[1]-a[1]).slice(0,6);
  if (topSessions.length) {
    html += '<div class="stat-card" style="margin-top:0;">';
    html += '<div style="font-size:0.73rem;font-weight:700;color:var(--text);margin-bottom:10px;">Most Active Sessions (24h)</div>';
    const maxVal = topSessions[0][1];
    for (const [s, cnt] of topSessions) {
      const pct = Math.round((cnt/maxVal)*100);
      html += '<div style="display:flex;align-items:center;gap:8px;margin-bottom:7px;">' +
        '<span style="font-size:0.78rem;color:var(--accent);min-width:100px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">' + escHtml(s) + '</span>' +
        '<div style="flex:1;background:var(--bg);border-radius:3px;height:5px;overflow:hidden;">' +
          '<div style="height:100%;width:' + pct + '%;background:var(--accent);border-radius:3px;transition:width 0.3s;"></div>' +
        '</div>' +
        '<span style="font-size:0.71rem;color:var(--dim);min-width:28px;text-align:right;">' + cnt + '</span>' +
      '</div>';
    }
    html += '</div>';
  }
  // Recent action breakdown
  const actionCounts = {};
  for (const e of h24) {
    if (e.type !== 'http') {
      const k = e.type + '.' + e.action;
      actionCounts[k] = (actionCounts[k]||0) + 1;
    }
  }
  const topActions = Object.entries(actionCounts).sort((a,b)=>b[1]-a[1]).slice(0,8);
  if (topActions.length) {
    html += '<div class="stat-card">';
    html += '<div style="font-size:0.73rem;font-weight:700;color:var(--text);margin-bottom:10px;">Top Actions (24h)</div>';
    for (const [k, cnt] of topActions) {
      const [type, action] = k.split('.');
      const tc = _LOG_TYPE_CFG[type] || {icon:'&#x2022;', color:'var(--dim)'};
      const ac = _LOG_ACTION_COLOR[action] || tc.color;
      html += '<div style="display:flex;align-items:center;gap:8px;margin-bottom:5px;">' +
        '<span>' + tc.icon + '</span>' +
        '<span class="log-evt-badge" style="background:' + ac + '1a;color:' + ac + ';border:1px solid ' + ac + '55">' + escHtml(action) + '</span>' +
        '<span style="font-size:0.71rem;color:var(--dim);">' + escHtml(type) + '</span>' +
        '<span style="font-size:0.78rem;color:var(--text);margin-left:auto;">' + cnt + '</span>' +
      '</div>';
    }
    html += '</div>';
  }
  el.innerHTML = html;
}

function logsSetTab(tab) {
  _logsSubtab = tab;
  ['activity','raw','stats'].forEach(t => {
    document.getElementById('lst-' + t)?.classList.toggle('active', t === tab);
    const p = document.getElementById('logs-' + t);
    if (p) p.style.display = t === tab ? '' : 'none';
  });
  if (tab === 'raw') fetchRawLogs();
  else if (tab === 'stats') { fetchLogs(); }
  else renderActivity();
}

function logsSetFilter(btn, filter) {
  _logsFilter = filter;
  document.querySelectorAll('.lf-btn').forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
  fetchLogs();
}

function _startLogsTimer() {
  if (_logsTimer) return;
  _logsTimer = setInterval(() => {
    if (activeView !== 'logs') return;
    if (_logsSubtab === 'raw') fetchRawLogs();
    else fetchLogs();
  }, 5000);
}
function _stopLogsTimer() {
  if (_logsTimer) { clearInterval(_logsTimer); _logsTimer = null; }
}
// ── end Logs tab ──────────────────────────────────────────────────────────────

async function fetchSchedules() {
  try {
    const r = await fetch(API + '/api/schedules');
    if (r.ok) { schedules = await r.json(); }
  } catch(e) {}
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
  // Use marked.js for full GFM support (tables, task lists, strikethrough, etc.)
  if (typeof marked !== 'undefined') {
    try {
      let html = marked.parse(raw, { gfm: true, breaks: true });
      html = html.replace(/<table>/g, '<div class="table-scroll"><table>').replace(/<\/table>/g, '</table></div>');
      return html;
    } catch(e) { /* fall through to basic renderer */ }
  }
  // Fallback: basic renderer if marked.js failed to load
  const parts = raw.split(/(```[\s\S]*?```)/g);
  return parts.map((part, i) => {
    if (i % 2 === 1) {
      const m = part.match(/^```(\w*)\n?([\s\S]*?)```$/);
      const code = m ? esc(m[2]) : esc(part);
      return '<pre><code>' + code + '</code></pre>';
    }
    let s = part.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
    s = s.replace(/^### (.+)$/gm, '<h3>$1</h3>');
    s = s.replace(/^## (.+)$/gm, '<h2>$1</h2>');
    s = s.replace(/^# (.+)$/gm, '<h1>$1</h1>');
    s = s.replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>');
    s = s.replace(/\*(.+?)\*/g, '<em>$1</em>');
    s = s.replace(/`([^`]+)`/g, '<code>$1</code>');
    s = s.replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_, text, url) => {
      const safe = /^(https?:\/\/|\/|#)/.test(url) ? url : '#';
      return '<a href="' + safe + '" target="_blank">' + text + '</a>';
    });
    s = s.replace(/^---$/gm, '<hr>');
    s = s.replace(/^&gt; (.+)$/gm, '<blockquote>$1</blockquote>');
    s = s.replace(/((?:^[-*] .+\n?)+)/gm, (block) => {
      const items = block.trim().split('\n').map(l => '<li>' + l.replace(/^[-*] /, '') + '</li>').join('');
      return '<ul>' + items + '</ul>';
    });
    s = s.replace(/\n\n+/g, '</p><p>');
    s = s.replace(/\n/g, '<br>');
    if (s && !s.startsWith('<h') && !s.startsWith('<ul') && !s.startsWith('<blockquote') && !s.startsWith('<hr') && !s.startsWith('<pre')) {
      s = '<p>' + s + '</p>';
    }
    return s;
  }).join('');
}

// ── Tag input widget (shared by add-modal "be" and detail view "bd") ──
const _tagState = { be: [], bd: [] };

function _beTagRenderChips(prefix) {
  const tags = _tagState[prefix];
  const wrap = document.getElementById(prefix + '-tag-wrap');
  const inp = document.getElementById(prefix + '-tag-input');
  if (!wrap || !inp) return;
  [...wrap.children].forEach(c => { if (c !== inp) c.remove(); });
  tags.forEach(t => {
    const chip = document.createElement('span');
    chip.className = 'be-tag-chip';
    chip.innerHTML = esc(t) + '<span class="be-tag-chip-remove" onclick="event.stopPropagation();_beTagRemove(' + JSON.stringify(prefix) + ',' + JSON.stringify(t) + ')">\u00d7</span>';
    wrap.insertBefore(chip, inp);
  });
  inp.placeholder = tags.length ? '' : 'Add tag\u2026';
}

function _beTagInputUpdate(prefix) {
  const inp = document.getElementById(prefix + '-tag-input');
  const q = inp ? inp.value.toLowerCase() : '';
  const allTags = [...new Set(boardItems.flatMap(i => i.tags || []))].sort();
  const suggestions = allTags.filter(t => !_tagState[prefix].includes(t) && (!q || t.toLowerCase().includes(q)));
  const el = document.getElementById(prefix + '-tag-suggestions');
  if (!el) return;
  el.innerHTML = suggestions.map(t =>
    '<button class="be-tag-suggestion" onclick="_beTagAdd(' + JSON.stringify(prefix) + ',' + JSON.stringify(t) + ');document.getElementById(' + JSON.stringify(prefix + '-tag-input') + ').value=\'\';_beTagInputUpdate(' + JSON.stringify(prefix) + ')">' + esc(t) + '</button>'
  ).join('');
}

function _beTagAdd(prefix, tag) {
  tag = tag.trim().replace(/,/g, '');
  if (!tag || _tagState[prefix].includes(tag)) return;
  _tagState[prefix].push(tag);
  _beTagRenderChips(prefix);
  _beTagInputUpdate(prefix);
}

function _beTagRemove(prefix, tag) {
  _tagState[prefix] = _tagState[prefix].filter(t => t !== tag);
  _beTagRenderChips(prefix);
  _beTagInputUpdate(prefix);
}

function _beTagKeydown(e, prefix) {
  if (e.key === 'Enter' || e.key === ',') {
    e.preventDefault();
    const val = e.target.value.trim().replace(/,/g, '');
    if (val) { _beTagAdd(prefix, val); e.target.value = ''; _beTagInputUpdate(prefix); }
  } else if (e.key === 'Backspace' && !e.target.value && _tagState[prefix].length) {
    _tagState[prefix].pop();
    _beTagRenderChips(prefix);
    _beTagInputUpdate(prefix);
  }
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

function boardColDrop(e, col) {
  e.preventDefault();
  e.currentTarget.classList.remove('drag-over');
  if (_boardDragId) {
    const item = boardItems.find(i => i.id === _boardDragId);
    if (item && item.status !== col) moveBoardItem(_boardDragId, col);
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
  const pinned = item.pinned ? 1 : 0;
  let h = '<div class="board-card' + (pinned ? ' board-card-pinned' : '') + '" data-id="' + item.id + '" onclick="openBoardDetail(\'' + item.id + '\')">';
  h += '<div class="board-drag-handle" onclick="event.stopPropagation()" title="Drag to move"><svg width="12" height="12" viewBox="0 0 12 12" fill="currentColor"><circle cx="3.5" cy="2.5" r="1.25"/><circle cx="8.5" cy="2.5" r="1.25"/><circle cx="3.5" cy="6" r="1.25"/><circle cx="8.5" cy="6" r="1.25"/><circle cx="3.5" cy="9.5" r="1.25"/><circle cx="8.5" cy="9.5" r="1.25"/></svg></div>';
  h += '<button class="board-pin-btn' + (pinned ? ' active' : '') + '" onclick="event.stopPropagation();_togglePin(\'' + item.id + '\')" title="' + (pinned ? 'Unpin' : 'Pin to top') + '">&#x1F4CC;</button>';
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

async function _togglePin(id) {
  const item = boardItems.find(i => i.id === id);
  if (!item) return;
  const newVal = item.pinned ? 0 : 1;
  await fetch(API + '/api/board/' + id, {
    method: 'PATCH', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ pinned: newVal }),
  });
}

function _renderBoardBySession(visible, container) {
  // Separate human (yours) vs agent items
  const humanItems = visible.filter(i => i.owner_type !== 'agent');
  const agentVisible = visible.filter(i => i.owner_type === 'agent');
  // Group agent items by session
  const groups = {};
  const noSession = [];
  agentVisible.forEach(function(item) {
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
    stCol.sort((a, b) => (b.pinned || 0) - (a.pinned || 0));
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
        animation: 120,
        easing: 'cubic-bezier(0.2, 0, 0, 1)',
        handle: '.board-drag-handle',
        ghostClass: 'board-sortable-ghost',
        chosenClass: 'board-sortable-chosen',
        dragClass: 'board-sortable-drag',
        filter: '.board-col-header, .board-add-btn, .board-empty',
        preventOnFilter: false,
        delay: 120,
        delayOnTouchOnly: true,
        touchStartThreshold: 3,
        swapThreshold: 0.6,
        onStart: function() { document.body.classList.add('board-dragging'); },
        onEnd: function(evt) {
          document.body.classList.remove('board-dragging');
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

// ── Schedule modal ────────────────────────────────────────────────────────────
function updateSchedTypeUI() {
  const t = document.getElementById('sched-type').value;
  document.getElementById('sched-once-fields').style.display = t === 'once' ? '' : 'none';
  document.getElementById('sched-rec-fields').style.display = t === 'recurring' ? '' : 'none';
}
function updateSchedRecUI() {
  const rec = document.getElementById('sched-recurrence').value;
  document.getElementById('sched-weekday-field').style.display = rec === 'weekly' ? '' : 'none';
  document.getElementById('sched-monthday-field').style.display = rec === 'monthly' ? '' : 'none';
  document.getElementById('sched-time-field').style.display = rec === 'hourly' ? 'none' : '';
}
function openSchedModal(editId) {
  _schedEditId = editId || null;
  const overlay = document.getElementById('sched-overlay');
  // Populate session list
  const sel = document.getElementById('sched-session');
  sel.innerHTML = (sessions || []).map(s => `<option value="${esc(s.name)}">${esc(s.name)}</option>`).join('');
  if (editId) {
    const s = schedules.find(x => x.id === editId);
    if (s) {
      document.getElementById('sched-title').value = s.title;
      sel.value = s.session;
      document.getElementById('sched-command').value = s.command;
      document.getElementById('sched-type').value = s.sched_type;
      document.getElementById('sched-run-at').value = s.run_at || '';
      if (s.recurrence) document.getElementById('sched-recurrence').value = s.recurrence;
    }
    document.getElementById('sched-save-btn').textContent = 'Update';
  } else {
    document.getElementById('sched-title').value = '';
    document.getElementById('sched-command').value = '';
    document.getElementById('sched-type').value = 'once';
    document.getElementById('sched-run-at').value = new Date(Date.now() + 3600000).toISOString().slice(0,16);
    document.getElementById('sched-save-btn').textContent = 'Save';
  }
  updateSchedTypeUI();
  updateSchedRecUI();
  overlay.style.display = 'flex';
  requestAnimationFrame(() => overlay.classList.add('active'));
  setTimeout(() => document.getElementById('sched-title').focus(), 50);
}
function closeSchedModal() {
  const overlay = document.getElementById('sched-overlay');
  overlay.classList.remove('active');
  setTimeout(() => { overlay.style.display = 'none'; }, 250);
  _schedEditId = null;
}
async function saveSchedModal() {
  const title = document.getElementById('sched-title').value.trim();
  const session = document.getElementById('sched-session').value;
  const command = document.getElementById('sched-command').value.trim();
  const stype = document.getElementById('sched-type').value;
  if (!title || !session || !command) return;
  let run_at, recurrence;
  if (stype === 'once') {
    run_at = document.getElementById('sched-run-at').value;
  } else {
    recurrence = document.getElementById('sched-recurrence').value;
    const time = document.getElementById('sched-time').value || '09:00';
    if (recurrence === 'weekly') {
      const wd = document.getElementById('sched-weekday').value;
      run_at = wd + ':' + time;
    } else if (recurrence === 'monthly') {
      const md = document.getElementById('sched-monthday').value || '1';
      run_at = md + ':' + time;
    } else {
      run_at = time;
    }
  }
  const payload = { title, session, command, sched_type: stype, recurrence: recurrence || null, run_at };
  const url = _schedEditId ? API + '/api/schedules/' + _schedEditId : API + '/api/schedules';
  const method = _schedEditId ? 'PATCH' : 'POST';
  const r = await fetch(url, { method, headers: {'Content-Type':'application/json'}, body: JSON.stringify(payload) });
  if (r.ok) {
    await fetchSchedules();
    renderCalendar();
    closeSchedModal();
  }
}
async function deleteSchedule(id) {
  if (!confirm('Delete this schedule?')) return;
  await fetch(API + '/api/schedules/' + id, { method: 'DELETE' });
  await fetchSchedules();
  renderCalendar();
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
  const dueTimeEl2 = document.getElementById('be-due-time');
  if (dueTimeEl2) dueTimeEl2.value = '';
  const sel = document.getElementById('be-status');
  sel.innerHTML = boardStatuses.map(s => '<option value="' + s.id + '">' + esc(s.label) + '</option>').join('');
  sel.value = status;
  _populateSessionSelect('be-session-add', peekSession || '');
  _tagState['be'] = [];
  _beTagRenderChips('be');
  _beTagInputUpdate('be');
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
  const tags = [..._tagState['be']];
  const dueEl = document.getElementById('be-due');
  const due = dueEl ? dueEl.value : '';
  const dueTimeEl = document.getElementById('be-due-time');
  const dueTime = dueTimeEl ? dueTimeEl.value : '';
  closeBoardEdit();
  await addBoardItem(title, desc, status, session, tags, due, undefined, dueTime);
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
  const dueTimeEl = document.getElementById('bd-due-time');
  if (dueTimeEl) dueTimeEl.value = draft ? (draft.due_time || '') : (item.due_time || '');
  boardDetailTab('edit');
  _tagState['bd'] = [...(item.tags || [])];
  _beTagRenderChips('bd');
  _beTagInputUpdate('bd');
  const meta = document.getElementById('bd-meta');
  const parts = [];
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
      const dueTimeEl2 = document.getElementById('bd-due-time');
      const due_time = dueTimeEl2 ? dueTimeEl2.value : (item.due_time || '');
      // Only save draft if something actually differs from saved state
      if (t !== (item.title || '') || d !== (item.desc || '') || s !== (item.session || '') || st !== (item.status || 'todo') || due !== (item.due || '') || due_time !== (item.due_time || '')) {
        _boardDrafts[boardDetailId] = { title: t, desc: d, session: s, status: st, due, due_time };
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
  const dueTimeInput = document.getElementById('bd-due-time');
  const changes = { title, desc, status: boardDetailStatus, due: dueInput ? dueInput.value : '', due_time: dueTimeInput ? dueTimeInput.value : '', tags: [..._tagState['bd']] };
  if (session !== undefined) changes.session = session;
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

async function addBoardItem(title, desc, status, session, tags, due, ownerType, dueTime) {
  ownerType = ownerType || 'human';
  const tempId = Math.random().toString(16).slice(2, 8);
  const now = Math.floor(Date.now() / 1000);
  const tempItem = { id: tempId, title, desc, status, session: session || '', tags: tags || [], due: due || '', due_time: dueTime || '', creator: _getDeviceName(), owner_type: ownerType, created: now, updated: now, _pending: true };
  boardItems.push(tempItem);
  saveBoardCache();
  renderBoard();
  const r = await apiCall(API + '/api/board', {
    method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ title, desc, status, session: session || '', tags: tags || [], due: due || '', due_time: dueTime || '', creator: _getDeviceName(), owner_type: ownerType })
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

function moveBoardItem(id, newStatus) {
  // Optimistic: update in-memory + cache immediately; Sortable already moved the DOM
  const idx = boardItems.findIndex(i => i.id === id);
  if (idx >= 0) boardItems[idx] = { ...boardItems[idx], status: newStatus, updated: Math.floor(Date.now() / 1000) };
  saveBoardCache();
  // Sync to backend silently — no renderBoard() so the drag stays smooth
  apiCall(API + '/api/board/' + id, {
    method: 'PATCH', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({ status: newStatus })
  }).then(r => {
    if (!r) return;
    r.json().then(updated => {
      const i = boardItems.findIndex(b => b.id === id);
      if (i >= 0) boardItems[i] = updated;
      saveBoardCache();
    });
  });
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
  const s3Url = window._AMUX_S3_ICAL_URL || '';
  const origin = window.location.origin;
  const localUrl = origin + '/api/calendar.ics';
  const isLocal = origin.includes('localhost') || origin.includes('127.0.0.1');
  // Prefer S3 URL for subscriptions (publicly reachable); fall back to local
  const subUrl = s3Url || localUrl;
  const googleUrl = 'https://calendar.google.com/calendar/r/settings/addbyurl?' +
    'url=' + encodeURIComponent(subUrl);
  const appleUrl = s3Url ? ('webcal://' + s3Url.replace(/^https?:\/\//, '')) :
    ('webcal://' + window.location.host + '/api/calendar.ics');

  function ical_url_esc(s) { return s.replace(/&/g,'&amp;').replace(/</g,'&lt;'); }

  let html = '<div style="font-size:0.9rem;line-height:1.7;">';
  html += '<p style="margin-bottom:0.8rem;font-weight:600;">Subscribe to amux calendar</p>';

  if (s3Url) {
    html += '<p style="margin-bottom:0.6rem;font-size:0.82rem;">Subscription URL (public S3):</p>';
    html += '<code style="display:block;background:var(--card-bg);padding:0.5rem 0.8rem;border-radius:6px;font-size:0.78rem;margin-bottom:1rem;word-break:break-all;">' + ical_url_esc(s3Url) + '</code>';
    html += '<div style="display:flex;gap:0.5rem;flex-wrap:wrap;">';
    html += '<a href="' + googleUrl + '" target="_blank" class="btn" style="font-size:0.8rem;">Add to Google Calendar</a>';
    html += '<a href="' + appleUrl + '" class="btn" style="font-size:0.8rem;">Add to Apple Calendar</a>';
    html += '</div>';
  } else if (isLocal) {
    html += '<p style="margin-bottom:0.8rem;color:var(--muted);font-size:0.82rem;">⚠️ You\'re on localhost — Google and Apple Calendar can\'t reach this URL directly.</p>';
    html += '<p style="margin-bottom:0.8rem;font-size:0.82rem;">Set <code>AMUX_S3_BUCKET</code> to enable a publicly reachable subscription URL via S3.</p>';
  } else {
    html += '<p style="margin-bottom:0.6rem;font-size:0.82rem;">Feed URL:</p>';
    html += '<code style="display:block;background:var(--card-bg);padding:0.5rem 0.8rem;border-radius:6px;font-size:0.78rem;margin-bottom:1rem;word-break:break-all;">' + ical_url_esc(localUrl) + '</code>';
    html += '<div style="display:flex;gap:0.5rem;flex-wrap:wrap;">';
    html += '<a href="' + googleUrl + '" target="_blank" class="btn" style="font-size:0.8rem;">Add to Google Calendar</a>';
    html += '<a href="' + appleUrl + '" class="btn" style="font-size:0.8rem;">Add to Apple Calendar</a>';
    html += '</div>';
  }

  // Always offer direct download
  html += '<hr style="margin:0.9rem 0;border:none;border-top:1px solid var(--border);">';
  html += '<a href="/api/calendar.ics" download="amux.ics" class="btn" style="font-size:0.8rem;">&#x2193; Download .ics file</a>';
  html += '</div>';

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
  (schedules || []).forEach(s => {
    if (!s.deleted && s.enabled && s.next_run) {
      const dateStr = s.next_run.slice(0, 10);
      if (!map[dateStr]) map[dateStr] = [];
      const time = s.next_run.slice(11, 16);
      map[dateStr].push({ ...s, _isSched: true, due: dateStr,
        title: '⏰ ' + s.title + (time ? ' ' + time : ''),
        status: 'sched' });
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
        if (item._isSched) {
          html += '<div class="cal-chip sched-chip"'
                + ' onclick="event.stopPropagation();openSchedModal(\'' + item.id + '\')"'
                + ' title="' + esc(item.title) + '">' + esc(item.title) + '</div>';
        } else {
          const sty = statusStyle(item.status || 'todo');
          html += '<div class="cal-chip" style="background:' + sty.bg + ';color:' + sty.color + '"'
                + ' onclick="event.stopPropagation();openBoardDetail(\'' + item.id + '\')"'
                + ' title="' + esc(item.title) + '">' + esc(item.title) + '</div>';
        }
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
      if (item._isSched) {
        html += '<div class="cal-week-chip sched-chip"'
              + ' onclick="event.stopPropagation();openSchedModal(\'' + item.id + '\')"'
              + ' title="' + esc(item.title) + '">' + esc(item.title) + '</div>';
        html += '<div class="cal-week-dot" style="background:#c084fc"></div>';
      } else {
        const sty = statusStyle(item.status || 'todo');
        html += '<div class="cal-week-chip" style="background:' + sty.bg + ';color:' + sty.color + '"'
              + ' onclick="event.stopPropagation();openBoardDetail(\'' + item.id + '\')"'
              + ' title="' + esc(item.title) + '">' + esc(item.title) + '</div>';
        html += '<div class="cal-week-dot" style="background:' + sty.color + '"></div>';
      }
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
      if (item._isSched) {
        html += '<div class="cal-day-issue" onclick="openSchedModal(\'' + item.id + '\')">';
        html += '<span class="cal-dot" style="background:#c084fc;width:8px;height:8px;flex-shrink:0;border-radius:50%;margin-top:5px;"></span>';
        html += '<div class="cal-day-issue-text">';
        html += '<div class="cal-day-issue-title">' + esc(item.title) + '</div>';
        if (item.desc) html += '<div class="cal-day-issue-desc">' + esc(item.desc) + '</div>';
        html += '</div>';
        html += '<span style="font-size:0.7rem;padding:2px 7px;border-radius:10px;background:rgba(163,113,247,0.18);color:#c084fc;flex-shrink:0;">schedule</span>';
        html += '</div>';
      } else {
        const sty = statusStyle(item.status || 'todo');
        html += '<div class="cal-day-issue" onclick="openBoardDetail(\'' + item.id + '\')">';
        html += '<span class="cal-dot" style="background:' + sty.color + ';width:8px;height:8px;flex-shrink:0;border-radius:50%;margin-top:5px;"></span>';
        html += '<div class="cal-day-issue-text">';
        html += '<div class="cal-day-issue-title">' + esc(item.title) + '</div>';
        if (item.desc) html += '<div class="cal-day-issue-desc">' + esc(item.desc) + '</div>';
        html += '</div>';
        html += '<span style="font-size:0.7rem;padding:2px 7px;border-radius:10px;background:' + sty.bg + ';color:' + sty.color + ';flex-shrink:0;">' + esc(item.status || 'todo') + '</span>';
        html += '</div>';
      }
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
  // Position below the tab bar so both header and tabs remain visible
  const tabBar = document.querySelector('.tab-bar');
  const ref = tabBar || document.querySelector('.header-row');
  if (ref) {
    const rect = ref.getBoundingClientRect();
    view.style.top = rect.bottom + 'px';
  }
  view.classList.add('active');
  // Mark Grid tab as active, deactivate others
  ['sessions','board','calendar','reports','notifications'].forEach(t => document.getElementById('tab-' + t)?.classList.remove('active'));
  document.getElementById('tab-grid').classList.add('active');
  _renderGridChips();
  _wsRenderProfileBar();
  _wsRestoreProfilesFromIdb(); // async, no-op if localStorage is already populated
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
      '<button class="gp-peek-btn" onclick="openPeek(\'' + safeName + '\');event.stopPropagation();" title="Open in peek">&#x2197;</button>' +
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
        '<div class="chip" onclick="doSend(\'' + safeName + '\',\'continue\')">continue</div>' +
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
  // Attach URL click handler (same as peek body — ensures links open in new tab in PWA mode)
  const gpBody = document.getElementById(sid + '-body');
  if (gpBody) {
    gpBody.addEventListener('click', _peekOpenLink);
    gpBody.addEventListener('touchend', _peekOpenLink, {passive: false});
  }
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
  const j = JSON.stringify(profiles);
  localStorage.setItem(_wsProfilesKey(), j);
  // Mirror to IDB so profiles survive iOS localStorage purges
  _idb.set(_wsProfilesKey(), j);
}

// Restore profiles from IDB into localStorage if localStorage was purged
async function _wsRestoreProfilesFromIdb() {
  const key = _wsProfilesKey();
  if (localStorage.getItem(key)) return; // already have it
  const val = await _idb.get(key);
  if (val) {
    localStorage.setItem(key, val);
    _wsRenderProfileBar();
  }
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
  } else if (e.key === 'ArrowUp' && inp && inp.selectionStart === 0) {
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
    const req = indexedDB.open('amux', 4);
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
      if (!d.objectStoreNames.contains('files')) {
        d.createObjectStore('files', { keyPath: 'path' });
      }
    };
    req.onsuccess = () => { db = req.result; db.onversionchange = () => { db.close(); db = null; }; resolve(db); };
    req.onerror = () => reject(req.error);
    req.onblocked = () => { if (db) { db.close(); db = null; } };
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
    // Files offline cache
    setFile: (path, val) => open().then(d => {
      const tx = d.transaction('files', 'readwrite');
      tx.objectStore('files').put({ ...val, path, ts: Date.now() });
    }).catch(() => {}),
    getFile: (path) => open().then(d => new Promise((resolve) => {
      const tx = d.transaction('files', 'readonly');
      const req = tx.objectStore('files').get(path);
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => resolve(null);
    })).catch(() => null),
    clearFiles: () => _txw('files', os => os.clear()),
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
          const firstLoad = !lastSessionsJSON;
          lastSessionsJSON = j;
          sessions = msg.payload;
          localStorage.setItem('amux_sessions_cache', j);
          render();
          // If workspace is open but no panes were restored yet (e.g. sessions
          // cache was empty on startup), retry restoration now that we have data.
          if (firstLoad && _grid && Object.keys(_gridPanes).length === 0) {
            _gridRestoreLayout();
          }
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
      } else if (msg.type === 'logs') {
        // Always accumulate log events even when tab is not active
        const newEvts = (msg.payload || []).map(e => ({
          ...e, type: e.type || e.category, target: e.target || e.detail, ip: e.ip || e.actor,
          status: e.level === 'error' ? 500 : (e.status || 200),
        }));
        if (newEvts.length) {
          _logsEvents = newEvts.concat(_logsEvents);
          if (_logsEvents.length > 2000) _logsEvents = _logsEvents.slice(0, 2000);
          if (activeView === 'logs') renderActivity();
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
_updateNotifBtn();
_checkTranscription();

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
      list.innerHTML = '<div style="color:var(--dim);font-size:0.85rem;text-align:center;padding:20px;">No skills yet. Click <b>+ New skill</b> to create one.</div>';
      return;
    }
    list.innerHTML = skills.map(s => {
      const cmd = '/' + esc(s.name);
      const hint = s.hint ? '<div class="skill-card-hint">' + esc(s.hint) + '</div>' : '';
      return '<div class="skill-card" style="cursor:pointer;" onclick="editSkill(\'' + esc(s.name) + '\')">' +
        '<div style="display:flex;align-items:center;justify-content:space-between;">' +
          '<span class="skill-card-name">' + cmd + '</span>' +
          '<button class="btn" style="font-size:0.65rem;padding:2px 8px;" onclick="event.stopPropagation();navigator.clipboard.writeText(\'' + esc(s.name) + '\');showToast(\'Copied!\')">Copy</button>' +
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
let _skillEditName = null;
async function editSkill(name) {
  _skillEditName = name || null;
  const modal = document.getElementById('skill-edit-modal');
  const nameInput = document.getElementById('skill-edit-name');
  const contentInput = document.getElementById('skill-edit-content');
  const delBtn = document.getElementById('skill-delete-btn');
  if (name) {
    document.getElementById('skill-edit-title').textContent = 'Edit: /' + name;
    nameInput.value = name;
    nameInput.readOnly = true;
    delBtn.style.display = '';
    try {
      const data = await fetch(API + '/api/skills/' + encodeURIComponent(name)).then(r => r.json());
      contentInput.value = data.content || '';
    } catch(e) { contentInput.value = ''; }
  } else {
    document.getElementById('skill-edit-title').textContent = 'New skill';
    nameInput.value = '';
    nameInput.readOnly = false;
    contentInput.value = '---\ndescription: \nallowed-tools: Bash, Read, Edit\nargument-hint: [args]\n---\n\n# Instructions\n\nThe user\'s request is: **$ARGUMENTS**\n';
    delBtn.style.display = 'none';
  }
  modal.classList.add('active');
  setTimeout(() => (name ? contentInput : nameInput).focus(), 50);
}
function closeSkillEdit() {
  document.getElementById('skill-edit-modal').classList.remove('active');
}
async function saveSkill() {
  const name = document.getElementById('skill-edit-name').value.trim().replace(/[^a-zA-Z0-9_-]/g, '-');
  const content = document.getElementById('skill-edit-content').value;
  if (!name) { showToast('Name required'); return; }
  if (!content.trim()) { showToast('Content required'); return; }
  const r = await fetch(API + '/api/skills/' + encodeURIComponent(name), {
    method: 'POST', headers: {'Content-Type':'application/json'},
    body: JSON.stringify({ content })
  });
  if (r.ok) {
    showToast('Saved /' + name);
    closeSkillEdit();
    openSkills();  // refresh list
  } else {
    showToast('Save failed');
  }
}
async function deleteSkill() {
  const name = _skillEditName;
  if (!name) return;
  const r = await fetch(API + '/api/skills/' + encodeURIComponent(name), { method: 'DELETE' });
  if (r.ok) {
    showToast('Deleted /' + name);
    closeSkillEdit();
    openSkills();
  }
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

// Bootstrap: on page load, read ?_sync= param and merge server list + prefs into localStorage
(function _bootstrapFromUrl() {
  const params = new URLSearchParams(location.search);
  const raw = params.get('_sync');
  if (!raw) return;
  try {
    const data = JSON.parse(atob(raw));
    // Merge server list (dedupe by URL)
    if (Array.isArray(data.servers)) {
      const existing = _getSavedServers();
      data.servers.forEach(s => {
        if (s && s.url && !existing.some(e => e.url.replace(/\/+$/, '') === s.url.replace(/\/+$/, ''))) {
          existing.push(s);
        }
      });
      _saveServers(existing);
    }
    // Restore device name only if not locally set
    if (data.deviceName && !localStorage.getItem('amux_device_name')) {
      localStorage.setItem('amux_device_name', data.deviceName);
    }
  } catch(e) {}
  // Clean the URL without reloading
  params.delete('_sync');
  const clean = location.pathname + (params.toString() ? '?' + params.toString() : '') + location.hash;
  history.replaceState({}, '', clean);
})();

function renderServerList() {
  const list = document.getElementById('server-list');
  const servers = _getSavedServers();
  const current = location.origin;
  const isPWA = navigator.standalone || window.matchMedia('(display-mode: standalone)').matches;
  // Pre-compute sync payload for <a> tags
  const allServers = [...servers];
  if (!allServers.some(srv => srv.url.replace(/\/+$/, '') === current)) {
    allServers.push({ name: location.host, url: current });
  }
  const payload = btoa(JSON.stringify({
    servers: allServers,
    deviceName: localStorage.getItem('amux_device_name') || ''
  }));
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
    const syncUrl = s.url + '/?_sync=' + encodeURIComponent(payload);
    html += '<a style="display:flex;align-items:center;justify-content:space-between;padding:6px 8px;border-radius:6px;margin-bottom:4px;cursor:pointer;transition:background 0.12s;text-decoration:none;color:inherit;" href="' + esc(syncUrl) + '" onclick="_switchServerUrl(' + i + ',event);">';
    html += '<div style="min-width:0;flex:1;">';
    html += '<div style="font-size:0.75rem;font-weight:500;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">' + esc(s.name || s.url) + '</div>';
    if (s.name) html += '<div style="font-size:0.65rem;color:var(--dim);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">' + esc(s.url.replace(/^https?:\/\//, '')) + '</div>';
    html += '</div>';
    html += '<button class="btn" style="font-size:0.6rem;padding:1px 6px;flex-shrink:0;margin-left:8px;" onclick="event.preventDefault();event.stopPropagation();removeServer(' + i + ');renderServerList()">&#x2715;</button>';
    html += '</a>';
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

function _normalizeServerUrl(url) {
  if (!/^https?:\/\//.test(url)) url = 'https://' + url;
  return url.replace(/\/+$/, '');
}
function _ensureCurrentServerSaved(servers) {
  // Auto-register the current server so the destination can switch back
  const cur = location.origin;
  if (!servers.some(s => s.url.replace(/\/+$/, '') === cur)) {
    servers.push({ name: location.host, url: cur });
  }
}
function saveNewServer() {
  const name = document.getElementById('add-server-name').value.trim();
  let url = _normalizeServerUrl(document.getElementById('add-server-url').value.trim());
  if (!url) { showToast('URL is required'); return; }
  const servers = _getSavedServers();
  if (servers.some(s => s.url.replace(/\/+$/, '') === url)) { showToast('Server already saved'); return; }
  _ensureCurrentServerSaved(servers);
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
  if (typeof renderSettingsServerList === 'function') renderSettingsServerList();
}

// Called from server list <a> onclick — handles PWA cross-origin navigation
function _switchServerUrl(idx, evt) {
  evt && evt.preventDefault();
  const servers = _getSavedServers();
  const s = servers[idx];
  if (!s) return;
  const currentOrigin = location.origin;
  const allServers = [...servers];
  if (!allServers.some(srv => srv.url.replace(/\/+$/, '') === currentOrigin)) {
    allServers.push({ name: location.host, url: currentOrigin });
  }
  const payload = btoa(JSON.stringify({
    servers: allServers,
    deviceName: localStorage.getItem('amux_device_name') || ''
  }));
  const url = s.url.replace(/\/+$/, '') + '/?_sync=' + encodeURIComponent(payload);
  const isPWA = navigator.standalone || window.matchMedia('(display-mode: standalone)').matches;
  if (isPWA) {
    window.open(url, '_blank');
  } else {
    location.href = url;
  }
}

function switchServer(idx) {
  const servers = _getSavedServers();
  const s = servers[idx];
  if (!s) return;
  // Ensure current server is in the list we pass so the destination can switch back
  const currentOrigin = location.origin;
  const allServers = [...servers];
  if (!allServers.some(srv => srv.url.replace(/\/+$/, '') === currentOrigin)) {
    allServers.push({ name: location.host, url: currentOrigin });
  }
  const payload = btoa(JSON.stringify({
    servers: allServers,
    deviceName: localStorage.getItem('amux_device_name') || ''
  }));
  const url = s.url + '/?_sync=' + encodeURIComponent(payload);
  // Use <a> element click — more reliable than location.href in PWA standalone mode
  // (location.href to a different origin is silently ignored on some iOS PWA builds)
  const isPWA = navigator.standalone || window.matchMedia('(display-mode: standalone)').matches;
  const a = document.createElement('a');
  a.href = url;
  if (isPWA) a.target = '_blank'; // opens in-app browser on iOS PWA
  a.style.display = 'none';
  document.body.appendChild(a);
  a.click();
  a.remove();
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
  const isPWA = navigator.standalone || window.matchMedia('(display-mode: standalone)').matches;
  // Pre-compute sync payload so server items can be real <a> tags (user gesture = direct tap)
  const allServers = [...servers];
  if (!allServers.some(srv => srv.url.replace(/\/+$/, '') === current)) {
    allServers.push({ name: location.host, url: current });
  }
  const payload = btoa(JSON.stringify({
    servers: allServers,
    deviceName: localStorage.getItem('amux_device_name') || ''
  }));
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
    const syncUrl = s.url + '/?_sync=' + encodeURIComponent(payload);
    // Use real <a> tag so the tap is a direct user gesture (Safari PWA requires this for window.open).
    // _switchServerUrl handles PWA vs non-PWA navigation correctly; stopPropagation prevents
    // ghost-tap click-through to session cards behind the settings panel.
    html += '<a class="settings-server-item" href="' + esc(syncUrl) + '" onclick="event.stopPropagation();closeSettings();_switchServerUrl(' + i + ',event);" style="text-decoration:none;color:inherit;">';
    html += '<div style="min-width:0;flex:1;">';
    html += '<div class="settings-server-name">' + esc(s.name || s.url.replace(/^https?:\/\//, '')) + '</div>';
    if (s.name) html += '<div class="settings-server-url">' + esc(s.url.replace(/^https?:\/\//, '')) + '</div>';
    html += '</div>';
    html += '<button class="btn" style="font-size:0.55rem;padding:1px 5px;flex-shrink:0;margin-left:6px;" onclick="event.preventDefault();event.stopPropagation();removeServer(' + i + ');renderSettingsServerList()">&#x2715;</button>';
    html += '</a>';
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
  let url = _normalizeServerUrl(document.getElementById('settings-new-server-url').value.trim());
  if (!url) { showToast('URL is required'); return; }
  const servers = _getSavedServers();
  if (servers.some(s => s.url.replace(/\/+$/, '') === url)) {
    showToast('Server already saved');
    return;
  }
  _ensureCurrentServerSaved(servers);
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

async function pullFromRemote(btn) {
  const el = document.getElementById('pull-status');
  btn.disabled = true; btn.textContent = '⏳ Pulling...';
  el.textContent = '';
  try {
    const r = await fetch(API + '/api/pull', { method: 'POST' });
    const d = await r.json();
    el.textContent = d.output || (d.ok ? 'Up to date' : 'Failed');
    el.style.color = d.ok ? 'var(--green)' : 'var(--red)';
    if (d.ok && !d.output.includes('Already up to date')) {
      setTimeout(() => forceUpdate(), 1500);
    }
  } catch(e) {
    el.textContent = 'Network error';
    el.style.color = 'var(--red)';
  }
  btn.disabled = false; btn.textContent = '⬇ Pull from remote';
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

<script>
// ── Email Events tab ──────────────────────────────────────────────────────────
let _emailEvents = [];

async function _emailLoad() {
  await _emailRender();
}

async function _emailRender() {
  try {
    const filter = document.getElementById('email-filter').value;
    const r = await fetch('/api/email/events' + (filter ? '?status=' + filter : ''));
    _emailEvents = r.ok ? await r.json() : [];
  } catch(e) { _emailEvents = []; }
  const el = document.getElementById('email-events-list');
  const visible = _emailEvents.filter(e => e.status !== 'not_event');
  if (!visible.length) {
    el.innerHTML = '<div style="color:var(--dim);font-size:0.82rem;text-align:center;padding:40px 0;">No events detected yet.<br><span style="font-size:0.75rem;">Click \u21BB Sync Now to scan Mail.app for ticket and event emails.</span></div>';
    return;
  }
  el.innerHTML = visible.map(ev => {
    const statusColor = ev.status === 'synced' ? 'var(--green)' : ev.status === 'dismissed' ? 'var(--dim)' : 'var(--yellow)';
    const statusLabel = ev.status === 'synced' ? '\u2713 On Calendar' : ev.status === 'dismissed' ? '\u2014 Dismissed' : '\u23f3 Pending';
    let startFmt = '';
    if (ev.event_start) {
      try {
        const d = new Date(ev.event_start.includes('T') ? ev.event_start : ev.event_start + 'T00:00:00');
        startFmt = d.toLocaleString(undefined, {dateStyle:'medium', timeStyle: ev.event_start.includes('T') ? 'short' : undefined});
      } catch(e2) { startFmt = ev.event_start; }
    }
    const from = escHtml((ev.email_from || '').replace(/<[^>]+>/g, '').trim());
    const acct = escHtml(ev.account_id || '');
    return '<div style="background:var(--card);border:1px solid var(--border);border-radius:7px;padding:10px 12px;">'
      + '<div style="display:flex;align-items:flex-start;gap:8px;">'
      + '<div style="flex:1;min-width:0;">'
      + '<div style="font-weight:600;font-size:0.85rem;color:var(--text);white-space:nowrap;overflow:hidden;text-overflow:ellipsis;">' + escHtml(ev.event_title || ev.email_subject || '(unknown)') + '</div>'
      + (startFmt ? '<div style="font-size:0.78rem;color:var(--accent);margin-top:2px;">\uD83D\uDCC5 ' + startFmt + '</div>' : '')
      + (ev.event_location ? '<div style="font-size:0.75rem;color:var(--dim);margin-top:2px;">\uD83D\uDCCD ' + escHtml(ev.event_location) + '</div>' : '')
      + (ev.event_description ? '<div style="font-size:0.75rem;color:var(--dim);margin-top:3px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;">' + escHtml(ev.event_description) + '</div>' : '')
      + '<div style="font-size:0.7rem;color:var(--dim);margin-top:4px;">' + (acct ? acct + ' \u2022 ' : '') + from + '</div>'
      + '</div>'
      + '<div style="display:flex;flex-direction:column;align-items:flex-end;gap:4px;flex-shrink:0;">'
      + '<span style="font-size:0.72rem;font-weight:600;color:' + statusColor + ';">' + statusLabel + '</span>'
      + (ev.status === 'pending' ? '<button class="btn" onclick="_emailPush(\'' + ev.id + '\')" style="font-size:0.7rem;padding:2px 8px;">\uD83D\uDCC5 Add</button>' : '')
      + (ev.status !== 'dismissed' ? '<button class="btn" onclick="_emailDismiss(\'' + ev.id + '\')" style="font-size:0.7rem;padding:2px 8px;color:var(--dim);">\u2715</button>' : '')
      + '</div></div></div>';
  }).join('');
}

async function _emailSync() {
  const btn = document.getElementById('email-sync-btn');
  btn.textContent = 'Syncing\u2026';
  btn.disabled = true;
  try {
    await fetch('/api/email/sync', {method:'POST'});
    // Sync runs in background (Mail.app can take a while); poll after a delay
    setTimeout(async () => {
      btn.textContent = '\u21BB Sync Now'; btn.disabled = false;
      await _emailRender();
      const ts = document.getElementById('email-last-sync');
      if (ts) ts.textContent = 'synced ' + new Date().toLocaleTimeString();
    }, 5000);
  } catch(e) { btn.textContent = '\u21BB Sync Now'; btn.disabled = false; }
}

async function _emailPush(id) {
  const r = await fetch('/api/email/events/' + id, {method:'PATCH', headers:{'Content-Type':'application/json'}, body: JSON.stringify({push: true})});
  const d = await r.json();
  if (d.ok) _emailRender();
  else alert('Calendar.app error: ' + (d.error || 'unknown'));
}

async function _emailDismiss(id) {
  await fetch('/api/email/events/' + id, {method:'PATCH', headers:{'Content-Type':'application/json'}, body: JSON.stringify({status:'dismissed'})});
  _emailRender();
}
</script>

<script src="https://cdn.jsdelivr.net/npm/marked@15/marked.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/sortablejs@1.15.6/Sortable.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.4/dist/chart.umd.min.js"></script>
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
    "id": "/",
    "start_url": "/",
    "display": "standalone",
    "background_color": "#0d1117",
    "theme_color": "#0d1117",
    "icons": [
        {"src": "/icon.svg", "sizes": "any", "type": "image/svg+xml", "purpose": "any"},
        {"src": "/icon-192.png", "sizes": "192x192", "type": "image/png"},
        {"src": "/icon-512.png", "sizes": "512x512", "type": "image/png"},
        {"src": "/icon.png", "sizes": "180x180", "type": "image/png", "purpose": "any"},
    ],
})

# Robust service worker: cache-first with localStorage fallback for multi-day offline
SERVICE_WORKER = r"""
const CACHE = 'amux-v0.6.2';
const SHELL_URLS = ['/', '/manifest.json', '/icon.svg', '/icon.png', '/icon-192.png', '/icon-512.png'];

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
        pass  # Suppressed — we do our own timing-aware logging in _route()

    def send_response(self, code, message=None):
        self._resp_status = code
        super().send_response(code, message)

    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, PATCH, DELETE, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("Access-Control-Allow-Private-Network", "true")
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
        log_cursor = len(_event_log)  # start from current position

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

                # Log events — push new entries from event ring buffer
                with _event_log_lock:
                    ring_len = len(_event_log)
                if ring_len > log_cursor:
                    with _event_log_lock:
                        new_events = list(_event_log)[log_cursor:]
                    log_cursor = ring_len
                    self.wfile.write(f"data: {json.dumps({'type': 'logs', 'payload': new_events})}\n\n".encode())
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
        self._resp_status = 200
        t0 = time.monotonic()
        try:
            return self._route_inner(method, path, qs)
        except Exception as e:
            import traceback
            slog(f"ERROR {method} {path} — {e}\n{traceback.format_exc()}")
            return self._json({"error": str(e)}, 500)
        finally:
            # Skip logging SSE (long-lived) connections
            if path != "/api/events":
                dt_ms = (time.monotonic() - t0) * 1000
                ip = self.client_address[0]
                slog(f"[{ip}] {method} {path} {self._resp_status} {dt_ms:.0f}ms")
                # Emit structured event — handlers may enrich via _req_tl.event
                etype, action, target, session = _classify_request(method, path)
                tl = getattr(_req_tl, "event", None)
                detail = ""
                if tl:
                    etype    = tl.get("type",    etype)
                    action   = tl.get("action",  action)
                    target   = tl.get("target",  target)
                    session  = tl.get("session", session)
                    detail   = tl.get("detail",  "")
                    _req_tl.event = None
                _emit_event(etype, action, target, session, detail, self._resp_status, ip)

    def _route_inner(self, method: str, path: str, qs: dict):

        # GET /
        if method == "GET" and path == "/":
            import json as _json
            page = DASHBOARD_HTML.replace(
                "</head>",
                f'<script>window._AMUX_S3_ICAL_URL={_json.dumps(_S3_CAL_URL)};</script></head>',
                1,
            )
            return self._html(page)

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
        if method == "GET" and path in ("/icon.svg", "/icon.png", "/icon-192.png", "/icon-512.png"):
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
                if len(content.strip()) < 50:
                    content = GLOBAL_MEMORY_DEFAULT
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

        # GET /api/skills — list skills from shared library (~/.amux/skills/)
        if method == "GET" and path == "/api/skills":
            skills = []
            skills_dir = CC_HOME / "skills"
            skills_dir.mkdir(parents=True, exist_ok=True)
            for f in sorted(skills_dir.glob("*.md")):
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

        # GET /api/skills/<name> — get full skill content
        if method == "GET" and path.startswith("/api/skills/"):
            name = path.split("/api/skills/", 1)[1]
            if not name or "/" in name:
                return self._json({"error": "invalid name"}, 400)
            f = CC_HOME / "skills" / (name + ".md")
            if not f.exists():
                return self._json({"error": "not found"}, 404)
            return self._json({"name": name, "content": f.read_text(errors="replace")})

        # POST /api/skills/<name> — create or update a skill
        if method == "POST" and path.startswith("/api/skills/"):
            name = path.split("/api/skills/", 1)[1]
            if not name or "/" in name:
                return self._json({"error": "invalid name"}, 400)
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
            content = body.get("content", "")
            if not content.strip():
                return self._json({"error": "content required"}, 400)
            skills_dir = CC_HOME / "skills"
            skills_dir.mkdir(parents=True, exist_ok=True)
            (skills_dir / (name + ".md")).write_text(content)
            return self._json({"ok": True, "name": name})

        # DELETE /api/skills/<name> — delete a skill
        if method == "DELETE" and path.startswith("/api/skills/"):
            name = path.split("/api/skills/", 1)[1]
            if not name or "/" in name:
                return self._json({"error": "invalid name"}, 400)
            f = CC_HOME / "skills" / (name + ".md")
            if f.exists():
                f.unlink()
            return self._json({"ok": True})

        # GET /api/prefs — read all prefs (or ?key=X for one)
        if method == "GET" and path == "/api/prefs":
            key = qs.get("key", [""])[0]
            db = get_db()
            if key:
                row = db.execute("SELECT value FROM prefs WHERE key=?", (key,)).fetchone()
                return self._json({"key": key, "value": row["value"] if row else None})
            rows = db.execute("SELECT key, value FROM prefs").fetchall()
            return self._json({r["key"]: r["value"] for r in rows})

        # POST /api/prefs — set a pref: {"key":"...", "value":"..."}
        if method == "POST" and path == "/api/prefs":
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
            key = body.get("key", "")
            value = body.get("value", "")
            if not key:
                return self._json({"error": "key required"}, 400)
            db = get_db()
            db.execute("INSERT INTO prefs (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=?", (key, value, value))
            db.commit()
            return self._json({"ok": True, "key": key, "value": value})

        # GET /api/logs — query structured event logs (SQLite + in-memory ring merged)
        if method == "GET" and path == "/api/logs":
            category = qs.get("category", [""])[0]
            session_f = qs.get("session", [""])[0]
            limit = min(int(qs.get("limit", ["200"])[0] or 200), 2000)
            # Pull from SQLite for persistence across restarts
            db = get_db()
            sql = "SELECT ts, category, action, session, actor, detail, level FROM logs"
            clauses, params = [], []
            if category:
                clauses.append("category=?"); params.append(category)
            if session_f:
                clauses.append("session=?"); params.append(session_f)
            if clauses:
                sql += " WHERE " + " AND ".join(clauses)
            sql += " ORDER BY ts DESC LIMIT ?"
            params.append(limit)
            rows = db.execute(sql, params).fetchall()
            db_events = [{
                "ts": r[0], "category": r[1], "action": r[2], "type": r[1],
                "session": r[3], "actor": r[4], "detail": r[5], "target": r[5],
                "level": r[6], "status": 500 if r[6] == "error" else 200,
            } for r in rows]
            # Merge with in-memory ring (has HTTP events + very recent structured events)
            with _event_log_lock:
                mem_events = list(_event_log)
            if category:
                mem_events = [e for e in mem_events if e.get("type") == category or e.get("category") == category]
            if session_f:
                mem_events = [e for e in mem_events if e.get("session") == session_f]
            # Deduplicate by (ts, action, category) — prefer db row
            seen = {(e["ts"], e["action"], e.get("category", e.get("type", ""))): True for e in db_events}
            for e in mem_events:
                key = (e.get("ts", 0), e.get("action", ""), e.get("category", e.get("type", "")))
                if key not in seen:
                    seen[key] = True
                    db_events.append(e)
            db_events.sort(key=lambda e: e.get("ts", 0), reverse=True)
            events = db_events[:limit]
            return self._json({"events": events, "count": len(events)})

        # GET /api/logs/raw — tail server.log
        if method == "GET" and path == "/api/logs/raw":
            lines = int(qs.get("lines", ["200"])[0] or 200)
            try:
                with open(SERVER_LOG, "r") as f:
                    all_lines = f.readlines()
                tail = all_lines[-lines:]
                return self._json({"lines": [l.rstrip() for l in tail], "total": len(all_lines)})
            except FileNotFoundError:
                return self._json({"lines": [], "total": 0})

        # GET /api/logs/stats — aggregated stats from in-memory ring
        if method == "GET" and path == "/api/logs/stats":
            with _event_log_lock:
                events = list(_event_log)
            by_cat: dict = {}
            by_session: dict = {}
            by_action: dict = {}
            for e in events:
                t = e.get("type", "")
                by_cat[t] = by_cat.get(t, 0) + 1
                s = e.get("session", "")
                if s:
                    by_session[s] = by_session.get(s, 0) + 1
                a = e.get("action", "")
                by_action[a] = by_action.get(a, 0) + 1
            return self._json({
                "total": len(events),
                "by_category": [{"category": k, "cnt": v} for k, v in sorted(by_cat.items(), key=lambda x: -x[1])],
                "by_session": [{"session": k, "cnt": v} for k, v in sorted(by_session.items(), key=lambda x: -x[1])[:20]],
                "by_action": [{"action": k, "cnt": v} for k, v in sorted(by_action.items(), key=lambda x: -x[1])[:20]],
            })

        # POST /api/pull — git pull in the repo directory
        if method == "POST" and path == "/api/pull":
            repo_dir = Path(__file__).resolve().parent
            try:
                r = subprocess.run(
                    ["git", "pull", "--ff-only"],
                    cwd=str(repo_dir), capture_output=True, text=True, timeout=30,
                )
                output = (r.stdout + r.stderr).strip()
                slog(f"[pull] rc={r.returncode} {output[:200]}")
                if r.returncode == 0:
                    return self._json({"ok": True, "output": output})
                return self._json({"ok": False, "output": output}, 500)
            except Exception as e:
                return self._json({"ok": False, "output": str(e)}, 500)

        # ── Remote browser endpoints ──
        if method == "POST" and path == "/api/browser/start":
            global _rb_profile
            body = self._read_body()
            profile = body.get("profile", "").strip()
            cmd = {"action": "start", "url": body.get("url", "")}
            if body.get("headed"):
                cmd["headed"] = True
            if body.get("record"):
                cmd["record"] = True
            if profile and profile != "default":
                cmd["profile"] = profile
                _rb_profile = profile
            else:
                _rb_profile = "default"
            result = _rb_send(cmd)
            if result.get("ok"):
                result["profile"] = _rb_profile
            return self._json(result, 200 if result.get("ok") else 500)

        if method == "POST" and path == "/api/browser/stop":
            global _rb_last_video
            result = _rb_send({"action": "stop"})
            _rb_last_video = None
            webm = result.get("videoPath")
            captions = result.get("captions") or []
            if webm and os.path.exists(webm):
                _rb_process_video(webm, captions or None, profile=_rb_profile)
            return self._json({"ok": True, "videoReady": bool(_rb_last_video)})

        # POST /api/browser/agent — start AI agent task
        if method == "POST" and path == "/api/browser/agent":
            with _rb_agent_lock:
                if _rb_agent_state["running"]:
                    return self._json({"error": "agent already running"}, 409)
            body = self._read_body()
            task = body.get("task", "").strip()
            if not task:
                return self._json({"error": "task required"}, 400)
            _rb_agent_update(
                running=True, task=task, step=0, total_steps=0,
                action="Initializing…", done=False, error=None, video_ready=False,
                _profile=body.get("profile", _rb_profile),
            )
            threading.Thread(
                target=_run_browser_agent,
                args=(task, body.get("url", ""), int(body.get("max_steps", 25))),
                daemon=True,
            ).start()
            return self._json({"ok": True})

        # GET /api/browser/agent/status — poll agent progress
        if method == "GET" and path == "/api/browser/agent/status":
            with _rb_agent_lock:
                return self._json(dict(_rb_agent_state))

        # POST /api/browser/agent/stop — cancel running agent
        if method == "POST" and path == "/api/browser/agent/stop":
            _rb_agent_update(running=False)
            return self._json({"ok": True})

        # GET /api/browser/video — download latest recorded video
        if method == "GET" and path == "/api/browser/video":
            if not _rb_last_video or not os.path.exists(_rb_last_video):
                return self._json({"error": "no video"}, 404)
            ext = Path(_rb_last_video).suffix.lower()
            mime = "video/mp4" if ext == ".mp4" else "video/webm"
            data = open(_rb_last_video, "rb").read()
            fname = Path(_rb_last_video).name
            self.send_response(200)
            self.send_header("Content-Type", mime)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Content-Disposition", f'attachment; filename="{fname}"')
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(data)
            return

        # GET /api/browser/profiles — list available profiles
        if method == "GET" and path == "/api/browser/profiles":
            default_path = CC_HOME / "playwright-auth" / "profile"
            profiles = [{"name": "default", "path": str(default_path),
                         "has_state": _profile_has_state(default_path)}]
            if _RB_PROFILES_DIR.exists():
                for d in sorted(_RB_PROFILES_DIR.iterdir()):
                    if d.is_dir() and not d.name.startswith("."):
                        profiles.append({"name": d.name, "path": str(d),
                                         "has_state": _profile_has_state(d)})
            return self._json({"profiles": profiles, "active": _rb_profile})

        # POST /api/browser/profiles — create a new profile
        if method == "POST" and path == "/api/browser/profiles":
            body = self._read_body()
            name = re.sub(r'[^a-zA-Z0-9_-]', '-', body.get("name", "").strip())
            if not name or name == "default":
                return self._json({"error": "invalid profile name"}, 400)
            profile_dir = _RB_PROFILES_DIR / name
            profile_dir.mkdir(parents=True, exist_ok=True)
            return self._json({"ok": True, "name": name}, 201)

        # DELETE /api/browser/profiles/<name>
        del_profile_m = re.match(r"^/api/browser/profiles/([a-zA-Z0-9_-]+)$", path)
        if method == "DELETE" and del_profile_m:
            name = del_profile_m.group(1)
            if name == "default":
                return self._json({"error": "cannot delete default profile"}, 400)
            profile_dir = _RB_PROFILES_DIR / name
            if profile_dir.exists():
                import shutil
                shutil.rmtree(str(profile_dir), ignore_errors=True)
            return self._json({"ok": True})

        if method == "POST" and path == "/api/browser/navigate":
            body = self._read_body()
            result = _rb_send({"action": "navigate", "url": body.get("url", "")})
            return self._json(result, 200 if result.get("ok") else 500)

        if method == "POST" and path == "/api/browser/action":
            body = self._read_body()
            result = _rb_send(body)
            return self._json(result, 200 if result.get("ok") else 500)

        if method == "GET" and path == "/api/browser/screenshot":
            result = _rb_send({"action": "screenshot"})
            if not result.get("ok") or "data" not in result:
                return self._json({"error": result.get("error", "no screenshot")}, 500)
            img_data = base64.b64decode(result["data"])
            self.send_response(200)
            self.send_header("Content-Type", "image/jpeg")
            self.send_header("Content-Length", str(len(img_data)))
            self.send_header("Cache-Control", "no-store")
            if result.get("url"):
                self.send_header("X-Page-Url", result["url"])
            if result.get("title"):
                from urllib.parse import quote
                self.send_header("X-Page-Title", quote(result["title"]))
            if result.get("tabs") is not None:
                import json as _json
                self.send_header("X-Tabs", _json.dumps(result["tabs"]))
                self.send_header("X-Active-Tab", str(result.get("activeTab", 0)))
            self.end_headers()
            self.wfile.write(img_data)
            return

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

        # GET /api/recordings — list all recordings in ~/.amux/recordings/
        if method == "GET" and path == "/api/recordings":
            import json as _json
            recs = []
            if _RB_RECORDINGS_DIR.exists():
                video_exts = {".mp4", ".webm", ".mov"}
                for f in sorted(_RB_RECORDINGS_DIR.iterdir(), key=lambda x: x.stat().st_mtime, reverse=True):
                    if f.suffix.lower() in video_exts and f.is_file():
                        srt = f.with_suffix(".srt")
                        meta_file = f.with_suffix(".json")
                        meta = {}
                        if meta_file.exists():
                            try: meta = _json.loads(meta_file.read_text())
                            except Exception: pass
                        st = f.stat()
                        recs.append({
                            "name": f.name, "path": str(f),
                            "size": st.st_size, "modified": int(st.st_mtime),
                            "srt": str(srt) if srt.exists() else None,
                            "profile": meta.get("profile"), "task": meta.get("task"),
                        })
            return self._json({"recordings": recs, "dir": str(_RB_RECORDINGS_DIR)})

        # GET /api/file/vtt?path=... — convert SRT → WebVTT for <track> elements
        if method == "GET" and path == "/api/file/vtt":
            import re as _re
            srt_p = qs.get("path", [""])[0]
            if not srt_p:
                return self._json({"error": "missing path"}, 400)
            srt_file = Path(srt_p).expanduser()
            if not srt_file.is_file():
                return self._json({"error": "not found"}, 404)
            srt_text = srt_file.read_text(encoding="utf-8", errors="replace")
            vtt = "WEBVTT\n\n" + _re.sub(r"(\d{2}:\d{2}:\d{2}),(\d{3})", r"\1.\2", srt_text)
            data = vtt.encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/vtt; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(data)
            return

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
                VIDEO_MIMES = {
                    ".mp4": "video/mp4", ".mov": "video/quicktime", ".webm": "video/webm",
                    ".avi": "video/x-msvideo", ".mkv": "video/x-matroska", ".m4v": "video/mp4",
                }
                AUDIO_MIMES = {
                    ".mp3": "audio/mpeg", ".wav": "audio/wav", ".ogg": "audio/ogg",
                    ".m4a": "audio/mp4", ".aac": "audio/aac", ".flac": "audio/flac",
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
                if ext in VIDEO_MIMES:
                    import json as _json
                    srt_path = p.with_suffix(".srt")
                    meta_file = p.with_suffix(".json")
                    meta = {}
                    if meta_file.exists():
                        try: meta = _json.loads(meta_file.read_text())
                        except Exception: pass
                    return self._json({"path": str(p), "is_video": True, "mime": VIDEO_MIMES[ext],
                                       "size": p.stat().st_size, "modified": int(p.stat().st_mtime),
                                       "srt": str(srt_path) if srt_path.exists() else None,
                                       "profile": meta.get("profile"), "task": meta.get("task")})
                if ext in AUDIO_MIMES:
                    return self._json({"path": str(p), "is_audio": True, "mime": AUDIO_MIMES[ext], "size": p.stat().st_size})
                # Detect other binary files (non-printable bytes in first 8KB)
                try:
                    sample = p.read_bytes()[:8192]
                    if b'\x00' in sample:
                        return self._json({"path": str(p), "is_binary": True, "size": p.stat().st_size, "ext": ext})
                except Exception:
                    pass
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

        # GET /api/file/raw?path=...&cwd=...  — streams binary files with range support
        if method == "GET" and path == "/api/file/raw":
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
            ext = p.suffix.lower()
            ALL_MIMES = {
                ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg",
                ".gif": "image/gif", ".webp": "image/webp", ".svg": "image/svg+xml",
                ".bmp": "image/bmp", ".ico": "image/x-icon",
                ".pdf": "application/pdf",
                ".mp4": "video/mp4", ".mov": "video/quicktime", ".webm": "video/webm",
                ".avi": "video/x-msvideo", ".mkv": "video/x-matroska", ".m4v": "video/mp4",
                ".mp3": "audio/mpeg", ".wav": "audio/wav", ".ogg": "audio/ogg",
                ".m4a": "audio/mp4", ".aac": "audio/aac", ".flac": "audio/flac",
            }
            mime = ALL_MIMES.get(ext, "application/octet-stream")
            file_size = p.stat().st_size
            range_header = self.headers.get("Range", "")
            import re as _re
            if range_header:
                m = _re.match(r'bytes=(\d*)-(\d*)', range_header)
                start = int(m.group(1)) if m and m.group(1) else 0
                end = int(m.group(2)) if m and m.group(2) else file_size - 1
                end = min(end, file_size - 1)
                length = end - start + 1
                self.send_response(206)
                self.send_header("Content-Type", mime)
                self.send_header("Content-Range", f"bytes {start}-{end}/{file_size}")
                self.send_header("Content-Length", str(length))
                self.send_header("Accept-Ranges", "bytes")
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                with open(p, "rb") as f:
                    f.seek(start)
                    self.wfile.write(f.read(length))
            else:
                self.send_response(200)
                self.send_header("Content-Type", mime)
                self.send_header("Content-Length", str(file_size))
                self.send_header("Accept-Ranges", "bytes")
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                self.wfile.write(p.read_bytes())
            return

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

        # ── Transcription (OpenAI Whisper) ──
        if path == "/api/transcribe":
            key = os.environ.get("OPENAI_API_KEY", "")
            if method == "GET":
                return self._json({"available": bool(key)})
            if method == "POST":
                if not key:
                    return self._json({"error": "transcription not configured"}, 503)
                body = self._read_body()
                audio_b64 = body.get("data", "")
                mime = body.get("mime", "audio/webm")
                ext = "webm" if "webm" in mime else "wav" if "wav" in mime else "m4a" if "m4a" in mime or "mp4" in mime else "webm"
                try:
                    audio_data = base64.b64decode(audio_b64)
                except Exception:
                    return self._json({"error": "invalid base64"}, 400)
                boundary = uuid.uuid4().hex
                parts = [
                    (f'--{boundary}\r\nContent-Disposition: form-data; name="model"\r\n\r\nwhisper-1').encode(),
                    (f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="audio.{ext}"\r\nContent-Type: {mime}\r\n\r\n').encode() + audio_data,
                    (f'--{boundary}--').encode(),
                ]
                multipart = b"\r\n".join(parts)
                import urllib.request as _urq
                import urllib.error as _urqe
                req = _urq.Request(
                    "https://api.openai.com/v1/audio/transcriptions",
                    data=multipart,
                    headers={
                        "Authorization": f"Bearer {key}",
                        "Content-Type": f"multipart/form-data; boundary={boundary}",
                    },
                )
                try:
                    import ssl as _ssl
                    try:
                        import certifi as _certifi
                        _ssl_ctx = _ssl.create_default_context(cafile=_certifi.where())
                    except ImportError:
                        _ssl_ctx = _ssl.create_default_context()
                    with _urq.urlopen(req, timeout=30, context=_ssl_ctx) as r:
                        result = json.loads(r.read())
                    return self._json({"text": result.get("text", "")})
                except _urqe.HTTPError as e:
                    try:
                        err_body = json.loads(e.read().decode("utf-8", errors="replace"))
                        err_msg = err_body.get("error", {}).get("message", str(e))
                    except Exception:
                        err_msg = str(e)
                    _log_event("transcribe", "error", detail=f"HTTP {e.code}: {err_msg}", level="warn")
                    return self._json({"error": err_msg}, e.code)
                except Exception as e:
                    return self._json({"error": str(e)}, 500)

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
                due_time = body.get("due_time", "").strip() or None
                creator = body.get("creator", "")
                desc = body.get("desc", "").strip()
                tags = [t for t in body.get("tags", []) if t]
                owner_type = body.get("owner_type", "agent" if session else "human")
                if owner_type not in ("human", "agent"):
                    owner_type = "human"
                db.execute(
                    """INSERT INTO issues (id, title, desc, status, session, creator, due, due_time, created, updated, owner_type)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                    (item_id, title, desc, status, session or None, creator, due, due_time, now, now, owner_type),
                )
                for tag in tags:
                    db.execute(
                        "INSERT OR IGNORE INTO issue_tags (issue_id, tag) VALUES (?, ?)",
                        (item_id, tag),
                    )
                db.commit()
                _sse_cache["board"]["time"] = 0  # invalidate SSE cache
                item = _item_by_id(item_id)
                _push_ical_bg()
                _req_tl.event = {"type": "board", "action": "created", "target": item_id,
                                 "session": session, "detail": f"{item_id}: {title}"}
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
                _push_ical_bg()
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
                    for k in ("title", "desc", "status", "session", "due", "due_time", "owner_type", "pinned"):
                        if k in body:
                            set_clauses.append(f"{k} = ?")
                            v = body[k]
                            params.append(None if v == "" and k in ("session", "due", "due_time") else v)
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
                    _push_ical_bg()
                    return self._json(_item_by_id(bid))

                if method == "DELETE":
                    now = int(time.time())
                    db.execute("UPDATE issues SET deleted = ? WHERE id = ?", (now, bid))
                    db.commit()
                    _sse_cache["board"]["time"] = 0
                    _push_ical_bg()
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


        # ── Schedules API ─────────────────────────────────────────────────────
        if path == "/api/schedules" or path.startswith("/api/schedules/"):
            import time as _time
            from datetime import datetime as _dt

            def _sched_row_to_dict(row, cols):
                return dict(zip(cols, row))

            def _sched_cols(db):
                return [d[1] for d in db.execute("PRAGMA table_info(schedules)").fetchall()]

            # GET /api/schedules
            if method == "GET" and path == "/api/schedules":
                db = get_db()
                rows = db.execute(
                    "SELECT * FROM schedules WHERE deleted IS NULL ORDER BY next_run ASC, created ASC"
                ).fetchall()
                cols = _sched_cols(db)
                self._json([_sched_row_to_dict(r, cols) for r in rows])
                return

            # POST /api/schedules
            if method == "POST" and path == "/api/schedules":
                db = get_db()
                data = self._read_body()
                now_ts = int(_time.time())
                sid = _next_issue_id("SCHED")
                stype = data.get("sched_type", "once")
                run_at = data.get("run_at", _dt.now().strftime("%Y-%m-%dT%H:%M"))
                sched = {
                    "id": sid, "title": data.get("title", ""),
                    "session": data.get("session", ""),
                    "command": data.get("command", ""),
                    "sched_type": stype,
                    "recurrence": data.get("recurrence"),
                    "run_at": run_at, "next_run": run_at,
                    "last_run": None, "enabled": 1,
                    "created": now_ts, "updated": now_ts, "deleted": None,
                }
                # compute next_run
                sched["next_run"] = _next_run_dt(sched) or run_at
                db.execute(
                    """INSERT INTO schedules (id,title,session,command,sched_type,recurrence,
                       run_at,next_run,last_run,enabled,created,updated,deleted)
                       VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)""",
                    (sched["id"], sched["title"], sched["session"], sched["command"],
                     sched["sched_type"], sched["recurrence"], sched["run_at"],
                     sched["next_run"], sched["last_run"], sched["enabled"],
                     sched["created"], sched["updated"], sched["deleted"])
                )
                db.commit()

                self._json(sched, 201)
                return

            sched_id = path.split("/api/schedules/", 1)[-1].split("?")[0]

            # GET /api/schedules/<id>
            if method == "GET":
                db = get_db()
                row = db.execute("SELECT * FROM schedules WHERE id=?", (sched_id,)).fetchone()
                if not row:
                    self._json({"error": "not found"}, 404); return
                self._json(_sched_row_to_dict(row, _sched_cols(db)))
                return

            # PATCH /api/schedules/<id>
            if method == "PATCH":
                db = get_db()
                row = db.execute("SELECT * FROM schedules WHERE id=?", (sched_id,)).fetchone()
                if not row:
                    self._json({"error": "not found"}, 404); return
                cols = _sched_cols(db)
                sched = _sched_row_to_dict(row, cols)
                body = self._read_body()
                for k in ("title","session","command","sched_type","recurrence","run_at","enabled"):
                    if k in body:
                        sched[k] = body[k]
                sched["next_run"] = _next_run_dt(sched) or sched.get("run_at", "")
                sched["updated"] = int(_time.time())
                db.execute(
                    """UPDATE schedules SET title=?,session=?,command=?,sched_type=?,recurrence=?,
                       run_at=?,next_run=?,enabled=?,updated=? WHERE id=?""",
                    (sched["title"], sched["session"], sched["command"], sched["sched_type"],
                     sched["recurrence"], sched["run_at"], sched["next_run"],
                     sched["enabled"], sched["updated"], sched_id)
                )
                db.commit()

                self._json(sched)
                return

            # DELETE /api/schedules/<id>
            if method == "DELETE":
                db = get_db()
                db.execute("UPDATE schedules SET deleted=?,updated=? WHERE id=?",
                           (int(_time.time()), int(_time.time()), sched_id))
                db.commit()

                self._json({"deleted": sched_id})
                return

        # ── Reports API ───────────────────────────────────────────────────────
        if path == "/api/reports" or path.startswith("/api/reports/"):
            import json as _json_r, time as _tr, urllib.request as _ur, urllib.error as _ue
            db = get_db()

            def _reports_list():
                rows = db.execute(
                    "SELECT id,name,type,config,position,created,last_refresh FROM reports ORDER BY position,created"
                ).fetchall()
                return [dict(r) for r in rows]

            # GET /api/reports/types — return registry metadata (no fetch fns)
            if method == "GET" and path == "/api/reports/types":
                out = {}
                for type_id, type_meta in _REPORT_TYPE_REGISTRY.items():
                    vendors_out = {}
                    for vid, vm in type_meta.get("vendors", {}).items():
                        vendors_out[vid] = {k: v for k, v in vm.items() if k != "fetch"}
                    out[type_id] = {
                        "label": type_meta.get("label", type_id),
                        "description": type_meta.get("description", ""),
                        "vendors": vendors_out,
                    }
                return self._json(out)

            # GET /api/reports
            if method == "GET" and path == "/api/reports":
                return self._json(_reports_list())

            # POST /api/reports — create report
            if method == "POST" and path == "/api/reports":
                body_d = self._read_body()
                name = body_d.get("name","New Report").strip()
                rtype = body_d.get("type","infra-spend")
                config = _json_r.dumps(body_d.get("config",{}))
                pos = body_d.get("position", 0)
                rid = f"rpt-{int(_tr.time()*1000)}"
                now = int(_tr.time())
                db.execute("INSERT INTO reports (id,name,type,config,position,created) VALUES (?,?,?,?,?,?)",
                           (rid, name, rtype, config, pos, now))
                db.commit()
                row = db.execute("SELECT * FROM reports WHERE id=?", (rid,)).fetchone()
                return self._json(dict(row), 201)

            # DELETE /api/reports/<id>
            del_m = re.match(r"^/api/reports/([A-Za-z0-9_-]+)$", path)
            if method == "DELETE" and del_m:
                rid = del_m.group(1)
                db.execute("DELETE FROM reports WHERE id=?", (rid,))
                db.commit()
                return self._json({"ok":True})

            # PATCH /api/reports/<id> — rename
            if method == "PATCH" and del_m:
                rid = del_m.group(1)
                body_d = self._read_body()
                if "name" in body_d:
                    db.execute("UPDATE reports SET name=? WHERE id=?", (body_d["name"], rid))
                    db.commit()
                row = db.execute("SELECT * FROM reports WHERE id=?", (rid,)).fetchone()
                return self._json(dict(row) if row else {"error":"not found"}, 200 if row else 404)

            # POST /api/reports/<id>/refresh — fetch live data from all vendors
            refresh_m = re.match(r"^/api/reports/([A-Za-z0-9_-]+)/refresh$", path)
            if method == "POST" and refresh_m:
                rid = refresh_m.group(1)
                row = db.execute("SELECT * FROM reports WHERE id=?", (rid,)).fetchone()
                if not row:
                    return self._json({"error":"not found"}, 404)
                cfg = _json_r.loads(row["config"] or "{}")
                rtype = row["type"]
                type_meta = _REPORT_TYPE_REGISTRY.get(rtype, {})
                all_vendors = type_meta.get("vendors", {})
                vendor_ids = cfg.get("vendors", list(all_vendors.keys()))
                results = {}
                if "report_fetch" in type_meta:
                    results = type_meta["report_fetch"](cfg)
                else:
                    for v in vendor_ids:
                        vm = all_vendors.get(v)
                        if vm and "fetch" in vm:
                            results[v] = vm["fetch"](cfg)
                        else:
                            results[v] = {"name":v,"error":"unknown vendor","daily":[],"monthly":[]}
                now = int(_tr.time())
                db.execute("UPDATE reports SET last_refresh=?, cached_data=? WHERE id=?",
                           (now, _json_r.dumps(results), rid))
                db.commit()
                return self._json({"ok":True, "data":results, "refreshed_at":now})

            # GET /api/reports/<id>/data — return cached data
            data_m = re.match(r"^/api/reports/([A-Za-z0-9_-]+)/data$", path)
            if method == "GET" and data_m:
                rid = data_m.group(1)
                row = db.execute("SELECT cached_data, last_refresh FROM reports WHERE id=?", (rid,)).fetchone()
                if not row:
                    return self._json({"error":"not found"}, 404)
                cached = _json_r.loads(row["cached_data"] or "{}") if row["cached_data"] else {}
                return self._json({"data":cached,"refreshed_at":row["last_refresh"]})

                # GET /api/calendar.ics — iCal subscription feed
        if method == "GET" and path == "/api/calendar.ics":
            ical_text = _generate_ical()
            body = ical_text.encode("utf-8")
            self.send_response(200)
            self._cors()
            self.send_header("Content-Type", "text/calendar; charset=utf-8")
            self.send_header("Content-Disposition", 'inline; filename="amux.ics"')
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        # GET /api/sync?since=<unix_seconds> — delta sync for offline clients
        if method == "GET" and path == "/api/sync":
            since = int(qs.get("since", ["0"])[0] or "0")
            db = get_db()
            rows = db.execute(
                """SELECT i.id, i.title, i.desc, i.status, i.session, i.creator,
                          i.due, i.due_time, i.created, i.updated, i.deleted,
                          COALESCE(i.pinned, 0) AS pinned,
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
        if method == "POST" and path == "/api/suggest-branch":
            body = self._read_body()
            sname = body.get("name", "").strip()
            dir_path = body.get("dir", "").strip()
            prompt = body.get("prompt", "").strip()
            slug = re.sub(r'[^a-z0-9-]', '-', sname.lower()).strip('-') or "session"
            fallback = [f"session/{slug}", f"feat/{slug}", f"wip/{slug}", slug]
            api_key = os.environ.get("ANTHROPIC_API_KEY", "")
            if not api_key:
                return self._json({"suggestions": fallback})
            try:
                import anthropic as _anthropic
                client = _anthropic.Anthropic(api_key=api_key)
                content = f"Suggest 4 git branch names for a coding session.\nSession name: {sname!r}"
                if dir_path:
                    content += f"\nProject directory: {dir_path!r}"
                if prompt:
                    content += f"\nGoal: {prompt!r}"
                content += ("\n\nReply with exactly 4 branch names, one per line, no explanations, "
                            "no bullets, no numbers. Use kebab-case. Vary the schema: one with "
                            "'feat/', one with 'session/', one descriptive without prefix, one short.")
                msg = client.messages.create(
                    model="claude-haiku-4-5-20251001",
                    max_tokens=120,
                    messages=[{"role": "user", "content": content}],
                )
                lines = [l.strip() for l in msg.content[0].text.strip().splitlines() if l.strip()]
                return self._json({"suggestions": lines[:4] if lines else fallback})
            except Exception:
                return self._json({"suggestions": fallback})

        if method == "POST" and path == "/api/sessions":
            body = self._read_body()
            name = body.get("name", "").strip()
            dir_path = body.get("dir", "").strip()
            # Fall back to saved files_cwd pref if no dir provided
            if not dir_path:
                try:
                    row = get_db().execute("SELECT value FROM prefs WHERE key='files_cwd'").fetchone()
                    if row and row["value"] and row["value"] != "/":
                        dir_path = row["value"]
                except Exception:
                    pass
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

        # GET /api/sessions/self?session=<name> — convenience for a session to look itself up
        if method == "GET" and path == "/api/sessions/self":
            sname = qs.get("session", [None])[0] or self.headers.get("X-Amux-Session", "")
            if not sname:
                return self._json({"error": "session param required"}, 400)
            sessions = list_sessions()
            match = next((s for s in sessions if s["name"] == sname), None)
            if not match:
                return self._json({"error": f"session '{sname}' not found"}, 404)
            return self._json(match)

        # GET /api/logs — structured event feed + raw server log
        if path == "/api/logs" and method == "GET":
            log_type  = qs.get("type",   ["events"])[0]   # events | raw | both
            since     = float(qs.get("since",  ["0"])[0])
            limit_n   = min(int(qs.get("limit", ["500"])[0]), 2000)
            filt      = qs.get("filter", [""])[0]          # event type filter
            result: dict = {}
            if log_type in ("events", "both"):
                with _event_log_lock:
                    evts = list(_event_log)
                if since:
                    evts = [e for e in evts if e["ts"] > since]
                if filt:
                    evts = [e for e in evts if e["type"] == filt]
                result["events"] = list(reversed(evts[-limit_n:]))
            if log_type in ("raw", "both"):
                lines_n = int(qs.get("lines", ["300"])[0])
                try:
                    with open(SERVER_LOG, "r", errors="replace") as _f:
                        all_lines = _f.readlines()
                    result["raw"] = "".join(all_lines[-lines_n:])
                    result["raw_total_lines"] = len(all_lines)
                except Exception:
                    result["raw"] = ""
                    result["raw_total_lines"] = 0
            return self._json(result)

        # ── /api/email/* — Mail.app + Calendar.app via AppleScript (no OAuth) ────
        if path.startswith("/api/email"):

            # GET /api/email/events
            if method == "GET" and path == "/api/email/events":
                status_f = qs.get("status", [""])[0]
                if status_f:
                    rows = get_db().execute(
                        "SELECT * FROM email_events WHERE status=? "
                        "ORDER BY event_start, created DESC LIMIT 200",
                        (status_f,),
                    ).fetchall()
                else:
                    rows = get_db().execute(
                        "SELECT * FROM email_events WHERE status != 'not_event' "
                        "ORDER BY event_start, created DESC LIMIT 200",
                    ).fetchall()
                return self._json([dict(r) for r in rows])

            # POST /api/email/sync — trigger manual sync in background
            if method == "POST" and path == "/api/email/sync":
                def _locked_sync():
                    with _email_sync_lock:
                        _email_sync()
                threading.Thread(target=_locked_sync, daemon=True).start()
                return self._json({"ok": True})

            # PATCH /api/email/events/<id> — dismiss or manually push to calendar
            _m_ev = re.match(r"^/api/email/events/([a-f0-9-]+)$", path)
            if _m_ev and method == "PATCH":
                ev_id = _m_ev.group(1)
                body  = self._body_json()
                ev = get_db().execute(
                    "SELECT * FROM email_events WHERE id=?", (ev_id,)
                ).fetchone()
                if not ev:
                    return self._json({"error": "not found"}, 404)
                if body.get("status") == "dismissed":
                    get_db().execute(
                        "UPDATE email_events SET status='dismissed' WHERE id=?", (ev_id,)
                    )
                    get_db().commit()
                    return self._json({"ok": True})
                if body.get("push"):
                    event_data = {
                        "title":       ev["event_title"],
                        "start":       ev["event_start"],
                        "end":         ev["event_end"],
                        "location":    ev["event_location"],
                        "description": ev["event_description"],
                    }
                    ok = _cal_create_event(event_data)
                    if ok:
                        get_db().execute(
                            "UPDATE email_events SET status='synced', calendar_event_id='applescript' WHERE id=?",
                            (ev_id,),
                        )
                        get_db().commit()
                        return self._json({"ok": True})
                    return self._json({"error": "Calendar.app event creation failed"}, 500)
                return self._json({"error": "nothing to update"}, 400)

            # DELETE /api/email/events/<id> — hard delete
            if _m_ev and method == "DELETE":
                ev_id = _m_ev.group(1)
                get_db().execute("DELETE FROM email_events WHERE id=?", (ev_id,))
                get_db().commit()
                return self._json({"ok": True})

            return self._json({"error": "not found"}, 404)

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
            if action == "git":
                wd = _session_work_dir(name)
                return self._json(_git_info(wd))
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
            if action == "git":
                body = self._read_body()
                branch = body.get("branch", "").strip()
                create = bool(body.get("create", False))
                wd = _session_work_dir(name)
                if not wd:
                    return self._json({"error": "session has no directory"}, 400)
                if not branch:
                    return self._json({"error": "branch name required"}, 400)
                if not re.match(r'^[a-zA-Z0-9_./@\-]+$', branch):
                    return self._json({"error": "invalid branch name"}, 400)
                cmd = ["git", "-C", wd, "checkout"]
                if create:
                    cmd.append("-b")
                cmd.append(branch)
                try:
                    r = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
                    if r.returncode == 0:
                        return self._json({"ok": True, "branch": branch})
                    return self._json({"ok": False, "error": (r.stderr or r.stdout).strip()}, 400)
                except Exception as ex:
                    return self._json({"ok": False, "error": str(ex)}, 500)
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
            if action == "archive":
                ok, msg = archive_session(name)
                return self._json({"ok": ok, "message": msg}, 200 if ok else 500)
            if action == "wake":
                ok, msg = wake_session(name)
                return self._json({"ok": ok, "message": msg}, 200 if ok else 500)
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

                # Toggle auto-continue
                if body.get("toggle_auto_continue"):
                    cur = cfg.get("CC_AUTO_CONTINUE", "0")
                    cfg["CC_AUTO_CONTINUE"] = "0" if cur in ("1", "true", "yes") else "1"
                    _write_env(env_file, cfg)
                    return self._json({"ok": True, "message": "auto_continue toggled"})

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

        # ── Cloud Waitlist ──
        if path == "/api/waitlist":
            db = get_db()
            if method == "POST":
                body = self._read_body()
                email = body.get("email", "").strip().lower()
                if not email or "@" not in email:
                    return self._json({"error": "invalid email"}, 400)
                note = body.get("note", "").strip()[:500]
                try:
                    db.execute(
                        "INSERT INTO waitlist (email, note, ts) VALUES (?,?,?)",
                        (email, note or None, int(time.time()))
                    )
                    db.commit()
                    return self._json({"ok": True})
                except sqlite3.IntegrityError:
                    return self._json({"ok": True, "already": True})
            if method == "GET":
                rows = db.execute(
                    "SELECT id, email, note, ts FROM waitlist ORDER BY ts DESC"
                ).fetchall()
                return self._json([dict(r) for r in rows])

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
# AUTO-UPDATE FROM GITHUB
# ═══════════════════════════════════════════

_AUTO_UPDATE_REPO = os.environ.get("AMUX_AUTO_UPDATE_REPO", "")   # e.g. "mixpeek/amux"
_AUTO_UPDATE_BRANCH = os.environ.get("AMUX_AUTO_UPDATE_BRANCH", "main")
_AUTO_UPDATE_INTERVAL = int(os.environ.get("AMUX_AUTO_UPDATE_INTERVAL", "60"))  # seconds


def _auto_update_check(_state: dict = {}):
    """Scheduled job: poll GitHub for changes and overwrite self if newer.
    Uses mutable default dict to carry last_sha across ticks."""
    import urllib.request as _ur, ast as _ast
    repo   = _AUTO_UPDATE_REPO
    branch = _AUTO_UPDATE_BRANCH
    script = Path(__file__).resolve()
    api_url = f"https://api.github.com/repos/{repo}/commits?path=amux-server.py&sha={branch}&per_page=1"
    raw_url = f"https://raw.githubusercontent.com/{repo}/{branch}/amux-server.py"
    try:
        req = _ur.Request(api_url, headers={"Accept": "application/vnd.github.v3+json"})
        with _ur.urlopen(req, timeout=10) as resp:
            commits = json.loads(resp.read())
        if not commits:
            return
        sha = commits[0]["sha"]
        if "last_sha" not in _state:
            _state["last_sha"] = sha
            return  # first poll — just record, don't update
        if sha == _state["last_sha"]:
            return
        slog(f"[auto-update] new commit {sha[:8]}, downloading...")
        with _ur.urlopen(raw_url, timeout=30) as resp:
            new_content = resp.read()
        try:
            _ast.parse(new_content)
        except SyntaxError as e:
            slog(f"[auto-update] SKIP — syntax error in remote file: {e}")
            _state["last_sha"] = sha
            return
        script.write_bytes(new_content)
        _state["last_sha"] = sha
        slog(f"[auto-update] updated to {sha[:8]} — file watcher will restart")
        _sync_skills_from_github(_ur, repo, branch)
    except Exception as e:
        slog(f"[auto-update] error: {e}")


def _sync_skills_from_github(_ur, repo, branch):
    """Download skills/*.md from GitHub repo into ~/.amux/skills/."""
    try:
        api_url = f"https://api.github.com/repos/{repo}/contents/skills?ref={branch}"
        req = _ur.Request(api_url, headers={"Accept": "application/vnd.github.v3+json"})
        with _ur.urlopen(req, timeout=10) as resp:
            files = json.loads(resp.read())
        skills_dir = CC_HOME / "skills"
        skills_dir.mkdir(parents=True, exist_ok=True)
        synced = 0
        for f in files:
            if not f["name"].endswith(".md"):
                continue
            with _ur.urlopen(f["download_url"], timeout=10) as resp:
                content = resp.read()
            (skills_dir / f["name"]).write_bytes(content)
            synced += 1
        if synced:
            slog(f"[auto-update] synced {synced} skills from {repo}")
    except Exception as e:
        slog(f"[auto-update] skills sync error: {e}")


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
                    # Serve any .crt/.pem file in TLS_DIR by name, plus /api/cert legacy path
                    import urllib.parse
                    req = urllib.parse.unquote(self.path.split("?")[0]).lstrip("/")
                    served = None
                    if req == "api/cert":
                        p = TLS_DIR / "cert.pem"
                        if p.exists():
                            served = (p, "application/x-pem-file", "amux.pem")
                    elif req and (req.endswith(".crt") or req.endswith(".pem")):
                        p = TLS_DIR / req
                        if p.exists():
                            fname = p.name
                            served = (p, "application/x-x509-ca-cert", fname)
                    if served:
                        p, ctype, fname = served
                        body = p.read_bytes()
                        self.send_response(200)
                        self.send_header("Content-Type", ctype)
                        self.send_header("Content-Disposition", f'attachment; filename="{fname}"')
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

    # Register all recurring jobs with the unified scheduler
    schedule_job(_yolo_loop,             interval=3,                    name="yolo",        initial_delay=3)
    schedule_job(_snapshot_loop,         interval=60,                   name="snapshot",    initial_delay=0)
    schedule_job(_email_sync_job,        interval=_EMAIL_SYNC_INTERVAL, name="email_sync",  initial_delay=20)
    if _AUTO_UPDATE_REPO:
        slog(f"[auto-update] watching {_AUTO_UPDATE_REPO}@{_AUTO_UPDATE_BRANCH} every {_AUTO_UPDATE_INTERVAL}s")
        schedule_job(_auto_update_check, interval=_AUTO_UPDATE_INTERVAL, name="auto_update", initial_delay=_AUTO_UPDATE_INTERVAL)

    # Start file watcher thread
    watcher = threading.Thread(target=_watch_self, args=(server,), daemon=True)
    watcher.start()

    # Initial snapshot immediately, then unified scheduler takes over
    threading.Thread(target=_snapshot_all_sessions, daemon=True).start()
    threading.Thread(target=_scheduler_loop, daemon=True).start()

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n\033[2mStopped.\033[0m")
        server.server_close()


if __name__ == "__main__":
    main()
