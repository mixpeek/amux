#!/usr/bin/env bash
# Dedicated amux host on a plain Linux VM (Hetzner CPX31 or larger): one
# customer (or one person) per box, reachable from another amux by URL plus
# username/password.
#
# What you get:
#   * amux-server under systemd as the `amux` user, from the CI-built binary.
#   * Caddy on 443 with a real Let's Encrypt cert for $AMUX_DOMAIN, gating
#     everything with HTTP basic auth. Each basic-auth user is an identity:
#     Caddy overwrites X-Amux-User-Email with the authenticated name and logs
#     every request (who, method, path, status) to /var/log/caddy/amux-access.log.
#     amux itself admits loopback peers, so Caddy is the only door.
#   * Claude Code configured with ANTHROPIC_API_KEY (pre-approved, no prompts).
#   * An XFCE desktop over RDP, with Chrome holding a loopback CDP port so
#     agents drive the browser a human can watch. RDP is not public: reach it
#     with `ssh -L 3389:localhost:3389 root@<ip>`, or over Tailscale if
#     TS_AUTHKEY was given.
#
# Run as root on a fresh Ubuntu 24.04:
#   AMUX_DOMAIN=wexus.amux.io \
#   AMUX_WEB_USERS='ethan:<pw>:ethan@x.com elliot:<pw>:elliot@y.com:group=wexus' \
#   ANTHROPIC_API_KEY=sk-ant-... \
#   bash bootstrap.sh
# Optional: AMUX_UPDATES=source (self-updating source install, see below),
# AMUX_UPDATE_FOLLOW_MIN (with source: fetch main every n minutes),
# TS_AUTHKEY, AMUX_PASSWORD (the RDP/unix password), SWAP_GB,
# AMUX_IMAGE, AMUX_TENANT=0 (keeps the server's ops-health cards on the board).
# A DNS A record for AMUX_DOMAIN must point at this box before Caddy can get a
# certificate. A user listed without a password gets a random one, printed once.
#
# Idempotent: safe to re-run after a partial failure, and re-running with a
# new AMUX_WEB_USERS or ANTHROPIC_API_KEY rotates them.
#
# amux Cloud fleet mode (docs/design/cloud-self-service.md): when cloud-init
# has written /etc/amux/fleet.env (AMUX_SLUG, AMUX_OWNER_EMAIL, AMUX_FLEET_SECRET,
# AMUX_CONTROL_URL), the domain is <slug>.amux.cloud, the owner is a member by
# email with no password (they arrive through cloud.amux.io's door token), and
# Caddy serves the Cloudflare Origin CA certificate the fleet agent fetched into
# /etc/amux/origin.{crt,key} instead of asking Let's Encrypt.
# AMUX_SKIP_PACKAGES=1 skips package installation on a prepared golden image.
# AMUX_IMAGE_BUILD=1 prepares that golden image: packages, the amux binaries,
# litestream, restic, the door and the fleet agent, then stops before anything
# that belongs to one customer (cloud/control/build_image.py snapshots it).
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
USER_NAME="${AMUX_USER:-amux}"
SWAP_GB="${SWAP_GB:-8}"
IMAGE="${AMUX_IMAGE:-ghcr.io/mixpeek/amux:latest}"
REPO="${AMUX_REPO:-https://github.com/mixpeek/amux.git}"
DOMAIN="${AMUX_DOMAIN:-}"
# How this host gets new amux: "image" copies the CI-built binary from the
# published image on every run; "source" keeps a checkout plus amux's own
# auto-builder, so "Pull from remote" in the dashboard (or the builder's
# origin/main authority) updates it like any source install.
UPDATES="${AMUX_UPDATES:-image}"
# With source updates, AMUX_UPDATE_FOLLOW_MIN=<n> also fetches origin main every
# n minutes so the host follows main by itself. The builder never fetches on its
# own (freshness is a deliberate act), so this is that act, set once by whoever
# provisions the host. 0 or unset: updates happen when someone presses Pull.
FOLLOW_MIN="${AMUX_UPDATE_FOLLOW_MIN:-0}"
WEB_USERS="${AMUX_WEB_USERS:-}"
FLEET_ENV=/etc/amux/fleet.env
FLEET=0
if [[ -f "$FLEET_ENV" ]]; then
  set -a; . "$FLEET_ENV"; set +a
  FLEET=1
  DOMAIN="${AMUX_SLUG:?fleet.env has no AMUX_SLUG}.${AMUX_CUSTOMER_DOMAIN:-amux.cloud}"
