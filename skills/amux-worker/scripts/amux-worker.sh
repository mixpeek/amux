#!/usr/bin/env bash
# amux-worker.sh — bash+curl+python3 client for the amux API.
#
# Works for ANY CLI agent (Claude Code, Codex, Gemini CLI, a bare shell) —
# it has no dependency on Claude Code's skill/tool machinery, just curl and
# python3 (both assumed present; no jq requirement).
#
# Usage: amux-worker.sh <family> <verb> [args...]
# Run with no args, or `help`, for the full command list.
set -euo pipefail

# ---------------------------------------------------------------------------
# Base URL resolution
# ---------------------------------------------------------------------------
resolve_url() {
  if [[ -n "${AMUX_URL:-}" ]]; then
    echo "$AMUX_URL"
    return
  fi
  if command -v amux >/dev/null 2>&1; then
    local u
    u=$(amux url 2>/dev/null || true)
    if [[ -n "$u" ]]; then
      echo "$u"
      return
    fi
  fi
  if [[ -f "$HOME/.amux/endpoint.json" ]]; then
    python3 -c '
import json
try:
    print(json.load(open("'"$HOME"'/.amux/endpoint.json"))["canonical_url"])
except Exception:
    pass
' 2>/dev/null && return
  fi
  echo "https://localhost:8824"
}
AMUX_API="$(resolve_url)"

# Attribution: every mutating call carries X-Amux-Worker so amux logs (and
# `/api/logs/analyze`) can trace it back to whichever agent made the change.
# Precedence: explicit AMUX_WORKER env var > AMUX_SESSION (tmux session name,
# set for Claude Code lanes running inside amux) > a generic fallback so a
# Codex/Gemini worker with neither still stamps SOMETHING identifiable.
WORKER_ID="${AMUX_WORKER:-${AMUX_SESSION:-amux-worker-skill}}"

_curl() { curl -sk "$@"; }
_pp() { python3 -m json.tool 2>/dev/null || cat; }

# Build a JSON object from python so we never hand-roll string escaping.
# Usage: _json key1 value1 key2 value2 ...  (values are always strings)
_json() {
  python3 -c '
import json, sys
argv = sys.argv[1:]
d = {}
for i in range(0, len(argv), 2):
    d[argv[i]] = argv[i + 1]
print(json.dumps(d))
' "$@"
}

die() { echo "error: $*" >&2; exit 1; }

