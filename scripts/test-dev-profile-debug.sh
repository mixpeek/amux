#!/usr/bin/env bash
# The workspace dev profile's debug level must equal the one safe-cargo and CI
# export (CARGO_PROFILE_DEV_DEBUG), or a bare cargo and safe-cargo build every
# crate twice into the shared target (disk RCA 20261030-193722).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
toml=$(awk '/^\[profile\.dev\]$/{p=1;next} /^\[/{p=0} p && /^debug *=/{gsub(/[ "]/,"");sub(/debug=/,"");print}' "$ROOT/Cargo.toml")
safe=$(sed -n 's/^export CARGO_PROFILE_DEV_DEBUG="\${CARGO_PROFILE_DEV_DEBUG:-\(.*\)}"$/\1/p' "$ROOT/scripts/safe-cargo.sh")
ci=$(sed -n "s/^ *CARGO_PROFILE_DEV_DEBUG: '\(.*\)'$/\1/p" "$ROOT/.github/workflows/rust.yml" | head -1)
echo "dev profile debug: Cargo.toml=${toml:-unset} safe-cargo=${safe:-unset} rust.yml=${ci:-unset}"
[ -n "$toml" ] && [ "$toml" = "$safe" ] && [ "$toml" = "$ci" ] || { echo "FAIL: they differ; a bare cargo and safe-cargo would build every crate twice"; exit 1; }
echo "ok: one dev profile"
