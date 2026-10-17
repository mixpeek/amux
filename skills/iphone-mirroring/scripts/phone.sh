#!/usr/bin/env bash
# Drive the owner's iPhone through macOS iPhone Mirroring (skills/iphone-mirroring.md).
#   phone.sh shot [out.png]   screenshot of the phone only; prints the path (coordinates = image pixels)
#   phone.sh tap X Y          tap at image coordinates from the last shot
#   phone.sh press X Y [SECS] long press (default 1 s)
#   phone.sh scroll [DY]      scroll the middle of the screen; DY<0 scrolls down (default -40)
#   phone.sh home | switcher  Home screen (Cmd-1) | app switcher (Cmd-2)
#   phone.sh open NAME        open an app through the phone's Spotlight (Cmd-3, type, Return)
#   phone.sh type TEXT        type into the focused field
#   phone.sh frame            window x y w h on the Mac screen
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="${AMUX_HOME:-$HOME/.amux}/bin/iphone-mx"
if [[ ! -x "$BIN" || "$HERE/mx.swift" -nt "$BIN" ]]; then
  mkdir -p "$(dirname "$BIN")"; swiftc -O "$HERE/mx.swift" -o "$BIN"
fi
front() { osascript -e 'tell application "iPhone Mirroring" to activate' >/dev/null; sleep 0.4; }
frame() {
  front
  osascript -e 'tell application "System Events" to tell process "iPhone Mirroring" to get {position, size} of window 1' | tr -d ' '
}
read_frame() { IFS=, read -r FX FY FW FH < <(frame); }
keys() { front; osascript -e "tell application \"System Events\" to $1"; }
case "${1:-}" in
  frame) frame ;;
  shot)
    read_frame
    out="${2:-${TMPDIR:-/tmp}/iphone-$(date +%s).png}"
    /usr/sbin/screencapture -x -R"$FX,$FY,$FW,$FH" "$out"   # not on PATH under launchd shells
    echo "$out" ;;
  tap) read_frame; "$BIN" click $((FX + $2)) $((FY + $3)) ;;
  press) read_frame; "$BIN" press $((FX + $2)) $((FY + $3)) "${4:-1.0}" ;;
  scroll) read_frame; "$BIN" scroll $((FX + FW / 2)) $((FY + FH / 2)) "${2:--40}" ;;
  home) keys 'keystroke "1" using command down' ;;
  switcher) keys 'keystroke "2" using command down' ;;
  open)
    keys 'keystroke "1" using command down'; sleep 1
    keys 'keystroke "3" using command down'; sleep 1.2
    keys "keystroke \"${2//\"/}\""; sleep 1.2
    keys 'key code 36' ;;
  type) keys "keystroke \"${2//\"/}\"" ;;
  *) sed -n '2,10p' "$0"; exit 2 ;;
esac
