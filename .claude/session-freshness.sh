#!/bin/bash
# SessionStart hook: say out loud, at the one moment it matters, whether this
# session is about to build on something stale.
#
# Two DIFFERENT staleness axes bit in one session on 2026-08-05, which is why
# this checks both:
#
#   1. The CHECKOUT was ~110 commits behind origin/main. Work got built on a
#      stale base; one fix turned out to duplicate a fix upstream already had,
#      and the rebase that followed conflicted twice.
#   2. The INSTALLED CLI (~/.local/bin/amux) was a Jul-31 copy missing the
#      `status-update` verb. It fell through to help and exited 0, so three of
#      the owner's status requests were silently swallowed (AMUX-2140 shape).
#
# Design constraints, each one deliberate:
#
#   * It FETCHES, never pulls. This is a shared checkout — CLAUDE.md records a
#     peer's `git pull --rebase` replaying another session's unpushed commit
#     onto origin. A hook that rewrites the working tree can destroy in-flight
#     work belonging to a session that is not even running right now. Report
#     and recommend; the human decides (ethos rule 8).
#   * It FAILS OPEN. Offline, no remote, detached HEAD, missing files — every
#     failure path exits 0 silently. A freshness check that blocks a session is
#     worse than the staleness it detects.
#   * It is SILENT when everything is current, so the one time it speaks is
#     signal rather than another banner to scroll past.
set -uo pipefail

[ "${AMUX_SKIP_FRESHNESS:-}" = "1" ] && exit 0

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." 2>/dev/null && pwd -P)" || exit 0
cd "$REPO" 2>/dev/null || exit 0
git rev-parse --git-dir >/dev/null 2>&1 || exit 0

out=""

# ── Axis 1: is the checkout behind its remote? ───────────────────────────────
# Bounded: a hook that hangs on a slow network is a hook that gets deleted.
if git remote get-url origin >/dev/null 2>&1; then
  timeout 10 git fetch -q origin 2>/dev/null
  base="$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || echo origin/main)"
  if git rev-parse --verify -q "$base" >/dev/null 2>&1; then
    behind="$(git rev-list --count "HEAD..$base" 2>/dev/null || echo 0)"
    ahead="$(git rev-list --count "$base..HEAD" 2>/dev/null || echo 0)"
    if [ "${behind:-0}" -gt 0 ]; then
      # Name the files that actually matter here, not just a number: "110
      # commits behind" reads as bookkeeping, "crates/ changed upstream"
      # reads as "your edit is going to conflict".
      #
      # THREE dots, and the distinction is the whole point of this line. In
      # `git diff`, two dots compare the two ENDPOINTS — so on a shared checkout
      # carrying unpushed work it reports OUR OWN files as upstream changes.
      # Measured 2026-08-09 (python era): 1 commit behind touching only the
      # server file, and the two-dot form also named `CLAUDE.md amux`, sending
      # the session to reconcile two files upstream had never touched. Three dots
      # diff from the merge-base, i.e. exactly "what $base added that I lack".
      # Note line 43 is correct as-is: two-dot rev-list already means that.
      # The bug was that one sentence mixed both conventions, so its count and
      # its file list disagreed — and it degraded precisely as the checkout got
      # busier, which is when the warning matters most.
      hot="$(git diff --name-only "HEAD...$base" 2>/dev/null \
             | grep -E '^(crates/|Cargo\.(toml|lock)$|amux$|CLAUDE\.md$)' \
             | head -6 | tr '\n' ' ')"
      out+="  - checkout is ${behind} commit(s) behind ${base}"
      [ -n "$hot" ] && out+=" — including: ${hot}"
      out+=$'\n'
      # `--rebase` REWRITES every unpushed commit, and on a shared checkout most
      # of them are not yours (AF-95: 28 unpushed across ~15 lanes). Recommend it
      # only when there is nothing local to rewrite.
      if [ "${ahead:-0}" -gt 0 ]; then
        out+=$'    git merge origin/main   (NOT --rebase: it rewrites the '"${ahead}"$' unpushed commit(s) here, not all of them yours)\n'
      else
        out+=$'    git pull --rebase origin main   (review first: this checkout is SHARED)\n'
      fi
    fi

    # ── Axis 1b: DIVERGED, so append-only shared files written here strand ───
    #
    # Behind alone is recoverable: pull, and your work still reaches origin.
    # Behind AND ahead means no fast-forward, so the hourly sync job refuses
    # (correctly — it must never rewrite a shared tree) and the append strands.
    #
    # This block used to say "until a HUMAN reconciles it" and to send the
    # session off to a clone that is current. Both were wrong on the canonical
    # checkout, which is where most lanes read this (AF-95, 2026-08-19): a plain
    # `git merge origin/main` reconciled 28-ahead/8-behind with no human, no
    # rewritten SHA, and one conflicted file; and "go log in a current clone" is
    # unfollowable when this IS the only clone — ethos rule 3, a constraint with
    # no honest path forward, shipped by the hook that exists to prevent a
    # different version of the same loss.
    #
    # Merge, not rebase, is the recommendation on purpose: a merge preserves
    # every lane's SHA and `--abort` restores exactly, so it is safe for one lane
    # to run unilaterally on a tree it shares with fifty others. Nothing here
    # PUSHES — that stays the human's (ethos rule 8), and the Deploy section of
    # CLAUDE.md records why.
    #
    # The reason this is worth its own line rather than folding into the count
    # above: the failure is SILENT AND WRITE-SHAPED. A stale checkout announces
    # itself the moment you pull. Appending to `frustrations.md` here succeeds,
    # prints nothing, and reaches nobody. On 2026-08-17 four entries went in
    # this way — that copy held 25 entries while origin held 124 — and it was
    # noticed only by chance, days later (AEAB-18).
    #
    # Names frustrations.md specifically because it is the append-only shared
    # file a session is INSTRUCTED to write on its own initiative, so it is the
    # one that gets stranded without anybody choosing to risk it.
    if [ "${behind:-0}" -gt 0 ] && [ "${ahead:-0}" -gt 0 ]; then
      out+="  - this checkout has DIVERGED (${ahead} unpushed, ${behind} behind ${base})"$'\n'
      out+=$'    it cannot fast-forward, so an append here reaches nobody until it is reconciled\n'
      out+=$'    RECONCILE IT: git merge origin/main   (rewrites no SHAs; abort is clean)\n'
      out+=$'    then append. If this is the only checkout, reconciling IS the remedy —\n'
      out+=$'    there is no current clone to go log in (.claude/rules/frustrations.md)\n'
    fi
  fi