fi
LOG=/var/log/amux-desktop-setup.log

log() { echo "[amux-desktop] $(date '+%H:%M:%S') $*" | tee -a "$LOG"; }
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
IMAGE_BUILD="${AMUX_IMAGE_BUILD:-0}"
[[ -n "$DOMAIN" || "$IMAGE_BUILD" == 1 ]] || { echo "AMUX_DOMAIN is required (the name Caddy gets a cert for)" >&2; exit 1; }
[[ -n "$WEB_USERS" || "$FLEET" == 1 || "$IMAGE_BUILD" == 1 ]] || { echo "AMUX_WEB_USERS is required, e.g. 'ethan:pw:ethan@x.com elliot:pw:elliot@y.com'" >&2; exit 1; }
[[ "$FLEET" == 0 || -n "${AMUX_OWNER_EMAIL:-}" ]] || { echo "fleet.env has no AMUX_OWNER_EMAIL" >&2; exit 1; }

# ── Swap ──
# The Linux amux box OOM-killed chromium and rustc with zero swap configured
# (scripts/amux-xvfb.service.template). An 8 GB VM running XFCE, Chrome and a
# few Claude processes needs the headroom.
if ! swapon --show | grep -q /swapfile; then
  log "creating ${SWAP_GB}G swapfile"
  fallocate -l "${SWAP_GB}G" /swapfile
  chmod 600 /swapfile
  mkswap /swapfile >/dev/null
  swapon /swapfile
  grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi

# ── Packages ──
if [[ "${AMUX_SKIP_PACKAGES:-0}" == 1 ]]; then
  log "packages: skipped (AMUX_SKIP_PACKAGES=1, prepared image)"
else
log "installing desktop, xrdp, caddy and amux runtime deps"
apt-get update -qq
apt-get install -y -qq \
  xfce4 xfce4-goodies dbus-x11 xrdp xorgxrdp \
  tmux git curl wget jq unzip python3 python3-pip python3-yaml ripgrep ffmpeg procps \
  openssh-client gettext-base build-essential ca-certificates gnupg ufw docker.io \
  debian-keyring debian-archive-keyring apt-transport-https \
  fonts-noto fonts-noto-color-emoji

if ! command -v caddy &>/dev/null; then
  curl -fsSL https://dl.cloudsmith.io/public/caddy/stable/gpg.key | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -fsSL https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -qq && apt-get install -y -qq caddy
fi

# Google Chrome from Google's repo. Ubuntu's chromium is a snap, and snaps
# misbehave inside an xrdp session.
if ! command -v google-chrome &>/dev/null; then
  log "installing Google Chrome"
  curl -fsSL https://dl.google.com/linux/linux_signing_key.pub | gpg --dearmor -o /usr/share/keyrings/google-chrome.gpg
  echo "deb [arch=amd64 signed-by=/usr/share/keyrings/google-chrome.gpg] https://dl.google.com/linux/chrome/deb/ stable main" \
    > /etc/apt/sources.list.d/google-chrome.list
  apt-get update -qq && apt-get install -y -qq google-chrome-stable
fi

# Node 22 + Claude Code CLI
if ! command -v node &>/dev/null; then
  log "installing Node 22"
  curl -fsSL https://deb.nodesource.com/setup_22.x | bash - >/dev/null
  apt-get install -y -qq nodejs
fi
command -v claude &>/dev/null || npm install -g @anthropic-ai/claude-code >/dev/null
# The other coding CLIs a customer can sign in to (plan 8.4).
command -v codex &>/dev/null || npm install -g @openai/codex >/dev/null
command -v gemini &>/dev/null || npm install -g @google/gemini-cli >/dev/null
fi

# ── User ──
if ! id "$USER_NAME" &>/dev/null; then
  log "creating user $USER_NAME"
  useradd -m -s /bin/bash -G sudo "$USER_NAME"
  PW="${AMUX_PASSWORD:-$(openssl rand -base64 18)}"
  echo "$USER_NAME:$PW" | chpasswd
  [[ -n "${AMUX_PASSWORD:-}" ]] || log "RDP password for $USER_NAME (shown once): $PW"
fi
HOME_DIR=$(getent passwd "$USER_NAME" | cut -d: -f6)
as_user() { sudo -u "$USER_NAME" -H "$@"; }
# Every directory under the user's home is the USER's, including parents that
# `install -d` would otherwise create as root. A root-owned ~/.local made
# Chrome die with "mkdir ~/.local/share: Permission denied" (first run).
for d in .local .local/bin .local/share .config .cache; do
  install -d -o "$USER_NAME" -g "$USER_NAME" "$HOME_DIR/$d"
