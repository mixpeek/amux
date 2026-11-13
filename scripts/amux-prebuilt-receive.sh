#!/usr/bin/env bash
# The ONLY thing a build host's key may do on a host that installs prebuilt binaries
# (docs/design/cicd-two-minutes.md, Z4): authorized_keys forces this command, so the key can
# write one file per commit into AMUX_DEPLOY_PREBUILT_DIR and nothing else.
#
#   command="/path/to/amux-prebuilt-receive.sh",no-port-forwarding,no-pty,... ssh-ed25519 AAAA...
#
# The build host runs: ssh <host> "receive <40-hex sha>" < amux-server
#                      ssh <host> "has <40-hex sha>"      (exit 0 when already received)
# rust-auto-build.sh then installs it for that exact sha, through its usual checks.
set -euo pipefail
DIR="${AMUX_DEPLOY_PREBUILT_DIR:-$HOME/.amux/prebuilt}"
KEEP="${AMUX_PREBUILT_KEEP:-10}"
read -r verb sha extra <<< "${SSH_ORIGINAL_COMMAND:-${*:-}}" || true
say() { echo "amux-prebuilt-receive: $*" >&2; logger -t amux-prebuilt "$*" 2>/dev/null || true; }
[ -z "${extra:-}" ] || { say "refused: extra arguments"; exit 2; }
[[ "${sha:-}" =~ ^[0-9a-f]{40}$ ]] || { say "refused: '${sha:-}' is not a 40-hex commit"; exit 2; }
case "${verb:-}" in
  has)
    [ -s "$DIR/$sha/amux-server" ] && exit 0 || exit 1 ;;
  receive)
    mkdir -p "$DIR/$sha"
    tmp=$(mktemp "$DIR/$sha/.incoming.XXXXXX")
    trap 'rm -f -- "$tmp"' EXIT
    cat > "$tmp"
    size=$(wc -c < "$tmp" | tr -d ' ')
    # A real amux-server is over 100 MB; a test stand-in is a few bytes (3283eeb11).
    if [ "$size" -le 1048576 ] || ! head -c 4 "$tmp" | grep -q "ELF"; then
      say "refused $sha: $size bytes, not an ELF executable verdict=prebuilt_refused"; exit 3
    fi
    chmod 0755 "$tmp"
    mv -f "$tmp" "$DIR/$sha/amux-server"
    trap - EXIT
    say "received $sha ($size bytes) verdict=prebuilt_received"
    # Start the builder now rather than at its next pass (up to 15 s): it is waiting
    # for exactly this file. Best effort: the timer runs it anyway.
    if [ "${AMUX_PREBUILT_KICK:-1}" = 1 ] && command -v systemctl >/dev/null 2>&1; then
      XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" systemctl --user start --no-block amux-builder.service 2>/dev/null \
        && say "started the builder for $sha verdict=prebuilt_builder_kicked" || true
    fi
    # Keep the newest KEEP commits.
    ls -1t "$DIR" 2>/dev/null | tail -n +"$((KEEP + 1))" | while read -r old; do
      [[ "$old" =~ ^[0-9a-f]{40}$ ]] && rm -rf -- "${DIR:?}/${old:?}"
    done
    ;;
  *)
    say "refused: verb '${verb:-}' (receive or has)"; exit 2 ;;
esac
