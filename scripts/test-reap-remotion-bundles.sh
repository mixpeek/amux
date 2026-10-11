#!/usr/bin/env bash
# Proves scripts/reap-remotion-bundles.sh reaps only stale, unopened bundles.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
mkdir -p "$T/remotion-webpack-bundle-old/public" "$T/remotion-webpack-bundle-new/public" "$T/other-old"
echo x > "$T/remotion-webpack-bundle-old/public/a.mp4"; echo y > "$T/remotion-webpack-bundle-new/public/b.mp4"
touch -t 202001010000 "$T/remotion-webpack-bundle-old" "$T/other-old"
out=$(REMOTION_TMP="$T" REAP_MIN=20 bash "$HERE/reap-remotion-bundles.sh")
fail=0
[ ! -e "$T/remotion-webpack-bundle-old" ] || { echo "FAIL: stale bundle not reaped"; fail=1; }
[ -e "$T/remotion-webpack-bundle-new" ] || { echo "FAIL: fresh bundle was reaped"; fail=1; }
[ -e "$T/other-old" ] || { echo "FAIL: a non-Remotion directory was touched"; fail=1; }
printf '%s' "$out" | grep -q "verdict=remotion_bundles_reaped" || { echo "FAIL: no verdict line: $out"; fail=1; }
out2=$(REMOTION_TMP="$T" REAP_MIN=20 bash "$HERE/reap-remotion-bundles.sh")
printf '%s' "$out2" | grep -q "verdict=remotion_bundles_none" || { echo "FAIL: second run should find nothing: $out2"; fail=1; }
[ "$fail" = 0 ] && echo "PASS: reap-remotion-bundles"
exit "$fail"
