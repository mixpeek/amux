#!/bin/bash
# One-time setup on the Mac Muse is paired with, when amux runs on another Mac.
# Installs muse-reply and muse-sync, writes ~/.muse-cli/amux.json (the amux
# server and its owner token, mode 600) and starts muse-sync every minute.
#   bash skills/muse/install-paired-mac.sh https://<amux server>:8824
set -euo pipefail
url="${1:?usage: install-paired-mac.sh https://<amux server>:8824}"
here="$(cd "$(dirname "$0")" && pwd)"
mkdir -p ~/.muse-cli ~/.local/bin
install -m 755 "$here/scripts/muse-reply.py" ~/.local/bin/muse-reply
install -m 755 "$here/scripts/muse-sync.py" ~/.local/bin/muse-sync
read -rsp "amux owner token (input hidden): " token; echo
umask 077
python3 - "$url" "$token" <<'PY'
import json, os, sys
json.dump({"url": sys.argv[1], "token": sys.argv[2]}, open(os.path.expanduser("~/.muse-cli/amux.json"), "w"))
PY
~/.local/bin/muse-sync --install
