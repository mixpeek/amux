#!/usr/bin/env bash
# amux grid — tile N worker terminals into a grid, Rectangle-style.
#
#   amux grid mvs-infra byo-ray tubescience desktop --cols 2
#   amux grid --rows 1 backend frontend            # one row, side by side
#   amux grid --remote --cols 3 $(amux ls --names | head -6)
#   amux grid --cols 2 a b c d --dry-run           # print the plan, open nothing
#
# WHY THIS EXISTS AS A COMMAND RATHER THAN A RECTANGLE SHORTCUT. Rectangle tiles
# whatever happens to be focused; it has no idea which window is which worker. The
# mapping from "these six lanes" to "these six rects" is the actual work, and it is
# the part a window manager cannot do. This opens the terminals AND places them, so
# the grid is reproducible from a worker list rather than from a sequence of drags.
#
# ── Design notes, because each of these was a real choice ────────────────────
#
# WINDOWS, NOT SPLIT PANES. iTerm2 panes would give a tighter grid (no titlebars),
# but they cannot be moved between displays, cannot be full-screened individually,
# and Rectangle/Mission Control cannot see them. The ask was Rectangle-shaped, so
# these are real windows and every normal macOS window affordance keeps working.
#
# GEOMETRY THROUGH iTerm2's OWN API, NOT System Events. Setting `bounds` on an
# iTerm2 window needs only Automation permission for iTerm2. Driving it through
# System Events would additionally need Accessibility, which is the permission
# class that is already causing trouble on this machine ("amux-server-rs was
# prevented from modifying apps"). Fewer permissions, fewer prompts, and it fails
# with a clear AppleScript error rather than silently doing nothing.
#
# visibleFrame, NOT frame. This is what makes it agree with Rectangle: NSScreen's
# visibleFrame already excludes the menu bar and the Dock, on whichever edge the
# Dock currently lives. Computing that by hand is where hand-rolled tilers go
# wrong. Read via JXA's ObjC bridge, so there is no pyobjc/yabai dependency: the
# stock `osascript` can see AppKit.
#
# EDGES, NOT WIDTHS. Cells are computed as rounded EDGE positions and each cell
# takes the span between its edges. Multiplying a truncated width by a column
# index leaves a growing dead strip on the right; this way column N's right edge
# is exactly the screen's right edge, at any count.
set -uo pipefail

GAP=8
COLS=""
ROWS=""
DISPLAY_IDX=0
ATTACH="amux"
DRY=0
EXCLUSIVE=0
WORKERS=()

die() { echo "amux grid: $*" >&2; exit 1; }

usage() {
  cat <<'EOF'
usage: amux grid [options] <worker>...

  -c, --cols N     columns (workers per row)
  -r, --rows N     rows (workers per column)
                   Give either one and the other is derived. Give neither and
                   the grid is as square as possible, preferring wider.
  -g, --gap PX     gap between windows and screen edge (default 8; 0 = flush)
  -d, --display N  display index, 0 = main (see --list-displays)
      --remote     attach with `amux-remote attach` (SSH) instead of `amux attach`
      --exclusive  detach other tmux clients first (local only)
      --dry-run    print the computed plan and the AppleScript; open nothing
      --list-displays
  -h, --help

Every window runs the attach in a login shell and DROPS TO A SHELL on detach,
so detaching a lane leaves you a usable terminal instead of closing the window.

NOTE ON SIZING. tmux fits a session to its SMALLEST attached client, so a lane
you already have open in a big window elsewhere will fight its grid cell: one of
the two shrinks. `--exclusive` detaches the other viewers first and makes the
layout deterministic. It destroys nothing — the session and everything running
in it keep going, only the other view goes away.
EOF
}

