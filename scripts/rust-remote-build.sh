#!/usr/bin/env bash
# Build amux-server --release on a remote Docker host instead of locally.
# Drop-in replacement for rust-auto-build.sh's own `cargo build --release`
# step: same inputs (a worktree of committed HEAD), same output contract
# (the binary lands at $INSTALL_TARGET on success; non-zero exit + a
# diagnostic on stdout/stderr on failure — the caller captures both exactly
# like it captures a local `cargo build`).
#
# Requires:
#   - AMUX_REMOTE_BUILD_HOST set (a `docker context` name already created
#     via `docker context create <name> --docker "host=ssh://user@host"` —
#     this script does not create the context itself, since that needs a
#     hostname/user, which is machine-specific and does not belong in this
#     public repo; see CLAUDE.local.md for this box's own value).
#   - `amux-rust-base` already built on that context (Dockerfile.rust-base,
#     see its own header — a one-time/occasional setup step, not run here).
#   - `docker` on PATH locally, reaching the remote context over SSH.
#
# Never fails SILENTLY into "looked like it worked" — CLAUDE.local.md's own
# hard-won gotchas are the reason for the two checks this script does that
# a naive version would skip:
#   1. The build must be against the REAL worktree, not a fresh clone that
#      could silently fall back to the wrong branch — solved structurally
#      here by shipping the worktree itself as the docker build context,
#      never a `git clone` on the remote end.
#   2. `docker cp`/`docker run cat` out of a busy remote context can
#      truncate a large file with exit 0 — the binary is verified by
#      md5sum against the in-container copy before being trusted, not by
#      file size alone.
set -euo pipefail

WORK="${1:?usage: rust-remote-build.sh <worktree-dir> <output-binary-path>}"
OUT="${2:?usage: rust-remote-build.sh <worktree-dir> <output-binary-path>}"

HOST="${AMUX_REMOTE_BUILD_HOST:-}"
if [ -z "$HOST" ]; then
  echo "AMUX_REMOTE_BUILD_HOST not set — see this box's CLAUDE.local.md for the docker context name" >&2
  exit 1
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "docker not on PATH — remote build unavailable" >&2
  exit 1
fi

if ! docker --context "$HOST" info >/dev/null 2>&1; then
  echo "docker context '$HOST' unreachable (AMUX_REMOTE_BUILD_HOST) — see this box's" \
       "CLAUDE.local.md for known-flaky-link notes before assuming it's really down" >&2
  exit 1
fi

if ! docker --context "$HOST" image inspect amux-rust-base >/dev/null 2>&1; then
  echo "amux-rust-base image missing on context '$HOST' — run Dockerfile.rust-base's" \
       "own build command once (see its header) before this script can work" >&2
  exit 1
fi

TAG="amux-remote-build-$$"
cleanup() { docker --context "$HOST" rmi -f "$TAG" >/dev/null 2>&1 || true; }
trap cleanup EXIT

if ! docker --context "$HOST" build -t "$TAG" -f "$WORK/Dockerfile.rust-build" "$WORK"; then
  echo "remote docker build failed on context '$HOST'" >&2
  exit 1
fi

# Verify against the IN-CONTAINER copy's own md5sum before trusting the
# copied-out file — file size equality is close to sufficient but the
# checksum is what actually proves it (docker cp truncation, confirmed
# live once already on this exact remote-build pattern).
REMOTE_MD5=$(docker --context "$HOST" run --rm "$TAG" md5sum /build/target/release/amux-server | awk '{print $1}')
if [ -z "$REMOTE_MD5" ]; then
  echo "could not compute the remote binary's md5sum" >&2
  exit 1
fi

TMP_OUT=$(mktemp "${OUT}.XXXXXX")
if ! docker --context "$HOST" run --rm "$TAG" cat /build/target/release/amux-server > "$TMP_OUT"; then
  rm -f "$TMP_OUT"
  echo "could not extract the built binary from context '$HOST'" >&2
  exit 1
fi

LOCAL_MD5=$(md5sum "$TMP_OUT" | awk '{print $1}')
if [ "$LOCAL_MD5" != "$REMOTE_MD5" ]; then
  rm -f "$TMP_OUT"
  echo "extracted binary md5sum mismatch (remote=$REMOTE_MD5 local=$LOCAL_MD5) —" \
       "treating this as a failed build, not a truncated-but-usable one" >&2
  exit 1
fi

mv "$TMP_OUT" "$OUT"
chmod 0755 "$OUT"
echo "remote build OK on context '$HOST', md5=$LOCAL_MD5"
