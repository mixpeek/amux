---
description: Repo/branch/file hygiene sweep — stray artifacts, doc drift, stale branches, worktrees, upstream sync, backup sprawl, duplicate build caches. Verifies before purging.
allowed-tools: Bash, Read, Edit, Write
argument-hint: [dry-run|full] (default: full)
---

# /cleanup — amux file cleanup & consolidation routine

A repeatable hygiene sweep for this repo and this box's `~/.amux` state.
Built from a real cleanup session (2026-08-30) that found: a 37MB binary
accidentally committed, a security-sensitive file (`CLAUDE.local.md`)
pushed to a public fork, a live systemd script left untracked, a doc file
independently drifting in **three** places, 6 stale branches, 2 worktrees
for merged PRs, 17 accumulated backup-file generations, and — in a
follow-up pass the same day — 5.9GB of pure disk waste: duplicate cargo
`target/` dirs in every PR worktree (the shared-target config is
gitignored, so worktrees never inherit it) plus stale manual binary
backups nobody cleaned up. None of that was hypothetical — every category
below is something that actually happened here, not a theoretical
checklist.

**Golden rule, stated explicitly because it was tested and mattered:**
verify before you purge. "Looks like a duplicate" and "confirmed
byte-for-byte subset with a diff/comm check" are different claims — only
act on the second. If a branch or file MIGHT hold unique value, flag it
and stop; don't delete on a hunch. (The user's own framing: "old stuff can
be purged but always double check.")

---

## 1. Tracked-file hygiene (repo)

```bash
# Find large tracked blobs that don't belong (compiled binaries, dumps)
git ls-files -z | xargs -0 du -b 2>/dev/null | sort -rn | head -20

# Cross-check: does anything live/running depend on a file that's
# UNTRACKED? (systemd ExecStart, cron, launchd plist — the audit trail
# must be real, ethos rule 6)
systemctl --user list-units --all 2>/dev/null | grep -oP '(?<=ExecStart=)\S+' 2>/dev/null
# or: systemctl --user cat <unit> | grep ExecStart
# then: git ls-files <that path> — empty means a fresh clone breaks
```

**Security check — do this every time, not just when something looks
wrong:** grep tracked files for names that match your own "never commit
this" conventions (`CLAUDE.local.md`, `*.env`, `*secrets*`, `*credentials*`,
anything your `.gitignore` comments call out as sensitive) and confirm
`.gitignore` actually covers them:

```bash
git ls-files | grep -iE 'local\.md$|\.env$|secret|credential|\.key$|\.pem$'
# for each hit, check: is it supposed to be here?
git check-ignore -v <path>   # empty = NOT ignored, even if it should be
```

If something sensitive is tracked and already pushed: `git rm --cached` +
add to `.gitignore` + commit stops it from being carried forward. That
does **not** scrub it from history on a remote it already reached — flag
that explicitly and let the human decide about a history rewrite
(rewriting a shared/in-review branch is disruptive; not a unilateral call).

**After adding a gitignore entry, verify the COMMIT, not the working
tree.** `git show HEAD:.gitignore | tail` — not `tail .gitignore`. Editing
the file and reporting "added to .gitignore" without an explicit `git add
.gitignore` is a real, repeatable mistake (happened twice in the same
session here): the commit only contains what got staged, and a later
`git reset --hard` silently drops the uncommitted rest with no error at
all. `git status`'s summary line does not catch this when the same pass
also has a staged deletion — check the actual diff that's about to be
committed (`git diff --cached --stat`), not just that *something* is
staged.

**If the human authorizes a history rewrite, scope it to exactly the
affected commit range — do not process full history.** `git filter-repo`
run without `--refs` restriction processes every reachable commit and can
reassign NEW HASHES even to commits whose tree never changed (its default
merge-simplification touches the whole graph) — this silently moves the
branch's merge-base with `main`, and a locally-clean rewrite can still
turn into spurious PR conflicts that have nothing to do with the actual
fix. `git filter-branch --index-filter '...' -- <bad-commit>^..HEAD`
(note the revision range after `--`) only touches commits in that range;
everything before it, including the true shared ancestor with `main`,
keeps its original hash. **Verify before pushing, every time:**
```bash
# merge-base must be IDENTICAL before and after the rewrite
git merge-base <old-ref> origin/main
git merge-base <rewritten-ref> origin/main
# and a real merge test should show 0 conflicts against the SAME origin/main
# snapshot the old branch was tested against (it may have moved since —
# check git log <old-merge-base>..origin/main for genuinely new, unrelated
# conflicts before blaming the rewrite for something upstream drift caused)
git merge-tree $(git merge-base <rewritten-ref> origin/main) <rewritten-ref> origin/main | grep -c '^<<<<<<<'
```
Tag the pre-rewrite tip locally (never pushed) before starting, so a wrong
rewrite has a clean recovery path.