fi

# ── Axis 1c: was the RUNNING SERVER built from an unmerged revision? ─────────
#
# AEAB-12. The rust auto-builder rebuilds whenever the BUILD SOURCE's local HEAD
# moves, on any branch, and the server self-adopts within 5s. That permissiveness
# is deliberate — this machine has survived weeks pinned to an unmerged fix
# branch — but it means a commit made on a feature branch inside the build source
# is serving the whole fleet about a minute later, with no CI and no review.
#
# On 2026-08-17 that ran for 9h42m unnoticed, while the same condition also
# stopped the machine tracking upstream, so the daily update schedule silently did
# nothing. Nothing looked wrong anywhere.
#
# The builder records what it installed; this reads it. Deliberately a REPORT, not
# a refusal, and not a re-derivation: only the builder knows which tree it built,
# and a second implementation guessing at it would be the drift this file keeps
# finding. Fails open like everything else here — no file, unreadable, or garbage
# means silence.
PROV="${AMUX_RS_BUILD_PROVENANCE:-$HOME/.amux/rust-build-provenance.json}"
if [ -r "$PROV" ]; then
  prov_on_main=$(sed -n 's/.*"on_main":"\([^"]*\)".*/\1/p' "$PROV" 2>/dev/null)
  prov_ref=$(sed -n 's/.*"ref":"\([^"]*\)".*/\1/p' "$PROV" 2>/dev/null)
  prov_sha=$(sed -n 's/.*"sha":"\([^"]*\)".*/\1/p' "$PROV" 2>/dev/null)
  if [ "${prov_on_main:-yes}" = "no" ]; then
    out+="  - the RUNNING SERVER was built from an unmerged revision: ${prov_sha:0:9} on '${prov_ref:-?}'"$'\n'
    out+=$'    it is the live build for the whole fleet, with no CI and no review behind it\n'
    out+=$'    intentional pin? fine. accident? put the build source back on main —\n'
    out+=$'    develop in a git worktree, never in the checkout the builder watches\n'
  fi

  # ── Axis 1d (AEAB-32): is the RUNNING BUILD behind origin/main? ────────────
  #
  # `on_main` above catches a build off a BRANCH. It says nothing about the more
  # common and more dangerous case: the build is on main, on_main reads `yes`,
  # and main has simply moved on without it. Both look identical in the
  # provenance file, so the one instrument that exists here could not express
  # the difference.
  #
  # It matters because MERGING IS NOT DEPLOYING. The builder rebuilds when the
  # build source's LOCAL HEAD moves, and it deliberately never fetches — it runs
  # on a 60s timer and must not touch the network, which is a decision recorded
  # in rust-auto-build.sh and left alone here. So a merged fix reaches the fleet
  # only when somebody advances that checkout by hand. Measured twice:
  # `423dd00c` fixed a live worker panic at 2026-08-13 22:15 and the fleet ran
  # the panicking binary until 09:11 the next morning (39 panics fired inside
  # that window); and on 2026-08-18 a merged PR did not reach the fleet until
  # the build source was moved off the feature branch by hand.
  #
  # This hook is the right place for it precisely because it ALREADY fetched
  # above — the number costs nothing here and would cost a network call per
  # minute in the builder. Report, never act: nothing below touches a tree.
  if [ -n "${prov_sha:-}" ] && git rev-parse --verify -q origin/main >/dev/null 2>&1; then
    if git cat-file -e "${prov_sha}^{commit}" 2>/dev/null; then
      # Count only commits the BUILDER would act on. The pathspec is copied
      # from rust-auto-build.sh's own trigger (`log -1 -- crates/ Cargo.toml
      # Cargo.lock`) — a view must share the predicate of the mechanism it
      # describes (ethos rule 1). Without it, a scripts-only or workflow-only
      # commit read as "the RUNNING SERVER is 1 behind ... a fix on main is
      # not a fix in prod", and sent a session reconciling a staleness that
      # did not exist (measured 2026-08-22: 2b7daf4, workflows+scripts only,
      # flagged as undeployed while the server was correctly current).
      built_behind="$(git rev-list --count "${prov_sha}..origin/main" -- crates/ Cargo.toml Cargo.lock 2>/dev/null || echo 0)"
      if [ "${built_behind:-0}" -gt 0 ]; then
        # Age of the OLDEST commit the running build is missing. A count alone
        # reads as bookkeeping; "and the oldest is 11 hours old" is the number
        # that says whether a fix has been sitting undeployed overnight.
        # `--reverse | head -1`, NOT `-1 ... | tail -1`. The `-1` limits git to ONE
        # commit — the NEWEST — so `tail -1` never sees a second line and the
        # "oldest" figure was always the newest. Measured 2026-08-20: it printed
        # "3 minutes ago" while the oldest undeployed commit was 23 HOURS old.
        # This line exists specifically to distinguish "just merged, builder is
        # about to pick it up" from "a fix has been sitting undeployed overnight",
        # and the second case is the one it could never show. A bare count reads
        # as bookkeeping; the age is what makes it actionable, so an age that is
        # always small makes the whole line reassuring noise.
        oldest="$(git log --reverse --format=%cr "${prov_sha}..origin/main" -- crates/ Cargo.toml Cargo.lock 2>/dev/null | head -1)"
        out+="  - the RUNNING SERVER is ${built_behind} commit(s) behind origin/main"$'\n'
        out+="    built ${prov_sha:0:9}; oldest undeployed commit landed ${oldest:-?}"$'\n'
        out+=$'    merging does NOT deploy — the build source advances only when someone\n'
        out+=$'    moves it. A fix on main is not a fix in prod until then.\n'
      fi
    else
      # An unknown sha is INFORMATIVE, not a parse failure to swallow: it means
      # the fleet is running a commit that never reached origin at all.
      out+="  - the RUNNING SERVER's build ${prov_sha:0:9} is not a commit this checkout knows"$'\n'
      out+=$'    it never reached origin — nothing upstream contains what is running\n'
    fi
  fi