# ── screen geometry ─────────────────────────────────────────────────────────
# One JXA call. AppKit gives visibleFrame in Cocoa coordinates (origin
# bottom-left, y up); iTerm2's `bounds` wants upper-left origin with y down. The
# conversion needs the PRIMARY screen's full height, which is why screens[0] is
# reported separately from the target display.
screens_json() {
  osascript -l JavaScript -e '
    ObjC.import("AppKit");
    const s = $.NSScreen.screens;
    const out = [];
    for (let i = 0; i < s.count; i++) {
      const v = s.objectAtIndex(i).visibleFrame, f = s.objectAtIndex(i).frame;
      out.push({i:i, vx:v.origin.x, vy:v.origin.y, vw:v.size.width, vh:v.size.height,
                fw:f.size.width, fh:f.size.height});
    }
    JSON.stringify({primaryH: s.objectAtIndex(0).frame.size.height, screens: out});
  ' 2>/dev/null
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -c|--cols)    COLS="${2:-}"; shift 2 ;;
    -r|--rows)    ROWS="${2:-}"; shift 2 ;;
    -g|--gap)     GAP="${2:-}"; shift 2 ;;
    -d|--display) DISPLAY_IDX="${2:-}"; shift 2 ;;
    --remote)     ATTACH="amux-remote"; shift ;;
    --exclusive)  EXCLUSIVE=1; shift ;;
    --dry-run)    DRY=1; shift ;;
    --list-displays)
      screens_json | python3 -c '
import json, sys
for s in json.load(sys.stdin)["screens"]:
    i, fw, fh = int(s["i"]), int(s["fw"]), int(s["fh"])
    vw, vh = int(s["vw"]), int(s["vh"])
    print("  display {}: {}x{}  usable {}x{}".format(i, fw, fh, vw, vh))
'
      exit 0 ;;
    -h|--help)    usage; exit 0 ;;
    -*)           die "unknown option: $1 (try --help)" ;;
    *)            WORKERS+=("$1"); shift ;;
  esac
done

