#!/usr/bin/env bash
# Install the amux gh shim (AMUX-3789).
#
# Creates ~/.amux/bin/gh as a SYMLINK to this checkout's scripts/gh-shim/gh, the
# same pattern the amux CLI already uses (~/.local/bin/amux -> <repo>/amux). A
# symlink means there is no vendored copy to go stale, which is the failure mode
# install-hooks.sh has to detect with a drift token.
#
# THE SYMLINK ALONE CHANGES NOTHING. Until ~/.amux/bin is on PATH, no `gh` call
# resolves to it. That is deliberate: putting the App identity in front of every
# fleet gh call is a real change of who the fleet is to GitHub, and the PATH line
# is left for the owner to add rather than written into their shell profile here.
# This script prints the line and stops.
#
# Re-running is safe: an existing correct symlink is left alone, an existing
# WRONG one is reported rather than replaced, and a real file at that path is
# never touched.
set -uo pipefail
cd "$(dirname "$0")/.."
SRC="$(pwd)/scripts/gh-shim/gh"
DEST_DIR="${AMUX_HOME:-$HOME/.amux}/bin"
DEST="$DEST_DIR/gh"

[ -x "$SRC" ] || { echo "no shim at $SRC (or not executable)" >&2; exit 1; }
bash -n "$SRC" || { echo "refusing to install: $SRC does not parse" >&2; exit 1; }

mkdir -p "$DEST_DIR" || exit 1

if [ -L "$DEST" ]; then
  cur=$(readlink "$DEST")
  if [ "$cur" = "$SRC" ]; then
    echo "already installed: $DEST -> $SRC"
  else
    echo "REFUSING: $DEST is a symlink to $cur, not $SRC." >&2
    echo "Someone else installed a different shim; remove it deliberately first." >&2
    exit 1
  fi
elif [ -e "$DEST" ]; then
  echo "REFUSING: $DEST exists and is not a symlink — not overwriting a real file." >&2
  exit 1
else
  ln -s "$SRC" "$DEST" || exit 1
  echo "installed: $DEST -> $SRC"
fi

case ":$PATH:" in
  *":$DEST_DIR:"*)
    echo "$DEST_DIR is on PATH — the shim is LIVE for this shell."
    echo "Verify with:  gh api /rate_limit --jq .resources.core.limit"
    echo "  8700-ish = the App token · 5000 = still the ambient user"
    ;;
  *)
    echo
    echo "NOT LIVE YET. $DEST_DIR is not on PATH, so every gh call still runs as the"
    echo "ambient user and competes with CI for its 5000/hr. To turn it on, add this"
    echo "to your shell profile (your file, so this script does not edit it):"
    echo
    echo "    export PATH=\"$DEST_DIR:\$PATH\""
    echo
    echo "To turn it off again: remove that line, or AMUX_GH_SHIM=0 for one call."
    ;;
esac
