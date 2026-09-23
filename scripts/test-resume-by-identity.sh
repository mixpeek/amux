#!/usr/bin/env bash
# shellcheck disable=SC1090,SC1091
# AMUX-5033. `claude --continue` resumes the NEWEST conversation in the working
# directory, so in a shared CC_DIR a lane adopts whichever peer started last.
#
# Measured 2026-09-23: `amux-gs-4-gke-minimization`'s pane was driving
# `gs-10-zero-base-cicd`'s conversation. They started 9 seconds apart in the
# same directory, which 37 lanes share. A message addressed to gs-4 was
# executed by gs-10, and gs-4's peek read a third transcript 22.7 hours stale.
#
# Drives the SHIPPED helpers by sourcing `amux`, so this tests the bytes the
# CLI runs rather than a restatement of them.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CLI="$ROOT/amux"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP:?}"' EXIT

CELLS=0; FAILED=0
ok()   { CELLS=$((CELLS+1)); echo "  ok    $1"; }
bad()  { CELLS=$((CELLS+1)); FAILED=$((FAILED+1)); echo "  FAIL  $1"; }
is()   { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1 (wanted '$3', got '$2')"; fi; }

export HOME="$TMP/home"
mkdir -p "$HOME/.amux/sessions" "$HOME/.claude/projects/-shared"
# shellcheck source=/dev/null
source "$CLI" >/dev/null 2>&1 || true
CC_SESSIONS="$HOME/.amux/sessions"

mkenv()  { printf 'CC_DIR="%s"\n' "$2" > "$CC_SESSIONS/$1.env"; }
mkmeta() { printf '{"cc_conversation_id":"%s"}\n' "$2" > "$CC_SESSIONS/$1.meta.json"; }

# ── the shared-directory predicate ─────────────────────────────────────────
mkenv alpha /work/shared
mkenv beta  /work/shared
mkenv gamma /work/alone

if _workdir_shared_with_another_lane alpha /work/shared; then ok "a directory two lanes declare reads as shared"
else bad "a directory two lanes declare reads as shared"; fi

if _workdir_shared_with_another_lane gamma /work/alone; then bad "a directory only one lane declares must NOT read as shared"
else ok "a directory only one lane declares is not shared"; fi

# A lane must not count ITSELF as the peer. Without the `$other == $me` skip
# every lane would read as shared and nothing would ever resume.
rm -f "$CC_SESSIONS/beta.env"
if _workdir_shared_with_another_lane alpha /work/shared; then bad "a lane alone in its directory must not match itself"
else ok "a lane does not count itself as the peer"; fi
mkenv beta /work/shared

# A TRAILING SLASH IS THE SAME DIRECTORY. In the fleet `/Users/ethan/Dev/amux`
# carries 8 lanes and `/Users/ethan/Dev/amux/` another 5; Claude Code's project
# encoding maps both to one place, so a verbatim compare would call those 13
# co-tenants two unshared groups and `--continue` every one of them.
# DELTA MUST BE THE ONLY CANDIDATE for the first cell, or `beta` (an exact
# match) satisfies it and the cell passes without exercising the normalization
# at all. Measured: with beta present, mutating EITHER half of the strip left
# this cell green and only the second one reddened, which is the A-or-B trap
# ethos rule 7 names.
rm -f "$CC_SESSIONS/beta.env"
mkenv delta "/work/shared/"
if _workdir_shared_with_another_lane alpha /work/shared; then ok "a PEER's trailing slash still counts as the same directory"
else bad "a peer's trailing slash was treated as a different directory"; fi
rm -f "$CC_SESSIONS/delta.env"
mkenv beta /work/shared
if _workdir_shared_with_another_lane alpha "/work/shared/"; then ok "OUR OWN trailing slash still matches a peer without one"
else bad "our own trailing slash was treated as a different directory"; fi

# ── the conversation-id lookup ─────────────────────────────────────────────
is "no meta file yields no id"          "$(_lane_conversation_id alpha)" ""
mkmeta alpha ""
is "an EMPTY id yields no id"           "$(_lane_conversation_id alpha)" ""
mkmeta alpha "conv-that-does-not-exist"
is "an id with no transcript is refused" "$(_lane_conversation_id alpha)" ""
# THE POINT OF THE REFUSAL ABOVE: `claude --resume <missing>` fails, and a
# launch that silently does something else is the bug being fixed.
: > "$HOME/.claude/projects/-shared/conv-real.jsonl"
mkmeta alpha "conv-real"
is "an id whose transcript exists is used" "$(_lane_conversation_id alpha)" "conv-real"

# ── the shipped decision, asserted against the launch block's own bytes ────
blk=$(awk '/RESUME BY IDENTITY WHEN THE DIRECTORY IS SHARED/,/^    fi$/' "$CLI")
if [[ -z "$blk" ]]; then
  echo "FAIL: could not find the resume block in $CLI; this test cannot run"
  exit 1
fi
case "$blk" in
  *'cmd="$cmd --resume $_own_conv"'*) ok "an own id becomes --resume, which cannot adopt a peer's" ;;
  *) bad "the block no longer resumes by id" ;;
esac
case "$blk" in
  *'_workdir_shared_with_another_lane'*) ok "the shared-directory case is consulted" ;;
  *) bad "the block no longer asks whether the directory is shared" ;;
esac
case "$blk" in
  *'AMUX-5033'*) ok "starting fresh instead of resuming is announced, not silent" ;;
  *) bad "the shared-directory branch says nothing; a silent choice here is what hid this" ;;
esac
# `--continue` must SURVIVE for the unambiguous case. INIT-3: without it a
# stop-all/start-all became 8 fresh untitled sessions.
case "$blk" in
  *'cmd="$cmd --continue"'*) ok "--continue is still used where recency is unambiguous" ;;
  *) bad "--continue was removed entirely; INIT-3 is the reason it must stay" ;;
esac

echo
if [[ "$FAILED" -eq 0 ]]; then echo "PASS ($CELLS outcome cells)"; exit 0; fi
echo "FAIL ($FAILED of $CELLS outcome cells)"; exit 1
