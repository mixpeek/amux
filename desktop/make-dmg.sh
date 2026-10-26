#!/bin/bash
# Package Amux.app as a disk image a person double-clicks: it opens a window
# with the app and an Applications shortcut to drag it onto (Ethan,
# 2026-10-26: "make an app that users can double click as a .dmg").
#
#   desktop/make-dmg.sh [path/to/Amux.app] [out.dmg]
#
# CI (desktop.yml) runs it on the universal build and publishes the result to
# the desktop-latest release beside Amux-macOS.zip. It checks its own output:
# the image mounts, and the app inside has a valid signature and the same
# minimum macOS as the one it was made from.
set -euo pipefail
APP="${1:-Amux.app}"
OUT="${2:-Amux-macOS.dmg}"
[ -d "$APP/Contents/MacOS" ] || { echo "make-dmg: no app bundle at $APP" >&2; exit 1; }

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/Amux.app"
ln -s /Applications "$STAGE/Applications"
rm -f "$OUT"
hdiutil create -volname "amux" -srcfolder "$STAGE" -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov "$OUT" >/dev/null

# Prove what was made: mount it read-only, check the app, detach.
MNT=$(mktemp -d)
hdiutil attach -readonly -nobrowse -mountpoint "$MNT" "$OUT" >/dev/null
ok=1
[ -L "$MNT/Applications" ] || { echo "make-dmg: the image has no Applications shortcut" >&2; ok=0; }
codesign --verify --deep "$MNT/Amux.app" || { echo "make-dmg: the app inside does not verify" >&2; ok=0; }
want=$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$APP/Contents/Info.plist")
got=$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$MNT/Amux.app/Contents/Info.plist")
[ "$want" = "$got" ] || { echo "make-dmg: minimum macOS changed ($want -> $got)" >&2; ok=0; }
hdiutil detach "$MNT" -quiet || hdiutil detach "$MNT" -force -quiet
rmdir "$MNT" 2>/dev/null || true
[ "$ok" = 1 ] || exit 1
echo "make-dmg: $OUT ($(du -h "$OUT" | cut -f1), Amux.app + Applications, macOS $want and later)"
