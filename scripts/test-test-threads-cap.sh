#!/usr/bin/env bash
# The test-thread cap (cpu RCA 2026-10-28): install.sh writes RUST_TEST_THREADS
# into the checkout's cargo config and the amux home's, cargo hands it to the
# test binary in a worktree under the amux home, and the cleanup tick names a
# test binary running without one (verdict=cpu_test_threads_unbounded).
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD
T=$(mktemp -d "${TMPDIR:-/tmp}/test-threads-cap.XXXXXX")
trap 'rm -rf "$T"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$3', got '$2'"; fails=$((fails + 1)); fi; }

# A. install.sh's cargo-config block, run on its own against a temp checkout and home.
sed -n '/^if \[\[ -n "\${AMUX_NO_CARGO_CONFIG:-}" \]\]; then/,/^fi$/p' install.sh > "$T/block.sh"
[ -s "$T/block.sh" ] || { echo "  FAIL could not find install.sh's cargo config block"; exit 1; }
run_block() { # extra env assignments...
  rm -rf "$T/checkout" "$T/home"; mkdir -p "$T/checkout" "$T/home"
  env "$@" SCRIPT_DIR="$T/checkout" AMUX_HOME="$T/home" SHARED_TARGET_DIR="$T/target" \
    bash -c 'say() { :; }; set -euo pipefail; source "$0"' "$T/block.sh"
}
run_block
check "the checkout's config caps test threads" "$(grep -c '^RUST_TEST_THREADS = "4"$' "$T/checkout/.cargo/config.toml")" 1
check "jobs stays under [build], before [env]" "$(awk '/^\[env\]/{e=1} /^jobs/{print (e?"env":"build")}' "$T/checkout/.cargo/config.toml")" ""
check "the amux home's config caps test threads" "$(grep -c '^RUST_TEST_THREADS = "4"$' "$T/home/.cargo/config.toml")" 1
run_block AMUX_CARGO_JOBS=2
check "with a jobs cap, jobs is still a [build] key" "$(awk '/^\[env\]/{e=1} /^jobs/{print (e?"env":"build")}' "$T/checkout/.cargo/config.toml")" "build"
run_block AMUX_TEST_THREADS=0
check "AMUX_TEST_THREADS=0 lifts the cap" "$(grep -c RUST_TEST_THREADS "$T/checkout/.cargo/config.toml" || true)" 0
check "and writes no amux home config" "$([ -e "$T/home/.cargo/config.toml" ] && echo yes || echo no)" no

# The cap reaches a test binary in a worktree under the amux home.
run_block
mkdir -p "$T/home/tmp/lane/wt/src"
cat > "$T/home/tmp/lane/wt/Cargo.toml" <<'EOF'
[package]
name = "threads_probe"
version = "0.0.0"
edition = "2021"
EOF
cat > "$T/home/tmp/lane/wt/src/lib.rs" <<'EOF'
#[test]
fn the_cap_reaches_the_test_binary() {
    assert_eq!(std::env::var("RUST_TEST_THREADS").as_deref(), Ok("4"));
}
EOF
if (cd "$T/home/tmp/lane/wt" && env -u RUST_TEST_THREADS CARGO_TARGET_DIR="$T/probe-target" cargo test -q >"$T/probe.log" 2>&1); then
  check "a worktree under the amux home runs its tests capped" pass pass
else
  check "a worktree under the amux home runs its tests capped" fail pass; tail -5 "$T/probe.log"
fi

# B. The tick's note on a test binary, with ps stubbed.
sed -n '/^test_binary_note() {/,/^}/p' scripts/mac-cleanup-tick.sh > "$T/note.sh"
[ -s "$T/note.sh" ] || { echo "  FAIL could not find test_binary_note in mac-cleanup-tick.sh"; exit 1; }
mkdir -p "$T/bin"
cat > "$T/bin/ps" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  -M) echo "USER PID TT %CPU"; for i in $(seq 28); do echo "ethan 1 s 1.0"; done ;;
  -E) echo "/x/target/debug/deps/amux_server-c84b671cb81b12ec social HOME=/Users/x ${STUB_CAP:-}" ;;
esac
EOF
chmod +x "$T/bin/ps"
note() { PATH="$T/bin:$PATH" STUB_CAP="${2:-}" bash -c 'source "$0"; test_binary_note 4242 "$1"' "$T/note.sh" "$1"; }
bin=/Users/x/.amux/rust-build-target/debug/deps/amux_server-c84b671cb81b12ec
check "an uncapped test binary is named, with its thread count" \
  "$(note "$bin" | grep -c '28 threads, RUST_TEST_THREADS unset: one test thread per core (verdict=cpu_test_threads_unbounded)')" 1
check "a capped one says its cap and no verdict" "$(note "$bin" RUST_TEST_THREADS=4)" " [cargo test binary, 28 threads, RUST_TEST_THREADS=4]"
check "any other process gets no note" "$(note /Users/x/.local/bin/amux-server-rs)" ""
check "the RCA bundle's top-CPU lines carry the note" \
  "$(grep -c 'lane: $(lane_for_pid "$pid" "$pmap")$(test_binary_note "$pid" "$cmd")"; done' scripts/mac-cleanup-tick.sh)" 1

if [ "$fails" -gt 0 ]; then echo "test-threads-cap: $fails check(s) FAILED"; exit 1; fi
echo "test-threads-cap: all checks passed"
