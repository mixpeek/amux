#!/usr/bin/env bash
# Sign a remote amux host into several Claude accounts, one folder per account, so
# scripts/claude-rotate.py on that host can switch between them (AMUX_CLAUDE_ROTATE=1 in
# bootstrap.sh installs the rotation). Run it from a machine with a desktop browser.
#
#   bash cloud/desktop/claude-accounts.sh ethan@1.2.3.4 work=me@work.com personal=me@home.com
#
# For each folder=email it starts `claude auth login --email` on the host as the amux user, with
# CLAUDE_CONFIG_DIR=~/.amux/claude-accounts/<folder>, signs in through a headed browser here
# (claude-signin.mjs: it reads the emailed link through amux when that inbox is connected, and
# otherwise waits for you to finish in the window) and pastes the code back. Already signed-in
# folders are skipped. Settings:
#   CDP_PORT=9333           a headed Chrome with remote debugging. Without it, an amux browser is
#                           started headed with AMUX_BROWSER_PROFILE (default: a scratch profile)
#   BOX_USER=amux           the host user that runs amux
#   SSH_OPTS="-i key"       extra ssh options
# Written from signing mixpeek-dev.amux.io into eight accounts by hand (2026-10-31).
set -uo pipefail
TARGET="${1:?usage: claude-accounts.sh <user@host> <folder=email>...}"; shift
[[ $# -gt 0 ]] || { echo "name at least one folder=email" >&2; exit 2; }
here="$(cd "$(dirname "$0")" && pwd)"
BOX_USER="${BOX_USER:-amux}"
# shellcheck disable=SC2086
SSH=(ssh -o ConnectTimeout=15 ${SSH_OPTS:-} "$TARGET")
# The script goes over stdin: `sudo -i bash -c` joins a multi-line script into one line.
on_box() { "${SSH[@]}" "sudo -u $BOX_USER -i bash -s" <<< "$1"; }
CODE_FILE=$(mktemp); trap 'rm -f "$CODE_FILE"' EXIT

if [[ -z "${CDP_PORT:-}" ]]; then
  CDP_PORT=$(curl -sk --max-time 90 -X POST -H 'Content-Type: application/json' \
    -H "X-Amux-Session: ${AMUX_SESSION:-}" "$(amux url)/api/browser/start" \
    -d "{\"profile\":\"${AMUX_BROWSER_PROFILE:-claude-signin}\",\"url\":\"https://claude.ai/login\",\"headless\":false,\"cdp_reason\":\"claude-accounts.sh: sign a remote host into Claude accounts\"}" |
    python3 -c 'import json,sys; print(json.load(sys.stdin).get("cdp_port") or "")')
  [[ -n "$CDP_PORT" ]] || { echo "could not start a headed amux browser; set CDP_PORT" >&2; exit 3; }
  # Stop the browser this script started, however it ends, so a re-run can start it again.
  trap 'rm -f "$CODE_FILE"; curl -sk --max-time 20 -o /dev/null -X POST -H "Content-Type: application/json" \
    -H "X-Amux-Session: ${AMUX_SESSION:-}" -d "{\"profile\":\"${AMUX_BROWSER_PROFILE:-claude-signin}\"}" \
    "$(amux url)/api/browser/stop"' EXIT
fi

ok=0; failed=()
for pair in "$@"; do
  folder=${pair%%=*}; email=${pair#*=}
  [[ "$folder" =~ ^[A-Za-z0-9._-]+$ && "$email" == *@* ]] || { echo "bad pair: $pair" >&2; failed+=("$pair"); continue; }
  dir="\$HOME/.amux/claude-accounts/$folder"
  if on_box "CLAUDE_CONFIG_DIR=$dir claude auth status 2>/dev/null" | grep -q "\"email\": \"$email\""; then
    echo "$folder: already signed in as $email"; ok=$((ok+1)); continue
  fi
  echo "== $folder ($email)"
  url=$(on_box "mkdir -p $dir && chmod 700 $dir; tmux kill-session -t claude-login 2>/dev/null;
    tmux new-session -d -s claude-login -x 400 -y 50 \"CLAUDE_CONFIG_DIR=$dir BROWSER=echo claude auth login --email $email; sleep 900\";
    for i in \$(seq 20); do u=\$(tmux capture-pane -p -t claude-login -J | grep -oE 'https://claude.(com|ai)/[^ ]*oauth/authorize[^ ]+' | head -1); [ -n \"\$u\" ] && { echo \"\$u\"; break; }; sleep 1; done")
  if [[ -z "$url" ]]; then echo "$folder: claude auth login printed no URL"; failed+=("$pair"); continue; fi
  : > "$CODE_FILE"
  if ! CODE_FILE="$CODE_FILE" node "$here/claude-signin.mjs" "$CDP_PORT" "$url" "$email"; then failed+=("$pair"); continue; fi
  # the code goes over ssh stdin into the waiting prompt; it is never on a command line
  "${SSH[@]}" "c=\$(cat); sudo -u $BOX_USER tmux send-keys -t claude-login -l \"\$c\"; sudo -u $BOX_USER tmux send-keys -t claude-login Enter" < "$CODE_FILE"
  : > "$CODE_FILE"
  sleep 6
  if on_box "CLAUDE_CONFIG_DIR=$dir claude auth status 2>/dev/null" | grep -q "\"email\": \"$email\""; then
    echo "$folder: signed in as $email"; ok=$((ok+1))
  else
    echo "$folder: the host did not accept the code"; failed+=("$pair")
  fi
  on_box "tmux kill-session -t claude-login 2>/dev/null" || true
done
# Hand the host's login to the rotator: with accounts signed in, a fixed CLAUDE_CODE_OAUTH_TOKEN
# in server.env would pin every worker to the account it came from (mixpeek-dev, 2026-10-31).
if [[ $ok -gt 0 ]] && on_box "test -f ~/.amux/claude-rotate/claude-rotate.py"; then
  first=$(for pair in "$@"; do echo "${pair#*=}"; done | head -1)
  on_box "claude auth status 2>/dev/null | grep -q '\"loggedIn\": true' || python3 ~/.amux/claude-rotate/claude-rotate.py switch $first | tail -1
    if grep -q '^CLAUDE_CODE_OAUTH_TOKEN=' ~/.amux/server.env; then
      cp -p ~/.amux/server.env ~/.amux/server.env.bak-\$(date +%Y%m%d%H%M)
      sed -i 's/^CLAUDE_CODE_OAUTH_TOKEN=/# claude-rotate owns the login: CLAUDE_CODE_OAUTH_TOKEN=/' ~/.amux/server.env
      tmux setenv -gu CLAUDE_CODE_OAUTH_TOKEN 2>/dev/null
      XDG_RUNTIME_DIR=/run/user/\$(id -u) systemctl --user restart amux-server
      echo 'server.env: CLAUDE_CODE_OAUTH_TOKEN dropped; workers started before now keep it until restarted'
    fi
    python3 ~/.amux/claude-rotate/claude-rotate.py pick | head -1"
fi
echo "done: $ok signed in, ${#failed[@]} failed${failed[*]:+ (re-run with: ${failed[*]})}"
[[ ${#failed[@]} -eq 0 ]]
