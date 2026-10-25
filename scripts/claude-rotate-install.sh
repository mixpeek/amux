#!/usr/bin/env bash
# Install claude-rotate: the script, the status-line recorder in the global
# Claude settings, and an amux shell schedule that runs `tick` every 5 minutes.
# Idempotent. Re-run after updating scripts/claude-rotate.py.
set -euo pipefail
src="$(cd "$(dirname "$0")" && pwd)/claude-rotate.py"
dest="$HOME/.amux/claude-rotate/claude-rotate.py"
mkdir -p "$(dirname "$dest")" "$HOME/.amux/claude-usage"
tmp="$(mktemp "$(dirname "$dest")/.new.XXXXXX")"
cp "$src" "$tmp" && chmod 755 "$tmp" && mv "$tmp" "$dest"
echo "installed $dest"

python3 - "$dest" <<'EOF'
import json, os, sys, tempfile
dest = sys.argv[1]
path = os.path.expanduser("~/.claude/settings.json")
data = json.load(open(path))
want = {"type": "command", "command": "python3 " + dest + " statusline", "padding": 0}
cur = data.get("statusLine")
if cur == want:
    print("statusLine already set")
elif cur not in (None, {}):
    sys.exit("statusLine is already set to something else; not overwriting: %r" % cur)
else:
    data["statusLine"] = want
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path))
    with os.fdopen(fd, "w") as f:
        f.write(json.dumps(data, indent=2) + "\n")
    os.replace(tmp, path)
    print("statusLine set in", path)
EOF

U="$(amux url)"
if curl -sk "$U/api/schedules" | python3 -c "import json,sys; d=json.load(sys.stdin); d=d.get('schedules',d) if isinstance(d,dict) else d; sys.exit(0 if any(s.get('title')=='claude-rotate tick' for s in d) else 1)"; then
  echo "schedule already present"
else
  code=$(curl -sk --max-time 20 -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    -H "X-Amux-Session: ${AMUX_SESSION:-owner}" \
    -d "{\"title\":\"claude-rotate tick\",\"command\":\"python3 $dest tick\",\"schedule_expr\":\"every 5m\",\"kind\":\"shell\"}" \
    "$U/api/schedules")
  case "$code" in 2??) echo "schedule created (every 5m, shell)";; *) echo "schedule create refused: $code" >&2; exit 1;; esac
fi
# Automatic switching is opt-in and off by default, even after install.
# CLAUDE_ROTATE_ENABLE=1 opts in here; `python3 $dest on` does it later.
if [ "${CLAUDE_ROTATE_ENABLE:-0}" = 1 ]; then python3 "$dest" on; else echo "automatic switching is off (opt-in); enable with: python3 $dest on"; fi
python3 "$dest" pick
