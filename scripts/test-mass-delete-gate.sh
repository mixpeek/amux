#!/usr/bin/env bash
# Cells for the mass-deletion gate in the pre-commit hook (AMUX-3921).
#
# WHY THIS EXISTS. MHC-531 found 233 tracked files absent from the shared
# ~/Dev/mixpeek worktree, all 233 still on origin/main, zero legitimate
# deletions. Nothing was staged, so nothing was armed; one `git add -A` or
# `git commit -a` by any lane would have staged all 233 and pushed them,
# removing server tests, MVS files and two security baselines from origin.
#
# `git-shared-guard.py` already refuses those commands UNCONDITIONALLY (AF-316),
# which is stronger than the conditional leg the handoff asked for — but it is
# wired as a Claude Code Bash hook, so it only sees commands a harness session
# runs. This gate is in pre-commit, which runs for every invoker.
#
# THE CONTROL MATTERS MORE THAN THE HEADLINE: an ordinary `git rm` of a few
# files must still commit with no override. A gate that taxes normal work
# teaches lanes to set the override by reflex, which removes the protection it
# was added for.
#
# Drives the scratch repo with `git -C` rather than `cd`: the shared-checkout
# guard infers the repo from the working directory and cannot see a `cd` inside
# a compound command, so a scratch `git reset` reads as a reset of the real
# tree. Its own refusal names this form as the fix.
set -uo pipefail
cd "$(dirname "$0")/.."
HOOK="$(pwd)/scripts/git-hooks/pre-commit"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
no(){ FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
R="$TMP/repo"
G(){ git -C "$R" "$@"; }

git init -q "$R"
G config user.email t@t
G config user.name t
G config commit.gpgsign false
mkdir -p "$R/pkg"
for i in $(seq 1 40); do echo "x$i" > "$R/pkg/f$i.txt"; done
G add pkg
G commit -qm base
# The gate compares against origin/main; a local ref stands in for the remote.
G update-ref refs/remotes/origin/main HEAD
cp "$HOOK" "$R/.git/hooks/pre-commit"
chmod +x "$R/.git/hooks/pre-commit"

echo "mass-delete gate cells (AMUX-3921)"

# 1. THE HAZARD: many deletions of files origin still has.
G rm -q pkg/f1.txt pkg/f2.txt pkg/f3.txt pkg/f4.txt pkg/f5.txt pkg/f6.txt \
        pkg/f7.txt pkg/f8.txt pkg/f9.txt pkg/f10.txt pkg/f11.txt pkg/f12.txt \
        pkg/f13.txt pkg/f14.txt pkg/f15.txt pkg/f16.txt pkg/f17.txt pkg/f18.txt \
        pkg/f19.txt pkg/f20.txt
if out=$(G commit -qm "sweep" 2>&1); then
  no "a 20-file deletion must be refused" "commit succeeded"
else
  case "$out" in
    *"still exist on origin/main"*) ok "refuses a mass deletion and names the count" ;;
    *) no "refused for the wrong reason" "$out" ;;
  esac
fi

# 2. CONTROL, AND THE ONE THAT MATTERS: an ordinary removal still commits with
#    no override.
G reset -q
G rm -q pkg/f1.txt pkg/f2.txt
if G commit -qm "remove two dead files" >/dev/null 2>&1; then
  ok "an ordinary two-file removal passes with no override"
else
  no "a small deliberate removal must NOT need an override" "$(G status --short | head -3)"
fi

# 3. The override exists and is explicit, so the gate has a truthful path.
G rm -q pkg/f3.txt pkg/f4.txt pkg/f5.txt pkg/f6.txt pkg/f7.txt pkg/f8.txt \
        pkg/f9.txt pkg/f10.txt pkg/f11.txt pkg/f12.txt pkg/f13.txt pkg/f14.txt \
        pkg/f15.txt pkg/f16.txt pkg/f17.txt pkg/f18.txt pkg/f19.txt pkg/f20.txt \
        pkg/f21.txt pkg/f22.txt
if AMUX_ALLOW_MASS_DELETE=1 G commit -qm "deliberate mass removal" >/dev/null 2>&1; then
  ok "AMUX_ALLOW_MASS_DELETE=1 permits a deliberate mass removal"
else
  no "the override must work, or the gate has no truthful path" ""
fi

# 4. Deletions of files ORIGIN NEVER HAD are a local cleanup, not a loss.
#    Without this arm the gate would fire on any large scratch cleanup, which is
#    the false positive most likely to make someone disable it.
mkdir -p "$R/scratch"
for i in $(seq 1 30); do echo "s$i" > "$R/scratch/s$i.txt"; done
G add scratch
G commit -qm "add scratch, never pushed" >/dev/null 2>&1
G rm -q scratch/s1.txt scratch/s2.txt scratch/s3.txt scratch/s4.txt scratch/s5.txt \
        scratch/s6.txt scratch/s7.txt scratch/s8.txt scratch/s9.txt scratch/s10.txt \
        scratch/s11.txt scratch/s12.txt scratch/s13.txt scratch/s14.txt scratch/s15.txt \
        scratch/s16.txt scratch/s17.txt scratch/s18.txt scratch/s19.txt scratch/s20.txt
if G commit -qm "drop scratch" >/dev/null 2>&1; then
  ok "deleting 20 files origin never had is a local cleanup and passes"
else
  no "files absent from origin are nobody else's loss and must not be gated" ""
fi

# 5. NO origin/main -> the gate cannot compare, and must SAY so rather than
#    passing quietly. Without the notice a fetch-less checkout has no gate and
#    nothing distinguishes that from a clean pass (mixpeek-homepage-claude's
#    caution, taken; they hit the same silent-correct/silent-broken shape in
#    their uptime canary the same morning).
G update-ref -d refs/remotes/origin/main
G rm -q pkg/f23.txt pkg/f24.txt pkg/f25.txt pkg/f26.txt pkg/f27.txt pkg/f28.txt \
        pkg/f29.txt pkg/f30.txt pkg/f31.txt pkg/f32.txt pkg/f33.txt pkg/f34.txt \
        pkg/f35.txt pkg/f36.txt pkg/f37.txt pkg/f38.txt pkg/f39.txt pkg/f40.txt
out=$(G commit -qm "no origin" 2>&1)
case "$out" in
  *"not resolvable"*) ok "with no origin/main the gate says it is NOT protecting the commit" ;;
  *) no "an unusable gate must announce itself, not pass silently" "$out" ;;
esac

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