done

# ── xrdp → XFCE ──
echo "startxfce4" > "$HOME_DIR/.xsession"
chown "$USER_NAME:$USER_NAME" "$HOME_DIR/.xsession"
# xrdp reads the TLS key through the ssl-cert group.
adduser xrdp ssl-cert >/dev/null 2>&1 || true
systemctl enable --now xrdp >/dev/null

# Chrome opens on login with a CDP port on loopback, so an agent on this box
# drives the browser you see (`amux cdp`, playwright connectOverCDP). One
# profile dir, so sign-ins persist across reconnects.
mkdir -p "$HOME_DIR/.config/autostart"
cat > "$HOME_DIR/.config/autostart/chrome-cdp.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Chrome (CDP 9222)
Exec=google-chrome --remote-debugging-address=127.0.0.1 --remote-debugging-port=9222 --user-data-dir=$HOME_DIR/.config/chrome-amux --no-first-run https://localhost:8824
X-GNOME-Autostart-enabled=true
EOF
chown -R "$USER_NAME:$USER_NAME" "$HOME_DIR/.config"

# ── Tailscale (optional) ──
if [[ -n "${TS_AUTHKEY:-}" ]]; then
  command -v tailscale &>/dev/null || curl -fsSL https://tailscale.com/install.sh | sh >/dev/null
  tailscale status &>/dev/null || tailscale up --authkey "$TS_AUTHKEY" --hostname "amux-${DOMAIN%%.*}" --ssh
fi

# ── Firewall: public SSH + HTTPS only. amux's 8824 and RDP's 3389 stay closed;
# Caddy reaches amux on loopback, and RDP goes through an SSH tunnel. ──
ufw default deny incoming >/dev/null
ufw default allow outgoing >/dev/null
ufw allow 22/tcp >/dev/null
ufw allow 80/tcp >/dev/null    # ACME http-01 challenge + redirect to 443
ufw allow 443/tcp >/dev/null
ip link show tailscale0 &>/dev/null && ufw allow in on tailscale0 >/dev/null
ufw --force enable >/dev/null

# ── amux: prebuilt binaries from the CI image, never compiled here ──
# A release build of amux-server takes ~27 minutes and several GB of RAM on a
# 4-thread box (CLAUDE.md, offloading section). The ghcr image already carries
# the CI-built binary, linked against bookworm's glibc 2.36, which runs here.
BIN_DIR="$HOME_DIR/.local/bin"
install -d -o "$USER_NAME" -g "$USER_NAME" "$BIN_DIR"
if [[ "$FLEET" == 1 && "${AMUX_SKIP_PACKAGES:-0}" == 1 && -x "$BIN_DIR/amux-server-rs" ]]; then
  log "binaries: baked into the image; releases arrive through the fleet agent"
  IMAGE_ID=baked
else
log "pulling $IMAGE for its binaries"
docker pull -q "$IMAGE" >/dev/null
CID=$(docker create "$IMAGE")
trap 'docker rm -f "$CID" >/dev/null 2>&1 || true' EXIT
for b in amux-server-rs amux-rs; do
  # Source mode: the auto-builder owns the installed binary once it exists;
  # the image only seeds a first boot. Overwriting it here would put an older
  # server back on every re-run of this script.
  if [[ "$UPDATES" == source && -x "$BIN_DIR/$b" ]]; then continue; fi
  docker cp "$CID:/usr/local/bin/$b" "$BIN_DIR/$b.new"
  mv "$BIN_DIR/$b.new" "$BIN_DIR/$b"   # rename, never overwrite a running binary
done
# A box running a build that is not on the published image yet (a branch
# under test) names it here, so a re-run does not silently put the image's
# older server back.
if [[ -n "${AMUX_SERVER_BINARY:-}" && "$UPDATES" != source ]]; then
  [[ -x "$AMUX_SERVER_BINARY" ]] || { log "FATAL: AMUX_SERVER_BINARY=$AMUX_SERVER_BINARY is not an executable"; exit 1; }
  cp "$AMUX_SERVER_BINARY" "$BIN_DIR/amux-server-rs.new" && mv "$BIN_DIR/amux-server-rs.new" "$BIN_DIR/amux-server-rs"
  log "server binary from AMUX_SERVER_BINARY=$AMUX_SERVER_BINARY ($(sha256sum "$BIN_DIR/amux-server-rs" | cut -c1-12)), not the image"
