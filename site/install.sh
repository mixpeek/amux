#!/usr/bin/env bash
# Install amux to /usr/local/bin
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
INSTALL_DIR="/usr/local/bin"

echo "Installing amux to $INSTALL_DIR..."

# Check dependencies
command -v tmux &>/dev/null || echo "Warning: tmux not found (required for sessions)"
command -v python3 &>/dev/null || echo "Warning: python3 not found (required for amux serve)"

chmod +x "$SCRIPT_DIR/amux"
chmod +x "$SCRIPT_DIR/amux-server.py"

if [[ -w "$INSTALL_DIR" ]]; then
  ln -sf "$SCRIPT_DIR/amux" "$INSTALL_DIR/amux"
  ln -sf "$SCRIPT_DIR/amux-server.py" "$INSTALL_DIR/amux-server.py"
  # Compat alias: cc → amux
  ln -sf "$SCRIPT_DIR/amux" "$INSTALL_DIR/cc"
else
  sudo ln -sf "$SCRIPT_DIR/amux" "$INSTALL_DIR/amux"
  sudo ln -sf "$SCRIPT_DIR/amux-server.py" "$INSTALL_DIR/amux-server.py"
  sudo ln -sf "$SCRIPT_DIR/amux" "$INSTALL_DIR/cc"
fi

# Verify
if command -v amux &>/dev/null; then
  echo "Installed: $(amux --version)"
  echo ""
  echo "Quick start:"
  echo "  amux register myproject --dir ~/Dev/myproject --yolo"
  echo "  amux start myproject"
  echo "  amux                     # open terminal dashboard"
  echo "  amux serve               # open web dashboard on :8822"
else
  echo "Warning: amux not found in PATH after install"
  echo "You may need to add $INSTALL_DIR to your PATH"
fi
