#!/usr/bin/env bash
# test-land-regenerate-reuse.sh [amux]: the regenerate step reuses one plain
# extract per repo and branch instead of a fresh TMPDIR tree per land
# (fseventsd escalation 20261007-030851). Lands in a row reuse it and push a
# fresh render; a generator sees no git repository (as with a fresh extract;
# d08ba296's git worktree broke exactly this); a tracked input a generator
# scribbled on is restored before the next render; nothing is extracted into
# TMPDIR; AMUX_LAND_REGEN_REUSE=0 still lands through the fresh extract.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lrr.XXXXXX")"; mkdir -p "$H/.amux/logs/land" "$H/tmp"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
mkdir gen; echo a > a.txt; echo keep > other.txt
# Renders a.txt, records whether git sees a repository, then scribbles on its input.
printf 'cksum < a.txt > gen/a.out\n(git rev-parse --is-inside-work-tree 2>/dev/null || echo none) > gen/git.out\necho scribble >> a.txt\n' > gen.sh
cksum < a.txt > gen/a.out; echo none > gen/git.out
git add a.txt other.txt gen.sh gen/a.out gen/git.out; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
git config --add amux.landRegenerable 'gen/*'
git config --add amux.landRegenerate 'sh gen.sh'
for p in a.txt gen.sh gen; do git config --add amux.landRegenerateInput "$p"; done
land() { HOME="$H" CC_HOME="$H/.amux" AMUX_HOME="$H/.amux" TMPDIR="$H/tmp" AMUX_LAND_REGEN_ROOT="$H/regen" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lrr "$@" bash "$AM" land --tries 1 --no-batch >/dev/null 2>&1; }
fail=0
git rebase -q origin/main 2>/dev/null; echo a2 > a.txt; git commit -qm a2 -- a.txt; land env; rc=$?; git fetch -q origin
[ "$rc" = 0 ] && [ "$(git show origin/main:gen/a.out)" = "$(echo a2 | cksum)" ] && echo "ok   the first land pushes a fresh render" || { echo "FAIL first land rc=$rc: $(git show origin/main:gen/a.out 2>&1)"; fail=1; }
# Second land changes only the generator, so a.txt is NOT a changed input: the
# render is right only if the scribble the first run left on a.txt was undone.
git rebase -q origin/main 2>/dev/null; echo '# v2' >> gen.sh; git commit -qm gen-v2 -- gen.sh; land env; rc=$?; git fetch -q origin
[ "$rc" = 0 ] && [ "$(git show origin/main:gen/a.out)" = "$(echo a2 | cksum)" ] && echo "ok   an input a generator scribbled on is restored before the next render" || { echo "FAIL second land rc=$rc: $(git show origin/main:gen/a.out 2>&1)"; fail=1; }
[ "$(git show origin/main:gen/git.out)" = none ] && echo "ok   the generator sees no git repository" || { echo "FAIL generator saw a repository: $(git show origin/main:gen/git.out)"; fail=1; }
[ "$(grep -ac 'verdict=land_regen_extract_reused' "$H/.amux/logs/land.log")" -ge 2 ] && echo "ok   the extract is reused across lands" || { echo "FAIL no reuse: $(grep -a 'regenerate' "$H/.amux/logs/land.log" | tail -3)"; fail=1; }
[ -z "$(ls -A "$H/tmp" | grep amux-land-regen)" ] && echo "ok   nothing is extracted into TMPDIR" || { echo "FAIL TMPDIR has $(ls "$H/tmp")"; fail=1; }
[ ! -e "$(ls -d "$H"/regen/*/ | head -1)other.txt" ] && echo "ok   the extract holds only the declared inputs" || { echo "FAIL other.txt in the extract"; fail=1; }
git rebase -q origin/main 2>/dev/null; echo a4 > a.txt; git commit -qm a4 -- a.txt; land env AMUX_LAND_REGEN_REUSE=0; rc=$?; git fetch -q origin
[ "$rc" = 0 ] && [ "$(git show origin/main:gen/a.out)" = "$(echo a4 | cksum)" ] && echo "ok   AMUX_LAND_REGEN_REUSE=0 lands through the fresh extract" || { echo "FAIL opt-out rc=$rc"; fail=1; }
cd / && rm -rf -- "${H:?}"
exit $fail
