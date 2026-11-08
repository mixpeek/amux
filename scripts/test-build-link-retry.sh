#!/usr/bin/env bash
# AF-1012: a deploy build whose LINK fails on unresolved symbols (a stale
# incremental cache) clears the amux_server incremental trees and retries once,
# so the deploy lands without anyone clearing the cache by hand. A compile
# error is not a stale cache and is not retried.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILDER="$ROOT/scripts/rust-auto-build.sh"
pass=0

run_case() {  # $1 = failure text the fake cargo prints while the stale tree exists
  local failure="$1"
  TMP=$(mktemp -d)
  REPO="$TMP/repo"
  FAKE_HOME="$TMP/home"
  INSTALL="$TMP/bin/amux-server-rs"
  git init -q -b main "$REPO"
  mkdir -p "$REPO/crates" "$REPO/scripts" "$FAKE_HOME/.cargo/bin" "$(dirname "$INSTALL")"
  printf '[workspace]\n' > "$REPO/Cargo.toml"
  printf 'fixture\n' > "$REPO/crates/input.rs"
  # A stub guard: the real one refuses to delete while it cannot identify every
  # process on the host, which a hardened Linux CI runner never lets it do
  # ("process executable unmeasured: pid 1219", red on cab92a17a). Its own
  # tests cover that. Here it records the clear the builder asked for and does it.
  cat > "$REPO/scripts/cargo-target-guard.py" <<GUARD
import shutil, sys
a = sys.argv[1:]
open("$TMP/guard.calls", "a").write(" ".join(a) + "\\n")
shutil.rmtree(a[a.index("--path") + 1])
print('{"verdict": "cargo_reclaim_done", "measured": true}')
GUARD
  cat > "$REPO/scripts/safe-cargo.sh" <<EOF
#!/bin/sh
echo call >> "$TMP/cargo.calls"
out=release; prev=
for a in "\$@"; do [ "\$prev" = --profile ] && out="\$a"; prev="\$a"; done
if ls -d "\$CARGO_TARGET_DIR/\$out/incremental"/amux_server-* >/dev/null 2>&1; then
  printf '%s\n' "$failure"
  exit 101
fi
mkdir -p "\$CARGO_TARGET_DIR/\$out"
printf '%s\n' '#!/bin/sh' 'printf "%s\\\\n" new-build' > "\$CARGO_TARGET_DIR/\$out/amux-server"
chmod 0755 "\$CARGO_TARGET_DIR/\$out/amux-server"
EOF
  chmod +x "$REPO/scripts/safe-cargo.sh"
  (cd "$REPO" && git add -A && git -c user.name=test -c user.email=test@example.com commit -qm fixture)
  mkdir -p "$FAKE_HOME/.amux/rust-build-target/deploy/incremental/amux_server-stale"
  echo stale > "$FAKE_HOME/.amux/rust-build-target/deploy/incremental/amux_server-stale/f"
  printf '#!/bin/sh\nprintf old-build\\n\n' > "$INSTALL"
  chmod 0755 "$INSTALL"
  HOME="$FAKE_HOME" \
  AMUX_REPO="$REPO" \
  AMUX_RS_INSTALL="$INSTALL" \
  AMUX_RS_BUILD_STAMP="$TMP/stamp" \
  AMUX_RS_BUILD_LOG="$TMP/build.log" \
  AMUX_RS_BUILD_LOCK="$TMP/build.lock" \
  AMUX_RS_BUILD_PROVENANCE="$TMP/provenance.json" \
  AMUX_RS_ACTIVATION_REF=HEAD \
  AMUX_BUILD_MIN_FREE_GB=0 \
  AMUX_BUILD_DEBUG_MAX_GB=999999 \
  AMUX_RS_SHELL_DEFER_MAX_S=0 \
  AMUX_ACTIVATION_ROLLBACK=0 \
  AMUX_RS_RUNS_URL=http://127.0.0.1:9/none \
    bash "$BUILDER" > "$TMP/stdout" 2>&1 || true
  cat "$TMP/stdout" "$TMP/build.log" 2>/dev/null > "$TMP/out"
  CALLS=$(wc -l < "$TMP/cargo.calls" | tr -d ' ')
  RAN=$("$INSTALL")
  GUARD=$(cat "$TMP/guard.calls" 2>/dev/null || true)
}

run_case 'ld: symbol(s) not found for architecture arm64'
if grep -q 'verdict=deploy_incremental_link_retry' "$TMP/out"; then pass=$((pass+1)); else echo "FAIL: no deploy_incremental_link_retry line"; cat "$TMP/out"; exit 1; fi
if [ "$CALLS" -eq 2 ]; then pass=$((pass+1)); else echo "FAIL: expected 2 cargo builds (fail, retry), got $CALLS"; cat "$TMP/out"; exit 1; fi
if [ "${RAN#new-build}" != "$RAN" ]; then pass=$((pass+1)); else echo "FAIL: the retried build was not installed (runs: $RAN)"; cat "$TMP/out"; exit 1; fi
case "$GUARD" in "clear --target "*"/deploy/incremental/amux_server-stale"*) pass=$((pass+1)) ;; *) echo "FAIL: the stale tree was not cleared through the target guard (calls: $GUARD)"; exit 1 ;; esac
rm -rf "${TMP:?}"

run_case 'error[E0425]: cannot find value `x` in this scope'
if [ "$CALLS" -eq 1 ]; then pass=$((pass+1)); else echo "FAIL: a compile error was retried ($CALLS builds)"; cat "$TMP/out"; exit 1; fi
if [ "${RAN#old-build}" != "$RAN" ]; then pass=$((pass+1)); else echo "FAIL: a failed compile replaced the live build (runs: $RAN)"; exit 1; fi
rm -rf "${TMP:?}"

echo "test-build-link-retry: $pass passed, 0 failed"
