#!/usr/bin/env bash
# Keep a standalone amux host (amux Cloud, cloud/start) on the newest CI-built amux, with no one
# touching it. Run as root by amux-update.timer (bootstrap.sh installs it): hourly, and 5 minutes
# after boot.
#
#   1. Fast-forward the amux checkout (the bash CLI, this script, the unit templates).
#   2. Compare ghcr.io/mixpeek/amux:latest with the digest installed last time. The image is
#      rebuilt only after CI passes on main (.github/workflows/deploy-cloud.yml), so "latest" is a
#      green build.
#   3. New digest: copy its binaries in by rename, restart the server, and require /health to answer
#      within 120 s. Otherwise put the previous binaries back and restart again.
#   4. Make sure the server and Caddy are running even when nothing changed (crash recovery).
#
# The outcome is written to /var/lib/amux-update/status.json, which amux-door serves at
# /_door/status, so cloud.amux.io's monitor sees a failed or stuck update without logging in.
# Every run logs one line with verdict=... to the journal (journalctl -t amux-update).
set -uo pipefail
USER_NAME="${AMUX_USER:-amux}"
HOME_DIR="$(getent passwd "$USER_NAME" | cut -d: -f6)"
BIN_DIR="$HOME_DIR/.local/bin"
SRC="$HOME_DIR/Dev/amux"
IMAGE_REPO="${AMUX_IMAGE_REPO:-mixpeek/amux}"
STATE=/var/lib/amux-update
STATUS="$STATE/status.json"
install -d -m 755 "$STATE"
exec 9>"$STATE/lock"; flock -n 9 || { echo "another update is running"; exit 0; }

