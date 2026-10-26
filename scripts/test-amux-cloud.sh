#!/usr/bin/env bash
# amux-cloud / amux-remote named remotes, with ssh, su, tmux and curl stubbed:
# connect proves the box answers before saving, every call goes over SSH as
# the box's amux user, a message arrives byte for byte (quotes, $(), backticks
# are data, never run), and attach opens the worker's tmux as that user.
set -uo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
T="$(mktemp -d)"
trap 'rm -rf "${T:?}"' EXIT
mkdir -p "$T/bin" "$T/home"
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }

# ssh: log the target and run the remote command line here, as the box would.
cat > "$T/bin/ssh" <<'EOF'
#!/usr/bin/env bash
target="" cmd=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -o|-i) shift 2 ;;
    -t) shift ;;
    *) if [[ -z "$target" ]]; then target="$1"; else cmd="$1"; fi; shift ;;
  esac
done
printf '%s\n' "$target" >> "$STUB_LOG.targets"
exec bash -c "$cmd"
EOF
# su - USER -c CMD: record who, run CMD.
cat > "$T/bin/su" <<'EOF'
#!/usr/bin/env bash
[[ "$1" == - ]] && shift
printf '%s\n' "$1" >> "$STUB_LOG.as"
shift; [[ "$1" == -c ]] && shift
exec bash -c "$1"
EOF
# curl: answer /health, and save a POST body exactly as received.
cat > "$T/bin/curl" <<'EOF'
#!/usr/bin/env bash
url="" body=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    -d|--data|--data-binary) body="$2"; shift 2 ;;
    -X|-H|-m) shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
printf '%s\n' "$url" >> "$STUB_LOG.urls"
case "$url" in
  */health) echo '{"status":"ok","commit":"abc123"}' ;;
  */api/sessions) echo '[{"name":"mtp-demo","status":"active"}]' ;;
  */send) printf '%s' "$body" > "$STUB_LOG.body"; echo '{"ok":true}' ;;
  *) echo '{}' ;;
esac
EOF
cat > "$T/bin/tmux" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_LOG.tmux"
EOF
chmod +x "$T/bin/"*
# The box's command line puts ~/.local/bin first, so the stubs live there too.
mkdir -p "$T/home/.local/bin" && cp "$T/bin/"* "$T/home/.local/bin/"
export PATH="$T/bin:$PATH" HOME="$T/home" STUB_LOG="$T/log" AMUX_CC=0
cloud="$root/amux-cloud"

out=$("$cloud" connect sandbox 2>&1)
if [[ "$out" == *"connected"*"commit abc123"* && -f "$HOME/.amux/remotes/sandbox.env" ]]; then ok "connect proves the box answers, then saves it"; else bad "connect: $out"; fi
grep -qx 'root@sandbox.amux.io' "$T/log.targets" && ok "sandbox means root@sandbox.amux.io" || bad "ssh target: $(cat "$T/log.targets")"
grep -qx 'amux' "$T/log.as" && ok "API calls run as the box's amux user" || bad "su user: $(cat "$T/log.as" 2>/dev/null)"
grep -q '^https://127.0.0.1:8824/health$' "$T/log.urls" && ok "it reads the box's loopback API, not the door" || bad "urls: $(cat "$T/log.urls")"
[[ "$(stat -c %a "$HOME/.amux/remotes/sandbox.env" 2>/dev/null || stat -f %Lp "$HOME/.amux/remotes/sandbox.env")" == 600 ]] && ok "the remote file is private (600)" || bad "remote file mode"

"$cloud" ls | grep -q mtp-demo && ok "ls lists the box's workers" || bad "ls"

msg='he said "hi" $(touch '"$T"'/pwned) `touch '"$T"'/pwned2` it'"'"'s 100%'
"$cloud" send mtp-demo "$msg" >/dev/null
got=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["text"], end="")' "$T/log.body" 2>/dev/null)
[[ "$got" == "$msg" ]] && ok "a message arrives byte for byte" || bad "message became: $got"
[[ ! -e "$T/pwned" && ! -e "$T/pwned2" ]] && ok "\$() and backticks in a message never run" || bad "a message's shell syntax ran"

"$cloud" attach mtp-demo >/dev/null 2>&1
grep -q 'attach-session -t amux-mtp-demo' "$T/log.tmux" 2>/dev/null && ok "attach opens the worker's tmux" || bad "tmux: $(cat "$T/log.tmux" 2>/dev/null)"
[[ "$(tail -1 "$T/log.as")" == amux ]] && ok "attach runs as the amux user" || bad "attach user"
"$cloud" attach 'x;touch '"$T"'/pwned3' >/dev/null 2>&1
[[ ! -e "$T/pwned3" ]] && ok "a worker name cannot inject a command" || bad "worker name injected"

# A failed connect saves nothing.
printf '#!/usr/bin/env bash\necho "curl: (7) Failed to connect"; exit 7\n' | tee "$T/bin/curl" > "$T/home/.local/bin/curl"
"$cloud" connect other.example.com >/dev/null 2>&1 && bad "connect succeeded with amux down" || ok "connect fails when amux does not answer"
[[ ! -e "$HOME/.amux/remotes/other.env" ]] && ok "a failed connect saves nothing" || bad "a failed connect saved a remote"

echo "$fails failed"
exit "$fails"
