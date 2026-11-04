<!-- Moved verbatim from root CLAUDE.md on 2026-10-30 (VISION.md A10). -->
## Deploy

**Before `git push origin main`:**
```bash
git fetch origin
scripts/push-consent.sh          # who must you ask, and who CANNOT be asked
```
If foreign commits exist, ask their author before pushing.

**Some authors cannot be asked, and the script names them rather than leaving you
to not know.** An ISOLATED lane refuses sends carrying a worker origin, so for its
commits that instruction has no truthful path — the two moves are push unasked
while a MANDATORY rule says otherwise, or never push. Found live 2026-09-07, when
a consent poll named 16 of 23 commits and missed 5 belonging to an isolated `amux`
(AF-548). Pushing is defensible: a lane committing to shared main has already
accepted that a peer will push it. Claiming consent you could not obtain is not.
State the exemption; a named exemption is a truthful path and silence is not.

**And a green Rust gate is not push-readiness for the range.** `cargo clippy
--workspace` and `cargo test -p amux-server` are scoped to a LANGUAGE and get
quoted as a verdict on a PUSH. In that same range 10 of 23 commits touched no
`.rs` file at all, including the commit of the lane that asked. The script prints
both counts so the denominator travels with the verdict.

**Run gates on a DETACHED worktree, not this one.** Every local check here reads a
tree with other lanes' uncommitted files in it, so a green is a claim about your
peers' drafts as much as about your commits:
```bash
git worktree add --detach /tmp/push-check main
git -C /tmp/push-check status --porcelain --untracked-files=no   # must be empty
```
This is how the 2026-09-07 push was found to carry a test that passes alone and
fails in the suite (AF-549) — 2013 passed, 1 failed, on bytes nobody had ever
compiled in isolation.

When user says "deploy": `git add` + `git commit` + verify above + `git push origin main`.

A fix in `git log` is not live until `/health`'s `commit` matches.