[[ ${#WORKERS[@]} -gt 0 ]] || { usage; exit 1; }
command -v osascript >/dev/null || die "osascript not found — this is macOS-only"
[[ -d /Applications/iTerm.app ]] || die "iTerm2 not installed (/Applications/iTerm.app)"

# VALIDATE THE NAMES BEFORE OPENING ANYTHING. Opening five good windows and
# discovering the sixth was a typo leaves you worse off than refusing: you now
# have to find and close five. A wrong name is also the likely input here, since
# these are typed from memory.
if [[ -n "${AMUX_URL:-}" ]]; then
  known=$(curl -sk --max-time 5 "$AMUX_URL/api/sessions" 2>/dev/null \
    | python3 -c 'import json,sys
try: print("\n".join(s["name"] for s in json.load(sys.stdin)))
except Exception: pass' 2>/dev/null)
  if [[ -n "$known" ]]; then
    missing=()
    for w in "${WORKERS[@]}"; do
      grep -qxF "$w" <<<"$known" || missing+=("$w")
    done
    if [[ ${#missing[@]} -gt 0 ]]; then
      echo "amux grid: unknown worker(s): ${missing[*]}" >&2
      echo "  did you mean:" >&2
      for m in "${missing[@]}"; do
        grep -i "$(printf '%s' "$m" | cut -c1-4)" <<<"$known" | head -3 | sed 's/^/    /' >&2
      done
      exit 1
    fi
  fi
  # A silent `known` (server down, no token) is NOT a validation failure — it is
  # an unrun check, and refusing on it would make the grid unusable exactly when
  # the server is the thing you are trying to look at. Say so and continue.
  [[ -n "$known" ]] || echo "amux grid: could not reach $AMUX_URL — skipping name validation" >&2
fi

GEO=$(screens_json)
[[ -n "$GEO" ]] || die "could not read display geometry (Automation permission for System Events/AppKit?)"

# GEO travels in the ENVIRONMENT, not on stdin: stdin is the heredoc carrying
# this program, so `sys.stdin.read()` here would return nothing at all.
PLAN=$(AMUX_GRID_GEO="$GEO" python3 - "$DISPLAY_IDX" "$COLS" "$ROWS" "$GAP" "$ATTACH" "$EXCLUSIVE" "${WORKERS[@]}" <<'PY'
import json, math, os, sys
geo = json.loads(os.environ["AMUX_GRID_GEO"])
di, cols, rows, gap, attach, exclusive, *workers = sys.argv[1:]
di, gap, exclusive = int(di), int(gap), int(exclusive)
n = len(workers)

scr = next((s for s in geo["screens"] if int(s["i"]) == di), None)
if scr is None:
    sys.exit(f"display {di} not found ({len(geo['screens'])} attached)")

# Derive the missing dimension. Preferring WIDER (ceil on cols) matches how
# terminals are actually read: 80+ columns of text matters more than height.
cols, rows = (int(cols) if cols else 0), (int(rows) if rows else 0)
if not cols and not rows:
    cols = math.ceil(math.sqrt(n)); rows = math.ceil(n / cols)
elif cols and not rows:
    rows = math.ceil(n / cols)
elif rows and not cols:
    cols = math.ceil(n / rows)
if cols * rows < n:
    sys.exit(f"{cols}x{rows} has {cols*rows} cells but {n} workers were given")

# Cocoa (origin bottom-left, y up) -> AppleScript bounds (origin top-left, y down).
left = scr["vx"] + gap
top = geo["primaryH"] - (scr["vy"] + scr["vh"]) + gap
width = scr["vw"] - 2 * gap
height = scr["vh"] - 2 * gap

def edge(i, total, span, base):
    return base + round(i * span / total)

cells = []
for k, w in enumerate(workers):
    r, c = divmod(k, cols)
    x1 = edge(c, cols, width, left)
    x2 = edge(c + 1, cols, width, left)
    y1 = edge(r, rows, height, top)
    y2 = edge(r + 1, rows, height, top)
    cells.append({"worker": w, "l": x1, "t": y1,
                  "r": x2 - (gap if c < cols - 1 else 0),
                  "b": y2 - (gap if r < rows - 1 else 0)})

# `exec $SHELL -l` after the attach: detaching a lane leaves a usable terminal
# rather than a window that vanishes mid-thought.
#
# --exclusive exists because of how tmux sizes a session: it fits the SMALLEST
# attached client. A lane you already have open full-screen elsewhere will drag
# its grid cell down to that size, or be dragged down to the cell's, and the
# result looks like the grid "did not work". Detaching other clients first makes
# the layout deterministic, and it destroys nothing: the session and everything
# running in it are untouched, only the other viewer goes away.
#
# Not the default, because kicking a window someone is reading is exactly the
# kind of decision that should be typed rather than inherited.
#
# Local only. Under --remote the tmux server is on the far side of the SSH, so a
# local detach-client would silently match nothing.
lines = ['tell application "iTerm"', '  activate']
for c in cells:
    pre = ""
    if exclusive and attach == "amux":
        pre = f"tmux detach-client -s amux-{c['worker']} 2>/dev/null; "
    cmd = f"{pre}{attach} attach {c['worker']}; exec $SHELL -l"
    esc = cmd.replace("\\", "\\\\").replace('"', '\\"')
    lines.append(f'  set w to (create window with default profile command "/bin/bash -lc \\"{esc}\\"")')
    lines.append(f"  set bounds of w to {{{c['l']}, {c['t']}, {c['r']}, {c['b']}}}")
lines.append("end tell")

print(json.dumps({"cols": cols, "rows": rows, "cells": cells,
                  "applescript": "\n".join(lines)}))
PY
) || exit 1

echo "$PLAN" | python3 -c '
import json, sys
p = json.load(sys.stdin)
print("amux grid: {}x{} - {} window(s)".format(p["cols"], p["rows"], len(p["cells"])))
for c in p["cells"]:
    w, h = c["r"] - c["l"], c["b"] - c["t"]
    print("  {:<22} {},{} -> {},{}  ({}x{})".format(
        c["worker"], c["l"], c["t"], c["r"], c["b"], w, h))
'

SCRIPT=$(echo "$PLAN" | python3 -c 'import json,sys; print(json.load(sys.stdin)["applescript"])')

if [[ "$DRY" -eq 1 ]]; then
  echo ""
  echo "--- AppleScript (not run) ---"
  echo "$SCRIPT"
  exit 0
fi

osascript -e "$SCRIPT" >/dev/null || die "iTerm2 refused the layout (Automation permission for iTerm2?)"
