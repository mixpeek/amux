#!/usr/bin/env bash
# Build each new commit on main into a Linux amux-server from a WARM cache and push it to the
# hosts that install prebuilt binaries (docs/design/cicd-two-minutes.md, Z4). Runs on a build
# host with Docker (mixpeek-dev), as a long-lived service; one line per action on stdout.
#
#   AMUX_PREBUILT_TARGETS  space-separated ssh destinations (e.g. amux@91.98.218.77); each one's
#                          authorized_keys forces scripts/amux-prebuilt-receive.sh for this key
#   AMUX_PREBUILT_KEY      the private key for them (default ~/.ssh/amux-prebuilt)
#   AMUX_PREBUILT_REPO     default https://github.com/mixpeek/amux.git
#   AMUX_PREBUILT_CPUS     CPU cap for the build container (default 40 of 48: the box is shared)
#   AMUX_PREBUILT_POLL_S   how often main is read (default 15)
#   AMUX_PREBUILT_NICE     the build's niceness (default 0; the CPU cap protects the box)
#
# Only the newest tip is built: a commit superseded while another builds is skipped, and the
# installing host's builder compiles that one itself if it ever needs it.
set -uo pipefail
TARGETS="${AMUX_PREBUILT_TARGETS:?set AMUX_PREBUILT_TARGETS}"
KEY="${AMUX_PREBUILT_KEY:-$HOME/.ssh/amux-prebuilt}"
REPO="${AMUX_PREBUILT_REPO:-https://github.com/mixpeek/amux.git}"
CPUS="${AMUX_PREBUILT_CPUS:-40}"
POLL="${AMUX_PREBUILT_POLL_S:-15}"
NICE="${AMUX_PREBUILT_NICE:-0}"   # the box idles at load ~10 of 48; the CPU cap is the guard
IMAGE="${AMUX_PREBUILT_IMAGE:-rust:1-bookworm}"
STATE="${AMUX_PREBUILT_STATE:-$HOME/.amux-prebuilt}"
mkdir -p "$STATE"
say() { echo "$(date -u +%FT%TZ) amux-prebuilt $*"; }
SSH=(ssh -i "$KEY" -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new)

# Returns 0 built, 1 failed, 2 superseded: a newer tip landed while this one built, so
# the build is stopped and the newest is built instead (b96fac4ef waited almost four
# minutes behind an older build, 2026-11-13). Cargo's incremental cache survives the stop.
build() { # <sha>: writes $STATE/out/amux-server
  local sha="$1" now
  mkdir -p "$STATE/out"; rm -f "$STATE/out/amux-server" "$STATE/rc"
  docker rm -f amux-prebuilt-build >/dev/null 2>&1
  ( docker run --rm --name amux-prebuilt-build --cpus "$CPUS" \
    -v amux-prebuilt-src:/src -v amux-prebuilt-cargo:/usr/local/cargo/registry \
    -v amux-prebuilt-rustup:/usr/local/rustup -v "$STATE/out:/out" \
    -e SHA="$sha" -e REPO="$REPO" -e NICE="$NICE" "$IMAGE" bash -c '
      set -e
      [ -d /src/amux/.git ] || git clone -q "$REPO" /src/amux
      cd /src/amux && git fetch -q origin main && git checkout -q -f "$SHA" && git clean -qfd -e target
      nice -n "$NICE" cargo build -q --profile deploy -p amux-server
      cp target/deploy/amux-server /out/amux-server' >"$STATE/build.log" 2>&1; echo $? > "$STATE/rc" ) &
  local pid=$! started; started=$(date +%s)
  while kill -0 "$pid" 2>/dev/null; do
    sleep "$POLL"
    # Only early on: amux-server compiles as one unit, so a stop late in the build loses
    # it all, and on a busy main a build that is always superseded never finishes.
    [ $(( $(date +%s) - started )) -lt "${AMUX_PREBUILT_SUPERSEDE_S:-60}" ] || continue
    now=$(git ls-remote "$REPO" refs/heads/main 2>/dev/null | awk '{print $1}')
    if [ -n "$now" ] && [ "$now" != "$sha" ]; then
      docker rm -f amux-prebuilt-build >/dev/null 2>&1; wait "$pid" 2>/dev/null
      say "stopped the build of $sha: main moved to $now verdict=prebuilt_superseded"
      return 2
    fi
  done
  wait "$pid" 2>/dev/null
  [ "$(cat "$STATE/rc" 2>/dev/null)" = 0 ] && [ -s "$STATE/out/amux-server" ]
}

# Same compile inputs as the last build: the binary is the same, so it is pushed under
# the new sha without building (bfc81b6d5 touched only scripts and took 156 s, which the
# next commit waited behind). The installing host stamps the identity, as on a restamp.
same_inputs() { # <old sha> <new sha>
  [ -n "$1" ] && [ -s "$STATE/out/amux-server" ] && docker run --rm -v amux-prebuilt-src:/src "$IMAGE" \
    bash -c "cd /src/amux && git fetch -q origin main && git diff --quiet $1 $2 -- crates Cargo.toml Cargo.lock rust-toolchain.toml" >/dev/null 2>&1
}

last=""
while :; do
  tip=$(git ls-remote "$REPO" refs/heads/main 2>/dev/null | awk '{print $1}')
  if [ -n "$tip" ] && [ "$tip" != "$last" ]; then
    t0=$(date +%s)
    if same_inputs "$last" "$tip"; then
      say "reused the $last binary for $tip: no compile input changed verdict=prebuilt_reused"
      ok=1
    else
      build "$tip"; rc=$?
      if [ "$rc" = 2 ]; then continue; fi      # the newest tip is built on the next pass
      [ "$rc" = 0 ] && ok=1 || ok=0
    fi
    if [ "$ok" = 1 ]; then
      size=$(wc -c < "$STATE/out/amux-server" | tr -d ' ')
      say "built $tip in $(( $(date +%s) - t0 ))s ($size bytes) verdict=prebuilt_built"
      for dest in $TARGETS; do
        if "${SSH[@]}" "$dest" "has $tip" 2>/dev/null; then continue; fi
        t1=$(date +%s)
        if "${SSH[@]}" "$dest" "receive $tip" < "$STATE/out/amux-server"; then
          say "pushed $tip to $dest in $(( $(date +%s) - t1 ))s verdict=prebuilt_pushed"
        else
          say "WARN could not push $tip to $dest verdict=prebuilt_push_failed"
        fi
      done
      last="$tip"
    else
      say "WARN build of $tip failed after $(( $(date +%s) - t0 ))s: $(tail -3 "$STATE/build.log" | tr '\n' ' ' | cut -c1-300) verdict=prebuilt_build_failed"
      last="$tip"     # do not retry the same commit every poll; the next push tries again
    fi
  fi
  sleep "$POLL"
done