fi
chown "$USER_NAME:$USER_NAME" "$BIN_DIR"/amux-*
IMAGE_ID=$(docker image inspect -f '{{.Id}}' "$IMAGE" | cut -c8-19)
fi

# Checkout for the bash CLI, worker templates and the systemd unit template.
SRC="$HOME_DIR/Dev/amux"
[[ -d "$SRC/.git" ]] || as_user git clone -q "$REPO" "$SRC"
as_user make -C "$SRC" install-cli BIN_DIR="$BIN_DIR" >/dev/null
grep -q '.local/bin' "$HOME_DIR/.profile" 2>/dev/null || echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME_DIR/.profile"

# ── Golden image: backup tools, the door and the fleet agent, then stop ──
if [[ "$IMAGE_BUILD" == 1 || "$FLEET" == 1 ]]; then
  command -v restic &>/dev/null || apt-get install -y -qq restic >/dev/null
  if ! command -v litestream &>/dev/null; then
    LV=v0.3.13
    curl -fsSL -o /tmp/litestream.deb "https://github.com/benbjohnson/litestream/releases/download/$LV/litestream-$LV-linux-amd64.deb"
    dpkg -i /tmp/litestream.deb >/dev/null && rm -f /tmp/litestream.deb
  fi
  install -d -m 755 /opt/amux /etc/amux
  SELF_DIR="$(dirname "$(readlink -f "$0")")"
  for f in bootstrap.sh door.py fleet_agent.py; do
    [[ -f "$SELF_DIR/$f" ]] && install -m 755 "$SELF_DIR/$f" "/opt/amux/$f"
  done
  cat > /etc/systemd/system/litestream.service <<'UNIT'
[Unit]
Description=litestream: amux.db to this server's own R2 bucket every 60 s
After=network-online.target
ConditionPathExists=/etc/amux/litestream.yml
[Service]
ExecStart=/usr/bin/litestream replicate -config /etc/amux/litestream.yml
Restart=always
RestartSec=5
[Install]
WantedBy=multi-user.target
UNIT
  cat > /etc/systemd/system/amux-fleet-agent.service <<'UNIT'