usage() {
  cat <<'EOF'
amux-worker.sh — bash client for the amux API (board, sessions, memory, notes, health)

  BOARD
    board list [status]                 List cards (optionally filter by status)
    board get <id>                      Full card detail
    board add <title> [desc] [type]     Create a card (status defaults to todo)
    board status <id> <new-status>      Move status (auto-acknowledges the type's gate)
    board claim <id> [session]          Atomically claim a card
    board delete <id>                   Delete a card

  SESSIONS
    sessions list                       List all sessions (name, status, backend, branch)
    sessions status <name>              Full detail for one session
    sessions peek <name> [lines]        Recent terminal output (default 80 lines)
    sessions send <name> <text>         Send a message into a session

  MEMORY
    memory get [session]                Read a session's memory (default: $AMUX_SESSION)
    memory set [session] <content>      Overwrite a session's memory
    memory global get                   Read global (fleet-wide) memory
    memory global set <content>         Overwrite global memory

  NOTES  (backed by /api/memories — the amux "memories" primitive; there is
          no separate /api/notes endpoint on this server, see amux-worker.md)
    notes list                          List global/org-scope notes
    notes get <id>                      Read one note
    notes create <name> <content> [type]  Create (type: reference|project|user|feedback, default reference)
    notes update <id> <content>         Update a note's content
    notes delete <id>                   Soft-delete a note

  HEALTH
    health                              GET /health, pretty-printed

Env:
  AMUX_URL      base URL (default: resolved via `amux url` / ~/.amux/endpoint.json / localhost:8824)
  AMUX_WORKER   identity stamped on X-Amux-Worker for mutating calls (default: $AMUX_SESSION or "amux-worker-skill")
EOF
}

# ---------------------------------------------------------------------------
# BOARD
# ---------------------------------------------------------------------------
cmd_board() {
  local verb="${1:-}"; shift || true
  case "$verb" in
    list)
      local status_filter="${1:-}"
      _curl "$AMUX_API/api/board" | python3 -c '
import json, sys
items = json.load(sys.stdin)
flt = sys.argv[1] if len(sys.argv) > 1 else ""
print("ID".ljust(10), "STATUS".ljust(10), "TYPE".ljust(12), "TITLE")
for c in items:
    if flt and c["status"] != flt:
        continue
    cid, st, ty, title = c["id"], c["status"], c["type"], c["title"]
    print(cid.ljust(10), st.ljust(10), ty.ljust(12), title)
' "$status_filter"
      ;;
    get)
      local id="${1:?usage: board get <id>}"
      _curl "$AMUX_API/api/board/$id" | _pp
      ;;
    add)
      local title="${1:?usage: board add <title> [desc] [type]}"
      local desc="${2:-}"
      local type="${3:-}"
      local body
      if [[ -n "$type" ]]; then
        body=$(_json title "$title" desc "$desc" status todo type "$type")
      else
        body=$(_json title "$title" desc "$desc" status todo)
      fi
      _curl -X POST -H 'Content-Type: application/json' \
        -H "X-Amux-Worker: $WORKER_ID" \
        -d "$body" "$AMUX_API/api/board" | _pp
      ;;
    status)
      local id="${1:?usage: board status <id> <new-status>}"
      local new_status="${2:?usage: board status <id> <new-status>}"
      # Non-terminal-column moves on gated types (default "code") are
      # blocked unless the request acknowledges the gate. gate_ack:true
      # acknowledges without auditing individual criteria — good enough
      # for worker automation; use `board update` with gate_checked for a
      # per-criterion ack. See amux-worker.md for what this trades away.
      local body
      body=$(python3 -c 'import json,sys;print(json.dumps({"status":sys.argv[1],"gate_ack":True}))' "$new_status")
      _curl -X PATCH -H 'Content-Type: application/json' \
        -H "X-Amux-Worker: $WORKER_ID" \
        -d "$body" "$AMUX_API/api/board/$id" | _pp
      ;;
    claim)
      local id="${1:?usage: board claim <id> [session]}"
      local session="${2:-$WORKER_ID}"
      _curl -X POST -H 'Content-Type: application/json' \
        -d "$(_json session "$session")" "$AMUX_API/api/board/$id/claim" | _pp
      ;;
    delete)
      local id="${1:?usage: board delete <id>}"
      _curl -X DELETE "$AMUX_API/api/board/$id" | _pp
      ;;
    *) die "board: unknown verb '$verb' (list|get|add|status|claim|delete)" ;;
  esac
}

# ---------------------------------------------------------------------------
# SESSIONS
# ---------------------------------------------------------------------------
cmd_sessions() {
  local verb="${1:-}"; shift || true
  case "$verb" in
    list)
      _curl "$AMUX_API/api/sessions" | python3 -c '
import json, sys
items = json.load(sys.stdin)
print("NAME".ljust(16), "STATUS".ljust(10), "BACKEND".ljust(8), "BRANCH")
for s in items:
    name, st, be, br = s["name"], s.get("status", ""), s.get("backend", ""), s.get("branch", "")
    print(name.ljust(16), st.ljust(10), be.ljust(8), br)
'
      ;;
    status)
      local name="${1:?usage: sessions status <name>}"
      _curl "$AMUX_API/api/sessions/$name" | _pp
      ;;
    peek)
      local name="${1:?usage: sessions peek <name> [lines]}"
      local lines="${2:-80}"
      _curl "$AMUX_API/api/sessions/$name/peek?lines=$lines" | _pp
      ;;
    send)
      local name="${1:?usage: sessions send <name> <text>}"
      shift
      local text="$*"
      [[ -n "$text" ]] || die "sessions send: text is required"
      _curl -X POST -H 'Content-Type: application/json' \
        -H "X-Amux-Worker: $WORKER_ID" \
        -d "$(_json text "$text")" "$AMUX_API/api/sessions/$name/send" | _pp
      ;;
    *) die "sessions: unknown verb '$verb' (list|status|peek|send)" ;;
  esac
}

