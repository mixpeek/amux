#!/usr/bin/env bash
# amux installer
set -euo pipefail

BOLD=$'\033[1m' GREEN=$'\033[32m' YELLOW=$'\033[33m' RED=$'\033[31m' RESET=$'\033[0m'

INSTALL_DIR="${AMUX_INSTALL_DIR:-/usr/local/bin}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "${BOLD}amux installer${RESET}"
echo ""

# Check dependencies
check_dep() {
  command -v "$1" &>/dev/null || { echo "${RED}missing:${RESET} $1 — install with: $2"; exit 1; }
}
check_dep tmux   "brew install tmux"
check_dep python3 "brew install python3"

# Install amux CLI
if [[ ! -w "$INSTALL_DIR" ]]; then
  echo "${YELLOW}note:${RESET} writing to $INSTALL_DIR requires sudo"
  SUDO=sudo
else
  SUDO=""
fi

$SUDO cp "$SCRIPT_DIR/amux" "$INSTALL_DIR/amux"
$SUDO chmod +x "$INSTALL_DIR/amux"

# Copy server next to the CLI so amux serve can find it
$SUDO cp "$SCRIPT_DIR/amux-server.py" "$INSTALL_DIR/amux-server.py"

# Copy the remote wrapper (drive a remote amux over its REST API)
$SUDO cp "$SCRIPT_DIR/amux-remote" "$INSTALL_DIR/amux-remote"
$SUDO chmod +x "$INSTALL_DIR/amux-remote"

# Copy the icon assets. The dashboard links /icon.svg, /icon.png and the PWA
# manifest references icon-192/512; the server resolves those relative to
# amux-server.py's own directory. Without this the endpoints 404 and the
# dashboard has no favicon or installable app icon.
if [[ -d "$SCRIPT_DIR/assets" ]]; then
  $SUDO mkdir -p "$INSTALL_DIR/assets"
  $SUDO cp "$SCRIPT_DIR"/assets/icon*.png "$SCRIPT_DIR"/assets/icon.svg "$INSTALL_DIR/assets/"
fi

echo "${GREEN}✓${RESET} installed ${BOLD}amux${RESET} → $INSTALL_DIR/amux"
echo "${GREEN}✓${RESET} installed ${BOLD}amux-server.py${RESET} → $INSTALL_DIR/amux-server.py"
echo "${GREEN}✓${RESET} installed ${BOLD}amux-remote${RESET} → $INSTALL_DIR/amux-remote"
[[ -d "$INSTALL_DIR/assets" ]] && echo "${GREEN}✓${RESET} installed ${BOLD}icon assets${RESET} → $INSTALL_DIR/assets/"
echo ""
echo "Quick start:"
echo "  amux register myproject --dir ~/Dev/myproject --yolo"
echo "  amux start myproject"
echo "  amux serve   # → https://localhost:8822"
echo ""
echo "To control a ${BOLD}remote${RESET} amux server (e.g. your desktop from a laptop):"
echo "  # Create ~/.amux/remote.env on the machine you're connecting FROM:"
echo "  echo 'AMUX_URL=https://<tailscale-ip>:8822'   >> ~/.amux/remote.env"
echo "  echo 'AMUX_TOKEN=<token from remote ~/.amux/auth_token>' >> ~/.amux/remote.env"
echo "  amux-remote ls           # list sessions on the remote"
echo "  amux-remote attach <name>  # SSH in and attach to that tmux session"