## 2. Doc/skill drift

The same fact living in more than one file is checked, not assumed —
this codebase's own rule ("never a second copy of the same fact") applies
to prose as much as code:

```bash
# Any obviously-paired doc files (same basename, different dirs)?
find . -iname "amux.md" -o -iname "README.md" 2>/dev/null | grep -v node_modules
diff <(cat copy1) <(cat copy2)   # if near-identical, consolidate

# Is a user-level copy (~/.claude/commands/, ~/.claude/skills/) shadowing
# a project one for lanes whose CWD isn't this repo? Compare both:
diff ~/.claude/commands/<name>.md .claude/commands/<name>.md
```

**Fix at the root, not by re-syncing every time:** if two files are
supposed to always match, make one a symlink to the other so drift is
structurally impossible, not just corrected once. For a *repo* file with a
*user-level* fallback (different lanes, different CWDs), there's no
symlink across that boundary — sync the content once, and note in this
routine's own run that it needs a manual re-sync after future doc edits
(or better: script the sync and call it from here).

## 3. Branch hygiene

Never delete a branch on "looks old" alone — check real containment:

```bash
git fetch --prune origin   # do this per remote; multiple remotes in one
git fetch --prune fork     # `git fetch --prune r1 r2` silently fails —
git fetch --prune upstream # extra names are parsed as refspecs, not remotes

for b in $(git branch --format='%(refname:short)'); do
  if git merge-base --is-ancestor "$b" origin/main 2>/dev/null; then
    echo "$b: literal ancestor of origin/main — safe to delete"
  else
    ahead=$(git rev-list --count origin/main.."$b" 2>/dev/null)
    echo "$b: NOT an ancestor (ahead by $ahead) — check further before touching"
  fi
done
```

**A branch can be safely mergeable and still not show as an ancestor** —
squash-merge rewrites the commit, so `merge-base --is-ancestor` returns
false even though the PR is fully merged. For anything that check flags
as "not an ancestor," check the PR's actual state before concluding
anything:

```bash
gh pr list --repo <org>/<repo> --state merged --head <branch> --json number,state
# if MERGED, confirm the squash commit really landed (not just metadata):
git log origin/main --oneline --grep="<something distinctive from the branch>"
```

For a branch that's genuinely ahead with unique commits, don't assume
they're either "definitely valuable" or "definitely redundant" — check:

```bash
git log origin/main..<branch> --oneline   # what's actually unique
# for each unique-looking commit, grep for its subject/symbol elsewhere:
git log --oneline --all --grep="<distinctive phrase>" -i
grep -rn "<the function/route/endpoint it adds>" <where it'd land if merged>
```

If it's real, unshipped work — don't delete it. Flag it by name with what
it contains, so a human can route it (cherry-pick, new PR, or explicit
"actually drop this").

## 4. Worktree hygiene

```bash
git worktree list
# for each, check the PR state (same command as above) — remove worktrees
# for MERGED or CLOSED PRs, keep worktrees for OPEN ones:
git worktree remove <path>
```

## 5. Diverged-but-same-content branches (rare, but real)

Sometimes two refs have commits with identical author+timestamp+message
but different tree content — a genuine divergence from a concurrent
session, not simple staleness. **Never force-push or blindly pick a side.**
Preview a real merge first:

```bash
git merge --no-commit --no-ff <other-ref>
# then VERIFY it's lossless, don't just trust "no conflicts":
comm -23 <(git show <other-ref>:<file> | sort) <(sort <file>)
# empty output = other side was a strict subset, nothing lost
```

Only commit the merge (a real merge commit, never `--force`) once that
check comes back empty for every file that actually differed.

## 6. Push safety

If a push-guard or similar blocks on commits authored by a different
session/lane: that's a deliberate ownership check, not a bug. Don't
override it on your own judgment — surface exactly what's blocking and
who it's attributed to, and let the human decide. If they say to proceed
regardless of authorship, the guard's own error message names the escape
hatch (e.g. `AMUX_ALLOW_FOREIGN=1`) — use exactly that, not a broader
bypass.

## 7. Backup-file sprawl

