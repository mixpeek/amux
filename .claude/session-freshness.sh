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
# EVERY copy, not just the winner (DESKT-22). `command -v` answers "which amux
# does THIS shell run", and the mechanism this axis describes is "which amux does
# a SESSION run" — which depends on that session's PATH, not on mine. A stale
# copy sitting in /usr/local/bin is invisible here whenever ~/.local/bin happens
# to come first, and shadows the real one for any lane ordered the other way.
# Measured 2026-08-24: /usr/local/bin/amux was an 18-day-old copy while this
# axis reported clean, because ~/.local/bin won on this session's PATH.
# The view must share the predicate of the mechanism it claims to describe.
live_cli="$(command -v amux 2>/dev/null || true)"
if [ -f "$REPO/amux" ]; then
  seen_cli=""
  for cand in $(command -v -a amux 2>/dev/null || true) /usr/local/bin/amux "$HOME/.local/bin/amux"; do
    [ -f "$cand" ] || continue
    case " $seen_cli " in *" $cand "*) continue ;; esac
    seen_cli="$seen_cli $cand"
    diff -q "$REPO/amux" "$cand" >/dev/null 2>&1 && continue
    # SYMLINK, NOT `cp`. This block used to prescribe a copy, which fixes the
    # report and rebuilds the hazard: a copy is stale again the next time anyone
    # edits ./amux, and that is exactly how the specimen below came to exist.
    # ~/.local/bin/amux has been a symlink since install.sh created it, so the
    # copy was also the only one of the two that could drift.
    #
    # Measured 2026-08-24: /usr/local/bin/amux was an Aug-6 227-line STUB that
    # knew two verbs and defaulted AMUX_URL to https://localhost:8822, the
    # retired port that no longer answers (AMUX-3046). A lane resolving it got
    # connection-refused on every call, or help-and-exit-0 on any verb outside
    # send/board. Eighteen days, reported by nobody.
    if [ "$cand" = "$live_cli" ]; then
      out+="  - installed CLI differs from this checkout: ${cand}  (THIS is the one you run)"$'\n'
      out+="    an unknown verb there may print help and exit 0 — a silent no-op"$'\n'
      out+="    ln -sfn \"$REPO/amux\" \"$cand\"   # a symlink cannot go stale; a copy can"$'\n'
    else
      out+="  - a SHADOWING amux copy differs from this checkout: ${cand}"$'\n'
      out+="    your PATH runs ${live_cli:-none} instead, so it is inert HERE and not"$'\n'
      out+="    for a lane whose PATH orders those directories the other way"$'\n'
      out+="    ln -sfn \"$REPO/amux\" \"$cand\"   # or remove it, if nothing should install there"$'\n'
    fi
  done
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
#
# AMUX_HEALTH_URL is a seam, not test-only scaffolding: a dev server on another
# port is a real case, and it is also what lets the cross-check below be tested
# against a `file://` fixture instead of whatever happens to be listening on the
# machine running the suite. A check whose verdict depends on the host's state is
# not testing the hook (the mistake test-session-freshness.sh already made once).
HEALTH_URL="${AMUX_HEALTH_URL:-https://localhost:8824/health}"
if command -v curl >/dev/null 2>&1; then
  hs="$(timeout 5 curl -sk "$HEALTH_URL" 2>/dev/null || true)"
  if [ -n "$hs" ] && ! printf '%s' "$hs" | grep -q '"server":"amux-rust"'; then
    out+="  - ${HEALTH_URL} is answering but not as amux-rust — check com.amux.server-rs"$'\n'
  fi

  # ── Axis 1e (AEAB-50 follow-up): does provenance agree with what is RUNNING? ─
  #
  # Everything above about the running build reads rust-build-provenance.json,
  # which is a CLAIM the builder makes. /health's `commit` is compiled into the
  # binary that answered, so it cannot be stale by construction. That asymmetry
  # is the whole point: this axis makes the reader independent of the builder
  # being honest.
  #
  # AEAB-50 fixed the builder — the file used to be written BEFORE the build and
  # nowhere else, so a failed build left it permanently asserting a deploy that
  # never happened. This catches the same shape from the other side, and it keeps
  # working for causes nobody predicted: a binary installed by hand, a file
  # edited, a future regression of exactly that bug. On 2026-08-23 provenance read
  # 9ef46b1f while both servers answered 23ddb8d1d91d, and nothing said so.
  #
  # PREFIX match anchored on the HEALTH value's length, because the two fields are
  # different widths — /health `commit` is 12 hex, provenance `sha` is 40. An
  # equality test would report a disagreement on literally every call: a detector
  # that always fires, which is the same defect as one that never can. A hardcoded
  # 12 would break silently the day the server widens the field, so the length
  # comes from the value itself.
  health_commit=$(printf '%s' "$hs" | sed -n 's/.*"commit":"\([0-9a-f]*\)".*/\1/p')
  if [ -n "${prov_sha:-}" ] && [ -n "$health_commit" ]; then
    case "$prov_sha" in
      "$health_commit"*) : ;;   # agree — say nothing
      *)
        out+="  - provenance and the RUNNING SERVER disagree about what is deployed"$'\n'
        out+="    provenance says ${prov_sha:0:12}; ${HEALTH_URL} answers ${health_commit}"$'\n'
        out+=$'    /health is compiled into the running binary and cannot be stale — believe it\n'
        out+=$'    provenance is a claim about the build; a build may be in flight or have failed\n'
        ;;
    esac
  fi