UID_N=$(id -u "$USER_NAME")
as_user() { runuser -u "$USER_NAME" -- env HOME="$HOME_DIR" "$@"; }
ucl() { as_user XDG_RUNTIME_DIR="/run/user/$UID_N" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$UID_N/bus" systemctl --user "$@"; }
say() { logger -t amux-update -- "$*"; echo "$*"; }
health() { curl -sk --max-time 5 https://127.0.0.1:8824/health; }

# Codex runs every worker command inside its own bubblewrap sandbox, which needs an unprivileged
# user namespace. Ubuntu 24.04 blocks those through AppArmor, so on a Cloud server every Codex
# command failed, even `pwd` ("bwrap: loopback: Failed RTM_NEWADDR", codex e2e round 3, C6-3).
# The server is single tenant, so allow them and let Codex's sandbox work.
USERNS=/proc/sys/kernel/apparmor_restrict_unprivileged_userns
if [[ -f "$USERNS" && "$(cat "$USERNS")" != 0 ]]; then
  echo 'kernel.apparmor_restrict_unprivileged_userns = 0' > /etc/sysctl.d/60-amux-codex-sandbox.conf
  if sysctl -q -p /etc/sysctl.d/60-amux-codex-sandbox.conf; then
    say "verdict=userns_allowed for the Codex sandbox"
  else
    say "verdict=userns_allow_failed Codex workers cannot run commands until user namespaces are allowed"
  fi
fi
commit_of() { health | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("commit",""))
except Exception: print("")'; }
wait_healthy() {
  for _ in $(seq 1 60); do [[ -n "$(commit_of)" ]] && return 0; sleep 2; done
  return 1
}
write_status() {  # verdict, detail
  python3 - "$STATUS" "$1" "$2" "$(cat "$STATE/digest" 2>/dev/null)" "$(commit_of)" "$(as_user git -C "$SRC" rev-parse --short HEAD 2>/dev/null)" <<'PY'
import json, os, sys, time
path, verdict, detail, digest, commit, checkout = sys.argv[1:]
old = {}
try: old = json.load(open(path))
except Exception: pass
now = int(time.time())
s = {"verdict": verdict, "detail": detail[:300], "checked_at": now, "digest": digest[:19],
     "commit": commit, "checkout": checkout,
     "updated_at": now if verdict == "updated" else old.get("updated_at", 0),
     "last_failure_at": now if verdict.startswith("failed") else old.get("last_failure_at", 0)}
tmp = path + ".tmp"; json.dump(s, open(tmp, "w")); os.chmod(tmp, 0o644); os.replace(tmp, path)
PY
  say "verdict=$1 commit=$(commit_of) $2"
}

# 1. Checkout (fast-forward only; a diverged checkout is reported, never forced).
SELF="$SRC/cloud/desktop/update.sh"
before=$(sha256sum "$SELF" 2>/dev/null | cut -c1-64)
if ! as_user git -C "$SRC" pull -q --ff-only 2>/dev/null; then
  say "WARN checkout $SRC did not fast-forward; keeping its current scripts"
fi
# This run is executing the bytes it started with. If the pull changed this script, run the new
# one now (once), so a fix in it lands in this run rather than an hour later.
if [[ -z "${AMUX_UPDATE_REEXEC:-}" && -f "$SELF" && "$(sha256sum "$SELF" | cut -c1-64)" != "$before" ]]; then
  say "update.sh changed in the pull; running the new version"
  exec 9>&-
  AMUX_UPDATE_REEXEC=1 exec bash "$SELF"
fi
as_user make -C "$SRC" install-cli BIN_DIR="$BIN_DIR" >/dev/null 2>&1 || say "WARN install-cli failed"

# The sign-in (door.py) ships from the checkout too, so its fixes arrive with the scripts.
if [[ -f "$SRC/cloud/desktop/door.py" ]] && ! cmp -s "$SRC/cloud/desktop/door.py" /opt/amux-door/door.py; then
  install -m 755 "$SRC/cloud/desktop/door.py" /opt/amux-door/door.py.new && mv /opt/amux-door/door.py.new /opt/amux-door/door.py
  systemctl restart amux-door && say "door.py updated from the checkout"
fi

# Claude status hooks, as install.sh registers them on a local install. Without them no Claude
# worker reports its state and its lane reads "unknown" (codex e2e round 3: the first worker never
# got its start prompt). Idempotent: the installer replaces only amux's own entries. Codex hooks are
# left out on purpose: Codex holds new hooks behind a "Hooks need review" screen until a person
# trusts them, which would stop every new Codex worker; its lanes are read from the screen instead.
HOOKS="$SRC/scripts/hooks/install-native-status-hooks.py"
if [[ -f "$HOOKS" ]]; then
  if as_user python3 "$HOOKS" --provider claude --settings "$HOME_DIR/.claude/settings.json" \
      --script "$HOME_DIR/.amux/native-status.py" >/dev/null; then
    say "verdict=status_hooks_installed provider=claude"
  else
    say "verdict=status_hooks_install_failed provider=claude workers will not report state"
  fi
fi

# Pull from remote sends local amux changes upstream as a pull request before replacing them
# (bootstrap.sh sets it on new servers). Added once; an owner who sets it to 0 keeps 0.
EF="$HOME_DIR/.amux/server.env"
if [[ -f "$EF" ]] && ! grep -q '^AMUX_PULL_SENDS_LOCAL_AS_PR=' "$EF"; then
  printf 'AMUX_PULL_SENDS_LOCAL_AS_PR=1\n' >> "$EF" && say "verdict=pull_sends_local_as_pr_enabled"
fi

# Config fixes that bootstrap.sh now writes, applied to servers bootstrapped before them. Each is
# idempotent, validated before it is used, and reverted if Caddy rejects it.
CF=/etc/caddy/Caddyfile
if [[ -f $CF ]] && grep -q 'header_up Cookie "amux_member=' $CF && ! grep -q 'header_up Host {host}' $CF; then
  cp -p $CF $CF.prev
  sed -i 's|^\( *\)header_up Cookie "amux_member=|\1header_up Host {host}\n\1header_up Cookie "amux_member=|' $CF
  if runuser -u caddy -- caddy validate --config $CF --adapter caddyfile >/dev/null 2>&1 && systemctl reload caddy; then
    say "caddy: amux now sees the public Host (team invite links)"
  else
    mv $CF.prev $CF; systemctl reload caddy; say "WARN caddy Host migration rejected; previous Caddyfile kept"
  fi
fi

# The "amux is restarting" page instead of a bare 502 (C14). The snippet is refreshed from the
# checkout every run; a Caddyfile from before it gains the import line once.
if [[ -f $CF && -f "$SRC/cloud/desktop/caddy-errors.caddy" ]] && ! cmp -s "$SRC/cloud/desktop/caddy-errors.caddy" /etc/caddy/amux-errors.caddy; then
  had=$([[ -f /etc/caddy/amux-errors.caddy ]] && echo 1 || echo 0)
  cp -p $CF $CF.prev; [[ $had == 1 ]] && cp -p /etc/caddy/amux-errors.caddy /etc/caddy/amux-errors.caddy.prev
  install -m 644 "$SRC/cloud/desktop/caddy-errors.caddy" /etc/caddy/amux-errors.caddy
  grep -q 'import /etc/caddy/amux-errors.caddy' $CF || sed -i '$ s|^}$|  import /etc/caddy/amux-errors.caddy\n}|' $CF
  if runuser -u caddy -- caddy validate --config $CF --adapter caddyfile >/dev/null 2>&1 && systemctl reload caddy; then
    say "verdict=caddy_error_page_installed"
  else
    mv $CF.prev $CF
    if [[ $had == 1 ]]; then mv /etc/caddy/amux-errors.caddy.prev /etc/caddy/amux-errors.caddy; else rm -f /etc/caddy/amux-errors.caddy; fi
    systemctl reload caddy; say "WARN verdict=caddy_error_page_rejected previous Caddyfile kept"
  fi
fi

# Public links (/pub/<id>/, `amux publish`): the snippet that lets them past the door, refreshed
# from the checkout every run; a Caddyfile from before it gains the import line once.
PUB="$SRC/cloud/desktop/caddy-public-links.caddy"
if [[ -f $CF && -f "$PUB" ]] && { ! cmp -s "$PUB" /etc/caddy/amux-public-links.caddy || ! grep -q 'import /etc/caddy/amux-public-links.caddy' $CF; }; then
  had=$([[ -f /etc/caddy/amux-public-links.caddy ]] && echo 1 || echo 0)
  cp -p $CF $CF.prev; [[ $had == 1 ]] && cp -p /etc/caddy/amux-public-links.caddy /etc/caddy/amux-public-links.caddy.prev
  install -m 644 "$PUB" /etc/caddy/amux-public-links.caddy
  grep -q 'import /etc/caddy/amux-public-links.caddy' $CF \
    || sed -i '0,/^  handle {$/s||  import /etc/caddy/amux-public-links.caddy\n  handle {|' $CF
  if runuser -u caddy -- caddy validate --config $CF --adapter caddyfile >/dev/null 2>&1 && systemctl reload caddy; then
    say "verdict=caddy_public_links_installed"
  else
    mv $CF.prev $CF
    if [[ $had == 1 ]]; then mv /etc/caddy/amux-public-links.caddy.prev /etc/caddy/amux-public-links.caddy; else rm -f /etc/caddy/amux-public-links.caddy; fi
    systemctl reload caddy; say "WARN verdict=caddy_public_links_rejected previous Caddyfile kept"
  fi
fi

# Apps inside amux on their own origin (https://<domain>:8443, caddy-apps-origin.caddy), for
# servers bootstrapped before it: the snippet refreshed from the checkout, a :8443 site block
# added once, port 8443 opened, and AMUX_APPS_URL set. Validated before reload; rolled back if
# Caddy rejects it.
APPS="$SRC/cloud/desktop/caddy-apps-origin.caddy"
if [[ -f $CF && -f "$APPS" ]]; then
  site=$(grep -m1 -E '^[A-Za-z0-9.-]+\.[A-Za-z]{2,} *\{' $CF | cut -d' ' -f1)
  if [[ -n "$site" ]] && { ! cmp -s "$APPS" /etc/caddy/amux-apps-origin.caddy || ! grep -q "^$site:8443 {" $CF; }; then
    had=$([[ -f /etc/caddy/amux-apps-origin.caddy ]] && echo 1 || echo 0)
    cp -p $CF $CF.prev; [[ $had == 1 ]] && cp -p /etc/caddy/amux-apps-origin.caddy /etc/caddy/amux-apps-origin.caddy.prev
    install -m 644 "$APPS" /etc/caddy/amux-apps-origin.caddy
    if ! grep -q "^$site:8443 {" $CF; then
      tls=$(grep -m1 -E '^ *tls /etc/amux/origin' $CF || true)
      printf '%s:8443 {\n%s\n  import /etc/caddy/amux-apps-origin.caddy\n}\n' "$site" "$tls" >> $CF
    fi
    if runuser -u caddy -- caddy validate --config $CF --adapter caddyfile >/dev/null 2>&1 && systemctl reload caddy; then
      say "verdict=caddy_apps_origin_installed site=$site:8443"
    else
      mv $CF.prev $CF
      if [[ $had == 1 ]]; then mv /etc/caddy/amux-apps-origin.caddy.prev /etc/caddy/amux-apps-origin.caddy; else rm -f /etc/caddy/amux-apps-origin.caddy; fi
      systemctl reload caddy; say "WARN verdict=caddy_apps_origin_rejected previous Caddyfile kept"
    fi
  fi
  command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q '^8443/tcp' || ufw allow 8443/tcp >/dev/null 2>&1 || true
  ENVA="$HOME_DIR/.amux/server.env"
  if [[ -n "$site" && -f "$ENVA" ]] && ! grep -q '^AMUX_APPS_URL=' "$ENVA"; then
    printf 'AMUX_APPS_URL=https://%s:8443\n' "$site" >> "$ENVA" && say "verdict=apps_url_set url=https://$site:8443"
  fi
fi

# The address workers build their links from, for servers bootstrapped before bootstrap.sh wrote
# it: the site name Caddy serves. Read per call by the server, so no restart is needed.
ENVF="$HOME_DIR/.amux/server.env"
if [[ -f $CF && -f $ENVF ]] && ! grep -q '^AMUX_PUBLIC_URL=' "$ENVF"; then
  site=$(grep -m1 -E '^[A-Za-z0-9.-]+\.[A-Za-z]{2,} *\{' $CF | cut -d' ' -f1)
  if [[ -n "$site" ]]; then
    printf 'AMUX_PUBLIC_URL=https://%s\n' "$site" >> "$ENVF"
    say "verdict=public_url_set url=https://$site"
  fi
fi

# The agent CLIs, once a day. bootstrap.sh installs them globally as root, so they cannot update
# themselves (Claude Code showed "Auto-update failed: no write permission to npm prefix" to the
# customer) and stayed at whatever the golden image had. Updating them here keeps every server on
# the current Claude Code, Codex and Gemini CLI. A running worker keeps its loaded version until it
# restarts. Claude's own updater is switched off in its settings, because it can never succeed here.
CLI_PKGS=(@anthropic-ai/claude-code @openai/codex @google/gemini-cli)
cli_versions() { npm ls -g --depth=0 "${CLI_PKGS[@]}" 2>/dev/null | grep -oE '@[a-z-]+/[a-z-]+@[0-9][^ ]*' | tr '\n' ' '; }
if command -v npm >/dev/null && [[ -z $(find "$STATE/clis-updated" -mmin -1380 2>/dev/null) ]]; then
  before=$(cli_versions)
  if timeout 600 npm install -g --no-fund --no-audit "${CLI_PKGS[@]/%/@latest}" >/dev/null 2>&1; then
    touch "$STATE/clis-updated"; after=$(cli_versions)
    if [[ "$before" == "$after" ]]; then say "verdict=agent_clis_current $after"; else say "verdict=agent_clis_updated from: $before to: $after"; fi
  else
    say "WARN verdict=agent_clis_update_failed still: $before"
  fi
fi
CS="$HOME_DIR/.claude/settings.json"
if [[ -s $CS ]] && ! jq -e '.env.DISABLE_AUTOUPDATER == "1"' "$CS" >/dev/null 2>&1; then
  jq '.env = ((.env // {}) + {"DISABLE_AUTOUPDATER": "1"})' "$CS" > "$CS.tmp" && chown "$USER_NAME:$USER_NAME" "$CS.tmp" && mv "$CS.tmp" "$CS" \
    && say "verdict=claude_self_update_off the agent CLIs are updated by this script"
fi

# 2. Newest green image.
TOKEN=$(curl -fsS --max-time 20 "https://ghcr.io/token?scope=repository:$IMAGE_REPO:pull" | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])' 2>/dev/null)
LATEST=$(curl -fsSI --max-time 20 -H "Authorization: Bearer $TOKEN" \
  -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' \
  "https://ghcr.io/v2/$IMAGE_REPO/manifests/latest" 2>/dev/null | awk 'tolower($1)=="docker-content-digest:"{print $2}' | tr -d '\r')
CURRENT=$(cat "$STATE/digest" 2>/dev/null || true)

if [[ -z "$LATEST" ]]; then
  VERDICT="failed_registry"; DETAIL="could not read the latest image digest from ghcr.io"
elif [[ "$LATEST" == "$CURRENT" ]]; then
  VERDICT="current"; DETAIL="already on the newest image"
else
  # 3. Install by rename, keep the previous binaries for rollback.
  CID=""
  cleanup() { [[ -n "$CID" ]] && docker rm -f "$CID" >/dev/null 2>&1; }
  trap cleanup EXIT
  if docker pull -q "ghcr.io/$IMAGE_REPO@$LATEST" >/dev/null && CID=$(docker create "ghcr.io/$IMAGE_REPO@$LATEST"); then
    ok=1
    for b in amux-server-rs amux-rs; do
      docker cp "$CID:/usr/local/bin/$b" "$BIN_DIR/$b.new" || ok=0
    done
    if [[ $ok == 1 ]]; then
      for b in amux-server-rs amux-rs; do
        [[ -x "$BIN_DIR/$b" ]] && cp -p "$BIN_DIR/$b" "$BIN_DIR/$b.prev"
        chown "$USER_NAME:$USER_NAME" "$BIN_DIR/$b.new"; mv "$BIN_DIR/$b.new" "$BIN_DIR/$b"
      done
      ucl restart amux-server.service
      if wait_healthy; then
        echo "$LATEST" > "$STATE/digest"
        VERDICT="updated"; DETAIL="installed ${LATEST:7:12}"
      else
        for b in amux-server-rs amux-rs; do [[ -x "$BIN_DIR/$b.prev" ]] && mv "$BIN_DIR/$b.prev" "$BIN_DIR/$b"; done
        ucl restart amux-server.service; wait_healthy
        VERDICT="failed_rolled_back"; DETAIL="${LATEST:7:12} did not pass /health in 120 s; previous binaries restored"
      fi
    else
      rm -f "$BIN_DIR"/amux-*.new
      VERDICT="failed_copy"; DETAIL="image ${LATEST:7:12} is missing a binary"
    fi
  else
    VERDICT="failed_pull"; DETAIL="docker could not pull ${LATEST:7:12}"
  fi
  docker image prune -af >/dev/null 2>&1 || true
fi

# 4. Recovery, whatever happened above.
if [[ -z "$(commit_of)" ]]; then
  ucl restart amux-server.service
  if wait_healthy; then DETAIL="$DETAIL; server was down and was restarted"
  else VERDICT="failed_server_down"; DETAIL="$DETAIL; amux-server does not answer /health after a restart"; fi
fi
systemctl is-active -q caddy || systemctl restart caddy
systemctl is-active -q amux-door 2>/dev/null || systemctl restart amux-door 2>/dev/null || true
write_status "$VERDICT" "$DETAIL"
