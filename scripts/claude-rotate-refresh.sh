#!/usr/bin/env bash
# Bring an installed claude-rotate up to this checkout's scripts/claude-rotate.py.
# Only the script is replaced; the status line, the tick schedule and the on/off
# switch stay as the owner set them (claude-rotate-install.sh sets those once).
# A host with no installed rotator is left without one: installing is opt-in.
#
# cloud/desktop/update.sh runs this after every pull. Before it did, a host ran
# whatever copy was installed by hand: on 2026-11-05 mixpeek-dev's rotator was a
# day behind main (no `hold`), and the reset-parsing fix reached it only because
# a worker copied it there.
#
# Prints one line: verdict=claude_rotate_refreshed | claude_rotate_current |
# claude_rotate_not_installed | claude_rotate_refresh_failed.
set -uo pipefail
src="${1:-$(cd "$(dirname "$0")" && pwd)/claude-rotate.py}"
dest="${2:-$HOME/.amux/claude-rotate/claude-rotate.py}"
sha() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -c1-12; }
if [[ ! -f "$dest" ]]; then
  echo "verdict=claude_rotate_not_installed dest=$dest"
  exit 0
fi
if [[ ! -f "$src" ]]; then
  echo "verdict=claude_rotate_refresh_failed why=no_source src=$src"
  exit 1
fi
if cmp -s "$src" "$dest"; then
  echo "verdict=claude_rotate_current sha=$(sha "$dest")"
  exit 0
fi
old="$(sha "$dest")"
# rename(2), so a tick running the old copy finishes on the bytes it started with.
tmp="$(mktemp "$(dirname "$dest")/.new.XXXXXX")" &&
  cp "$src" "$tmp" && chmod 755 "$tmp" && mv "$tmp" "$dest" || {
  rm -f "${tmp:-}"
  echo "verdict=claude_rotate_refresh_failed why=copy dest=$dest"
  exit 1
}
echo "verdict=claude_rotate_refreshed old=$old new=$(sha "$dest") dest=$dest"