# ---------------------------------------------------------------------------
# MEMORY  (per-session memory files + global memory doc)
# ---------------------------------------------------------------------------
cmd_memory() {
  local verb="${1:-}"; shift || true
  case "$verb" in
    get)
      local session="${1:-${AMUX_SESSION:-}}"
      [[ -n "$session" ]] || die "memory get: no session given and \$AMUX_SESSION is unset"
      _curl "$AMUX_API/api/sessions/$session/memory" | python3 -c 'import json,sys;print(json.load(sys.stdin)["content"])'
      ;;
    set)
      local session content
      if [[ $# -ge 2 ]]; then
        session="$1"; shift; content="$*"
      else
        session="${AMUX_SESSION:-}"; content="${1:-}"
      fi
      [[ -n "$session" ]] || die "memory set: no session given and \$AMUX_SESSION is unset"
      _curl -X POST -H 'Content-Type: application/json' \
        -d "$(_json content "$content")" "$AMUX_API/api/sessions/$session/memory" | _pp
      ;;
    global)
      local sub="${1:-}"; shift || true
      case "$sub" in
        get) _curl "$AMUX_API/api/memory/global" | python3 -c 'import json,sys;print(json.load(sys.stdin)["content"])' ;;
        set)
          local content="$*"
          _curl -X POST -H 'Content-Type: application/json' \
            -d "$(_json content "$content")" "$AMUX_API/api/memory/global" | _pp
          ;;
        *) die "memory global: unknown verb '$sub' (get|set)" ;;
      esac
      ;;
    *) die "memory: unknown verb '$verb' (get|set|global)" ;;
  esac
}

# ---------------------------------------------------------------------------
# NOTES  (documents / reference material — backed by /api/memories, the
# amux "memories" primitive; there is no dedicated /api/notes route on this
# server, see the "Notes" section of amux-worker.md for why)
# ---------------------------------------------------------------------------
cmd_notes() {
  local verb="${1:-}"; shift || true
  case "$verb" in
    list)
      _curl "$AMUX_API/api/memories" | python3 -c '
import json, sys
d = json.load(sys.stdin)
print("ID".ljust(30), "TYPE".ljust(10), "SCOPE".ljust(8), "NAME")
for it in d["items"]:
    mid, mt, sc, name = it["id"], it["memory_type"], it["scope"]["level"], it["name"]
    print(mid.ljust(30), mt.ljust(10), sc.ljust(8), name)
'
      ;;
    get)
      local id="${1:?usage: notes get <id>}"
      _curl "$AMUX_API/api/memories/$id" | _pp
      ;;
    create)
      local name="${1:?usage: notes create <name> <content> [type]}"
      local content="${2:?usage: notes create <name> <content> [type]}"
      local mtype="${3:-reference}"
      local body
      body=$(python3 -c '
import json, sys
name, content, mtype = sys.argv[1:4]
print(json.dumps({
    "scope": {"level": "global"},
    "name": name,
    "content": content,
    "memory_type": mtype,
}))
' "$name" "$content" "$mtype")
      _curl -X POST -H 'Content-Type: application/json' \
        -d "$body" "$AMUX_API/api/memories" | _pp
      ;;
    update)
      local id="${1:?usage: notes update <id> <content>}"
      local content="${2:?usage: notes update <id> <content>}"
      _curl -X PATCH -H 'Content-Type: application/json' \
        -d "$(_json content "$content")" "$AMUX_API/api/memories/$id" | _pp
      ;;
    delete)
      local id="${1:?usage: notes delete <id>}"
      _curl -X DELETE "$AMUX_API/api/memories/$id" | _pp
      ;;
    *) die "notes: unknown verb '$verb' (list|get|create|update|delete)" ;;
  esac
}

# ---------------------------------------------------------------------------
# HEALTH
# ---------------------------------------------------------------------------
cmd_health() {
  _curl "$AMUX_API/health" | _pp
}

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------
main() {
  local family="${1:-}"; shift || true
  case "$family" in
    board) cmd_board "$@" ;;
    sessions) cmd_sessions "$@" ;;
    memory) cmd_memory "$@" ;;
    notes) cmd_notes "$@" ;;
    health) cmd_health "$@" ;;
    ""|help|-h|--help) usage ;;
    *) die "unknown family '$family' (board|sessions|memory|notes|health|help)" ;;
  esac
}

main "$@"