[Unit]
Description=amux Cloud fleet agent (heartbeat, certificate, backups, releases)
After=network-online.target
Wants=network-online.target
ConditionPathExists=/etc/amux/fleet.env
[Service]
ExecStart=/usr/bin/python3 /opt/amux/fleet_agent.py run
Restart=always
RestartSec=10
[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable amux-fleet-agent.service litestream.service >/dev/null 2>&1 || true
fi
if [[ "$IMAGE_BUILD" == 1 ]]; then
  # Nothing customer-specific may be in the snapshot: cloud-init runs again on
  # first boot and writes fleet.env; host keys and machine-id are regenerated.
  cloud-init clean --logs >/dev/null 2>&1 || true
  rm -f /etc/ssh/ssh_host_* /etc/amux/fleet.env /etc/amux/origin.* /var/lib/amux-fleet/* 2>/dev/null || true
  truncate -s 0 /etc/machine-id
  log "golden image prepared: $(as_user "$BIN_DIR/amux-server-rs" --version 2>/dev/null || echo 'amux-server-rs present')"
  exit 0
fi

# ── server.env: credentials and tenant settings, read by the server at start ──
AMUX_HOME_DIR="$HOME_DIR/.amux"
install -d -m 700 -o "$USER_NAME" -g "$USER_NAME" "$AMUX_HOME_DIR"
ENV_FILE="$AMUX_HOME_DIR/server.env"
touch "$ENV_FILE"
set_env() {  # replace-or-append one KEY=value line
  local k="$1" v="$2"
  grep -v "^$k=" "$ENV_FILE" > "$ENV_FILE.tmp" || true
  printf '%s=%s\n' "$k" "$v" >> "$ENV_FILE.tmp"
  mv "$ENV_FILE.tmp" "$ENV_FILE"
}
[[ -n "${ANTHROPIC_API_KEY:-}" ]] && set_env ANTHROPIC_API_KEY "$ANTHROPIC_API_KEY"
# A customer box: the server's own invariant/disk cards are operator noise on
# the customer's board (same reasoning as the cloud image, AMUX-3014).
[[ "${AMUX_TENANT:-1}" == "1" ]] && set_env AMUX_OPS_HEALTH_CARDS 0
# Caddy is the only door, and it reaches amux from loopback. With amux's own
# bearer token on, the server treats every proxied browser as local, injects
# the token into the page, and the SPA then sends "Authorization: Bearer ..."
# on every API call, which replaces the browser's Basic credentials, so Caddy
# 401s the whole dashboard (first real run). Same arrangement as the cloud
# containers behind their gateway: amux auth off, 8824 closed by ufw.
set_env AMUX_AUTH_TOKEN none
# Source mode: the Pull button and the served-commit check find the checkout.
[[ "$UPDATES" == source ]] && set_env AMUX_REPO_DIR "$SRC"
chown "$USER_NAME:$USER_NAME" "$ENV_FILE"; chmod 600 "$ENV_FILE"

# ── Claude Code first-run gates ──
# 1. The --dangerously-skip-permissions acceptance screen (no persisted answer
#    without this flag; a fresh pane would sit there forever, AC-346).
# 2. The "use this API key?" prompt: approval is stored as the key's last 20
#    characters in ~/.claude.json. Merged, never clobbered.
install -d -o "$USER_NAME" -g "$USER_NAME" "$HOME_DIR/.claude"
CS="$HOME_DIR/.claude/settings.json"; [[ -s "$CS" ]] || echo '{}' > "$CS"
jq '. * {"skipDangerousModePermissionPrompt": true}' "$CS" > "$CS.tmp" && mv "$CS.tmp" "$CS"
CJ="$HOME_DIR/.claude.json"; [[ -s "$CJ" ]] || echo '{}' > "$CJ"
if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  jq --arg k "${ANTHROPIC_API_KEY: -20}" \
    '.hasCompletedOnboarding = true
     | .customApiKeyResponses.approved = ((.customApiKeyResponses.approved // []) + [$k] | unique)
     | .customApiKeyResponses.rejected = (.customApiKeyResponses.rejected // [])' \
    "$CJ" > "$CJ.tmp" && mv "$CJ.tmp" "$CJ"
fi
chown "$USER_NAME:$USER_NAME" "$CS" "$CJ"

# ── Server as a systemd user unit, from the same template install.sh uses ──
UNIT_DIR="$HOME_DIR/.config/systemd/user"
install -d -o "$USER_NAME" -g "$USER_NAME" "$UNIT_DIR"
# The template reads five variables; leaving any out ships a literal
# "$AMUX_HOME" that systemd rejects as a non-absolute path.
BIN_DIR="$BIN_DIR" SCRIPT_DIR="$SRC" AMUX_HOME="$AMUX_HOME_DIR" PORT=8824 HOME="$HOME_DIR" \
  envsubst '$BIN_DIR $SCRIPT_DIR $AMUX_HOME $PORT $HOME' \
  < "$SRC/scripts/amux-server.service.template" > "$UNIT_DIR/amux-server.service"
if grep -n '\$[A-Z_]' "$UNIT_DIR/amux-server.service" | grep -v '^[0-9]*:#'; then
  log "FATAL: unsubstituted variable left in amux-server.service (above)"; exit 1
fi
chown "$USER_NAME:$USER_NAME" "$UNIT_DIR/amux-server.service"
# Without linger a user unit stops at logout and never starts at boot.
loginctl enable-linger "$USER_NAME"
UID_N=$(id -u "$USER_NAME")
# Linger only schedules the user manager. On a fresh box it is not up yet, and
# `systemctl --user` then fails with "Failed to connect to bus", which under
# set -e killed the first real run of this script. Start it and wait for the
# bus socket rather than racing it.
systemctl start "user@$UID_N.service"
for _ in $(seq 1 30); do [[ -S "/run/user/$UID_N/bus" ]] && break; sleep 1; done
[[ -S "/run/user/$UID_N/bus" ]] || { log "FATAL: user manager bus never appeared at /run/user/$UID_N/bus"; exit 1; }
ucl() { as_user XDG_RUNTIME_DIR="/run/user/$UID_N" DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$UID_N/bus" systemctl --user "$@"; }
ucl daemon-reload
ucl enable amux-server.service >/dev/null
ucl restart amux-server.service   # restart, not start: picks up a rotated server.env

# ── Source mode: Rust toolchain + amux's own auto-builder ──
# The builder (scripts/rust-auto-build-authority.sh) builds origin/main in a
# clean detached worktree, installs ~/.local/bin/amux-server-rs, and the server
# adopts its own new binary. "Pull from remote" fetches origin, so a press is a
# deploy. First build on a CPX/CX 4-vCPU box: ~10-15 minutes, then incremental.
if [[ "$UPDATES" == source ]]; then
  apt-get install -y -qq cmake perl pkg-config >/dev/null
  if [[ ! -x "$HOME_DIR/.cargo/bin/cargo" ]]; then
    log "installing the Rust toolchain for $USER_NAME"
    as_user bash -c 'curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1'
  fi
  as_user git -C "$SRC" config pull.ff only
  # The builder unit runs scripts from this checkout, so bring it to origin/main
  # first (fast-forward only; a dirty or diverged checkout is left as is).
  as_user git -C "$SRC" pull -q --ff-only 2>/dev/null || log "WARN: checkout $SRC did not fast-forward; builder runs its current scripts"
  for t in amux-builder.service amux-builder.timer; do
    SCRIPT_DIR="$SRC" HOME="$HOME_DIR" envsubst '$SCRIPT_DIR $HOME' \
      < "$SRC/scripts/$t.template" > "$UNIT_DIR/$t"
    chown "$USER_NAME:$USER_NAME" "$UNIT_DIR/$t"
  done
  if [[ "$FOLLOW_MIN" =~ ^[1-9][0-9]*$ ]]; then
    cat > "$UNIT_DIR/amux-follow-main.service" <<EOF
[Unit]
Description=amux: fetch origin main so the builder can adopt it
[Service]
Type=oneshot
ExecStart=/usr/bin/git -C $SRC fetch -q origin main
EOF
    cat > "$UNIT_DIR/amux-follow-main.timer" <<EOF
[Unit]
Description=amux: follow origin main every ${FOLLOW_MIN} min
[Timer]
OnBootSec=2min
OnUnitActiveSec=${FOLLOW_MIN}min
[Install]
WantedBy=timers.target
EOF
    chown "$USER_NAME:$USER_NAME" "$UNIT_DIR"/amux-follow-main.*
  else
    rm -f "$UNIT_DIR"/amux-follow-main.*
  fi
  ucl daemon-reload
  ucl enable --now amux-builder.timer >/dev/null
  if [[ -f "$UNIT_DIR/amux-follow-main.timer" ]]; then
    ucl enable --now amux-follow-main.timer >/dev/null
  else
    ucl disable --now amux-follow-main.timer >/dev/null 2>&1 || true
  fi
  log "follow main: $([[ -f "$UNIT_DIR/amux-follow-main.timer" ]] && echo "every ${FOLLOW_MIN}m ($(ucl is-active amux-follow-main.timer))" || echo "off (updates on Pull)")"
  log "source updates on: builder timer $(ucl is-active amux-builder.timer), checkout $SRC at $(as_user git -C "$SRC" rev-parse --short HEAD)"
fi

# ── Identity: one amux member per web user ──
# Each AMUX_WEB_USERS entry is user:password:email. The email becomes an amux
# invited member (org_invites/org_members, global scope), the server's own
# human identity: it is verified from the amux_member cookie on every request,
# feeds the request log and the board, and is revoked by deleting the member.
# Caddy holds each member's cookie and attaches it after basic auth, so the
# password a person types IS their amux identity, and a browser never sees a
# token it could hand to someone else.
for _ in $(seq 1 30); do curl -sk -m 2 -o /dev/null https://127.0.0.1:8824/health && break; sleep 1; done
# Member tokens live with amux-door, which returns the verified user's token to
# Caddy as X-Amux-Member and re-validates it every minute (cloud/desktop/door.py).
id amux-door &>/dev/null || useradd --system --no-create-home --shell /usr/sbin/nologin amux-door
install -d -m 770 -o root -g amux-door /etc/amux-door
MEMBERS=/etc/amux-door/members           # "user-or-email token" per line
if [[ ! -s "$MEMBERS" && -s /etc/caddy/amux-members ]]; then cp /etc/caddy/amux-members "$MEMBERS"; fi
touch "$MEMBERS"; chown amux-door:amux-door "$MEMBERS"; chmod 600 "$MEMBERS"
member_ok() {  # token still a verified member WITH the wanted scope ("global" or "group:<name>")?
  curl -sk -m 5 -H "Cookie: amux_member=$1" https://127.0.0.1:8824/api/identity \
    | python3 -c '
import json,sys
d=json.load(sys.stdin); s=d.get("access_scope") or {}
have = "global" if s.get("level")=="global" else "%s:%s" % (s.get("level"), s.get("name"))
sys.exit(0 if d.get("is_local_member") and have==sys.argv[1] else 1)' "${2:-global}" 2>/dev/null
}
member_token() {  # user email scope -> token (reuse when the scope matches, else invite + accept)
  local u="$1" email="$2" scope="${3:-global}" tok body
  tok=$(awk -v u="$u" '$1==u{print $2}' "$MEMBERS")
  if [[ -n "$tok" ]] && member_ok "$tok" "$scope"; then echo "$tok"; return; fi
  # A changed scope goes through a fresh invite: accepting it updates the
  # existing member row (org.rs accept_invite), so the grant really changes.
  if [[ "$scope" == global ]]; then
    body="{\"email\":\"$email\",\"scope_level\":\"global\",\"grant_host_access\":true}"
  else
    body="{\"email\":\"$email\",\"scope_level\":\"${scope%%:*}\",\"scope_name\":\"${scope#*:}\"}"
  fi
  tok=$(curl -sk -m 10 -X POST -H 'Content-Type: application/json' -d "$body" \
    https://127.0.0.1:8824/api/org/invites \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("token") or d.get("url","").rsplit("/",1)[-1])')
  [[ -n "$tok" ]] || { log "FATAL: invite for $u ($email) was refused"; exit 1; }
  curl -sk -m 10 -o /dev/null -X POST --data-urlencode "email=$email" --data-urlencode "name=$u" \
    "https://127.0.0.1:8824/invite/$tok"
  member_ok "$tok" "$scope" || { log "FATAL: accepted invite for $u did not yield a member with scope $scope"; exit 1; }
  grep -v "^$u " "$MEMBERS" > "$MEMBERS.tmp" || true
  echo "$u $tok" >> "$MEMBERS.tmp"; mv "$MEMBERS.tmp" "$MEMBERS"; chown amux-door:amux-door "$MEMBERS"; chmod 600 "$MEMBERS"
  echo "$tok"
}

# ── amux-door: the sign-in (form + cookie for browsers, Basic for the CLI) ──
# Caddy asks it about every request (forward_auth). Browsers get a sign-in
# page and a 30-day cookie; another amux's Settings > Connect > Connections
# signs in by posting to it. Basic auth keeps working for scripts. See
# cloud/desktop/door.py for why a URL with embedded credentials cannot work.
install -d -m 750 -o root -g amux-door /opt/amux-door
if [[ "$FLEET" == 1 ]]; then chgrp amux-door "$FLEET_ENV"; chmod 640 "$FLEET_ENV"; fi   # the door verifies tokens with it
DOOR_PY="$(dirname "$(readlink -f "$0")")/door.py"            # shipped beside this script
[[ -f "$DOOR_PY" ]] || DOOR_PY="$SRC/cloud/desktop/door.py"       # else the checkout's copy
install -m 755 "$DOOR_PY" /opt/amux-door/door.py
[[ -s /etc/amux-door/secret ]] || openssl rand -hex 32 > /etc/amux-door/secret
chown root:amux-door /etc/amux-door/secret; chmod 640 /etc/amux-door/secret
cat > /etc/systemd/system/amux-door.service <<'UNIT'
[Unit]
Description=amux-door sign-in for the dedicated amux host
After=network.target
[Service]
User=amux-door
ExecStart=/usr/bin/python3 /opt/amux-door/door.py serve
Restart=always
RestartSec=2
[Install]
WantedBy=multi-user.target
UNIT

# ── Caddy: TLS + door sign-in + member identity + access log ──
CREDS_OUT=""
: > /etc/amux-door/users.new
for entry in $WEB_USERS; do
  IFS=: read -r u p email grant <<<"$entry"
  # Optional 4th field: group=<name> (or worker=<name>) limits the account to
  # those workers, their cards and files. Absent means global access.
  case "${grant:-}" in
    "") scope=global ;;
    group=*|worker=*) scope="${grant%%=*}:${grant#*=}" ;;
    *) echo "user $u: 4th field must be group=<name> or worker=<name>" >&2; exit 1 ;;
  esac
  [[ "$u" =~ ^[a-zA-Z0-9._-]+$ ]] || { echo "bad username: $u" >&2; exit 1; }
  [[ "$email" == *@*.* ]] || { echo "user $u needs an email: user:password:email" >&2; exit 1; }
  [[ -n "$p" ]] || { p=$(openssl rand -base64 18); CREDS_OUT+=" $u:$p"; }
  printf '%s\n' "$p" | python3 /opt/amux-door/door.py hash "$u" >> /etc/amux-door/users.new
  member_token "$u" "$email" "$scope" >/dev/null
done
if [[ "$FLEET" == 1 ]]; then
  member_token "$AMUX_OWNER_EMAIL" "$AMUX_OWNER_EMAIL" global >/dev/null   # the paying customer, by email
fi
chown root:amux-door /etc/amux-door/users.new; chmod 640 /etc/amux-door/users.new
mv /etc/amux-door/users.new /etc/amux-door/users
systemctl daemon-reload
systemctl enable amux-door >/dev/null
systemctl restart amux-door
install -d -o caddy -g caddy /var/log/caddy
TLS_LINE=""
if [[ "$FLEET" == 1 ]]; then
  [[ -s /etc/amux/origin.crt && -s /etc/amux/origin.key ]] || { log "FATAL: fleet mode needs /etc/amux/origin.{crt,key} from the fleet agent"; exit 1; }
  chgrp caddy /etc/amux/origin.key; chmod 640 /etc/amux/origin.key
  TLS_LINE="  tls /etc/amux/origin.crt /etc/amux/origin.key"
fi
cat > /etc/caddy/Caddyfile <<EOF
# Generated by amux cloud/desktop/bootstrap.sh. Re-run the script to change users.
# Member tokens live in /etc/amux-door/members, not here.
$DOMAIN {
$TLS_LINE
  log {
    output file /var/log/caddy/amux-access.log {
      roll_size 50MiB
      roll_keep 20
    }
    format json
  }
  # The sign-in page itself is public.
  handle /_door/* {
    reverse_proxy 127.0.0.1:8890
  }
  handle {
   # route keeps this block in the order written. Without it Caddy runs
   # request_header after forward_auth and strips the door's own X-Amux-Member,
   # so no member token reaches amux (found by a real-Caddy test, 2026-10-14).
   route {
    # Cookie, Basic or a cloud.amux.io token, checked by amux-door. On success
    # X-Door-User is the verified person and X-Amux-Member their amux member
    # token. Anything the client sent under either name is removed first.
    request_header -X-Amux-Member
    forward_auth 127.0.0.1:8890 {
      uri /_door/check
      copy_headers X-Door-User X-Amux-Member
    }
    log_append door_user {http.request.header.X-Door-User}
    # A client that names its own lane (the amux CLI from another machine
    # sends X-Amux-Session) keeps it, prefixed with the verified user. A
    # browser sends none and gets none: a bare user name there reads as a
    # WORKER and the board refuses its writes (cross_board_create_forbidden).
    @lane header X-Amux-Session *
    request_header @lane X-Amux-Session "{http.request.header.X-Door-User}/{http.request.header.X-Amux-Session}"
    request_header -X-Amux-Worker
    request_header -X-Amux-Worker-Token
    reverse_proxy https://127.0.0.1:8824 {
      transport http {
        tls_insecure_skip_verify
      }
      # The ONLY cookie amux sees is the member cookie for the verified user,
      # so a client cannot present someone else's (and amux_door stays here).
      header_up Cookie "amux_member={http.request.header.X-Amux-Member}"
      header_up -X-Amux-Member
      header_up -Authorization
      header_up -X-Door-User
      flush_interval -1
    }
   }
  }
}
EOF
# Validate AS the caddy user: validate opens the log writer, and run as root it
# leaves a root-owned access log the service then cannot open (first real run).
chown root:caddy /etc/caddy/Caddyfile; chmod 640 /etc/caddy/Caddyfile   # holds member tokens
chown -R caddy:caddy /var/log/caddy
sudo -u caddy caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null
systemctl enable caddy >/dev/null
systemctl reload caddy 2>/dev/null || systemctl restart caddy

# ── Verify: every line below is computed, none asserted ──
sleep 4
LOCAL=$(curl -sk --max-time 5 https://127.0.0.1:8824/health | jq -r '.commit // empty' 2>/dev/null || true)
ANON=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 --resolve "$DOMAIN:443:127.0.0.1" "https://$DOMAIN/api/sessions" || true)
KEY_SET=$(grep -c '^ANTHROPIC_API_KEY=.' "$ENV_FILE" || true)
log "image: $IMAGE ($IMAGE_ID)"
log "amux /health on loopback, commit: ${LOCAL:-UNREACHABLE}"
log "https://$DOMAIN/api/sessions without credentials: HTTP $ANON (401 expected; 000 = no cert yet, check DNS)"
log "ANTHROPIC_API_KEY lines in server.env: $KEY_SET"
log "xrdp listeners on 3389: $(ss -tln | grep -c ':3389 ' || true)"
[[ -n "$CREDS_OUT" ]] && log "generated web credentials (shown once):$CREDS_OUT"
log "connect from another amux: AMUX_API=https://<user>:<pw>@$DOMAIN amux ls"
