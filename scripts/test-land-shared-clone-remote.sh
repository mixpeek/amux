#!/usr/bin/env bash
# test-land-shared-clone-remote.sh [amux]: a land from a `git clone --shared`
# scratch clone, whose origin is a local checkout with a stale main, compares
# and pushes against the canonical remote. Before the fix the conflict check
# fetched the stale checkout and refused with "conflicts with origin/main" on
# a commit built on the canonical main (mxp-mongo-migration, 2026-11-26).
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/lsc.XXXXXX")" && pwd -P)"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
id() { git config user.email t@t; git config user.name t; }
git init -q --bare --initial-branch=main canon.git
git clone -q canon.git checkout 2>/dev/null
(cd checkout && id && echo a > f && git add f && git commit -qm base && git push -q origin HEAD:main 2>/dev/null)
# main moves on the canonical remote; the checkout never fetches it
git clone -q canon.git peer 2>/dev/null
(cd peer && id && echo b > f && git commit -qm peer -- f && git push -q origin HEAD:main 2>/dev/null)
# the checkout also holds a local edit of f that never reached the remote, so
# its stale main conflicts with the lane's commit where the canonical one does not
(cd checkout && echo x > f && git commit -qm local -- f)
git clone -q --shared checkout scratch 2>/dev/null; cd scratch || exit 1; id
# the lane builds on the canonical main, as the reporting lanes did
git fetch -q "$H/canon.git" main && git reset -q --hard FETCH_HEAD
echo c > f; git commit -qm change -- f
fail=0
HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lsc timeout 120 bash "$AM" land --no-gate --sha HEAD > "$H/out" 2>&1; rc=$?
[ "$rc" = 0 ] && echo "ok   the land succeeded" || { echo "FAIL land rc=$rc"; sed 's/^/     /' "$H/out"; fail=1; }
grep -q 'conflicts with' "$H/out" && { echo "FAIL a false conflict against the stale checkout"; fail=1; } || echo "ok   no false conflict"
[ "$(git -C "$H/canon.git" log -1 --format=%s main)" = change ] && [ "$(git -C "$H/canon.git" show main:f)" = c ] \
  && echo "ok   the canonical main carries the commit" || { echo "FAIL canonical main is $(git -C "$H/canon.git" log -1 --format='%h %s' main)"; fail=1; }
[ "$(git -C "$H/checkout" log -1 --format=%s main)" = local ] && echo "ok   nothing was pushed into the local checkout" || { echo "FAIL the local checkout's main moved"; fail=1; }
[ "$(git remote get-url origin)" = "$H/canon.git" ] && echo "ok   origin now names the canonical remote" || { echo "FAIL origin is $(git remote get-url origin)"; fail=1; }
grep -q 'verdict=land_remote_canonicalized' "$H/.amux/logs/land.log" && echo "ok   land.log says why" || { echo "FAIL no verdict in land.log"; fail=1; }
# a plain clone of a bare repository keeps its local origin
cd "$H/peer" || exit 1
HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lsc timeout 60 bash "$AM" land --no-gate > /dev/null 2>&1
[ "$(git remote get-url origin)" = "$H/canon.git" ] && echo "ok   a bare local origin is left alone" || { echo "FAIL peer origin changed"; fail=1; }
cd / && rm -rf -- "${H:?}"
exit $fail