fi

# ── Axis 3: is THIS HOOK the current one? (AEAB-46) ─────────────────────────
#
# Every axis above lives inside the file most likely to be stale, and this hook
# is LOADED FROM THE CHECKOUT IT REPORTS ON — so the staler that checkout is, the
# less this can say about it, and a MISSING axis is indistinguishable from an
# axis that found nothing. The instrument is subject to the condition it measures.
#
# Not hypothetical. On 2026-08-22 a session started in ~/Developer/amux, 1278
# commits behind, and ran the Aug 5 copy of this file: `grep -c "RUNNING SERVER
# is"` returns 0 there, so the deploy axis did not exist and the banner never
# mentioned that the fleet was 23 commits behind. That was found by hand, four
# hours and five merged PRs into the session. Every axis added since makes it
# worse, because there is more that can be silently absent.
#
# The fetch above already happened, so comparing this file against origin's copy
# costs nothing extra. Content, not a commit count: a file can be stale without
# any commit touching it in this checkout's history, and bytes are what decide
# whether an axis is present.
self_path="${BASH_SOURCE[0]}"
if [ -r "$self_path" ] && git -C "$REPO" rev-parse --verify -q origin/main >/dev/null 2>&1; then
  if upstream_hook="$(git -C "$REPO" show origin/main:.claude/session-freshness.sh 2>/dev/null)" \
     && [ -n "$upstream_hook" ]; then
    if ! printf '%s\n' "$upstream_hook" | diff -q - "$self_path" >/dev/null 2>&1; then
      out+="  - THIS FRESHNESS HOOK is not the one on origin/main"$'\n'
      out+=$'    axes it does not have cannot warn you, and a missing axis is SILENT —\n'
      out+=$'    treat everything above as possibly incomplete rather than as all-clear\n'
      out+="    git -C $REPO diff origin/main -- .claude/session-freshness.sh"$'\n'
    fi
  fi
fi

[ -z "$out" ] && exit 0

printf 'amux freshness — this session may be building on something stale:\n\n%s\n' "$out"
printf 'Reconcile before starting work, or say so in your first message if you are deliberately not.\n'
# PROVENANCE FOOTER, printed only when the banner is ALREADY speaking. Silence
# stays silence — that is the whole reason this hook is worth reading — but any
# banner now names WHICH copy produced it, so a reader who sees three axes where
# the current hook has six can tell. That is rule 7's precondition applied to the
# instrument itself: before believing a negative, confirm the probe could have
# produced a positive. It also covers the case the axis above cannot: no origin
# to compare against, offline, or a detached checkout.
printf '(from %s)\n' "$self_path"
exit 0