Accumulation without discrimination (ethos rule 5) — multiple generations
of the same backup pattern (`file.bak-<date>`, `file.bak-<reason>`) piling
up with nothing ever pruning them:

```bash
# Keep the newest per basename, ARCHIVE (don't delete) the rest:
mkdir -p ~/.amux/backup-archive-$(date +%F)
for base in <list of base filenames with .bak-* variants>; do
  ls -1t ${base}.bak-* 2>/dev/null | tail -n +2 | xargs -I{} mv {} ~/.amux/backup-archive-$(date +%F)/
done
```

Archiving beats deleting for anything that might be someone's operational
checkpoint — the cost of keeping 17 small files is negligible; the cost of
deleting the one that mattered is not.

## 8. Build-artifact & disk-cache hygiene

A shared-build-cache setup (e.g. a gitignored `.cargo/config.toml` pinning
`target-dir` to one shared path — see CLAUDE.md's `CARGO_TARGET_DIR` rule)
only works where that gitignored file actually exists. **Worktrees don't
inherit gitignored files from the main checkout** — each is a separate
directory, so a build run inside one silently falls back to its own local
`./target` unless the config is copied there too. Confirmed cost: 1.4GB
across two PR worktrees, invisible until someone actually measured it.

```bash
# Does every worktree have the same build-cache pin as the main checkout?
for wt in $(git worktree list --porcelain | grep ^worktree | cut -d' ' -f2); do
  [ -f "$wt/.cargo/config.toml" ] || echo "$wt: MISSING shared build-cache config"
done
# fix (root cause, not just cleanup — do this before deleting the stray dir):
cp .cargo/config.toml <worktree>/.cargo/config.toml

# Find local target/ or node_modules/ dirs that duplicate a shared cache:
find . <worktree-paths> -maxdepth 2 -type d \( -name target -o -name node_modules \) -exec du -sh {} \;
# a target/ dir OLDER than the shared-cache config's own mtime is dead —
# nothing has written to it since the config started redirecting builds:
find <target-dir> -newer .cargo/config.toml | head -1   # empty = confirmed stale
```

**Manual build/deploy byproducts left in `~/.local/bin` (or wherever
binaries get manually swapped) are the same "forgot to clean up" pattern
as the `.new` file that ended up committed to git (category 1) — just
outside the repo, so nothing catches it automatically:**

```bash
ls -la ~/.local/bin/*.backup-* ~/.local/bin/*.new 2>/dev/null
# for each: is it the one actually running?
readlink -f /proc/<server-pid>/exe   # compare against the candidate's path
# not referenced anywhere and not the live binary -> safe to delete,
# it's a build output, fully regenerable from source
```

**A disk-usage `du` sweep needs dotfiles included, or the biggest
consumers hide entirely:** `du -sh ~/*` silently skips everything starting
with `.` — `~/.cargo`, `~/.rustup`, `~/.npm`, `~/.amux` and similar can be
the majority of usage and never show up. Use `du -sh ~/.[!.]* ~/*` (or
`du -sha` at the parent and sort) so hidden top-level dirs are actually in
the comparison, not silently absent from a "here's what's using space"
report.

**A health check's disk threshold can be correct in general and wrong for
THIS host.** If `/health` or similar reports `critical`/`warn` on an
absolute free-GB threshold, sanity-check the threshold against this
specific disk's TOTAL size before treating the reading as a cleanup
target: a threshold requiring e.g. 75GB free for "ok" is unreachable by
construction on a 35GB disk, no matter how clean it is. That's a
miscalibrated check for this host, not a disk problem — flag the
threshold itself rather than chasing a number that can never move into
the "ok" band; changing a shared health-check threshold that other hosts
in the fleet may rely on isn't a unilateral cleanup call either.

## 9. Report, don't sweep

End every run with a clear before/after: what was deleted (with the
verification that made it safe), what was flagged instead (with why it
wasn't safe to touch), and what's still open pending a human decision.
Ethos rule 8: report and recommend, don't silently decide for someone
else's data.

---

## Instructions

`$ARGUMENTS`: `dry-run` reports every finding above without deleting,
moving, or committing anything — use this for the first pass, or whenever
asked to "check" rather than "clean up". `full` (default) executes the
safe categories (1, 2 via symlink, 3–4 once verified, 6–8) and stops to
ask before anything in category 5, a history rewrite in category 1, or
anything flagged as ambiguous in category 3.

Always finish with the category-9 report, even on a dry run.
