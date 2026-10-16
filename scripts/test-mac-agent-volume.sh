#!/usr/bin/env bash
# test-mac-agent-volume.sh: scripts/mac-agent-volume.sh reports each opt-out from
# what it can READ, and verify passes only when all of them are measured in place.
# diskutil and mdutil are fakes driven by state files, and the mount point is a
# temp directory, so no cell touches a real disk.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
S="$HERE/mac-agent-volume.sh"
FIX=$(mktemp -d "${TMPDIR:-/tmp}/agentvol.XXXXXX")
trap 'chmod -R u+rwx "${FIX:?}" 2>/dev/null; rm -rf -- "${FIX:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { printf '%s' "$1" | grep -qF -- "$2" && echo yes || echo no; }

MNT="$FIX/mnt"; ST="$FIX/state"; mkdir -p "$ST"
cat > "$FIX/diskutil" <<EOF
#!/bin/bash
echo "\$*" >> "$ST/calls"
case "\$1 \${2:-}" in
  "info /") echo "   APFS Container:            diskT" ;;
  "info "*) [ -f "$ST/exists" ] || exit 1; [ -f "$ST/mounted" ] && echo "   Mounted:                   Yes" || echo "   Mounted:                   No" ;;
  "apfs addVolume") touch "$ST/exists" "$ST/mounted"; mkdir -p "$MNT" ;;
  "mount "*) [ -f "$ST/exists" ] && touch "$ST/mounted" && mkdir -p "$MNT" ;;
esac
EOF
cat > "$FIX/mdutil" <<EOF
#!/bin/bash
echo "\$*" >> "$ST/mdcalls"
case "\$1" in
  -s) [ -f "$ST/indexing_off" ] && printf '%s:\n\tIndexing disabled.\n' "\$2" || printf '%s:\n\tIndexing enabled.\n' "\$2" ;;
  -i) [ "\$2" = off ] && touch "$ST/indexing_off" ;;
esac
EOF
chmod +x "$FIX/diskutil" "$FIX/mdutil"
run() { AMUX_AGENT_VOLUME_NAME=TestVol AMUX_AGENT_VOLUME_MOUNT="$MNT" AMUX_AGENT_VOLUME_DISKUTIL="$FIX/diskutil" \
        AMUX_AGENT_VOLUME_MDUTIL="$FIX/mdutil" AMUX_AGENT_VOLUME_SUDO_MDUTIL="$FIX/mdutil" bash "$S" "$@" 2>&1; }
reset() { chmod -R u+rwx "$MNT" 2>/dev/null || true; rm -rf -- "${MNT:?}" "${ST:?}"; mkdir -p "$ST"; }

echo "1. no volume: say so, and verify fails"
reset; out=$(run status) || true
check "it reports the volume missing"                 yes "$(has "$out" 'agent-volume: TestVol exists=no')"
check "and names create as the next step"             yes "$(printf '%s' "$out" | grep -qE '^next: .*mac-agent-volume.sh create$' && echo yes || echo no)"
# The tick runs a temporary copy of the committed bytes; the step must still name
# the script a person can run, never the copy (found in the e2e run, 2026-10-16).
cp "$S" "$FIX/tmp.copy"; out=$(AMUX_REPO_DIR=/the/repo AMUX_AGENT_VOLUME_NAME=TestVol AMUX_AGENT_VOLUME_MOUNT="$MNT" \
  AMUX_AGENT_VOLUME_DISKUTIL="$FIX/diskutil" AMUX_AGENT_VOLUME_MDUTIL="$FIX/mdutil" bash "$FIX/tmp.copy" status 2>&1)
check "run from a copy, it names the real script"     yes "$(has "$out" 'next: /the/repo/scripts/mac-agent-volume.sh create')"
check "and never the copy"                            no  "$(has "$out" 'tmp.copy')"
run verify >/dev/null && rc=0 || rc=$?; check "verify exits 1"         1   "$rc"

echo "2. create adds it to the boot container and turns Spotlight off, once"
reset; out=$(run create) || true
check "addVolume names the boot container"            yes "$(grep -c 'apfs addVolume diskT APFS TestVol' "$ST/calls" | sed 's/^1$/yes/')"
check "Spotlight is turned off"                       yes "$(grep -q '^-i off' "$ST/mdcalls" && echo yes || echo no)"
check "and the report reads it back as off"           yes "$(has "$out" 'spotlight=off')"
run create >/dev/null || true
check "a second create adds no second volume"         1   "$(grep -c 'apfs addVolume' "$ST/calls")"

echo "3. fseventsd is judged from what can be read"
reset; touch "$ST/exists" "$ST/mounted" "$ST/indexing_off"; mkdir -p "$MNT/.fseventsd"; chmod 000 "$MNT/.fseventsd"
out=$(run status) || true
check "a root-only .fseventsd reads unknown, not off" yes "$(has "$out" 'fseventsd=unknown')"
check "and the owner step writes no_log"              yes "$(has "$out" "touch \"$MNT/.fseventsd/no_log\"")"
check "makes the marker readable"                     yes "$(has "$out" "chmod 711 \"$MNT/.fseventsd\"")"
check "and remounts so fseventsd reads it"            yes "$(has "$out" 'unmount "TestVol" && ')"
run verify >/dev/null && rc=0 || rc=$?; check "verify exits 1 while unknown" 1 "$rc"
chmod 711 "$MNT/.fseventsd"; out=$(run status) || true
check "a searchable .fseventsd with no marker is on"  yes "$(has "$out" 'fseventsd=on')"
touch "$MNT/.fseventsd/no_log"; out=$(run verify) && rc=0 || rc=$?
check "with the marker it reads off"                  yes "$(has "$out" 'fseventsd=off spotlight=off')"
check "and verify passes"                             0   "$rc"
check "with no next step left"                        no  "$(has "$out" 'next')"

echo "4. Spotlight back on fails verify and names the fix"
rm -f "$ST/indexing_off"; out=$(run verify) && rc=0 || rc=$?
check "verify exits 1"                                1   "$rc"
check "the next step turns indexing off"              yes "$(has "$out" "-i off \"$MNT\"")"

echo "5. an unmounted volume is not reported as fine"
rm -f "$ST/mounted"; out=$(run verify) && rc=0 || rc=$?
check "mounted=no is said"                            yes "$(has "$out" 'exists=yes mounted=no')"
check "and verify exits 1"                            1   "$rc"
check "the hang hint says whether a dialog is open"   yes "$(printf '%s' "$out" | grep -qE 'SecurityAgent is (running|not running)' && echo yes || echo no)"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-agent-volume — all checks passed"; exit 0; fi
echo "mac-agent-volume: $fails check(s) FAILED"; exit 1