fi

# ── Axis 2: does what is INSTALLED match this checkout? ──────────────────────
# The repo copy is the source; install.sh copies it. Editing the repo alone
# changes nothing that a session or the dashboard actually executes.
live_cli="$(command -v amux 2>/dev/null || true)"
if [ -n "$live_cli" ] && [ -f "$REPO/amux" ]; then
  if ! diff -q "$REPO/amux" "$live_cli" >/dev/null 2>&1; then
    out+="  - installed CLI differs from this checkout: ${live_cli}"$'\n'
    out+="    an unknown verb there may print help and exit 0 — a silent no-op"$'\n'
    out+="    cp \"$REPO/amux\" \"$live_cli\""$'\n'
  fi
fi

# ── Axis 2b: are the INSTALLED GIT HOOKS the ones in this checkout? ──────────
#
# Same failure as the CLI axis directly above, one layer down and with worse
# consequences, because a hook's staleness is INVISIBLE: a missing verb at least
# prints help, while a missing guard just... does not guard, and looks exactly
# like nothing being wrong.
#
# Measured 2026-08-23. Every checkout on this machine — ~/amux,
# ~/Developer/amux and ~/Projects/amux-gtm — had `.git/hooks/pre-commit` dated
# Aug 5, eighteen days old, while `scripts/git-hooks/` was current. `guard_version`
# appeared 0 times in the installed hooks and 3 times in the repo's. And
# `.git/hooks/pre-push` never called `append-only-push-guard` at all, so the
# guard added to stop a stale republish silently reverting pushed entries in
# frustrations.md (MG-1483, 10 entry-lines lost) had never run here.
#
# NOTHING WAS WATCHING FOR THIS, which is the point of the axis and is worth
# stating precisely, because I first got it wrong. The server does log
# "[staged-guard] OUTDATED HOOK: <session> in <repo> sent no guard_version ...
# Reinstall: scripts/install-hooks.sh" — 128 times over 8 days here — and it
# reads exactly like a file-staleness detector. It is not one. git_guard.rs sets
# it from the REQUEST BODY:
#
#     let guard_version = obj.get("guard_version").as_i64().unwrap_or(0);
#     let hook_outdated = guard_version < 2;
#
# so any caller that omits the field is "outdated" by construction, and
# git-shared-guard.py omits it on most of its posts. It discriminates the CALLER,
# not the file (amux-frustrations, AF-156, who found this and were right). Its
# remedy is also unwalkable — reinstalling installs the same source that omits
# the field, so the warning returns immediately, which is AMUX-2140's shape.
#
# So a CONTENT diff is not a second spelling of that flag; it is the check that
# did not exist. Bytes are the only thing that can distinguish "this file is not
# the one in the checkout" from "this caller did not say who it was", and only
# the first is what a session needs to know at start. I cited that log line as
# evidence for file staleness before reading the code that emits it — the
# staleness was real and independently measured (mtimes, and
# append-only-push-guard absent entirely), but the flag was never evidence for
# it. A message that names a plausible cause is not a measurement of it.
#
# `git rev-parse --git-path hooks` rather than "$REPO/.git/hooks": in a git
# WORKTREE `.git` is a file, not a directory, and the naive path does not exist —
# so a hardcoded one would report nothing in exactly the checkouts AEAB-26 says
# the guard is already blind in, i.e. it would be silent where it is needed most.
hooks_dir="$(git -C "$REPO" rev-parse --git-path hooks 2>/dev/null || true)"
case "$hooks_dir" in /*) : ;; ?*) hooks_dir="$REPO/$hooks_dir" ;; esac
if [ -n "${hooks_dir:-}" ] && [ -d "$REPO/scripts/git-hooks" ]; then
  stale_hooks=""
  for src in "$REPO"/scripts/git-hooks/*; do
    [ -f "$src" ] || continue
    name="$(basename "$src")"
    # post-commit and the shared library are installed under other names or not
    # at all by install-hooks.sh; only flag what that script actually places, or
    # this axis nags forever about files it has no remedy for.
    case "$name" in pre-commit|pre-push|prepare-commit-msg|amux-staged-guard) ;; *) continue ;; esac
    dst="$hooks_dir/$name"
    if [ ! -e "$dst" ]; then
      stale_hooks="${stale_hooks}${stale_hooks:+, }${name} (MISSING)"
    elif ! diff -q "$src" "$dst" >/dev/null 2>&1; then
      stale_hooks="${stale_hooks}${stale_hooks:+, }${name}"
    fi
  done
  if [ -n "$stale_hooks" ]; then
    out+="  - installed git hooks differ from this checkout: ${stale_hooks}"$'\n'
    out+=$'    a stale guard does not announce itself — it just stops guarding\n'
    out+="    ./scripts/install-hooks.sh"$'\n'
  fi
fi

# The RUNNING server's freshness is the builder's job, not this hook's:
# com.amux.server-rs-builder rebuilds COMMITTED rust source every 60s and the
# server self-adopts the new binary. A file diff cannot compare a binary to a
# source tree, but /health's `build` hash names exactly which build answers —
# so report only when the running server looks stale relative to the checkout.
if command -v curl >/dev/null 2>&1; then
  hs="$(timeout 5 curl -sk https://localhost:8824/health 2>/dev/null || true)"
  if [ -n "$hs" ] && ! printf '%s' "$hs" | grep -q '"server":"amux-rust"'; then
    out+="  - https://localhost:8824/health is answering but not as amux-rust — check com.amux.server-rs"$'\n'
  fi
fi

[ -z "$out" ] && exit 0

printf 'amux freshness — this session may be building on something stale:\n\n%s\n' "$out"
printf 'Reconcile before starting work, or say so in your first message if you are deliberately not.\n'
exit 0
