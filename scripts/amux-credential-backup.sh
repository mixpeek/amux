#!/usr/bin/env bash
# Periodic backup of single-copy credential-shaped files under ~/.amux —
# files this box has NO other recovery path for if they vanish (AMUX-76:
# ~/.amux/gmail-oauth-client.json disappeared from disk with root cause
# never determined; recovery only worked because its client_id/secret
# happened to ALSO live in server.env under a different key — a lucky
# coincidence this script exists so the next file doesn't need).
#
# Deliberately narrow: only files known to be (a) single-copy — nothing
# else on this box holds their content, and (b) not already covered by
# git or another backup path. Add to this list as new single-copy
# credential files are introduced; do NOT widen it to "everything in
# ~/.amux" — most of that is either regenerable, DB-backed (already has
# its own durability story), or explicitly gitignored on purpose.
#
# No secret VALUES appear in this script or its own output — only paths.
set -euo pipefail

AMUX_HOME="${AMUX_HOME:-$HOME/.amux}"
BACKUP_DIR="$AMUX_HOME/backups/credentials"
KEEP=10 # prune to the last N backups per file, so this can't grow unbounded

FILES=(
  "$AMUX_HOME/gmail-oauth-client.json"
)

mkdir -p "$BACKUP_DIR"
chmod 700 "$BACKUP_DIR"

ts="$(date -u +%Y%m%dT%H%M%SZ)"
backed_up=0
for f in "${FILES[@]}"; do
  name="$(basename "$f")"
  if [ ! -f "$f" ]; then
    echo "amux-credential-backup: $name is MISSING right now — cannot back up what is not there" >&2
    continue
  fi
  dest="$BACKUP_DIR/${name}.${ts}"
  cp "$f" "$dest"
  chmod 600 "$dest"
  backed_up=$((backed_up + 1))
  # Prune: keep only the newest $KEEP backups for this file.
  # shellcheck disable=SC2012
  ls -1t "$BACKUP_DIR/${name}."* 2>/dev/null | tail -n +$((KEEP + 1)) | xargs -r rm -f
done
echo "amux-credential-backup: backed up $backed_up/${#FILES[@]} file(s) to $BACKUP_DIR"
