#!/usr/bin/env bash
# scripts/rust-prebuild.sh compiles a commit in the builder's stable directory
# and installs nothing; it never runs beside the builder, never twice for one
# commit, and says what it did.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PRE="$ROOT/scripts/rust-prebuild.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }

REPO="$TMP/repo"
git init -q -b main "$REPO"
mkdir -p "$REPO/scripts" "$REPO/crates"
printf 'fixture\n' > "$REPO/crates/input.rs"
# The stand-in cargo: records the directory and profile it ran with.
cat > "$REPO/scripts/safe-cargo.sh" <<EOF
#!/bin/sh
echo "\$(pwd) \$*" >> "$TMP/cargo.calls"
EOF
chmod +x "$REPO/scripts/safe-cargo.sh"
(cd "$REPO" && git add -A && git -c user.name=t -c user.email=t@example.com commit -qm one)
SHA=$(git -C "$REPO" rev-parse HEAD)

run() {
  env HOME="$TMP/home" AMUX_RS_BUILD_LOG="$TMP/build.log" AMUX_RS_BUILD_LOCK="$TMP/build.lock" \
    AMUX_DEPLOY_SRC="$TMP/stable" AMUX_DEPLOY_TARGET_DIR="$TMP/target" AMUX_PREBUILD_DONE="$TMP/done" \
    AMUX_BUILD_MIN_FREE_GB=0 AMUX_DEPLOY_JOBS=2 "$@" bash "$PRE" "$REPO" "$SHA"
}
mkdir -p "$TMP/home"

run
grep -q "verdict=prebuild_done" "$TMP/build.log" && ok "it compiles and says so" || bad "no prebuild_done: $(cat "$TMP/build.log")"
grep -q "^$TMP/stable build --profile deploy -p amux-server" "$TMP/cargo.calls" \
  && ok "in the builder's stable directory, with the deploy profile" || bad "cargo ran as: $(cat "$TMP/cargo.calls")"
[ "$(git -C "$TMP/stable" rev-parse HEAD)" = "$SHA" ] && ok "at the commit asked for" || bad "stable dir is not at $SHA"
[ ! -e "$TMP/build.lock" ] && ok "the builder's lock is released" || bad "lock left behind"
[ ! -e "$TMP/home/.local/bin/amux-server-rs" ] && [ ! -e "$TMP/home/.amux/rust-build-stamp" ] \
  && ok "nothing is installed and no stamp is written" || bad "prebuild changed live state"

n=$(wc -l < "$TMP/cargo.calls")
run
[ "$(wc -l < "$TMP/cargo.calls")" = "$n" ] && ok "the same commit is not compiled twice" || bad "compiled twice"

rm -f "$TMP/done"
mkdir "$TMP/build.lock"; echo $$ > "$TMP/build.lock/pid"
run
grep -q "verdict=prebuild_skipped reason=lock" "$TMP/build.log" && ok "a held builder lock is a skip" || bad "ran beside the builder"
[ -d "$TMP/build.lock" ] && [ "$(cat "$TMP/build.lock/pid")" = $$ ] && ok "and the builder's lock is left alone" || bad "took the builder's lock"
rm -rf "$TMP/build.lock"

run AMUX_BUILD_MIN_FREE_GB=999999
grep -q "verdict=prebuild_skipped reason=disk" "$TMP/build.log" && ok "low disk is a skip" || bad "built on low disk"

printf 'exit 1\n' >> "$REPO/scripts/safe-cargo.sh"
(cd "$REPO" && git -c user.name=t -c user.email=t@example.com commit -qam broken)
SHA=$(git -C "$REPO" rev-parse HEAD)
run
grep -q "verdict=prebuild_failed" "$TMP/build.log" && ok "a failed compile is logged as one" || bad "no prebuild_failed"

echo "test-rust-prebuild: $fails failed"
exit "$fails"
