#!/bin/bash
# mac-agent-volume.sh — a volume for agent scratch that fseventsd and Spotlight skip.
#
# fseventsd records every file change on a volume. With dozens of agents
# building and writing scratch files, it grows for days (2026-10-14: 1.43 GB at
# 109% CPU, reboot the only remedy; the cleanup tick reported it every hour).
# Measured 2026-10-16 over 30 minutes: ~110k changed files under
# ~/.amux/tmp and /private/tmp/claude-501, 26 under ~/Dev/amux. The churn is
# agent scratch, so that is what belongs on this volume.
#
# A separate APFS volume in the boot container shares its free space (no fixed
# size) and can opt out of both: Spotlight with `mdutil -i off`, fseventsd with a
# `.fseventsd/no_log` file at its root, read when the volume mounts. Idea from
# https://x.com/fforres/status/2110883187920617472.
#
#   mac-agent-volume.sh status   one line: exists, mounted, fseventsd, Spotlight, size
#   mac-agent-volume.sh create   add the volume if missing, turn Spotlight off, then
#                                print the one root step fseventsd needs
#   mac-agent-volume.sh verify   exit 0 only when every opt-out is measured in place
#
# macOS creates .fseventsd owned by root and readable only by root, and the
# passwordless sudo on this Mac covers only purge and mdutil, so `no_log` needs
# one command from the owner. That step also makes .fseventsd searchable (711)
# so status can READ the marker as a user, rather than trusting it was written.
# Nothing here moves data; pointing scratch at the volume is a separate step.
set -uo pipefail

NAME=${AMUX_AGENT_VOLUME_NAME:-AgentScratch}
# The path a person can run. Not $0: the tick runs a temporary copy of the
# committed bytes, and a next step naming /var/folders/.../tmp.X is unrunnable.
ME=${AMUX_AGENT_VOLUME_SELF:-${AMUX_REPO_DIR:-$HOME/Dev/amux}/scripts/mac-agent-volume.sh}
MNT=${AMUX_AGENT_VOLUME_MOUNT:-/Volumes/$NAME}
# Seams: the test stands these in so no cell touches a real disk.
DISKUTIL=${AMUX_AGENT_VOLUME_DISKUTIL:-/usr/sbin/diskutil}
MDUTIL=${AMUX_AGENT_VOLUME_MDUTIL:-/usr/bin/mdutil}
SUDO_MDUTIL=${AMUX_AGENT_VOLUME_SUDO_MDUTIL:-sudo -n /usr/bin/mdutil}

volume_exists() { "$DISKUTIL" info "$NAME" >/dev/null 2>&1; }
is_mounted() { "$DISKUTIL" info "$NAME" 2>/dev/null | grep -qE '^ *Mounted: *Yes'; }
container_of_root() { "$DISKUTIL" info / 2>/dev/null | awk -F': *' '/APFS Container:/ {print $2; exit}'; }

# on | off | unknown. Read, never assumed.
fse_state() {
  [ -d "$MNT/.fseventsd" ] || { echo unknown; return; }
  if [ -e "$MNT/.fseventsd/no_log" ]; then echo off
  elif [ -x "$MNT/.fseventsd" ]; then echo on     # searchable and no marker: it is logging
  else echo unknown; fi                          # root-only: the marker cannot be read
}
spotlight_state() {
  local s; s=$("$MDUTIL" -s "$MNT" 2>/dev/null)
  case "$s" in *"Indexing disabled"*) echo off ;; *"Indexing enabled"*) echo on ;; *) echo unknown ;; esac
}
used_gb() { df -k "$MNT" 2>/dev/null | awk 'NR==2 { printf "%.1fG", $3/1048576 }'; }

root_step() {
  printf '%s\n' "sudo sh -c 'touch \"$MNT/.fseventsd/no_log\" && chmod 711 \"$MNT/.fseventsd\"' && $DISKUTIL unmount \"$NAME\" && $DISKUTIL mount \"$NAME\""
}

status_line() {
  if ! volume_exists; then echo "agent-volume: $NAME exists=no"; return; fi
  if ! is_mounted; then echo "agent-volume: $NAME exists=yes mounted=no"; return; fi
  echo "agent-volume: $NAME exists=yes mounted=yes fseventsd=$(fse_state) spotlight=$(spotlight_state) used=$(used_gb) at $MNT"
}

next_step() {
  if ! volume_exists; then echo "next: $ME create"; return; fi
  if ! is_mounted; then echo "next: $DISKUTIL mount \"$NAME\" (if it hangs, an authorization dialog may be open on screen: SecurityAgent is $(pgrep -x SecurityAgent >/dev/null 2>&1 && echo running || echo "not running"))"; return; fi
  [ "$(spotlight_state)" = off ] || echo "next: $SUDO_MDUTIL -i off \"$MNT\""
  case "$(fse_state)" in
    off) ;;
    *) echo "next (owner, needs a password): $(root_step)" ;;
  esac
}

case "${1:-status}" in
  status)
    status_line; next_step ;;
  create)
    if ! volume_exists; then
      c=$(container_of_root)
      [ -n "$c" ] || { echo "agent-volume: could not find the boot APFS container; created nothing" >&2; exit 1; }
      "$DISKUTIL" apfs addVolume "$c" APFS "$NAME" || { echo "agent-volume: addVolume failed" >&2; exit 1; }
    fi
    is_mounted || "$DISKUTIL" mount "$NAME" >/dev/null
    [ "$(spotlight_state)" = off ] || $SUDO_MDUTIL -i off "$MNT" >/dev/null 2>&1 || echo "agent-volume: could not turn Spotlight off (is passwordless sudo for mdutil still granted?)"
    status_line; next_step ;;
  verify)
    line=$(status_line); echo "$line"
    case "$line" in *"mounted=yes fseventsd=off spotlight=off"*) exit 0 ;; esac
    next_step; exit 1 ;;
  *)
    echo "usage: $ME status|create|verify" >&2; exit 2 ;;
esac
