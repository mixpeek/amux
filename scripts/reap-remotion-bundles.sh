#!/usr/bin/env bash
# Reap Remotion's leftover webpack bundles (Mac fseventsd/disk RCA 20261010-211803).
#
# Remotion's bundle() copies the project's public/ folder into
# $TMPDIR/remotion-webpack-bundle-XXXX on every call and never deletes it. A
# render loop that bundles per render (short-form-videos, 2026-10-10) left 24
# bundles = 238 GB in two hours and took the disk to 8 GB free. A bundle is
# build output, regenerated on the next bundle(); one nothing has open and
# nothing has touched in REAP_MIN minutes is garbage.
#
# Env: REMOTION_TMP (default $TMPDIR), REAP_MIN (default 20), DRY=1 to only report.
# Prints one verdict line: remotion_bundles_reaped / remotion_bundles_none.
set -euo pipefail
ROOT="${REMOTION_TMP:-${TMPDIR:-/tmp}}"
ROOT="${ROOT%/}"
MIN="${REAP_MIN:-20}"
n=0; kept=0; kb=0
while IFS= read -r -d '' d; do
  if command -v lsof >/dev/null 2>&1 && lsof +D "$d" >/dev/null 2>&1; then kept=$((kept+1)); continue; fi
  sz=$(du -sk "$d" 2>/dev/null | cut -f1); sz=${sz:-0}
  if [ "${DRY:-0}" = 1 ]; then echo "would reap: $d (${sz}K)"; n=$((n+1)); kb=$((kb+sz)); continue; fi
  rm -rf -- "${d:?}" && { n=$((n+1)); kb=$((kb+sz)); }
done < <(find "$ROOT" -maxdepth 1 -type d -name 'remotion-webpack-bundle-*' -mmin "+$MIN" -print0 2>/dev/null)
if [ "$n" -gt 0 ]; then
  echo "remotion-bundles: reaped $n bundle(s), $((kb/1048576))G, kept $kept open (verdict=remotion_bundles_reaped root=$ROOT min=$MIN)"
else
  echo "remotion-bundles: nothing to reap, kept $kept open (verdict=remotion_bundles_none root=$ROOT min=$MIN)"
fi
