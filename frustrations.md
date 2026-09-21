# amux frustrations

Friction that **amux itself** caused a session working inside it. Appended to as we
hit things; read when deciding what to fix next.

The rule for when and how to log is in
[`.claude/rules/frustrations.md`](.claude/rules/frustrations.md). The short version:
log friction the NEXT session will also hit, link a card, and record the cost in what
it actually cost.

**Current retirement instruction (Ethan, 2026-09-13; AF-780):** every entry must
link a concrete issue on the amux-frustrations board and retain its originating
`SESSION`. Only mark the issue Verified after that originating session validates
the exact entry and explicitly agrees it is complete, with all resolved board
gates satisfied. Then remove the entry from this file using the archive tool,
preserving its text and actual agreement. An unavailable or ambiguous originator
is unresolved; another verifier or a publishing committer cannot stand in for them.
This instruction supersedes the older AF-352 independent-retirement exception for
this drain. `ORIGINAL_CARD` preserves a historical pointer when `CARD` is repaired.

## Format — fixed fields so this greps

Append at the bottom. One entry per distinct friction. Never rewrite an existing
entry; add a new one that supersedes it and say so.

The template below is INDENTED two spaces on purpose: at column 0 it would match the
same greps as real entries, and the header would count itself as a frustration. An
instrument that measures itself is the bug this file exists to record.

```
  ## <one-line title, the symptom not the theory>
  AREA: <cli|board|attribution|notices|instruments|gates|browser|cloud|scheduler>
  SEVERITY: <blocks|slows|annoys>
  STATUS: <open|fixed>
  DATE: <YYYY-MM-DD>
  SESSION: <who hit it>
  CARD: <ID, or `none` only if genuinely unfilable>
  SYMPTOM: <what you actually saw — the output, the exit code, the wrong value>
  COST: <what it cost: minutes, a wrong conclusion, a blocked push, a false close>
  FIX: <what would fix it, or the sha if STATUS is fixed>
```

Greps that should keep working:

```bash
grep '^STATUS: open' frustrations.md          # what is still live
grep '^AREA: attribution' frustrations.md     # cluster by subsystem
grep '^SEVERITY: blocks' frustrations.md      # what stops work outright
grep -B1 -A8 '^## ' frustrations.md           # whole entries
```

**Why fixed fields:** three entries sharing an `AREA` is an argument that one thing
needs rebuilding. No single entry makes that argument, and free-form prose cannot be
counted.

---
## Ghost-rescue can only rescue the messages that happen to carry a timestamp prefix
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: (agent, AMUX-2629)
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in 131d932484db7a4e8eb3a0d9708f9e00dcb7f7c3. Committer identity is not author validation.
CARD: AF-782
ORIGINAL_CARD: AMUX-2629
SYMPTOM: the ported `[ghost-rescue]` sweep decides a stuck message is amux's — and so
safe to submit — only when the composer text starts with the dashboard's `[H:MM AM]`
stamp (py:9160, the only sound discriminator: anything else risks submitting a
half-written human thought). A read-only scan of the live fleet found 13 lanes holding
composer text with no matching user message in their transcript — `backend` "continue
with the queue", `ethan-dev` "push it", `mvs-infra` "Run the MVS prod health loop per
the runbook", and ten more — and ZERO of the 13 carry the stamp. The dashboard applies
the prefix inconsistently (`cmd_history` for amux-rust alone has both prefixed and
unprefixed human sends in the same hour), and agent-to-agent and nudge messages never
carry it.
COST: not yet counted in minutes, but it is 13 messages the fleet is currently sitting
on, and a fallback that covers 0% of the live population reads as protection that is
not there. Deliberately not widened: guessing "this looks like amux" would eventually
submit a person's unfinished sentence, which is worse than the stall.
FIX: two honest options, both upstream of the sweep. (1) Make the stamp universal — if
every amux-originated message carried a machine-readable origin marker, the guard would
be exact instead of a heuristic. (2) Better: deliver over the structured protocol, where
there is no composer to get stuck in and nothing to sweep for; the sweep's exit condition
is written into its module docs for that reason.

## A peer's `install` shipped my uncommitted, unverified WIP straight to the live server
AREA: cli
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: board-drive (AMUX-2637)
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in e48983ff8e44675a85692c09fdbab6f310e65a31. Committer identity is not author validation.
CARD: AF-783
ORIGINAL_CARD: AMUX-2637
SYMPTOM: I created `crates/amux-server/src/runtime_jobs/board_drive.rs` and wired it
  into `lib.rs` at ~22:0x, having run NO tests yet. At 22:07 another session rebuilt
  and installed `~/.local/bin/amux-server-rs` from this shared checkout; `strings` on
  the live binary shows `runtime_jobs/board_drive.rs`, and `/api/debug/board-drive` —
  an endpoint I had written minutes earlier — answered on :8822. Within 3 minutes the
  live loop had claimed AF-38 and AR-112 and routed two review nudges on the real
  fleet. I never installed anything.
COST: Unverified code reached production and mutated the live board. It happened to be
  correct (AF-38/AF-34/AF-33/RH-96 all moved, WIP-1 held), but two defects I found
  MINUTES LATER by testing shipped with it: a lane was told "you went idle holding
  BDQ-1" one tick after being handed BDQ-1, and a review route re-fired every 60s until
  the 24h per-card budget was spent in three minutes. The live build still carries both.
  The `git push` guard in CLAUDE.md ("check what you are shipping that is not yours")
  covers the git dimension only; the BUILD dimension has no guard at all, and it is
  strictly worse — a push ships committed work, an install ships whatever is in the
  working tree, including a file that has never been compiled by its author.
FIX: The install path should refuse, or at minimum announce, a build made from a dirty
  tree containing files no commit references. Cheapest honest version: have the
  installer stamp `git status --porcelain` + the untracked file list into the binary
  and surface it at `/health` as `built_from_dirty_tree: [...]`, so "is this build
  someone's WIP?" is answerable from the instrument everyone already reads instead of
  from `strings`. Related to the shared-checkout push rule, same root: on a shared
  checkout, one session's routine action ships another session's in-flight work.

## A worker whose pane died at launch reports `running: true` / `idle`
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux (cloud rust image, AMUX-2619)
CARD: AF-784
ORIGINAL_CARD: AMUX-2644
SYMPTOM: Started a worker in the new cloud container. `GET /api/workers/<id>` returned
  `{"status":"idle","running":true,"state":{"state":"idle"}}` — a healthy-looking lane.
  `peek` showed what had actually happened: `--dangerously-skip-permissions cannot be
  used with root/sudo privileges for security reasons` … `Pane is dead (status 1)`.
  The tmux SESSION still exists after the pane dies (`remain-on-exit on`), so "the
  session is there" is true and "the agent is running" is false, and the status field
  reports the first while reading like the second.
COST: This is the single blocking defect of the cloud rust cutover — every agent lane in
  every workspace would have died at launch — and the worker list said nothing was wrong.
  It was found only because I peeked at a lane I had no reason to suspect. On the live
  host the same failure would present as "the fleet is idle", which is the one shape
  nobody investigates. `idle` is also what a correctly-waiting lane reports, so no
  amount of watching the status column can distinguish them.
FIX: `idle` must not be reachable when the pane is dead. tmux already knows
  (`#{pane_dead}` / `#{pane_dead_status}` are one `display-message` away, and the peek
  text carries `Pane is dead (status N)`), so this is a state the detector can express
  and currently does not. A `dead` state — or at minimum `running:false` — with the exit
  status attached. Related: the browser failure in the same container named its symptom
  (`CDP never answered within 12s`) and not its cause; both are the ethos rule 4 shape,
  where the diagnosis is impossible from what the instrument reports.

---
## Uncommitted migrations reach the LIVE database within minutes, from another agent's server
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: rust-rebuild (RR-0109/0110 lane)
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in 572047d0369f3390f8e6f75004165a5b7a0fd9d8. Committer identity is not author validation.
CARD: AF-786
ORIGINAL_CARD: ARE-10
SYMPTOM: I created `crates/amux-server/migrations/0013_search.sql` at 22:16:42 EDT and
  never installed or restarted anything. At 22:18:23 EDT the migration was applied to
  `~/.amux/amux.db` — the live 269MB database — creating 2 tables, 24 triggers and
  backfilling 5,021 rows. `scripts/rust-auto-build.sh` is NOT the culprit: it builds
  from a `git worktree` of HEAD and 0013 is not in HEAD. The cause is that some other
  session on this shared checkout ran a working-tree build of `amux-server` with the
  default `AMUX_DB`, which is the live file.
COST: No damage this time — the migration is additive and applied cleanly, and it is
  in fact the best live evidence I have. But I explicitly set out to test against a
  `.backup` copy precisely so I would not write to the live DB, and the live DB had
  already taken my schema before I made the copy. A session cannot honour "never touch
  the live database" when a peer's ordinary `cargo run` applies that session's
  uncommitted migrations to it. The same mechanism with a destructive or wrong
  migration is a data-loss event with no author and no audit line.
FIX: make the live database opt-IN for a locally-built binary. Either default
  `AMUX_DB` to a scratch path unless `AMUX_ALLOW_LIVE_DB=1`, or refuse to apply a
  migration whose version is absent from HEAD unless the same flag is set — the
  discriminator (`git cat-file -e HEAD:<migration>`) is one cheap call, and it exactly
  separates "this build is the deployed one" from "this build is someone's working
  tree". Right now nothing distinguishes them and the live file is the default.

## A peer's commit shipped this run's in-flight work to origin, mid-edit
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: (Claude Code in iTerm — not a fleet lane, hence no session stamp)
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in 963e40f4d3632f41d6867163c10dc505455d9bdf. Committer identity is not author validation.
CARD: AF-787
ORIGINAL_CARD: AMUX-2663
SYMPTOM: TWICE in ~40 minutes, by different peers. `e679bdb` ("fix(hygiene): five carded
  defects") took an in-progress `/report` attribution change in `api/session_verbs.rs` and
  a brand-new test file that had not yet passed — it was still 404ing on a missing rig
  fixture at that moment. Then `3b24fcd` ("fix(build): main has not compiled since 22:43")
  took the whole in-progress status derivation in `api/sessions_legacy.rs`, 495-line test
  module included, mid-refinement. Both are on origin/main
  (`git rev-list --count origin/main..main` = 0) before either was noticed.
COST: Benign by luck — the swept-up code passes now. But this run was explicitly
  instructed never to commit or push, and its work was pushed anyway, twice, once with a
  red test. Also cost the confusion of `git status` no longer listing files that were
  definitely modified minutes earlier.
FIX: Not a rule ("remember to `git add` specific files" is the kind of rule that does not
  run). Two things that would close it structurally: a pre-commit check that refuses a
  commit touching files whose most recent writer was a different session — the
  `Amux-Session` trailer machinery in `scripts/git-hooks/prepare-commit-msg` already makes
  the writer knowable — or per-lane git worktrees, which the harness already supports.
  CLAUDE.md's Deploy section documents the REBASE version of this hazard; this is the
  `git add -A` version, and it needs the same warning.

## A peer's `git add` swept my uncommitted migration into their commit and it applied to the live DB
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux-rust (AMUX-2647 lane)
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in 7ec5e3102961eaf966427e74a8542bd3770c829f. Committer identity is not author validation.
CARD: AF-788
ORIGINAL_CARD: AMUX-2647
SYMPTOM: I wrote `migrations/0015_schedule_run_delivery.sql` and registered it in
  `migrate.rs`, uncommitted, under an explicit instruction never to commit. Commit
  4d76ff3 ("feat: universal FTS5 search …") picked up my `migrate.rs` edit; the .sql
  file was still untracked, so a clean checkout could not compile (`include_str!`
  resolves at build time), and 6689a74 then tracked my file to repair the dangling
  reference. The auto-builder shipped it and the live server applied 0015 to
  `~/.amux/amux.db` at 03:22:43 — schema I authored, live, hours before the code that
  writes those columns exists anywhere but my working tree.
COST: no damage — the columns are additive and NULL reads as "not recorded" — but the
  live DB now has two columns nothing populates, and neither author chose that. The
  deploy path is committed-HEAD-only *precisely* so half-finished work cannot ship;
  a broad `git add` in a shared checkout defeats it, and the second author was doing
  the right thing (repairing a dangling reference) with no way to know the file was
  mid-flight. The existing rule covers the direction "check what you are pushing that
  is not yours"; this is the mirror, and no check catches it.
FIX: the pre-commit guard should refuse a `git add` that stages files no lane has
  claimed — or, cheaper, `prepare-commit-msg` already stamps `Amux-Session`, so warn
  when a commit's file set spans more than one lane's recent edits. Until then: write
  new files outside the repo until the change is ready, which is what I should have
  done here.

---
## Booting a second amux-server to test something drives the PRODUCTION tmux fleet
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-10
SESSION: autofix (subagent)
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in 9a9194525bcb31e0d80454e107a851b07839f69a. Committer identity is not author validation.
CARD: AF-789
ORIGINAL_CARD: AF-69 (investigation, signed off) + AMUX-3221 (the FIX, open)
SYMPTOM: Started an isolated server (`AMUX_HOME=/tmp/amux-af-home`, port 8899, own DB) to
  verify a change without touching the fleet. Within 4 seconds its log showed:
    pane-size: restoring detached window ... session=amux-amux from=220x50 to=220x50
    pane-size: restoring detached window ... session=amux-mixpeek-autopilot ...
    pane-size: one-shot repair complete count=3 sessions=["amux-amux", ...]
  `pane_size::spawn()` takes no state and enumerates tmux DIRECTLY, so AMUX_HOME does not
  scope it. `ghost_rescue` is the same shape and it SUBMITS STUCK MESSAGES — i.e. a test
  instance can press Enter in a production lane's pane. Neither has an off switch;
  `commit_nudge` and `board_drive` both do (`AMUX_*_SECS=0`).
COST: Killed the instance and rebuilt the whole live verification as in-process router
  tests instead. This time the resize was a no-op (220x50 -> 220x50) so nothing was lost,
  but that is luck: a peer is running `/tmp/amux-sched-target/debug/amux-server` on this
  same box right now, and the repo's own docs tell you to build to a private target dir
  and run it.
FIX: STILL OPEN — the hazard is live. AF-69 (the INVESTIGATION) was signed off by amux
  2026-08-16; the FIX is AMUX-3221 and has not been started. Signing off an investigation
  is not the same as fixing the thing, and this entry stays until AMUX-3221 lands.
  CONFIRMED STILL BROKEN 2026-08-16: pane_size and ghost_rescue have NO env knob;
  commit_nudge (AMUX_COMMIT_NUDGE_SECS) and board_drive (AMUX_BOARD_DRIVE_SECS) do. No
  global isolation guard exists (grepped AMUX_NO_FLEET / AMUX_ISOLATED / is_isolated /
  AMUX_TMUX_READONLY — none).
  THE ENTRY'S OWN PROPOSED FIX IS INCOMPLETE, measured not assumed: adding the knob at the
  top of `pane_size::spawn` covers only its one-shot `sweep(true)`; the SAME function then
  calls `super::spawn_periodic("pane_size", TICK_SECS, ..)`, which keeps sweeping the fleet.
  A per-job knob there looks done and is not. That half-fix is stashed, not committed
  ("AF-69: incomplete pane_size guard").
  CORRECT SEAM (amux verified it): `runtime_jobs/mod.rs:128 spawn_periodic_every` is the
  ONLY constructor of a PeriodicTask — its own comment already leans on that to guarantee
  every job appears in the registry — so a knob there, derived from the job name
  (pane_size -> AMUX_PANE_SIZE_SECS, ghost-rescue -> AMUX_GHOST_RESCUE_SECS), gives every
  periodic job a disable for free, including ones written later. Requires a test proving a
  0 knob stops the sweep while a normal value still ticks, and that a disabled job stays
  REGISTERED (inert, not invisible) so it does not become a silent skip.

## Deleting 450GB freed 8GB, because hourly Time Machine snapshots pin every deleted block
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-10
SESSION: storage-audit
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in e188b0eb86a3d5406d2b29e53f0e6889d0c8e100. Committer identity is not author validation.
CARD: AF-790
ORIGINAL_CARD: AMUX-2701
SYMPTOM: With the volume at 741MB free, ~450GB of stale cargo target dirs was deleted and
  `df` moved to 9.0GB free — about 8GB recovered from 450GB deleted. Deleting a further
  26.8GB moved free space DOWN (8.1Gi -> 6.6Gi). The cause was 24 hourly APFS local Time
  Machine snapshots spanning 2026-08-09 13:18 to 2026-08-10 12:18: a snapshot pins the
  blocks of every file deleted after it was taken, so deletion frees nothing until the
  snapshots age out (24h) or are thinned. They had accumulated because the Time Machine
  destination ("My Book") is not connected, so nothing ever thinned them. macOS eventually
  purged all 24 on its own under pressure and free space jumped to 418Gi.
COST: A wrong conclusion that was already corroborated: two sessions independently read
  "deleted a lot, freed nothing" as "we deleted the wrong things", whose remedy is deleting
  MORE — the one action that could not work. It also produced an owner alert asking for a
  root password (`sudo tmutil thinlocalsnapshots`) that turned out not to be needed, which
  is a fire alarm spent on a self-resolving condition.
FIX: Partly fixed: the new autofix `disk` detector puts `tmutil listlocalsnapshots / | wc -l`
  in the card's evidence with an explicit "READ THIS BEFORE DELETING ANYTHING" note, so the
  next session sees the discriminator in the place it is already looking rather than having
  to know APFS semantics. Still open: nothing warns that the TM destination has been absent
  for long enough to accumulate a full day of local snapshots, which is the actual upstream
  condition and is invisible until it interacts with a disk-full event.

## The shared cargo target dir served a stale rlib, so `cargo test` blamed three innocent files
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: claude (AMUX-2619/2780 lane)
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in e84525e61cf0afaf8f0a9aa9a25e6104c9b4e600. Committer identity is not author validation.
CARD: AF-791
ORIGINAL_CARD: AMUX-2799
SYMPTOM: With the now-mandated `CARGO_TARGET_DIR=~/.amux/rust-build-target` (e188b0e, "ONE
  shared cargo target"), `cargo test -p amux-server` reported, in sequence, three DIFFERENT
  compile errors in files I had never touched: `unresolved import
  amux_server::runtime_jobs::registry`, `cannot find function title_needs_self_description
  in module amux_core::board`, and a `migrate.rs` precondition panic naming the shared
  target path. All three sources were byte-correct — I verified `pub mod registry;` with
  `od -c`. The actual cause: the cached `libamux_server-*.rlib` was built from an older
  tree. `strings` on it showed 6108 hits for `runtime_jobs..autofix` and ZERO for
  `registry` and `storage`, the two newest modules, while the same rlib's own crate
  compiled fine and lib.rs line 210 uses `runtime_jobs::registry`. Cargo's mtime
  fingerprint never noticed, because mod.rs (13:24) was older than the rlib (14:27).
COST: ~40 minutes, and three wrong conclusions I came close to reporting — twice I
  concluded "another lane's uncommitted work has broken main" and started to write it up,
  and once I concluded a committed test was broken under the mandated target dir. Every one
  of those would have sent a peer to debug correct code. `cargo clean -p amux-server`
  removed 48,516 files / 28.9GiB and fixed it for one invocation before it recurred;
  `touch crates/amux-server/src/runtime_jobs/mod.rs` is what actually forced the rebuild.
FIX: The failure mode is specific and cheap to detect: an rlib that does not export a
  module its own crate source declares. A preflight in the test gate — compare `pub mod`
  lines in each `mod.rs` against the built rlib, or simply `cargo build -p amux-server --lib`
  and fail loudly if it is a no-op while sources are newer — would turn 40 minutes of
  blaming peers into one line of output. Until then the recipe is: when `cargo test` names
  a symbol you can see in the source with your own eyes, suspect the ARTIFACT before the
  code, and `touch` the `mod.rs` that declares it. Related to the shared-checkout cluster
  above: same root (one resource, many lanes), different resource (build artifacts, not
  the git index).

## A probe read a hook file that git never executes, and a correct measurement certified the wrong conclusion
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-11
SESSION: amux
CARD: AF-792
ORIGINAL_CARD: AMUX-2841
SYMPTOM: Retracting a peer's report of a tree-wide mtime restamp, I grepped
  .git/hooks/pre-commit on amux and mixpeek for `git stash`, found none, and wrote
  "the mechanism does not exist" onto MI-4650. Three independent reasons it could not
  work: the stash is done by the pre-commit FRAMEWORK wrapping the hooks; it is
  spelled diff-index + `checkout -- .` + apply, never `git stash`; and mixpeek sets
  core.hooksPath=.githooks, so the file I opened is DEAD — git never runs it.
COST: A wrong retraction published onto another session's card, contradicting a
  correct report from creative-dna. Two peers spent turns re-establishing a fact that
  was already established.
FIX: The generalisable half is the CORROBORATION, not the bad grep. I confirmed the
  retraction by watching a file's mtime across a real commit and seeing it unchanged —
  true, and worthless, because I ran it in the amux tree, which has no
  .pre-commit-config.yaml and never invokes the framework. A correct measurement in
  the wrong scope arrives as EVIDENCE rather than as reasoning, and evidence is harder
  to doubt because you can point at it. Nothing felt like the moment to recheck.
  Wanted: before believing a negative about a mechanism, confirm the probe ran where
  the mechanism could fire — for hooks specifically, resolve core.hooksPath first,
  because the file at the obvious path may not be the one that runs.

## Verified gate rejects a cross-group reporter's verification, so the strongest evidence cannot close the card
AREA: gates
SEVERITY: slows
STATUS: open
DATE: 2026-08-14
SESSION: amux
CARD: AF-793
ORIGINAL_CARD: AMUX-3119
SYMPTOM: AMUX-3116 and AMUX-3117 (amux CLI fixes) were verified end-to-end by gtm-engine
  with negative controls, field-level CC_* diffs and a server-API cross-check, which is
  stronger than a typical same-group review. But the code verified-gate criterion is
  "peer-reviewed by a worker in group `amux`", and gtm-engine is group `gtm`. Acking it
  would be untrue, so both stay `done`.
COST: Two genuinely-verified cards cannot reach `verified`; the strongest verification
  available (the affected user, who also reported the bug) does not count toward the gate.
FIX: The verified gate should accept verification by the originating reporter, or by any
  worker when the card records who plus their evidence (AMUX-3119).

## staged-guard can't see a subagent's own edits, so it blocks the subagent's real work as "foreign"
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-16
SESSION: amux (file-manager subagent)
CARD: AF-794
ORIGINAL_CARD: AMUX-3249
SYMPTOM: The pre-commit staged-guard bases its verdict on per-session EDIT RECORDS in a
  time window, not on the staged diff. Running as a subagent, my Edits to app.css /
  index.html / sw.js produced no edit record under my session, so the guard reported
  "they wrote it (transcript); you have no edit record on this path" and BLOCKED the
  commit, naming `desktop` as sole author of files I had just rewritten this session.
COST: the commit was blocked; I had to read the FULL staged diff of app.css and index.html
  by hand to confirm every hunk was mine, then use `AMUX_VERIFIED_SOLO=1` to override. The
  guard's own advice ("keep only your hunks") assumed the peer's work was mixed in when it
  was not. The dangerous edge: a subagent conditioned to reach for AMUX_VERIFIED_SOLO on
  every commit will eventually rubber-stamp a diff that DOES carry foreign hunks, since the
  guard cries wolf on every subagent commit.
FIX: the guard needs a signal a subagent's edits actually exist — attribute Edit-tool writes
  to the running (sub)agent session, or fall back to the staged diff (not edit records) when
  no edit record exists for EITHER party. Basing the verdict on the staged diff directly
  would make it correct regardless of who recorded what.

## SUPERSEDES both entries above on DESKT-10: blob existence is unsound in the STALE section too
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-17
SESSION: desktop
CARD: AF-795
ORIGINAL_CARD: DESKT-10
SYMPTOM: My fix 5b923db moved the direction-unknown branches to the ancestry test but DELIBERATELY kept `git cat-file -e $(git hash-object <path>)` in the STALE section, with a comment arguing it was correct there because the classifier had already proven the path was behind. cold-outbound proved that wrong and I reproduced it: commit v1, edit to v2, `git add` without committing, and cat-file -e reports EXISTS while `git log --all --find-object=<blob>` is empty. `git add` writes the blob into .git/objects, so cat-file -e answers "ever written to the object DB", not "ever committed". The prescribed `git checkout origin/main -- <path>` then deletes the never-committed mid-edit. cold-outbound hit a live 4-minute near-miss on server-fast-checks.yml, mid-keystroke.
COST: a destructive false positive shipped into standing advice for every lane, for about 14 hours, and a near-miss on someone else's uncommitted work. The gap is not exotic: any session that stages incrementally produces it constantly, and it fires in the delete direction rather than the redundant-commit direction.
FIX: `git log --all --find-object=<blob>`; empty means never committed anywhere. `--all` matters, since a blob committed only on origin or another branch reads empty under a HEAD-only search, which errs safe but still misclassifies. amux has a fix agent in flight across commit_nudge.rs, the shell guards and session-freshness.sh, with a regression test; I am staying off those files rather than being a second editor. What generalises past this bug: I decomposed the question correctly (once a path is known behind, ask pure-old-copy vs novel-mid-edit) and then never checked that the instrument answered the sub-question I had just posed. A correct decomposition makes the wrong instrument feel already-validated, because the reasoning that selected it was sound. Verify the mechanism, not the verdict, applies to the sub-question too, and I had quoted that rule at another session hours earlier.

---

## Two amux servers on one SQLite DB, and endpoint.json points at the wrong one
AREA: port
SEVERITY: blocks
STATUS: open — owner's decision
DATE: 2026-08-17
SESSION: amux-errors-and-bugs
CARD: AF-654
ORIGINAL_CARD: AEAB-11
SYMPTOM: Two launchd jobs both run the Rust server against `~/.amux/amux.db` —
  `com.amux.server-rs` (pid 22521, port 8824, last exit -9) and `com.amux.serve`
  (pid 22053, port 8823, exit 0) — same binary, same build, both logging "schedule loop
  starting (FIRING)". Every `starting amux-rust` line before today was 8824 and single;
  8823 starts begin 2026-08-17 03:53:41.
COST: One batch of request-log rows was DROPPED (`request-log insert failed; rows
  dropped error=database is locked`, 04:07:34) — the first and only lock error in the
  file, all time, inside the dual-instance window. `endpoint.json` now advertises 8823,
  so every hook self-healing a stale AMUX_URL off it reaches the OTHER server; my own
  sync-github.sh resolver (frustration above / LR-22) now resolves to 8823 and works
  only because 8823 happens to answer. And it doubled the log: both instances tick the
  same 5s stall loop, so those warnings appear twice ~200ms apart, which is 77% of the
  24h log volume and buried the lock error above.
  DOCS NOW WRONG, second time for this class: CLAUDE.md asserts as ground truth
  "re-measured 2026-08-06" that "com.amux.serve.plist is the only server plist on disk"
  and gives `launchctl kickstart -k gui/$(id -u)/com.amux.serve` as THE restart command.
  There are two server plists now, and that command restarts 8823, not the canonical
  port. The note is emphatic that a wrong label costs a debugging session; it is now
  wrong itself.
FIX: Not applied — choosing which job is canonical can take the dashboard down, and a
  dev instance with its own AMUX_HOME is a legitimate configuration this could also be
  (ethos rule 8). Needed: decide, `launchctl bootout` the loser, delete its plist,
  correct CLAUDE.md's launchd note.

## The two causes behind that outage are not amux bugs, and amux had nothing to say about either
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-18
SESSION: amux-errors-and-bugs
CARD: AF-656
ORIGINAL_CARD: AEAB-28
SYMPTOM: The machine was up and on the network at 15:18; amux did not start until the
  console login at 18:28 — 3h10m later. All four amux units are user LaunchAgents in
  `~/Library/LaunchAgents` with no `LimitLoadToSessionType`, so they are `Aqua`: they
  load at GUI LOGIN, not at boot. `ls /Library/LaunchDaemons | grep -i amux` -> none.
  `RunAtLoad=true` is doing exactly what it says; "load" just never happened. Separately,
  the machine died in the first place from a hardware undervoltage fault
  (`Boot faults: uv,vdd_boost_uvlo`, `Boot failure count: 2`) — AEAB-30.
COST: Turned a ~75-minute hardware outage into a 4h26m amux outage. On a headless box
  this is unbounded: it ends when a human happens to sit down.
FIX: Owner's call, and genuinely a trade — `LimitLoadToSessionType = Background` starts
  at boot but leaves the login keychain locked, so lanes needing provider credentials
  may fail in a way that looks like a broken lane rather than a locked keychain;
  automatic login is simpler but is incompatible with FileVault and is a posture change
  on a Tailscale-reachable machine. Filed rather than chosen (ethos rule 8). What is NOT
  the owner's call and should ship regardless: `install.sh` says nothing about this
  property, so every amux install has it and no operator has been told.

---
## `amux board done --outcome-stdin` printed a warning about the outcome and silently applied NOTHING
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-19
SESSION: amux-errors-and-bugs
CARD: AF-657
ORIGINAL_CARD: AEAB-36
SYMPTOM: Closing AEAB-34, the entire output was:
    warning: outcome NOT recorded — server sent no JSON
  Verified against the API immediately afterwards: status still `review`, desc_len
  unchanged at 2792, no new log line. NEITHER the outcome NOR the status transition
  landed. Re-running the identical command with the identical ~2.9KB input succeeded
  completely (`AEAB-34 → done`, EXIT=0, desc +2915 chars). Nothing appeared in
  server-rs.log for the failed request.
COST: Caught only because I checked the operand I had just written — the habit this repo
  learned from desc_append/AMUX-2161. Without that check the card would have sat in
  `review` while I reported it closed, and the next nudge about it would have read as the
  board misbehaving rather than as my write evaporating. The warning actively misleads:
  it names ONE of the two things the command does, so the natural reading is "status moved,
  prose lost" — the opposite of what happened.
FIX: The CLI cannot know what landed when the server sends no JSON, so it must say exactly
  that ("no change may have been applied — re-run and verify") and exit non-zero, rather
  than emitting a field-scoped warning that implies the rest succeeded. Separately, a
  request that produces neither a response body nor a server log line is its own defect —
  whatever path this took leaves no trace, which is the AMUX-2140 shape. Note this is the
  SANCTIONED path: `--outcome-stdin` exists precisely so a gated transition never needs a
  hand-rolled curl, so a silent no-op here pushes people back to curl, which is how
  attribution gets lost.
---
## Every PR conflicts with every other, because the friction log is append-only and mandatory
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-20
SESSION: amux-errors-and-bugs
CARD: AF-658
ORIGINAL_CARD: AEAB-40
SYMPTOM: `.claude/rules/frustrations.md` mandates an entry for any amux friction and says
  "Append at the bottom", so every branch doing real work ends by appending to the same last
  line of the same file. Two branches in flight is a guaranteed textual conflict. Hit three
  times today on PRs #132, #133 and #136.
COST: ~20 minutes of CI per occurrence, three times, because GitHub does not run PR
  workflows on a head it cannot merge — so the PR shows NO CHECKS AT ALL rather than a
  failure. "no checks reported" and "all checks passed" are one glance apart in
  `gh pr checks`; I nearly read the absence as green. All three branches were mine, so no
  peer was blocked this time, but a peer would have been.
FIX: Open, and it is a design call rather than a patch — carded as AEAB-40 and parked
  needs:you. NOT `merge=union` in .gitattributes: this repo's own history records union-
  merging this file splicing fragments of different entries together, leaving one entry
  carrying another's `FIX:` line, which silently corrupts the `grep '^STATUS: open'` counts
  the file exists for. A conflict that stops you beats a merge that lies. The candidate I
  would pick is one file per entry (`frustrations/YYYY-MM-DD-slug.md`), which makes the
  conflict structurally impossible, with the work being the greps in the rules, CLAUDE.md
  and `scripts/frustrations_audit.py`. Interim recipe, which worked three times today: take
  origin's file, append your entries VERBATIM, never let git interleave, then run the audit.
## A peer's half-saved file blocks an unrelated commit's gate — third sighting in one day
AREA: shared-checkout
SEVERITY: slows
STATUS: open
DATE: 2026-08-22
SESSION: amux
CARD: AF-796
ORIGINAL_CARD: AMUX-1315
SYMPTOM: my commit of a one-file autofix.rs fix was refused because the pre-commit gate
  (cargo check/clippy) compiles the WHOLE workspace, which at that moment contained a
  peer's mid-edit mdai.rs (their AF-141 work, uncommitted). The suite also wedged and two
  unrelated test families went red — all of it their in-flight tree, none of it my change.
  Same shape amux-frustrations hit this morning (a missing STALL_SECS const failing THEIR
  build during MY reclaim work), and their AF-132 near-pickup at noon. Three sightings,
  one day, three different victims.
COST: one blocked commit and a diagnosis cycle to establish "not my code" (the failing
  tests were a peer's own passing-in-CI features, which reads as a regression I caused);
  my staged change sat hostage until their edit completed.
FIX: none here — this IS AMUX-1315 (per-lane worktrees), and today is its strongest
  argument yet: the workaround everyone reaches for (an isolated worktree to get a stable
  tree) is the proposal itself, applied by hand, per victim, per incident. The count now
  argues for the build.

---

## Every checkout's git hooks are 18 days stale, and amux has been saying so into a log for 11
AREA: instruments
SEVERITY: blocks
STATUS: half-fixed — detection reaches a session now; the reinstall is the owner's call
DATE: 2026-08-23
SESSION: amux-errors-and-bugs
CARD: AF-659
ORIGINAL_CARD: AEAB-47
SYMPTOM: `.git/hooks/pre-commit` is dated Aug 5 22:39 in ~/amux, ~/Developer/amux AND
  ~/Projects/amux-gtm, while `scripts/git-hooks/` is current. `grep -c guard_version` returns
  0 in the installed hooks and 3 in the repo's. `.git/hooks/pre-push` never calls
  `append-only-push-guard`, so the guard added after MG-1483 silently reverted 10 pushed
  entry-lines of this very file has never run on this machine.
COST: the cross-session staged-guard has been degraded fleet-wide for 18 days, and I pushed
  frustrations.md on 2026-08-22 with the data-loss guard absent without knowing. The detector
  was never the problem: the server logged "OUTDATED HOOK ... Reinstall:
  scripts/install-hooks.sh" 128 times across 8 days, naming 9 session/repo pairs, correctly,
  with the remedy — into server-rs.log, which nobody tails.
FIX: the detection now reaches a session — `.claude/session-freshness.sh` gains a content
  diff of the installed hooks at SessionStart. Content rather than `guard_version`, because
  the server's detector only fires for hooks too old to send a version at all; and
  `git rev-parse --git-path hooks` rather than `$REPO/.git/hooks`, because in a worktree
  `.git` is a file and the naive path is silent in exactly the checkouts AEAB-26 says the
  guard is already blind in.
  The reinstall itself is deliberately NOT done here: the current hooks are strictly more
  blocking than the installed ones, so running install-hooks.sh changes push behaviour for
  every other session on this machine.
  The general shape, and it is the fourth instance in two days after AEAB-46, AEAB-47 and
  AEAB-49: amux knows the dangerous fact, computes it correctly, and files it where the
  person who needs it never looks. `install-hooks.sh` also COPIES (`install -m 0755`) rather
  than symlinking, which is the mechanism that lets every one of these drift.

NOTE (amux, 2026-08-24, STRUCTURAL REPAIR — not my content, and deliberately not completed):
  a heading "Developing on branches in the build source put my unreviewed code on the whole
  fleet" carrying `AREA: cloud` and NO other fields was committed in 7fae11a1. A `## ` heading
  with no field block fails scripts/frustrations_audit.py, which turned CI red on main at
  12:10 and kept the required `checks` status failing for every push after it, including two
  of mine that inherited it.
  Demoted to this note rather than deleted or filled in. Deleting would lose an author's text;
  filling in SEVERITY/SYMPTOM/COST/FIX would mean inventing someone else's reasoning and
  signing their name to it, which is worse than the breakage it fixes.
  The entry immediately below cites AEAB-49 and its SYMPTOM, COST and FIX are entirely about
  THIS title's subject (branch code reaching the fleet), with nothing about a debug log or a
  disk. So these are most likely ONE entry that acquired a spurious heading. That is a guess
  and I have not acted on it. amux-errors-and-bugs owns the correction; their lane is not
  running, which is why I repaired the structure rather than routing it.
## `amux board` has no verb that sets `desc`, so recording findings on a card requires raw curl
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-23
SESSION: desktop
CARD: AF-797
ORIGINAL_CARD: DESKT-21
SYMPTOM: `amux board desc DESKT-21 --stdin` -> `amux board: unknown subcommand: desc`. The
  full verb list (`amux help board`) is `done|doing|todo`, `add <title>`, `list`. There is no
  way to write a card's description from the sanctioned CLI at all. `amux board done` accepts
  `--outcome`, so desc is writable ONLY as a side effect of closing a card — a card that is
  still `todo` cannot be given one. The only path left is
  `curl -X PATCH -d '{"desc":...}' $(amux url)/api/board/<id>`.
COST: two extra round trips to discover the verb does not exist, then a hand-rolled curl that
  I had to remember to stamp with `X-Amux-Session` myself. That is the AMUX-2325 shape exactly:
  the CLI is what makes attribution automatic, so every gap in the CLI manufactures an
  unattributed write from anyone who does not remember the header. Nothing warns you.
FIX: add `amux board desc <ID> [--stdin|--file|<text>]` alongside the existing status verbs,
  reusing the `--outcome` plumbing that already writes desc as its own PATCH. One verb closes
  the gap for every card state, not just `done`.

## A stale second `amux` CLI shadows the real one on any PATH that puts /usr/local/bin first, and silently ate a card title
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-23
SESSION: desktop
CARD: AF-798
ORIGINAL_CARD: DESKT-22
SYMPTOM: `amux board add --stdin <<'EOF' ... EOF` created a card whose TITLE IS THE
  LITERAL STRING `--stdin`, and threw the real title away. Exit 0, a full JSON card body
  echoed back, nothing wrong-looking. The identical command an hour earlier had worked
  and printed `DESKT-21 -> todo`.
  Cause: there are TWO amux CLIs on this machine.
    ~/.local/bin/amux -> ~/Dev/amux/amux   (live, tracks the repo, 89 stdin refs)
    /usr/local/bin/amux                     (standalone POSIX-sh copy, dated Aug 6, NO
                                             --stdin support anywhere in it)
  Default login PATH has ~/.local/bin at position 1, so normally you get the live one.
  I had prepended `/usr/local/bin` to PATH for an unrelated reason (`networksetup` and
  `ifconfig` are not on the sandboxed default PATH), which silently swapped the CLI
  under me mid-session. The two calls in this transcript differ ONLY in PATH order.
  The output shape is the tell nobody would think to look at: the live CLI prints
  `DESKT-21 -> todo`, the stale one dumps raw JSON. Same verb, same flags, same exit code.
COST: one card created with a garbage title and its real title destroyed, caught only
  because I re-read the card afterwards to get its ID. Worse than the lost title: the
  global CLAUDE.md mandates `--stdin` as the FLEET CONVENTION specifically to stop the
  shell evaluating backticks and $(...) in titles (AMUX-1888 — a garbled message, a
  leaked credential, and a stray `git rebase --quit`). On the stale CLI that mandated
  form silently discards your text, and the natural recovery is to fall back to inline
  quoting, which walks straight back into AMUX-1888. The safety convention degrades into
  the hazard it was written to prevent, with no error at any step. That is the
  AMUX-2140 shape: following the sanctioned instruction exactly is what produces the
  failure, and it returns success.
FIX: remove /usr/local/bin/amux — install.sh owns ~/.local/bin and nothing should be
  shipping a second copy to /usr/local/bin. Belt and braces, since a stale copy can
  reappear: have `amux` print its own resolved path and repo sha on any parse error, and
  make an unrecognised leading `--flag` on `board add` a hard error rather than a title.
  A CLI that accepts an unknown flag AS DATA cannot fail loudly, which is why 17 days of
  drift produced no signal.

## A shared checkout has ONE git index, so a peer's `git commit` shipped MY staged work under THEIR message
AREA: attribution
SEVERITY: blocks
STATUS: open
DATE: 2026-08-23
SESSION: desktop
CARD: AF-799
ORIGINAL_CARD: DESKT-22
SYMPTOM: I staged four files for DESKT-22 (`git add` of a migration, heartbeat.rs,
  health.rs, migrate.rs), then ran `git commit -m ...`. It died with
  `fatal: cannot lock ref 'HEAD': is at c8272bf17 but expected 78b77653b`. My commit
  never existed. But the worktree was CLEAN afterwards and my code was in HEAD anyway:
  peer session `amux` had committed in the same instant, and because a shared checkout
  has ONE index, their commit swept my four staged files in. c8272bf1 now reads
  "fix(push-guard): the consent exit now works for ISOLATED workers (AMUX-3533)" and
  contains 330 lines of unrelated downtime-cause instrumentation alongside their two
  scripts/ files. Neither author reviewed the other's half.
  Two things made it worse than a merge collision:
  1. THE TRAILER LIED, and it is the exact field the deploy recipe says to trust.
     CLAUDE.md's push section says `%an` is shared by every session so "the Amux-Session
     trailer, stamped by prepare-commit-msg, is the real discriminator". c8272bf1 is
     trailered `Amux-Session: desktop` — ME — while its `Claude-Session:` URL is a
     different agent session from mine, and the card it names (AMUX-3533) is owned by
     session `amux` on the board. The same peer's other commit that hour
     (78b77653) is correctly trailered `amux`. So the one anti-footgun the docs point
     you at reported the sweeping commit as mine.
  2. THE STAGED-GUARD WARNED IN THE WRONG DIRECTION. It fired four notices, each saying
     my files "were also edited by session 'amux' N minutes ago — if that is MORE than
     you wrote, their work is in it". That is the mirror of what was about to happen:
     the risk was MY work landing in THEIRS, and the guard has no phrasing for it. It
     even appended the AMUX-3497 caveat suggesting the co-edit signal was probably just
     my own writes seen twice, which is the reading that makes you proceed.
COST: my work is merged and correct but permanently uncitable — DESKT-22 has no commit
  of its own, and the card now carries a paragraph explaining why anyone looking for one
  will not find it. A reviewer of AMUX-3533 gets 330 unrelated lines. Not fixable after
  the fact: rewriting shared history to separate them is strictly worse than a wrong
  message. Roughly 20 minutes to establish what had happened, because every obvious
  signal (clean tree, code present in HEAD, my own session on the trailer) said the
  commit was mine.
FIX: the index is the shared resource nobody is arbitrating. Either (a) take a lock
  around stage+commit so the pair is atomic across sessions — the staged-guard already
  runs at exactly the right moment and already knows who else is live, so it is the
  natural place, or (b) stop sharing the index: per-session worktrees (`git worktree`)
  give each lane its own index and HEAD against one object store, which is the durable
  answer and kills the whole class including the documented mirror cases (a peer's
  `git pull --rebase` replaying unpushed work, 2026-08-03; a peer's commit sweeping
  staged deletions, 2026-08-09 — this file's third entry in that family).
  Separately and cheaply: prepare-commit-msg must stamp the session of the process
  actually running git, and the staged-guard must warn in BOTH directions — "your
  staged files may ride out under someone else's commit" is the half it cannot say.

---
## The browser guard is absent against the one lane the dashboard is hardcoded to impersonate
AREA: attribution
SEVERITY: blocks
STATUS: open
DATE: 2026-08-23
SESSION: amux-frustrations
CARD: AF-183
SYMPTOM: A session is handed "a browser is already running under session '(unattributed)' —
  starting yours would DESTROY its state (staged logins included)". It names no owner, so there
  is nobody to ask and the only safe move is to do nothing. Measured: 451 of 535
  /api/browser/start rows all-time (84%) carry no X-Amux-Session, so the guard's whole safety
  property, naming the owner you are about to destroy, is unavailable for most collisions.
  Worse, app.js:32951 hardcodes `let _bwSession = 'amux'` with the deeplink as its only setter,
  so a browser a human opens from the Browser tab is recorded as owned by the `amux` LANE. The
  guard's same-session shortcut then treats that lane's start as the human's own restart:
  no refusal, no takeover flag, staged logins gone.
COST: A blocked browser for whoever hits the refusal, and a live path for an agent to silently
  destroy a human's signed-in session. The text is also verbatim the text of AF-181, an
  auto-captured card that was DISCARDED and then folded into an unrelated card, so it recurs
  and the discard is what let it recur.
FIX: Put the recoverable facts in the SENTENCE (pid, started_at, profile are already in the
  body but not the string) and let the refusal consult _amux_request_log for the start row, so
  "started 10h ago from 127.0.0.1 by curl/8.7.1" replaces "(unattributed)". Separately, and
  routed to Ethan because it is an identity decision, the dashboard must stop calling itself
  `amux`. AF-183.
NOTE: this is AMUX-1768's class one layer up. browser.rs:104-113 removed the SERVER-side default
  constant in writing, for exactly this reason ("framing that lane for every anonymous call ...
  and worse, the guard's same-session shortcut let any TWO anonymous callers stomp each other").
  The client-side constant survived the fix. Fourth member of the 2026-08-23 misattribution
  cluster with AF-179 and AF-182; the other three name a WRONG owner, which is recoverable, and
  this one names none.
STATUS-2026-09-01: HALF SHIPPED, and the half that is left is not code. The
  request-log lookup this entry asks for EXISTS and is wired: api/browser.rs
  carries `StartOrigin` with three states (Found / NotFound / NotLooked, so "we
  looked and found nothing" cannot collapse into "we did not look"),
  `lookup_start_origin` reads client_ip and user_agent off `_amux_request_log`,
  and the refusal consults it. So the caller now gets "127.0.0.1 + curl/8.7.1" or
  "100.66.26.84 + Mozilla/5.0 (Macintosh...)" instead of "(unattributed)", which
  is the discrimination the COST line names: an agent on this box against a human
  at a browser.
  The TITLE's claim is still true. `let _bwSession = 'amux';` is live at
  app.js:34858, so a browser a human opens from the Browser tab is still recorded
  as owned by the `amux` LANE, and the guard's same-session shortcut still treats
  that lane's start as the human's own restart. The entry stays open on that
  clause alone.
  Not fixable from here without deciding what the dashboard should call itself,
  which is whose identity it is (ethos rule 8). AF-183 is in `needsyou` with the
  question in one sentence and a recommendation.
STATUS-2026-09-10: the card was silently DISCARDED at 13:48 by an unaudited fleet-wide
  event ("bulk-migrated needsyou -> discarded by amux-3", zero trace in server-rs.log,
  "amux-3" not a registered session) that hit 58 cards across 22 sessions, several
  touching money, revenue and security. cold-outbound found and restored its own
  7 first (CO-266); this lane's sweep found 14, this one among them, restored and
  verified by read-back, reported to mixpeek-funnel who is coordinating the
  fleet-wide tally. Re-checked the underlying defect while restoring it: `let
  _bwSession = 'amux';` is still live (app.js:38230, confirmed against
  origin/main@634e5a86) — a setter for the #browser= deeplink case (AMUX-3073) was
  added since this entry was filed, but the ordinary Browser-tab default is
  unchanged. The entry stays open on the same clause it always was.

## A peer's mid-edit fails MY test run, and a rerun is the only way to tell
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-24
SESSION: amux
CARD: AF-800
ORIGINAL_CARD: AF-182
SYMPTOM: `cargo test -p amux-server --lib` returned "1284 passed; 1 failed" twice tonight,
  hours apart, and BOTH times the failure vanished on an immediate rerun with no change to my
  tree (1282/0, then 1285/0). The suite prints the count in the tail but the failing test name
  scrolls past in ~1290 lines, so the first thing you see is a number, not a name. On the
  second occurrence I read the tail, saw the count, and committed and pushed before registering
  the `1 failed` beside it.
COST: A commit message (d237f886) that states "1284 lib tests" for a run that was not clean.
  Caught and corrected on the card within minutes, but the message is pushed and wrong, and the
  correction lives somewhere the next reader of that commit will not look. The expensive
  direction has not happened yet: a session learning this shape and re-running past a REAL
  failure because "it is probably a peer".
FIX: The shipped half of AF-182 — lint-blame partitioning offenders into yours / a peer's
  in-flight work / already-broken-on-HEAD — is exactly the discriminator this needs, and it
  currently runs only in the pre-commit hook. A `scripts/cargo-blame.sh test` wrapper that pipes
  a failing run through the same analysis with STAGED empty would answer "is this mine" in one
  line instead of a rerun. amux-frustrations proposed that wrapper for `check`/`clippy`; this is
  the same gap for `test`, and the test case is worse because the signal is a count rather than
  a compiler error naming a file.
NOTE: This is the transient-unbuildable half of AF-182 that I own, showing up in a form I had
  not predicted. My entry there described the window as breaking a peer's BUILD. It also breaks
  a peer's TEST RUN, where there is no filename in the output to attribute — you get an
  arithmetic difference between two numbers and no clue whose edit caused it. e6077bcb fixed the
  commit path; neither of us has fixed the ad-hoc path, and this is the second cost from it.

## Two servers on one DB reap each other's live work and halve each other's thresholds
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-22
SESSION: amux-errors-and-bugs
CARD: AF-664
ORIGINAL_CARD: AEAB-43
SYMPTOM: `reap_orphaned_scans` runs `UPDATE reclaim_scans SET status='interrupted',
  error='server restarted mid-scan; the scan thread did not survive' WHERE
  status='running'` — no owner on the row. 8824 boots 10s after 8823 and reaps 8823's
  healthy scan. Both of the two scans that have ever run say the thread did not survive;
  both threads logged progress five minutes later, with no restart. And because every
  terminal write is guarded `AND status='running'`, the true outcome can never be
  recorded afterwards — it matches zero rows and logs nothing.
  Separately: `reclaim_skipped` shows ~/Downloads at hits=2 with first_seen and last_seen
  NINE SECONDS apart, so a threshold documented as "needs 2 such scans" was satisfied by
  one incident counted twice, and ~/Downloads is now permanently skipped.
COST: 2 of 2 reclaim scans ever run carry a false cause, on the machine where disk is the
  live risk. Any hits-based threshold in amux is silently halved the same way.
FIX: an owner column (pid or per-process boot ulid) on the scan row, reaping only rows
  whose owner is neither this process nor a live pid. The general form, which is the
  third entry this week under AEAB-11: any predicate that means "mine" or "twice" is
  wrong on a shared DB with two writers, and the failures do not look alike from outside.

## A rejected review has no status, so the reviewer is nudged to review their own rejection
AREA: board
SEVERITY: annoys
STATUS: open
DATE: 2026-08-24
SESSION: amux (hit it, twice), amux-frustrations (verified the mechanism)
CARD: AF-801
ORIGINAL_CARD: AF-214 (nudge skip, done) / AMUX-3668 (the `changes-requested` status, open)
SYMPTOM: amux reviewed AF-203, rejected it with four specifics, and was re-nudged twice with
  "[amux] AF-203 sits in 'review' and names YOU as reviewer". The nudge predicate
  (board_drive.rs:2461) is `status == review AND reviewer == you`, and its own instruction —
  "if not, say what fails on the card" — is a DESC write that does not change status. So
  following it exactly leaves the card in the state that re-fires the nudge, until the 24h
  budget is spent. Verified against the running board: the status vocabulary is backlog, todo,
  doing, review, done, verified, discarded. There is no cell for "reviewed, rejected, back with
  the author", so both honest-looking moves misdescribe reality — `review` claims it awaits a
  REVIEWER when it awaits the AUTHOR, and `doing` reads as the reviewer working it when the
  reviewer is finished.
COST: two wasted reviewer turns on one card, each a full re-read to conclude "I already did
  this". Small per instance and it recurs on every rejected review. The larger cost is the
  board lying to every reader until the author notices: a card in `review` is indistinguishable
  from one nobody has looked at yet.
FIX: a `changes-requested` status (or `review` + a `rejected` flag) — it is the true state, it
  removes the card from the reviewer-nudge predicate, and it returns the card to the AUTHOR's
  queue where the work is. Cheaper fallback if that is too much surface: skip the reviewer
  nudge when the card's most recent activity is the REVIEWER's own note, since they have
  demonstrably reviewed it. REJECTED: raising the nudge budget — that makes an uninformative
  nudge fire less often, which is not the same as making it informative.
NOTE: amux's own move was the correct read and the vocabulary still could not hold it: "Not a
  second review — my findings stand... this is a status correction so the card stops describing
  itself as awaiting a reviewer when what it awaits is four small edits by its author." This is
  the AMUX-2140 shape (the sanctioned instruction does not reach an exit) in the review loop
  rather than the CLI.

NARROWED 2026-08-24 to the VOCABULARY half. The re-nag is fixed; the lying status is not.
  SHIPPED (c98ac2c1, AF-214): the reviewer nudge now skips a card whose reviewer has written
  to it since it entered review. amux verified independently — always-return-true reddens 3 of
  5 cells, dropping the round scoping reddens the resubmit cell alone, both counts as claimed.
  They also checked the NEEDLE against AF-203's real stored log rather than a fixture, which
  is the check that matters since a matcher that never matches makes the whole thing inert
  while every test passes: "` amux:" matches the reviewer's own desc row and does NOT match
  `amux-frustrations:` (the trailing colon anchors it), `authz:`, or `commit <sha> —`. And the
  skip is legible in the drive's own output as `Advance::None { reason: "reviewer-already-acted" }`
  rather than a silent no-op, because a nudge that stops firing and one that was never
  eligible look identical from outside.
  STILL OPEN, and it is the half that fixes the class: there is no status for "reviewed,
  rejected, back with the author". `review` claims the card awaits a REVIEWER when it awaits
  the AUTHOR; `doing` reads as the reviewer working it when they are finished. amux has taken
  it as AMUX-3668 (board_drive is theirs and they are the one who hit it), going with
  preference (a), a `changes-requested` status.
  WORTH KEEPING, amux's own: their first mutation pass reported BOTH mutations surviving,
  because they filtered `cargo test -- a_reviewer_who_has_written` and matched one cell of
  five. Naming the target before searching for it — the same instrument error this entry is
  about, made while checking the fix for it.

---
## Worker session does not auto-restart when server restarts
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-29
SESSION: 6527367a-8ff6-431a-ace9-e421554fb30d
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in 67d7478c9e2c419df362d32c476f8bb98a38f0f2. Committer identity is not author validation.
CARD: AF-802
ORIGINAL_CARD: none
SYMPTOM: After `systemctl --user restart amux.service` (from a deployment), the amux
  worker session stays down: `GET /api/sessions/amux` returns `running: false`. Inbound
  Telegram messages have nowhere to route into until someone manually calls `POST
  /api/sessions/amux/start`. The `amux-worker-start.service` is a boot-time-only unit
  (runs once at `systemd --user` init), not triggered by manual server restarts.
COST: 5 minutes of diagnostics; live Telegram messages silently drop inbound until
  manually restarted. In production with unattended amux, a server restart from a
  deployment would leave Telegram routing dead until noticed and fixed manually.
FIX: Either (a) change `amux-worker-start.service` to have `Restart=always` so it
  auto-restarts with amux.service, or (b) add a post-startup hook to amux.service
  that calls `POST /api/sessions/amux/start`, or (c) wire the worker start into a
  systemd timer that verifies worker is up on server start. The root cause is that
  system-startup and service-restart are different events (both need the worker up),
  and the current unit only handles the first.

## A fix that brings the fleet back up can itself make local cargo unsafe again
AREA: build
SEVERITY: blocks
STATUS: open
DATE: 2026-08-31
SESSION: amux
CARD: AF-668
ORIGINAL_CARD: AMUX-48
SYMPTOM: Shortly after fixing AMUX-49 (every registered lane, not just `amux`,
  now comes back up after a reboot — 6 more Claude sessions went from stopped to
  running as a direct result), a plain `cargo check -p amux-server` — the ONE
  cargo invocation the existing offload-builds guidance called safe to run
  locally, single-crate, `.cargo/config.toml`'s `jobs=1`/`incremental=false`
  throttle already active — got OOM-killed (exit 137) anyway. `free -h`
  immediately after: 5.5GiB available out of 13GiB, zero swap. `.cargo/
  config.toml`'s own header (written 2026-08-28, FRONT-2) already names the
  mechanism: its throttle was tuned and verified against THAT day's baseline
  memory occupancy, and it explicitly warns a kill under pressure is not
  necessarily the build's own process — the OOM killer can reap an unrelated
  Claude Code session as collateral instead. AMUX-49 raised this box's
  baseline occupancy (8 running Claude processes instead of 2, ~200-400MB RSS
  each) without anyone re-measuring whether the existing throttle still holds
  against the new baseline.
COST: A gate that could not be honestly satisfied: AMUX-48's new invariants
  check (session.registered_lane_is_running) is written and follows an
  established, already-working pattern closely, but could not be verified to
  even COMPILE locally without risking re-crashing the same session AMUX-49
  had just recovered — the exact irony of one fix undermining the safety
  margin a sibling fix depended on. Remote build hosts were ALSO unreachable
  at the same time (a separate, unrelated baar-site netbird outage), so there
  was no fallback verification path at all for a period.
FIX: none yet — this is a structural gap, not a one-line bug. The honest
  interim mitigation (applied 2026-08-31): `offload-builds` memory widened to
  say `cargo check -p <single-crate>` is no longer a blanket-safe default —
  check `free -h` for real headroom before ANY local cargo invocation, treat
  the margin as a property of current fleet occupancy, not of the command's
  scope. A real fix would be either a durable local swap file (this box
  currently has NONE — `free -h` shows `Swap: 0B`, so there is zero graceful
  degradation under pressure and the OOM killer fires immediately) or a
  standing, always-available remote build target instead of relying on
  the specific remote hosts named in CLAUDE.local.md (private, this repo
  is public) being up when needed.

## Same root cause as above, escalated: the auto-builder itself now fails repeatedly, not just a manual check
AREA: build
SEVERITY: blocks
STATUS: open
DATE: 2026-08-31
SESSION: amux
CARD: AF-669
ORIGINAL_CARD: AMUX-48
SYMPTOM: Supersedes/extends "A fix that brings the fleet back up can itself
  make local cargo unsafe again" (same date, above) — that entry covered a
  manual `cargo check` getting OOM-killed once. Verifying AMUX-48's `done`
  card an hour later surfaced something worse: `amux-builder.timer`
  (enabled, polling every 60s) has been trying to build commit d7af60f5
  since it landed and failed SIX consecutive times over ~15 minutes, every
  attempt dying with a bare `Terminated` right after "Preparing worktree"
  finishes, before any `Compiling` line ever appears in the log. Host load
  climbed the whole time this was observed: 43.59 -> 58.08 (1-min, 4
  cores) — not a one-off spike, a sustained, worsening trend. The
  builder's own lock (mkdir-based, `scripts/rust-auto-build.sh`) IS working
  correctly — attempts are serialized, not overlapping — so this is not the
  builder compounding its own problem, it's the AMBIENT load (this
  session's 8 concurrent Claude processes + a desktop stack (Xvfb/x11vnc/
  openbox/chromium) that restarted mid-observation for unrelated reasons
  (see FRONT-4) + everything else on this box) leaving no room for even a
  single serialized release build to complete.
COST: `/health`'s `commit` field has been stuck at `5e5f4b24da71` through
  three real fix commits (e6d48d53, d428277a, d7af60f5) landing on top of
  it — the fleet has been running increasingly-stale code for the whole
  window, and AMUX-48's own invariants check (meant to catch OTHER
  processes dying silently) cannot itself be confirmed live because the
  binary that would contain it never finishes building. The exact
  "outcome confirmed to still hold" a `verified` gate asks for could not be
  honestly claimed for the live-deploy half of that question — recorded
  as a caveat on the card rather than papered over.
FIX: none yet. Same interim mitigation as the prior entry (offload,
  headroom-check before local cargo) doesn't cover THIS case — the builder
  is a system service, not something a session chooses to run or skip.
  A real fix needs either genuinely lowering this box's baseline occupancy
  (durable question: does this box need to run 8 concurrent Claude
  sessions plus a full desktop stack plus periodic release builds, or does
  one of those need to move), or giving the builder itself a remote-offload
  path the way this session now does manually for ad hoc verification.

---

## A latency card named an innocent endpoint with a verdict that was confidently backwards
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-26
SESSION: amux
CARD: AF-803
ORIGINAL_CARD: AMUX-3772
SYMPTOM: A host-wide stall that RAMPS files a single-family outlier card on the scan where fewer than AMUX_OUTLIER_ROLLUP_AT (3) families have crossed the threshold. That card's verdict then says "This is not a percentile shift — it is individual requests going wrong, so look at the request, not the family", which is the exact opposite of the truth, and it names an endpoint that answered in 0.09s minutes later. The rollup that describes it correctly already exists and fires on every subsequent scan; nothing revisits the card filed at the leading edge.
COST: One lane-turn to diagnose, and the diagnosis only landed because `host_load_at_worst` was in the payload and I followed it. A reader who trusts the verdict audits innocent code. ethos.md rates a loud wrong probe worse than a silent one, and this is one: it answers, names a specific target, and is wrong.
FIX: none yet, deliberately. The obvious fix — suppress a single-family card when an open ROLLUP exists — is WRONG while a rollup card can sit parked in backlog indefinitely, because it would mute every genuine single-endpoint regression. That prerequisite is AMUX-3774 and is now fixed; this card is parked with that as its trigger. Recorded because building the wrong fix first is exactly what I did, and the order matters.

## Discarding an autofix card as a "duplicate" deletes the only thing suppressing the re-file
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-28
SESSION: amux
CARD: AF-804
ORIGINAL_CARD: AMUX-3849
SYMPTOM: A live outage (`/api/browser/start` 502) produced FOUR cards in three hours. I hand-filed AMUX-3842 with the diagnosis, then discarded the two autofix cards as duplicates of it, twice, and a fourth arrived anyway. `open_card_for_fault` suppresses on `source_ref LIKE 'autofix:<ident>|%'` for any card not done/verified/discarded — so a HAND-FILED card carries no signature and can never suppress, and discarding the autofix ones removes the only cards that could. The two look identical on the board: same title shape, same status vocabulary, no visible difference between a card the detector will honour and one it cannot see. `discarded` not suppressing is DELIBERATE and correct (it is what lets a genuinely new occurrence file after a judged one), so every individual piece behaved as designed while the composite guaranteed a re-file loop.
COST: Three discards, four cards, and the wrong conclusion available at every step — the obvious reading is "the dedupe is broken", which is what I would have reported if I had not gone and read `fault_identity`. The detector was right and I had deleted its memory. Also self-inflicted noise on a shared board while the underlying outage sat correctly parked in `needsyou`.
FIX: none yet. Immediate workaround, applied: copy the autofix signature onto the hand-filed card's `source_ref`, which makes it suppress (verified against the LIKE). Two candidate real fixes, cheapest first: (a) `amux board discard` warns when the card carries an autofix signature AND is the last non-terminal card holding that ident — a discard that turns the detector back on should say so; (b) `board add` for a fault already carded by autofix is the wrong move entirely and the honest path is folding the diagnosis INTO the autofix card, which nothing currently suggests. The transferable shape: a card's suppressing power lives in a field nobody looks at, so two cards that read identically to a human behave oppositely to the detector.

## `git commit -a` in a shared checkout swept three lanes' in-flight work into one lane's commit, twice in four hours
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-30
SESSION: amux
CARD: AF-805
ORIGINAL_CARD: AF-342
SYMPTOM: Mid-task on AMUX-3886 I had ~87 uncommitted lines in
 crates/amux-server/src/api/browser.rs (a `with_cause` helper plus 28 call sites).
 ts-gke committed 78009d90, "browser-reaper: add hard TTL to kill old browsers
 regardless of page state", touching the same file for an unrelated reason. All 87 of my
 lines went in with it. `git log -S with_cause --oneline` now answers with a commit about
 a TTL arm. I found out only because `git diff` on my own file came back a single hunk
 when I had made two, which is a coincidence of what I happened to check next.
COST: About 25 minutes: reconstructing what had moved, proving the sweep from
 `git log -S`, and then rebuilding a mine-only tree in a scratch worktree because the
 shared checkout by then held three lanes' in-flight edits and would not compile. The
 durable cost is the record: the fix for a browser 502 is filed under a browser-reaper
 TTL commit, and the next person to run `git log -S` or `git blame` on it gets a wrong
 answer with nothing marking it wrong. Not rewriting history over it — 172 unpushed
 commits with live lanes — so this entry and the follow-up commit body are the record.
SEVERITY-NOTE (appended same day, after the recurrence): raising this from `slows`.
 It happened AGAIN four hours later, same lane. 8a990ebd, "browser-reaper: activity arm",
 carries THREE lanes' work: my remaining AMUX-3886 change (+281 integrations/browser.rs,
 +59 api/browser.rs), amux-frustrations' entire AF-342 fix (+199 git_guard.rs, +100
 test-staged-guard-render.sh, the hook, checks.yml, their ledger entry), and ts-gke's own
 reaper arm. The second sweep landed AFTER ts-gke had read the diagnosis of the first,
 agreed with it in writing, and said they were adopting the explicit-paths guard. So this
 class does not require a careless session; it requires a lane that intends the right
 thing and reaches for a familiar verb.
 AND THE FIX FOR THIS WAS ONE OF THE THINGS SWEPT. amux-frustrations had AF-342 STAGED,
 holding the commit on a full-suite result, when someone else's commit took the index. A
 lane that stages early and verifies before committing is MORE exposed, not less, because
 its work sits in the shared index longer. That is the argument against every advisory
 guard on this path.
 ATTRIBUTION CORRECTION (same day, after ts-gke checked my evidence). I claimed above
 that both sweeps were the SAME LANE and leaned on "same Amux-Session AND same
 Amux-Conversation" as two agreeing signals. They are ONE signal. Read
 .git/hooks/prepare-commit-msg: `stamp="$AMUX_SESSION"`, then `conv` is a lookup of
 `~/.amux/sessions/$stamp.meta.json` for `cc_conversation_id`. The conversation field is
 DERIVED FROM the session field, so a wrong stamp produces a wrong conversation id
 identically and the commit reads as doubly confirmed. Everything reduces to one
 env var in whatever process ran `git commit`, and AMUX_SESSION is inherited by any
 child of a lane.
 So "two sweeps by one lane, the second after that lane agreed in writing" is NOT
 established, and I withdraw it. What survives: two sweeps happened, and the mechanism
 is `git commit -a` (established independently — my UNTRACKED test file was not taken
 while every modified TRACKED file was, which `git add -A` would not produce). The class
 argument does not need the actor to be identified, which is the useful part.
 Contrary evidence worth keeping: all three ts-gke-stamped commits carry
 `Co-Authored-By: Claude Sonnet 4.6` while that lane runs opus-5, and `Claude-Session:
 session_01Gg7LPMY45VdVgrq29tHv2A` is on 78009d90 and 2a914717 but ABSENT from 8a990ebd
 — a field no amux hook writes. None of that is conclusive (the hook's own comment
 measures Claude-Session on ~30% of commits, so absence proves nothing), and that is the
 point: the record cannot answer who committed, in either direction.
 CARDED as AMUX-3916: the stamp needs one field the committing process cannot inherit.
 MECHANISM, narrower than the first entry had it. My untracked test file was NOT taken
 while every modified TRACKED file was: that is `git commit -a`, not `git add -A`. `-a`
 stages every modified tracked file at commit time — exactly the set a shared checkout
 fills with peers' work — and it never touches the index beforehand, so it walks straight
 past AF-316's staging refusal. The guard to state is "never pass -a", not "prefer
 explicit paths".
FIX: This is AF-342 (filed by amux-frustrations ~20 minutes before 78009d90 landed)
 seen from the other end, and it CORRECTS one clause of that entry. AF-342's COST says
 "The guard correctly kept the peer's two dirty browser.rs files OUT of the commit, so
 its load-bearing half worked." On the very next commit, on one of those same two files,
 it did not: the load-bearing half is exactly what failed here. Both observations are
 real — amux-frustrations was warned and stopped, ts-gke was not — which means the
 guard's protection is not a property of the guard, it is a property of whether the
 committing session happens to read 93 lines of warning it has learned to scroll past.
 That is the argument AF-342's own SYMPTOM makes ("warnings that fire on the normal path
 are the ones people learn to scroll past, which is how the peer-hunk case gets missed"),
 now with the case attached. ts-gke's diagnosis, unprompted and worth keeping: the
 property the guard needs is "this path has no edit record from the COMMITTING session",
 not "this path was edited via shell" — heredocs are one way to be invisible, and a
 codegen step, a `git checkout` and a peer's editor are three more. Scope AF-342's fix to
 the general property.

## A trustworthy test run on a contended file now requires a private worktree, and each one costs a full dependency rebuild
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-30
SESSION: amux-frustrations
CARD: AF-336
SYMPTOM: Verifying the AF-342 fix, `cargo test -p amux-server --lib git_guard` failed to
 compile for ~35 minutes on errors entirely inside a peer's in-flight
 crates/amux-server/src/api/browser.rs (E0308 tuple arity, then an unterminated json!
 macro) while three lanes edited the tree. `cargo test` builds the TREE, so a red result
 said nothing about my change and a green one would have been equally uninformative.
 Both amux and amux-frustrations independently reached for the same workaround in the
 same hour, neither having proposed it to the other: `git worktree add --detach <tmp>
 HEAD`, apply only your own diff, test there.
COST: ~35 minutes of blocked verification on this pass, plus a full dependency rebuild
 per worktree because CARGO_TARGET_DIR keys on the workspace path, so the shared build
 cache does not carry over. The durable cost is that the sanctioned verification command
 in VERIFY.md is now untrustworthy for any contended file, with nothing in its output
 saying so: scripts/test-contended.sh reports whether a BUILD was running, which is a
 different question from whether a peer's half-saved source is in your tree. Two lanes
 converging on an unshared workaround in one hour is the signal that it is the norm.
FIX: AF-336 (per-lane worktree) ends this class rather than detecting it, and this entry
 is evidence for it rather than a new proposal. Until then the cheap half is honesty in
 the instrument: have scripts/test-contended.sh report, beside its result, whether any
 tracked source in the crate under test is dirty and attributed to another session. A
 compile failure in a file you did not touch would then read as such instead of as your
 own regression.
STATUS-2026-09-10: THE CHEAP HALF SHIPPED (c7911c2d). test-contended.sh now prints
 "N of M are under <pkg>/, the package this command selected with -p <pkg>" beside its
 result, resolved from `cargo metadata`. Mutation-checked in scripts/test-selector-clauses.sh
 (cells 7-9: in-package, out-of-package, no -p at all). THE REAL FIX IS STILL OPEN: this
 only detects a dirty peer file honestly, it does not stop one from being able to redden
 a red you did not cause. A trustworthy run on a contended file still requires the private
 worktree this entry named. Entry stays open on that clause.

## The observed-edit record has no content hash, so "who edited this" is unfalsifiable by construction
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-31
SESSION: amux-frustrations
CARD: AF-806
ORIGINAL_CARD: AMUX-3954
SYMPTOM: The staged-guard named me as a co-editor of
 crates/amux-server/src/runtime_jobs/autofix.rs. Three timestamps break the claim:
   my observed record for that path   20:41:38
   the file's actual mtime            22:06:42   <- the bytes that were committed
   the mass `cargo fmt` sweep         22:10:14   (alerts.rs, auth.rs, ~180 files)
 My record is 85 minutes BEFORE the write whose content landed, and the file is 3.5
 minutes off the fmt sweep, so it was a third, separate write. The record is
 `<ts> <session> n=<count> paths=<names>` with no hash anywhere (confirmed in the writer
 by amux), so the guard compares a TIMESTAMP WINDOW against a file that moved, and any
 write to that path inside the window inherits whoever's window it was.
COST: Two mis-attributions by one lane in a single day. This one, and earlier amux told
 ts-gke their commit had absorbed 220 lines — the trailer evidence showed the commit was
 not even ts-gke's conversation. Different signal, same shape: a name with no way to test
 it. Each costs a round trip between two lanes to disprove, and the durable cost is worse
 than the minutes: a guard that names the wrong peer teaches lanes to discount it, which
 spends the credibility it needs for the cases where it is right. On this same day the
 SAME guard correctly stopped a real sweep, so both outcomes are live.
FIX: Hash each path at observation time and compare against the staged blob — match, name
 them; differ, drop the name and say why. That turns "someone touched this path recently"
 into "someone touched THIS CONTENT", which is the claim the warning already makes in
 prose. Tracked as AMUX-3954, deliberately NOT built at the end of a long session: it is a
 change to a safety-critical guard, which is how a fix becomes the next incident.
STATUS-2026-09-11: THE CHEAP HALF SHIPPED (commit 6278427f). Every co-edit claim the
 guard evaluates — fired in full or downgraded by the existing
 AF-391/MC-1561 corroboration checks — is now logged to
 ~/.amux/staged-guard-mirror-notices.jsonl, so "how often is a fired claim right" is
 finally a query instead of whoever happened to check that day. Pure additive logging:
 no verdict changed, no content hash added. 4 new cells
 (scripts/test-staged-guard-coedit.sh, 8 passed -> 12 passed), mutation-verified: killing
 either log call site, and killing the peer-guard on the fired call, each reddened
 exactly the cell naming that property. THE REAL FIX NAMED ABOVE —
 hash each path at observation time and compare against the staged blob — is still
 open. The signal is still time-keyed, not content-keyed; this entry stays open on
 that clause.
NOTE THE THIRD OUTCOME, because neither party had a slot for it: this was not "you were
 right" or "I was wrong". The signal was REAL and pointed at the WRONG EVENT. An
 attribution system keyed on time rather than content will keep producing that verdict,
 and the AF-179 caveat is doing real work — it is why amux hedged instead of asserting —
 but a caveat cannot make an unfalsifiable signal falsifiable.

## Reading the shared worktree to understand code returns a peer's draft, and the wrong decision leaves no artifact
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-02
SESSION: amux-frustrations
CARD: AF-336
SYMPTOM: Reported by general-canvas-apps, self-traced by mixpeek-homepage-claude. A lane
  changed a PUBLIC ARGUMENT'S SEMANTICS after reading a gate's invocation out of the
  shared worktree, which held another lane's uncommitted draft of the same job. The
  draft's line was broken. The committed line was correct and carried a comment, three
  lines from the one they quoted, that would have stopped the change.
  DISTINCT FROM THIS CARD'S OTHER ENTRY, which is the BUILD case: there, a peer's
  in-flight edit reddens your test run, which is loud and self-correcting on a rerun.
  Here the tree poisons a DECISION. Nobody pushes anything, the reader's commit is
  entirely their own work and looks correct, and the wrongness lives in a conclusion
  drawn from bytes that were nobody's committed truth.
COST: One wrong public-API semantics change, caught only because its author went back and
  traced their own reasoning. THE REAL COST IS THAT THERE IS NOTHING TO COUNT. The four
  write-side races on this card each left a diff and all four were caught — three by the
  victim running a receipt diff, one by the racing author. This class leaves no diff, no
  repair commit and no receipt, so the observed rate of one is not a measurement, it is
  the absence of an instrument. It also retires the strongest objection to AF-336: at
  four catchable races the counter-argument was "the cost is repair commits and may be
  cheaper than 125 worktrees", and a class with no artifact has no such bound.
FIX: Two halves, and only the first is shipped.
  DISCIPLINE, done: ~/.claude/CLAUDE.md's shared-checkout section covered a peer's edit
  redding your BUILD and said nothing about a peer's draft poisoning your READING. It now
  carries the distinction, the specimen, and the two commands — `git show
  origin/main:<path>` for what everyone actually runs, `git show HEAD:<path>` for what
  this checkout last committed — with general-canvas-apps' line kept because it is the
  memorable form: a worktree read is a snapshot of nobody's truth.
  ISOLATION, still needsyou on AF-336: per-lane worktrees make the read CORRECT rather
  than merely well-advised. That is the difference between a rule every lane must
  remember on every read and a property of the environment. A rule that must be
  remembered is exactly what this file exists to stop relying on.

---

## Runtime hook copies drift from HEAD silently — install.sh has no supervision
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux
CARD: AF-670
ORIGINAL_CARD: AMUX-99
SYMPTOM: GET /api/health/invariants showed hooks.report_hook_matches_committed
  and hooks.shared_guard_matches_committed both failing — runtime hook sha
  differs from the sha baked into the running binary. ~/.amux/hooks/
  git-shared-guard.py and ~/.amux/hook-report.sh were both installed 2026-08-30
  20:48 and never reinstalled since, while their source kept getting real
  commits — most notably e782b68a (AMUX-3932), a genuine guard-BYPASS fix
  ("command substitution inside a quoted argument bypassed the shared-checkout
  guard"). That fix passed every CI gate and sat in git history, never live on
  this box, because nothing re-runs install.sh's hook-install step
  automatically. AMUX-28/AMUX-29 already covered this exact invariant pair and
  are marked done with no evidence recorded on either — the drift came back
  because the underlying gap (install.sh only runs manually, unlike the Rust
  binary auto-builder / amux-builder.timer) was never closed the first time.
COST: a real security-relevant fix (a shared-checkout guard bypass) sat
  undeployed for days on a box running unsupervised agents against a shared
  checkout, with the health invariant correctly flagging it the whole time and
  nothing consuming that signal. Discovered only because this session was
  sweeping GET /api/health/invariants for other reasons.
FIX: manually re-ran install.sh's own install_hook_from_head sequence for both
  files (git show HEAD:<rel> + chmod +x + sha256 sidecar). Confirmed live:
  invariant failures dropped from 6 to 4, both hooks.* entries cleared.
  NOT fixed: the durable gap. AMUX-99 is the recurrence card and names the two
  real options (a systemd timer polling install.sh's hook block the way
  amux-builder.timer polls the Rust build, or the invariant self-healing since
  it already computes the right bytes) — a design choice, not made here.

## Claude completion notifications could precede the subagent's actual completion
AREA: provider-integration
SEVERITY: slows
STATUS: open (provider-side notification defect; amux lifecycle handling is fixed)
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: AF-807
ORIGINAL_CARD: ATE-10
SYMPTOM: Claude produced an initial subagent completion notification while that agent
  still reported waiting and its requested file did not exist; a second notification
  arrived only after the file was actually written.
COST: Treating notification prose as lifecycle truth would have marked delegated work
  complete early.
FIX: Amux does not infer lifecycle from Claude's notification text. The status fix
  consumes the provider's explicit subagent start/stop hooks and keeps notification
  content as display-only evidence. The provider-side duplicate/early notification
  remains outside this repository.

## A correct answer makes a wrong reason feel checked, and the reason is what gets generalised into a rule
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-445
SYMPTOM: Named by mixpeek-cicd, 2026-09-03, about their own near-miss, and it applies to two
  of mine from the same day. Three instances, all with the same shape: a TRUE sub-fact made a
  FALSE conclusion feel established, and in every case the conclusion was about to become a
  rule rather than a one-off answer.
    1. (mixpeek-cicd) They cleared three staged-guard notices correctly and generalised the
       reason into a proposed guard change: downgrade when provenance is `observed`, because
       observed means no recorded edit. What actually settled their three cases was different
       and per-instance — the trailer named a peer, their own commits on that path were days
       old, and they knew from memory they had not opened it. Their words, which are the
       entry: "I picked `observed` as the safety discriminator while producing nothing but
       `observed` records all night, which is a fair definition of not having checked." Every
       file they shipped that day was a heredoc write, i.e. exactly the record their rule
       would have dismissed. Three right answers, one wrong rule, aimed at a guard every lane
       reads.
    2. (mine, AF-290) The card said seven session verbs are duplicates "another route already
       expresses", and a `mutate.sh` run had PASSED — route.callers_have_routes did not fire
       when the routes were deleted. Both true. The conclusion was false: `/api/workers/{id}`
       is mounted and resolves NOTHING (0 of 12 fleet lanes, 0 workers against 129 sessions),
       so migrating would have handed the dashboard "worker not found" on every destructive
       path. The passing mutation is what made the premise feel verified; it asks whether a
       route EXISTS, not whether it ANSWERS.
    3. (mine, AF-346) The card said the slim board serializer "drops desc and log, which is
       why the response carries none". The response does carry none — true, and checkable in
       one curl. The conclusion, that hydration can stop selecting them, was false: the slim
       branch makes five derivations over those columns. The correct observation is what made
       the plan look established.
COST: none shipped, in all three, and that is the problem with counting it. Instance 1 was
  caught because the recipient of the proposal had spent the day writing heredocs and
  recognised the record; instance 2 because I probed a running server instead of reading the
  card; instance 3 because I read the serializer instead of the card's summary of it. Each
  catch was a coincidence of what the reader happened to have in hand that hour. The rate at
  which this class is CAUGHT is not evidence about the rate at which it OCCURS, and all three
  were one review-pass away from becoming a rule other people would follow.
FIX: no tooling proposed, deliberately. `mutate.sh seams` and `survey` both answer "is this
  held?"; neither can answer "is the reason for this the reason it is true?", which needs a
  second derivation rather than a second run — and instance 2 is the proof, because a
  mutation PASSED and that pass is what did the damage.
  mixpeek-cicd's sentence is the whole of it and is worth quoting rather than paraphrasing:
  the answer being right is what makes the reason feel checked. The practical form, which is
  the only part that has ever worked for me: when a correct answer is about to become a RULE,
  re-derive it from a different starting point than the one that produced it. Instance 2 took
  a live probe against a running server, instance 3 took reading the code rather than the
  card, and instance 1 took a reader with different recent history. None took more than
  minutes; all three took a DIFFERENT SOURCE, not more care with the same one.
  Logged rather than built because I do not have a mechanism and would rather say so than
  ship a checklist item that joins the prose nobody enforces.
INSTANCE 4, and it is MINE, produced inside the card for this entry within the hour. Having
  written "no mechanism proposed", I built one: group the request log by family, flag any
  family that was called and never returned 2xx. It reported ONE finding across 89 families
  and looked clean and cheap. /api/workers was not in it — the family reports 4,016 of 4,394
  succeeding, because /api/workers/{id}/<verb> is 4,006/4,368 while /api/workers/{id} itself
  is 1/17. So the detector I wrote to catch instance 2 answered CORRECTLY at the granularity
  I chose and could not have found instance 2. A pass from it would have felt like evidence
  that AF-290's premise was fine. Re-run by ROUTE SHAPE it finds the defect immediately:
  713 shapes -> 9 candidates -> 1 survives a "is it actually mounted" filter, which is
  `GET /api/workers/{id}` at 0/15. Predicate and blind spots recorded on AF-298.
INSTANCE 5, from mixpeek-cicd, applying this entry to their own work an hour after reading
  it — and it sharpens the entry's own remedy rather than repeating it. They had pinned a
  config file with an assertion that the line above a key STARTS WITH `#`. `# TODO: revisit
  this setting` satisfies it, while the comment's actual job is to stop a future editor from
  restoring pytest defaults and silently deleting 49 tests. A comment-EXISTS check wearing
  comment-ANSWERS clothes.
  THE PART THAT CHANGES HOW I WORK: they had mutation-tested it. Their mutation DELETED the
  comment, which the weak assertion already caught, so the mutation passed and told them
  nothing. Their words: "A mutation is derived from the same understanding as the assertion,
  so it inherits the same blind spot by default. Mine was not a second derivation, it was the
  first one run backwards."
  That lands directly on this session, which has treated a killed mutation as proof roughly
  twenty times today. A killed mutation proves the assertion catches THE FAILURE I IMAGINED.
  It says nothing about the failure I did not. Their tell is the cheap version and it costs
  one sentence: STATE A MUTATION THE ASSERTION SHOULD CATCH AND DOES NOT. If you cannot
  generate one, that is a fact about your imagination, not about the assertion.
  Applied immediately to instance 4's own predicate before proposing it, which produced four
  blind spots I would otherwise have shipped silently — the worst being that it keys on
  STATUS, so a route answering 200 with an error body passes it, across 1,646,523 2xx rows
  nothing inspects for that shape.
THE TAXONOMY, from mixpeek-cicd reading instances 4 and 5 back and refusing to let them be
  one thing. Three shapes, and the remedies differ, which is why separating them is worth the
  paragraph:
    NARROWER than the question. The predicate is weaker than the property, over the right
      object. Their pytest.ini: "line starts with #" against "the comment explains why not to
      change this". Remedy: state a mutation the assertion should catch and does not.
    COARSER than the question. The predicate is right, the population is a superset that
      CONTAINS its own counterexample. My /api/workers: 4,016/4,394 at family level is a true
      number that includes the 1/17 it hides. Remedy: re-key at the granularity of the
      finding. Their sentence for why this one survives review better: the number it reports
      is genuinely true.
    WRONG FIELD. The predicate is right-shaped and reads a different field than the one
      carrying the answer. Blind spot 4 above: keyed on STATUS, so a 200 with an error body
      passes, across 1,646,523 rows every one of which is genuine evidence of something you
      are not asking about. Remedy: ask which field carries the answer before asking whether
      it is held.
  ONE CLAUSE OF THEIRS IS TOO STRONG, and saying so is the same courtesy they paid me on the
  absorption wording. They wrote that no amount of second-derivation fixes the coarse case,
  "because the second derivation would also have been per-family". In fact the live probe —
  `GET /api/workers/{lane}` -> 404 across 12 lanes — is what found it, and that IS a second
  derivation from a different source. What their argument correctly establishes is narrower
  and more useful: A SECOND DERIVATION HELPS ONLY IF IT VARIES THE DIMENSION THE FIRST ONE
  COLLAPSED. Same source at a finer granularity works; a different source at the same
  granularity does not. "Re-derive from a different source" was my own remedy two paragraphs
  up and it is underspecified: the axis matters more than the source.
INSTANCE 6, mixpeek-cicd's, and it is the coarse shape on a third surface — which matters,
  because three instances in one repo would be three names for one thing. Their words:
    "npm audit reported `1 high` on the homepage lockfile. The count is accurate and names no
    package, so it cannot be routed: severity is an aggregate over advisories, and the
    decision needs the advisory. `npm audit --json` per package is the finer key, and it
    turned a number into a name. The failure mode is not a wrong count, it is a correct count
    that excludes the item, which is why nobody challenges it and why it sat."
  A CI guard, a route table and a package audit. Three surfaces that fail differently, one
  shape.
AND A DEFECT IN HOW THIS FILE IS WRITTEN, which is mine and worth more than the instance.
  mixpeek-cicd built their too-strong clause from my WRITE-UP order — family detector first,
  live probe second — when my WORK order was the reverse. Their note on it: an account of a
  finding is ordered for the reader, so treating its sequence as causal is a free way to be
  wrong about method. Every entry in this file is ordered for the reader. When the ORDER is
  load-bearing for the method — when the point is which step found the thing — say which
  order you are giving, because a reader reasoning about method from a narrative sequence is
  doing something reasonable that the narrative did not warn them about.
THE UNIFYING FORM, mixpeek-cicd's, better than my "no remedy subsumes another": each shape is
  a PROJECTION that loses a different dimension, so a remedy restoring one cannot restore the
  others. Narrower loses predicate strength, coarser loses granularity, wrong-field loses the
  field. That is also why their enumeration guard and ts-gke's denominator check are not
  ranked — projections of one corpus along axes neither reaches from the other.
  Their consequence, which is the sentence I would put at the top of this entry if entries had
  tops: "my guard passes" is never a statement about the system, only about the axis, and the
  only honest closing line is which axis somebody else is holding.
NOTE: distinct from AF-435 (checks that ran, passed and could not have failed). That one is
  about an instrument with no discriminating power. This is about an instrument that
  discriminated CORRECTLY and a human generalising the wrong invariant from the result.
  Instances 4 and 5 are the bridge between them: a check with real discriminating power, at
  the wrong granularity or over the wrong property, produces a TRUE result that supports a
  false conclusion — and a mutation drawn from the same understanding confirms it.

## staged-guard blocks on an edit-ownership record that a plain `git diff` is enough to create
AREA: attribution
SEVERITY: blocks
STATUS: open
DATE: 2026-09-03
SESSION: amux
CARD: AF-808
ORIGINAL_CARD: AMUX-4083
SYMPTOM: Two independent blocks in one hour, both false, both naming a session
  that had only READ the file.
  (1) mixpeek-oss went to commit two browser.rs paths and staged-guard refused,
  reporting that session `amux` had an edit record on both files 3 minutes
  prior. What `amux` had actually done in that window was `git diff` and
  `grep` on those paths, to describe them accurately in a message ASKING
  mixpeek-oss to commit them. No write. They cleared it with
  AMUX_VERIFIED_SOLO=1 after checking the diff content and line counts were
  identical before and after.
  (2) Fifteen minutes later the guard blocked `amux` from running
  `git checkout --theirs` on app.css and sw.js to resolve a MERGE CONFLICT,
  naming amux-homepage: "discarding a file ANOTHER SESSION HAS ALSO EDITED ...
  UNRECOVERABLE". Reconstructing the ours-side of the conflict and diffing it
  against HEAD gave 0 differing lines for sw.js, and every app.css difference
  traced to #184's own auto-merged hunks. No peer content existed in either file.
COST: About 25 minutes across two sessions, and a cross-session round trip that
  existed only to clear the first block. The second one is worse than the time:
  the refusal text says UNRECOVERABLE and instructs you to stash or ask the named
  peer, so the honest response to a false positive is to stop and ask a session
  that has nothing to do with the file. It also teaches the wrong lesson, since
  the way past it is an override flag, and a guard whose normal resolution is its
  own bypass stops being read.
FIX: Do not derive edit ownership from mtime alone. CLAUDE.md already states the
  rule the guard violates: "An owner derived from mtime is not evidence ...
  reports whoever was ACTIVE, not whoever WROTE, because every lane shares the
  cwd." Record ownership from an actual WRITE — the PostToolUse hook already sees
  Edit/Write tool calls and could stamp content identity (a hash of the file
  before and after) instead of a timestamp. AMUX-3954 is the same defect stated
  as "an observed co-edit record carries no content identity, so it names a
  session for a write it did not make"; this entry is two measured specimens of
  it, one of which blocked a peer rather than the recorder. Second, a file in
  CONFLICTED state is a distinct case the guard does not model: its content is
  git-generated, so "another session also edited it" cannot be inferred from the
  working copy at all.
CO-SIGNED: mixpeek-oss, who hit specimen (1) from the blocked side and
  independently verified it the same way ("read-only git diff/grep during
  message composition, flagged as an edit ... a signal with no way to
  distinguish read from write").

## An archived card is listed as actionable and refuses every closing action

AREA: board
SEVERITY: wrong-conclusion
STATUS: open
DATE: 2026-09-03
SESSION: gtm-engine
CARD: AF-460
SYMPTOM: a card can hold `archived: 1`, `status: backlog` and `closed_at: None` at
 once. It appears in the DEFAULT `/api/board` list, which is what the idle nudge
 reads, so it is offered as a drainable backlog card with "you have to pull from
 it". Every closing verb then refuses with `archived_task_immutable` / "task is
 archived; restore it first". The nudge says drain it; the board says you cannot.
 The asymmetry is what makes it permanent: `--trigger` DOES work on an archived
 card, so such a card is silenceable forever and closeable never.
COST: 26 days on GE-564, whose trigger sat 617h stale while it re-listed. A triage
 on 2026-08-20 chose ARCHIVE, the archive neither closed nor hid it, and nobody
 could close it afterwards. SECOND INSTANCE, and mine is the worse one: I hit the
 identical refusal on AF-224 the same day and read it as "already archived, no
 action needed" rather than as a defect. A lane that shrugs at the refusal never
 reports it, which is why one card absorbed 26 days before anyone said so.
FIX: not chosen — three candidates land in different places and it is a data-model
 call: (1) archiving sets a terminal status, (2) the default list excludes
 archived, (3) the nudge filters them. Recommending (1), because (2) and (3) leave
 a card that is simultaneously backlog and archived and merely stop showing it to
 one reader. Workaround that works today and is documented nowhere: PATCH
 archived:0, then done. Companion entry: the refusal message is correct and only
 reaches you when you ACT, never where the card is listed (AF-461).

## A green shared-target build embedded another worktree's dashboard
AREA: build
SEVERITY: wrong-conclusion
STATUS: open
DATE: 2026-09-04
SESSION: amux
CARD: AF-809
ORIGINAL_CARD: AMUX-4142
SYMPTOM: A post-commit `scripts/safe-cargo.sh build -p amux-server` in the
 Basecoat integration worktree exited 0 and `/health` reported that worktree's
 `11c1b789` commit, but the same process served `APP_VER=0.9.804` and no
 `ui-system.js` from another worktree instead of its own `0.9.807` Basecoat
 assets. Both worktrees use the required shared `CARGO_TARGET_DIR`; Cargo
 treated the other checkout's `amux-dashboard` RustEmbed artifact as current.
COST: Seven minutes, an extra 2m32s server build, and a browser run that would
 have falsely certified the old UI if it had checked appearance without joining
 `/health.commit` to the actually served asset version.
FIX: Open as AMUX-4142. Make embedded-asset provenance part of the build
 fingerprint or have the build/deploy gate compare served APP_VER/CACHE with
 the source tree and emit a sweep-visible mismatch verdict.

## Multiplayer workspace switching was replayed later as offline work
AREA: cloud
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-codex
CARD: AF-810
ORIGINAL_CARD: AC-416
SYMPTOM: In three simultaneous saved browser profiles, switching workspaces
 returned synthetic HTTP 202 `queued/offline`, reloaded as though it succeeded,
 and either stayed in the old workspace or changed context later when the
 outbox replayed. The god-mode account's switcher also rendered all 62 inherited
 workspaces as a giant green invitation banner, and chrome-cdp's shared
 `pages.json` made listing profile C erase the target lookup for profiles A/B.
COST: Ethan, god mode, and the Gmail participant could not be kept in one
 workspace long enough to prove cross-user board/log visibility; retries
 created delayed context switches, and the operator-facing page exposed the
 whole customer directory above the actual dashboard.
FIX: Workspace switching is now explicitly non-replayable, requires a real
 JSON acknowledgement, and reports `workspace_switch_failed` instead of
 reloading on failure. The gateway acknowledges JSON clients before entering a
 tenant container and logs `[org-switch] ... verdict=switched`. Org rows say
 `via_god_mode`, so inherited access remains in Settings without becoming an
 invite banner. chrome-cdp now scopes targets/sockets by profile or port and
 selects the requested profile from the multi-browser status array.

## Three-user cloud test saturated on minute-long workspace requests
AREA: cloud
SEVERITY: blocks
STATUS: open
DATE: 2026-09-04
SESSION: amux-codex
CARD: AF-811
ORIGINAL_CARD: AC-416
SYMPTOM: The Gmail workspace stayed on "Starting your workspace" for several
 minutes while board/session reads in the other two profiles took 50-135s.
 The local 8824 control plane simultaneously repeated its known failure mode:
 TCP accepted, but TLS `/health` handshakes timed out until the watchdog or a
 manual launchd restart replaced the process.
COST: A browser-created Backlog canary existed only in Ethan's optimistic page
 state; after more than a minute the owner profile still had an empty board, so
 real cross-user observation and actor attribution could not be certified.
FIX: AC-416. The saved profiles and exact three-identity browser path now
 reproduce it without credentials, and the watchdog/server log records the TLS
 hang. Diagnose tenant wake latency and the local request-path stalls before
 claiming realtime multiplayer from a cached shell.

## Staged-guard attributed this Codex task's files to two peer lanes and blocked its commit
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-09-05
SESSION: amux (Codex agent; no $AMUX_SESSION in env)
CARD: AF-812
ORIGINAL_CARD: AMUX-3249
SYMPTOM: After implementing and browser-testing local multiplayer invites, the commit
  guard attributed the staged files to `amux-cloud` and `amux-frustrations` and refused
  the commit even though every staged hunk was produced by this task. The shell had an
  empty $AMUX_SESSION, but its tmux name resolved to `amux-amux` and the installed
  MR-43 prepare-commit hook already contained that fallback, so the commit stamp and
  the edit-record ownership used by the guard still disagreed.
COST: One refused commit and about 5 minutes re-reading all nine staged files by hand
  before the documented AMUX_VERIFIED_SOLO override could be used honestly.
FIX: AMUX-3249. Attribute Codex tool writes to the active agent/session, or make the
  guard distinguish absent agent edit records from affirmative peer ownership so a
  missing producer cannot be rendered as evidence that a peer authored the diff.

## Concurrent Bash observations are treated as file ownership
AREA: attribution
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: mixpeek-ops-server
CARD: AF-813
ORIGINAL_CARD: MOS-33
SYMPTOM: A concurrent reader was named owner of three ops research files after their mtimes changed during its Bash command. The production classifier reproduces a foreign block with provenance observed while its explanation asserts a transcript write. A later observation can also replace an existing recorded writer.
COST: The reader had to disown files it never edited; publication required an ownership check and this repair.
FIX: Keep mtime observations in a separate, counted advisory. They cannot name an owner or replace recorded edits; preserve recorded-writer and blind-cotenant protection. Regression: concurrent_reader_observations_cannot_claim_the_ops_research_files.
VERIFIED: dd416c753b24 is running (build eee97f2b86189c02). The same five staged paths changed from three observed-only foreign blocks to zero foreign owners, with three advisory paths/four observer records retained; the MOS-33 log records that denominator. Final source passes 70 guard tests, six real hook-main controls, existing protection checks, clippy and cargo check.

---
## Fleet read as stopped while its original tmux server still held 62 sessions
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-09-07
SESSION: amux
CARD: AF-814
ORIGINAL_CARD: AMUX-4203
SYMPTOM: At 17:27:23 EDT fleet captures began timing out; at 17:29:49 the socket refused connections. A new tmux server created at 17:30:07 replaced the default socket while its original owner remained alive with 62 sessions. /api/debug/tmux measured only the replacement (7 sessions at first inspection), and invariants classified the original workers as stopped. Kernel socket owners and server identity were absent from both instruments.
COST: 30 minutes with most of the fleet inaccessible before diagnosis; original sessions had to be recovered via a separately preserved socket and 57 non-archived workers reconciled with the replacement fleet. The initiating stall cannot be proven from retained logs.
FIX: AMUX-4203 adds independent socket-owner evidence, persistent stall process/stack samples, an invariant WARN, and guarded tmux creation. The initial stall remains unproven; do not read recovery as proof of its cause.

---
## Worker startup joined the environment cleanup and agent launch into one shell command
AREA: cli
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AF-815
ORIGINAL_CARD: AMUX-4203
SYMPTOM: During fleet recovery, handoff-consumer-0907 displayed `unset ANTHROPIC_API_KEYclaude --dangerously-skip-permissions ...`; bash rejected the agent flags as unset identifiers. Two other workers remained at shell prompts after accepted starts. `type_line` used separate tmux clients for literal input and Enter, discarded errors, and proceeded to the next command under capture load.
COST: Three individual launch retries, plus manual verification that all 57 non-archived workers had live agents rather than merely an accepted start response.
FIX: Submit each literal line and Enter together in one tmux command queue. WARN with shell_line_submission_failed when tmux does not confirm it, without recording shell command contents. A private-socket regression test verifies two complete shell commands reach the shell.

---
## Bounded fleet probes manufactured timeouts by waiting before reading their output
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AF-816
ORIGINAL_CARD: AMUX-4203
SYMPTOM: Both synchronous fleet-probe runners polled child exit before draining stdout/stderr. A child writing 262,144 bytes filled the pipe and was killed on its three-second deadline; both regression tests failed against the shipped functions. The capture runner justified this with “30 lines”, which is not a byte bound. Its timeout WARN and diagnostic note blamed an unresponsive tmux without recording bytes read or whether the child had already exited.
COST: The original fleet incident produced 511 capture timeout warnings during 17:27–17:29, but the instrument could not distinguish an upstream stall from its own unread pipe. The investigation had to reproduce the runner separately before its timeout verdict could be trusted. Current captures were below pipe capacity, so this entry does not claim the pipe defect initiated that incident.
FIX: Drain both pipes nonblockingly while polling the child, with the deadline covering continuously producing children and descendants holding a pipe after the child exits. Preserve byte counts, PID, elapsed time and child-exit versus pipe-EOF phase in WARN logs and /api/debug/tmux, including when the diagnostic's own fleet query fails. Trigger bounded independent tmux/host evidence collection on the first timeout, at most once per minute; retain host load and processes ranked by CPU/RSS without process arguments. Regression fixtures test large stdout and stderr, real hangs, continuous output, inherited pipes and successful output preservation.

---
## A clean detached tree ran a dashboard test executable from another checkout
AREA: gates
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AF-817
ORIGINAL_CARD: AMUX-4225
SYMPTOM: A full gate on clean 21909b7e reported a missing cache prefix and a
  card-syncing assertion absent from that tree. The executable in the shared
  target directory embedded a different PR review checkout as its manifest
  path. Clean source did not imply that the process executed its test binary.
COST: A full validation run spent more than 15 minutes and reported stale-code
  failures that could have prompted edits to already-correct source.
FIX: For this proof, Cargo's RUSTC_WORKSPACE_WRAPPER namespaces workspace
  artifacts while retaining the one shared CARGO_TARGET_DIR. The wrapper pins
  the server from its hashed compiler output for the existing AMUX_RESTART_BIN
  test seam and logs manifest/full-commit origins, refusing a source mismatch.
  The private receipt and reproducible wrapper are in ~/.amux/logs/amux-4225/.
  This corrects the verification setup; the default test-contended warning
  alone remains insufficient proof of executable provenance.

---
## A worked human command disappeared from Doing back into Backlog
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-09
SESSION: mvs-research
CARD: AF-818
ORIGINAL_CARD: MR-174
SYMPTOM: MSG-50976 correctly linked to MR-174, status-update correctly claimed the
  card as doing, and the worker registered its board-drain report asset. At 08:12
  the unchanged captured-prompt envelope nevertheless accepted `doing -> backlog`,
  gained a 14-day revisit plus a prose trigger, and the board then truthfully showed
  no active task while the original command had no terminal or decomposed disposition.
COST: The user had to compare Messages, card history, artifact links, the worker
  terminal, and `/api/debug/board-drive` to determine whether work happened. The
  drive loop then held all 15 backlog cards as trigger-parked, so an orchestration
  command to grind out the board became indistinguishable from future blocked work.
FIX: Refuse an unreshaped capture envelope retreating from doing to backlog or todo,
  with the named `capture_requeue_refused` log marker and a structured response that
  requires the model to discard, reshape one task, decompose into ordered children,
  or record a terminal disposition. A same-PATCH desc rewrite preserves ordinary
  parking, and attributed reasoned force remains as the audited escape. The production
  MR-174 shape plus positive controls run through the real PATCH handler in tests.

---
## Team creation timestamps were missing from the unit registry
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-09
SESSION: amux-testing-e2e
CARD: AF-819
ORIGINAL_CARD: ATE-128
SYMPTOM: The authored-entry audit's isolated timestamp_units_declared target
  failed on org_teams.created_at. The earlier full CI run stopped at another
  integration target before reaching this guard, so a green library result
  did not cover the new migration's timestamp contract.
COST: A missing declaration from 0060 remained hidden behind an earlier CI
  failure and required a separate focused audit to identify.
FIX: Declare org_teams.created_at as seconds, matching both Rust timestamp()
  writers and migration strftime('%s'). The existing schema.timestamp_units_declared
  and timestamp-unit runtime invariants expose missing declarations and drift;
  the migration-chain test supplies the regression and measured scan control.

---

## Cold card acceptance can be intercepted by the onboarding tour
AREA: tests
SEVERITY: blocks
STATUS: open
DATE: 2026-09-09
SESSION: amux-testing-e2e
CARD: AF-820
ORIGINAL_CARD: ATE-130
SYMPTOM: An isolated 54-case browser audit had five cold #issue navigation
  timeouts. A focused unchanged-source rerun passed 8/9; the remaining desktop
  card-details case reached its asset assertions, then the onboarding backdrop
  intercepted the History click. Screenshots confirm that last cause; the
  initial missing-overlay failures are not yet attributed to the same cause.
COST: Card, callback and terminal-summary validation required a second run to
  distinguish their actual contracts from unrelated first-run setup behavior.
FIX: Open. Reproduce with explicit onboarding/configured-install controls and
  preserve intended card navigation. ATE-130 retains both runs and screenshots;
  do not treat retries or a global removal of onboarding as a product fix.

---
## Numbered terminal output detached its source gutters on phones and reparsed loaded history while streaming
AREA: browser
SEVERITY: blocks
STATUS: open
DATE: 2026-09-09
SESSION: amux-testing-e2e
CARD: AF-821
ORIGINAL_CARD: AF-640
SYMPTOM: Ethan's phone terminal squeezed split diff/tool output into unreadable
  columns, wrapped code away from its line numbers and overlaid controls on output.
  The live/history split still reparsed all history on each changed snapshot and
  replaced its DOM; live ticks also walked all loaded prompt descendants.
COST: The worker terminal was unusable for reviewing changes at phone widths.
  Large active transcripts added avoidable parsing and scrolling work while typing.
FIX: Initial attempt 69490b05 introduced gutter/code cells, unified split rows below 600px,
  a separate controls row, stable ANSI-aware chunks and animation-frame burst
  coalescing. Render counters and slow-update client-debug expose regressions.
  81/81 browser scenarios and 26/26 Node tests passed; cache and mobile-layout
  mutations failed named assertions. Exact live build 9f259f186724b394/app
  0.9.853 was viewed at 390x844 and 1280x844. A 1.038MB/6000-row live synthetic
  stream kept scrollTop 1800 and chunk identity; eight active updates changed
  16 chunks, parsed 67,492 characters and preserved typed 01234567 plus focus.
  Screenshots: /private/tmp/af640-live-mobile-diff.png and
  /private/tmp/af640-live-desktop-diff.png. No claim of server pool health.
  CORRECTION 2026-09-09, originating session amux-testing-e2e: the rendering
  acceptance above was too narrow. Plain grep context such as 38- background
  matched the numbered-row heuristic, including its space-only split fallback.
  A scroll-lock badge in the toolbar flow moved the terminal each time it toggled.
  The unrelated chips pan-x pan-y change was also reverted. Authoritative amux
  commits cd8c7bfc and 91091e28 remove those parts and retain the chunk cache,
  ANSI/OSC-8 carry and frame coalescing. Do not restore the removed renderer from
  the old fixture proof. Mobile diff presentation remains unvalidated; the
  performance measurements only support the retained incremental-render path.
  The Node suite still required the removed helpers (8/8 failed before repair).
  Corrected coverage preserves literal grep/column text, measures geometry across
  repeated lock transitions at 390px and 1280px, and tests horizontal chip touch
  policy. Against the committed pre-revert source, the three text contracts fail
  for rendered-output mismatches while the five cache/coalescing tests pass.
  FURTHER CORRECTION 2026-09-09, originating session amux-testing-e2e:
  c3183a27 supersedes those partial reverts and removes the entire renderer
  rewrite, including the cache, ANSI/OSC-8 carry and frame coalescing. The amux
  worker reports a live prompt-highlight wrapper covering 44.2% of a
  106,680-character pane. Parsing input fragments let document constructs cross
  parser boundaries; the earlier passing fixtures did not establish structural
  correctness. All renderer/performance acceptance above is withdrawn, not
  evidence for re-landing that implementation. The deleted renderer suites stay
  deleted. e2e1e643 adds worker lifecycle coverage; a future renderer must also
  prove markup boundaries and visible layout, beyond preserving textContent.
  The independent 5abadb51 session-read recovery and horizontal chip gesture
  remain. The original mobile/readability and performance request stays open.
  Integration then found merges 9461039b/a29882d1 had resurrected the parser,
  inferred diff markup and deleted suites. Reconcile the authoritative revert
  with 22d1561f's tab persistence, compact controls, prompt attribution and
  history/live overlap protection; retain the later menu/path fixes and move
  314fd8b6's pane-width cap into the restored HTTP refresh path. The
  existing peek-poll client-debug beacon now reports whether the input chunk
  parser is present. Product/lifecycle tests assert the removed wrappers stay
  absent, and product checks deliver updates through refreshPeek's HTTP path.
  CI's terminal-render.mjs argument goes with the removed suite. Before the
  merge, Node 22 silently ignored that missing file and both commands passed
  the 18 surviving outage-recovery tests; that was stale wiring, not a failing
  gate. Lifecycle fixture failures also exposed a 250ms entrance-animation
  measurement, column-default rather than exact-card acknowledgements, an
  artifact refusal masking the acknowledgement checks, and a bare API DELETE
  that correctly lacked the dashboard UI token. The fixture now waits for the
  named entrance transition, distinguishes those gates, and confirms deletion
  through the dashboard. It imports the shared candidate-asset fixture so the
  installed API binary cannot silently substitute its embedded dashboard.
  No server code changes or renewed mobile/performance acceptance.


---
## The outage test still treated a refused write as a lost connection
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. Committer identity is not author validation.
CARD: AF-822
ORIGINAL_CARD: AMUX-4362
SYMPTOM: GitHub's e2e job failed the shipped-function Node test because it still expected a failed outbox write to make the connection badge read Sync error. The current product deliberately reports connection/read health separately and shows pending operation failures in the outbox.
COST: The stale assertion stopped the browser CI job before its browser cases could run.
FIX: Align the assertion with the documented connection behavior, retain the checks for pending counts and read/auth errors, and explicitly assert the failed operation still displays its error. The existing named Node assertion is the local/CI diagnostic; no runtime behavior is changed.
## A dead database writer left health green while browser sync failed
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in 091bc3a9d0c12f859dcf7d35bbf2189c8f28df61. Committer identity is not author validation.
CARD: AF-823
ORIGINAL_CARD: AMUX-4416
SYMPTOM: During host ENOSPC, heartbeat repeatedly reported "writer thread is gone" and request-log rows were dropped, while /health returned store:"ok" from a read-only probe. The browser retained 26 queued operations. Failure injection also showed journal and COMMIT errors poisoning the next transaction.
COST: Hours of failed writes could look healthy to the watchdog and an empty request-log analysis; cache deletion did not free snapshot-retained blocks. Recovery required explicit snapshot-reclamation approval and an API restart. Separate reader-pool exhaustion during recovery is not attributed to a specific borrower by these tests.
FIX: Guard every write transaction through commit, catch mutation unwinding without killing the writer, emit failure verdicts, and include a bounded no-op writer transaction in health. Four baseline regressions failed before the change; panic, journal, commit, unwritable-writer and stalled-writer cases now cover recovery. Keep the detailed read failure in the Sync error modal and update it on recovery. See docs/incidents/2026-09-11-offline-sync.md for the causal limits.

## Network-first bypass reintroduced a blocking composer and noisy sync sequence
AREA: ux
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-824
ORIGINAL_CARD: AMUX-4416
SYMPTOM: Send/Queue bypassed local persistence and waited on the API, then fell back into Queued/Syncing. An optimistic follow-up cleared draft text and uploads before durable acceptance.
COST: A slow mobile connection became a composer delay; failed local storage could lose the working draft. Automatic retries opened delivery progress during ordinary sends.
FIX: Restore both modes through the existing durable local outbox, clear only the accepted draft/files, retain newer edits, and run automatic replay quietly. Tests exercise held responses, refusal, quota failure, reload and retry on desktop/mobile/WebKit. Two contract controls reproduce the old behavior on 091bc3a9. Historical causes are recorded in docs/incidents/2026-09-11-offline-sync.md.

## Gemini idle terminal cannot receive the worker's first queued task
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-825
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Fresh Gemini CLI 0.58 authenticated and displayed an empty composer, but /api/debug/steering held its first task at not-at-turn-boundary. Workers displayed idle. The captured-frame regression returns empty status rather than idle; the thin-rule input box is also unknown to the delivery verifier.
COST: The new worker's lifecycle acceptance could not begin for more than ten minutes; no deliverables were produced.
FIX: Recognize Gemini's provider-owned footer and current input box, preserve active/picker/pending-input controls, and emit idle_display_without_delivery_boundary when the display and delivery disagree. Rerun the live provider suite before closing.

## Completion callbacks ask the requester to notify themselves again
AREA: coordination
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-826
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Gemini's same-group run completed implementation, review and handoff but kept creating review/capture tasks after completion receipts. The server appended its default "Notify the requesting worker" instruction to the callback already addressed to that requester.
COST: Repeated reviews, acknowledgement messages and capture cleanup consumed turns while the complete-board acceptance remained red.
FIX: The automatic callback is the notification. Do not add another notify instruction; suppress the old generated instruction on existing rows, preserve explicit custom callbacks, and identify receipts that need no acknowledgement. callback_echo_instruction_suppressed logs legacy rows. The regression inspects the real durable callback queue. Live loop reduction is not yet claimed.

## Removed attachment reappears after immediate mobile reload
AREA: messaging
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-827
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The consolidated mobile regression failed on desktop and mobile Chromium: a removed attachment returned after immediate reload. The chip vanished before its asynchronous IndexedDB deletion committed.
COST: Cancelled files could be unintentionally reattached. Two of 33 mobile/offline checks failed; the 256 MiB interrupted upload and checksum checks passed.
FIX: Save a per-attachment cancellation intent before removing the chip; suppress restoration and recover deletion after reload. Keep the attachment when saving cancellation fails. upload-storage reports cancellation-intent and recovery failures. The regression holds deletion forever before reloading and checks actual stored bytes are then removed.

## Gemini peer messages disappear from the terminal Workers filter
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-828
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The real Gemini pair finished the review/revision cycle and its tasks, but terminal search for REVIEW_APPROVED with the Workers filter returned zero. Messages history contained the confirmed receipt. The renderer only recognized Claude/Codex prompt glyphs, while Gemini echoes input with >.
COST: The live pair case failed after 9.9 minutes; three dependent upload/cross-group/queue cases could not run.
FIX: Recognize Gemini input glyphs only for Gemini workers, preserve multiline provenance and exclude the actual input placeholder. The regression uses the real terminal filter/search buttons and verifies non-Gemini > lines remain unclassified. Navigation diagnostics now include provider beside considered prompts and match results.

## Browser caches leave no room for the mandatory local message outbox
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-829
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Seven studio-plg composer failures on Safari 0.9.900 reported only unconfirmed. The live Safari WebApp localStorage held 5,193,082 bytes, chiefly command history and reproducible board/schedule/HTML caches; its outbox was empty. The client attempted the failed local write twice and replaced the specific quota error with a terminal-confirmation message.
COST: Messages could fail before reaching the server despite a healthy connection. A real WebKit quota reproduction against pre-fix source returned failed instead of queued.
FIX: User-intent writes reclaim only reproducible HTML/board/schedule caches and retry the same atomic write, preserving other drafts, operations, attachment journals and the offline worker list. Local refusal returns once with its storage reason; outbox-storage logs measured byte counts, browser capabilities and the failure category without content. Quiet background replay no longer announces queued-operation completion. The original seven failures lacked a reason field, so their exact exception cannot be recovered retrospectively; quota is reproduced against the observed storage condition.

## Watchdog restarts a progressing database after short health deadlines
AREA: server
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-830
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Production watchdog logs explicitly issued kickstart -k at 13:16:19 and 13:29:32 on September 11 after three health responses with measured:false / probe_deadline_exceeded. launchd recorded SIGTERM, not an application crash. The 250 ms health deadline detached its ongoing writer/read probe but discarded its later success, so each slow sample could imply a hung store despite intervening progress.
COST: The monitor itself disconnected clients and restarted the server; both restarts were followed by more slow probes rather than durable recovery.
FIX: Retain monotonic completion and in-flight ages for real probes after HTTP timeout. Readiness remains unmeasured/503; the watchdog defers a restart only with recent successful progress or bounded initial work. Real writer failures, pool exhaustion, absent listeners and stale progress retain recovery. slow_probe_completed and watchdog restart-deferred logs expose the decision. Rust exercises a blocked writer twice and requires the detached first probe's receipt during the second timeout; Python tests cover actual HTTP 503 classification and both restart/no-restart loop controls.

## Gemini New conversation restarts the old provider conversation
AREA: workers
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-831
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The real Gemini upload acceptance run clicked New conversation and received a successful config response, but the native terminal exited with Invalid session identifier. The handler cleared only the Claude conversation key, and Gemini/Codex launch paths ignored skip_conv_id, so the supposedly fresh launch still used --resume.
COST: Upload acceptance could not start; the worker remained at a shell while the UI reported a reset.
FIX: Fresh resets clear all provider resume keys and both hookless launch paths respect the fresh flag. New Gemini identities are UUIDs with random leading bytes, matching the CLI's documented --session-id contract and avoiding time-derived filename prefixes. conversation_recycled now logs provider identity. Tests exercise the config handler and a stale Gemini identity followed by fresh launch and exact subsequent resume.

## Gemini uploads stop at native read approval before submission
AREA: workers
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-832
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The real UI upload launched a fresh Gemini session, then its @uploaded-file prompt opened a native read approval outside the checkout. Amux's send verifier returned stuck. Gemini was launched with the log directory included but not the uploads directory.
COST: The user-uploaded file could not reach a completed receipt task and the composer reported a send failure.
FIX: Include the Amux uploads directory in the Gemini workspace, using its supported repeated --include-directories option. The real upload case must read the attached bytes, produce a matching JSON receipt, finish its board card and expose the delivered message and terminal on desktop and mobile.

## Fresh conversation accepts a message into the retiring process
AREA: workers
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-833
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The real Gemini upload run received reset acceptance at 18:18:24Z, delivered its prompt at 18:18:26Z, and only launched the replacement process at 18:18:46Z. The test saw the previous terminal's banner while reset was still stopping that process.
COST: A following send could appear accepted and then lose its native conversation when the asynchronous reset killed the old process.
FIX: Acquire the existing per-lane send boundary before accepting a running reset and retain it through stop/start. Ordinary sends during that interval persist immediately into steering, with an acceptance receipt; interactive commands refuse without an effect. Holding the HTTP request itself through restart was disproven by a mobile timeout and pending duplicate receipt. Other lanes remain independent. conversation_restart_send_boundary and existing lane-send-serialised logs expose the ordering. The live upload scenario deliberately sends through the UI immediately after reset, then requires real attachment data and completed board evidence.

## Board recovery hides the full assignment from the sanctioned CLI
AREA: workers
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-834
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Gemini recovered an auto-captured upload task through amux board show. The card preview ended at `row`, before `row_count` and the attachment path. The API already returned the full linked source message, but the Bash CLI dropped messages entirely. The worker searched logs and produced `rows` rather than the required `row_count`.
COST: A completed receipt had the wrong schema despite the original request remaining in durable history; recovery spent tokens searching terminal logs for context the API already provided.
FIX: Board show exposes linked message IDs and a supported --messages option for full assignments. Structured recovery explicitly reads that option for captured prompt previews, while ordinary board reads remain compact. A fake-transport regression uses the real CLI with a requirement beyond the 300-character preview and requires it only on the explicit full-message read.


## Queued delivery observation reads an unstamped command receipt
AREA: testing
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-835
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The real Gemini upload reached steering history with outcome sent, produced its correct file and completed LG1A-7, but the acceptance helper timed out waiting for cmd_history.delivered_at, which the steering drain does not stamp.
COST: A delivered message was reported as undelivered, stopping the remaining acceptance cases.
FIX: Expose the existing steering outcome and submission verdict, and the exact queue ID for restart acceptance. The observer checks this delivery instrument and excludes dead-letter rows despite their timestamps. The real handler regression covers confirmed, retried and discarded histories; existing steering-delivered logs remain the operational signal.


## Messages normalization discards recorded delivery metadata
AREA: ui
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-836
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The real Gemini mobile upload screenshot showed direct? on MSG-80 although its API receipt recorded queued. Both the shared history cache mapping and _msgNorm discarded delivery metadata before the shared renderer read it.
COST: New messages looked like legacy records, and failed submission indicators could disappear from all three message surfaces.
FIX: Preserve recorded delivery, queue timestamps, wait duration and submission verdict through the shared normalizer, and use it for initial history loading too. A browser regression fetches controlled direct, queued and stuck API rows through the actual scoped loader, renders all three message surfaces and requires their real labels. The existing server delivery logs and exposed steering outcome remain the diagnostic signal.


## Delivered message still appears locally unsent while its response is pending
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-837
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The user's 11:55:17 screenshot showed a homepage request in Claude's native queue while Messages said not yet delivered. Its exact MSG-55405 server record had direct/confirmed delivery at 11:55:12. Local pending state was tied to the entire POST response, including downstream board processing, instead of the durable acceptance already recorded.
COST: The client contradicted the terminal and offered cancellation as if an already-attempted message could still be prevented from sending.
FIX: Expose a read-only, non-cacheable receipt lookup scoped by session and msg_id. During an in-flight send, a bounded lookup can acknowledge the exact durable receipt without repeating delivery or cancelling the original handler's board work. Persist attempted state before transport, label uncertainty as Awaiting confirmation, and refuse local cancellation once attempted; legacy entries without attempt provenance are conservative. acceptance_receipt_read and outbox_acceptance_receipt expose reconciliation. Tests hold the original POST open, reject wrong-ID/unaccepted receipts, and check the real handler never reserves or sends on a lookup.


## Rapid input inherits retry backoff and full-history terminal refreshes
AREA: messaging
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-838
ORIGINAL_CARD: AMUX-4417
SYMPTOM: After confirming the stale queued banner was gone, the user reported a slight delay before input appeared in the terminal. A message appended during an in-flight replay missed its snapshot and inherited retry backoff. The deterministic counterexample selected an 8000 ms timer. The post-input UI also launched two full-history refreshes; five read-only production samples were approximately 128 KB each versus 5 KB for a live frame.
COST: Rapid messages waited unnecessarily and mobile terminal updates transferred scrollback to display newly arrived input.
FIX: Newly added, unattempted operations resume on the next tick after the active replay, retaining FIFO delivery and receipt checks. Replace overlapping full refreshes with one bounded live-frame loop: first tick at 40 ms, then 100 ms intervals for 1.5 seconds, with normal cadence afterward. Remember pending turn-end history refreshes. The executable latency regression and its attached dispatch/render measurements detect recurrence; disabling immediate continuation makes the counterexample fail.


## Send button mistakes a second rapid press for the first tap's click echo
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-839
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The rapid-send lifecycle case failed on desktop, mobile and iPhone WebKit: the first local send cleared, but the second distinct message remained in the composer after Send. _btnFire suppressed every activation within 350 ms instead of only the synthesized echo of one gesture.
COST: A legitimate new message required another tap and made the local-first composer appear stuck.
FIX: Reset per-button echo suppression on a new pointerdown/touchstart and allow distinct keyboard activation. Keep the same gesture's pointerup/touchend/click echoes deduplicated. The rapid-send UI case exercises two different messages and verifies two unique IDs, immediate continuation and terminal rendering; the event contract verifies duplicate echoes still fire once. Existing send-fire diagnostics retain the pre/post composer length and event sequence.


## Semantic intake acceptance listed worker-message coverage but only exercised the board API
AREA: testing
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-840
ORIGINAL_CARD: AMUX-4417
SYMPTOM: LC-SEMANTIC-INTAKE posted candidate tasks directly to /api/board. The canonical case also promised captured worker messages, but no executable scenario sent those messages through a composer and checked their surviving task links.
COST: A direct-board semantic pass could be mistaken for proof that ordinary new messages avoid near-duplicate board tasks.
FIX: Add LC-SEMANTIC-MESSAGES to live discovery: six composer messages must produce three tasks, four linked source messages on one survivor, measured append/update decisions and preserved requirements. Follow source links in desktop/mobile details. Record live prerequisites separately: the first attempt failed worker admission under host memory pressure before sending, so it is not a semantic pass. Preserve the dedicated run's health and trace evidence.

## Phone composer squeezed the draft beside a misaligned Queue button
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-841
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The phone composer put the textarea, top-aligned more button and bottom-aligned Queue button on one row. Removing the corrected full-width rule reproduces a 204px input in a 363px row. The expanded test also found the attachment menu 16px above the viewport in landscape.
COST: Another user screenshot and a failed landscape acceptance run before the clipping was corrected.
FIX: This commit gives phones a full-width input and a separate aligned 44px toolbar, bounds the long draft and attachment menu, and adds inputW/actionDelta to the existing layout diagnostic. Source-built LC-COMPOSER-LAYOUT, LC-LATENCY and LC-RECEIPT: 9 passed across desktop, mobile and iPhone WebKit; 32 outbox contracts passed. The CSS negative control fails on input width (204.34375px versus at least 362px).

## Automatic quota resumption was labelled needs input
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-11
SESSION: codex
AUTHOR_PROVENANCE: Original label preserved; exact originating-session identity remains unconfirmed. First published in 217a57929afb3b57b70edf726504da8a6a19d6c4. Committer identity is not author validation.
CARD: AF-842
ORIGINAL_CARD: AMUX-4420
SYMPTOM: Ethan's mixpeek-frustrations screenshot showed NEEDS INPUT over Claude's usage limit with automatic resumption at 6:10pm. Preview cancellation text overwrote the provider state; the sweep discarded this banner's reset clock. The wider audit found ready-composer events overwriting quota/error states and missing Starting/Error badges.
COST: User had to inspect the terminal and report a question that did not exist; independent state projections disagreed.
FIX: Current provider-footer classification, clock-preserving observation, typed state projection, idle-prompt event semantics, and explicit dashboard badges. Controlled provider/model and browser chaos regressions; diagnostic verdicts preview_quota_over_input, provider_auto_resume_quota, and ready_composer_idle.

## Worker terminal opened in the middle of its history
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-11
SESSION: codex
CARD: AF-843
ORIGINAL_CARD: AMUX-4421
SYMPTOM: Ethan opened mixpeek-general and landed midway through old terminal output instead of at the latest output.
COST: Each open required finding and scrolling to the worker's current output.
FIX: Preserve bottom-follow intent through asynchronous history/live rendering and resizing, cancel it on deliberate reading/navigation, and flush buffered output on resume. Desktop/phone race tests and bottom-anchor-restored diagnostics.

## Accepted details message survived as a partial card draft
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-11
SESSION: codex
CARD: AF-844
ORIGINAL_CARD: AMUX-4424
SYMPTOM: Ethan sent a message to amux from worker details, but an earlier partially typed copy remained in the worker card. The 250ms draft mirror lagged; exact-match acceptance left the partial copy alive, and lifecycle DOM harvesting could save it again. Fullscreen edits and separate browser contexts also missed draft synchronization.
COST: User could mistake already-submitted text for unsent work and submit it twice.
FIX: Immediate per-worker draft updates across card/details/fullscreen and same-origin tabs/grid; revision-bound acceptance preserves newer edits, lifecycle events never overwrite storage from stale DOM, and failed storage retains text with a visible warning. Server client-debug verdicts composer_locally_accepted and composer_draft_storage_failed. Regression reproduced on pre-fix source; desktop, phone and WebKit coverage alongside durable outbox tests.

## Offline banner promised to retry permanently failed edits
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-845
ORIGINAL_CARD: AMUX-4417
SYMPTOM: A blocked 409 board edit displayed as queued and promised to send on reconnect while offline. The regression reproduced that exact text before the fix.
COST: The user could wait for an automatic retry that will never occur.
FIX: This commit separates failed and pending counts in offline mode, preserves review/dismiss actions for failed-only queues, and adds LC-BLOCKED-OUTBOX across desktop/mobile/WebKit. The final focused run passed 9 cases including gate revisions and linked records; screenshots were opened.

## Stale failed-row dismissal deleted an edit resumed in another tab
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-846
ORIGINAL_CARD: AMUX-4417
SYMPTOM: A stale failed-row action removed its operation by ID even after durable storage changed its state to pending. The regression lost the resumed entry before the fix.
COST: Potential loss of a pending edit when two tabs act on the same outbox.
FIX: This commit checks blocked state inside the shared storage lock, refreshes the UI and emits outbox_dismiss_ignored when the action is stale. The contract verifies pending work survives both individual and bulk failed-only dismissal. All 34 outbox contracts passed.


## More-specific mobile flex rule narrowed the composer again
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AF-847
ORIGINAL_CARD: AMUX-4417
SYMPTOM: After integrating the latest toolbar change, LC-COMPOSER-LAYOUT failed on all three projects: the 320px phone input shrank to 155px instead of its available 308px.
COST: Long drafts become difficult to read beside More and Queue.
FIX: Remove the conflicting ac-wrap flex override, retain compact chrome and aligned action controls, and preserve the full-width mobile writing row. Existing composer-layout diagnostics record inputW and actionDelta; the browser case checks short/long drafts, Send/Queue, narrow/landscape viewports and attachment-menu reachability.

## Uncertain native submission was deleted and counted as synced
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-848
ORIGINAL_CARD: AMUX-4417
SYMPTOM: An uncertain 409 send response removed the durable message and marked its progress row done.
COST: The only recoverable intent disappeared while the UI reported a success.
FIX: Keep the original message ID, text and attachment references in a blocked outbox row; outbox_retry_failed reports the rejection. The new uncertain-submission contract rejects false checkmarks.

## Steering preview depended on expiring in-memory text matches
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-849
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Pending steering used a temporary map cleared by matching text or a two-minute expiry, while actual intent lived in durable storage.
COST: A reload or delay could erase the preview; identical messages could be conflated.
FIX: Render pending steering directly from durable outbox entries with stable IDs. Distinct identical requests survive reload and age. steering_accept_failed identifies acceptance failures.

## Worker-card file picker was missing on touch screens
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-850
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Details offered Attach file while the worker card relied on drag/drop.
COST: Phone users could not select files from the worker-list composer.
FIX: Add a 44px card file picker using the same durable upload pipeline. card_files_selected logs file counts; lifecycle covers real upload/download bytes and Send/Queue from both surfaces.

## Mobile attachment menu overflowed after compact composer layout
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-851
ORIGINAL_CARD: AMUX-4417
SYMPTOM: At 320px and iPhone WebKit the attachment menu extended 10–16px beyond the screen edge.
COST: All three layout runs failed their reachability check.
FIX: Right-align the menu with its More control. Desktop, phone and WebKit layout checks now pass at narrow, landscape and keyboard heights; existing composer geometry diagnostics expose bounds.

## Disabling browser idle expiry disabled hard and activity lifetimes
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-852
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The reaper returned immediately when idle expiry was zero, skipping independently configured hard and activity TTLs.
COST: Browsers could retain processes indefinitely despite configured age limits.
FIX: Evaluate activity and hard expiry before the idle-only switch. Existing reaper warnings report the actual expiry arm. The real-stop contract exercises both lifetimes with idle expiry disabled; notices now name AMUX_BROWSER_IDLE_REAP_S correctly.

## Automatic browser capture could raise a user window
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-853
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Screenshot retry restored the browser and invoked bringToFront; omitted API headless settings also launched a visible window.
COST: Background automation could interrupt the foreground application.
FIX: Default API automation to headless and remove all screenshot focus recovery. capture_failed_without_focus logs a failed capture; explicit headed sign-in remains available. Real browser lifetime and foreground checks are tracked in LC-BROWSER-BACKGROUND.

## Reconnect hid individual progress and failed-step evidence
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-854
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Reconnect requested a quiet sync and failures immediately hid the step list.
COST: Users could not follow which saved operations had succeeded.
FIX: Reconnect shows per-operation progress, only acknowledged changes receive checkmarks, and failed steps remain reviewable. LC-SYNC-PROGRESS holds three real edits at successive boundaries, injects one conflict and verifies explicit recovery.

## Composer cleared before durable local acceptance
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-855
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The recent fire-and-forget path cleared the editor before local persistence and restored text later on refusal.
COST: A failed local write or intervening edit could create misleading success feedback.
FIX: Clear only after the fetch interceptor durably accepts the intent; delivery remains asynchronous. Existing composer_locally_accepted and composer_unconfirmed diagnostics identify the boundary. Newer text and files survive refusal.

## Quoted Gemini picker blocked an idle worker's steering boundary
AREA: steering
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-856
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The existing current_questions_survive_but_quoted_questions_do_not replay failed: a quoted boxed Gemini selector above a newer empty Claude prompt classified the worker as waiting.
COST: Automatic steering could refuse an idle boundary based on historical output.
FIX: Keep Gemini's live picker-over-placeholder behavior but disregard a boxed selector preceding a newer bare prompt. stale_picker_ignored emits a debug verdict. The existing cross-provider replay is the pre-fix failure; native completion remains separately blocked by host admission.

## Browser reaper reported disabled while its hard lifetime was running
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-857
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The real browser-lifetime probe expired Chrome successfully, but its follow-up system-jobs assertion found no enabled reaper: setting one expiry arm to zero marked the entire job disabled.
COST: Operators could not distinguish a disabled lifetime arm from a stopped cleanup loop.
FIX: The catalog no longer treats arm-specific zero values as job-level disable switches. Actual per-job and global isolation still report disabled. LC-BROWSER-BACKGROUND requires an enabled reaper and disabled unrelated loops before launching, then observes real expiry; system-jobs exposes the corrected status and tick count.

## Reconnect toasts covered the sync checkmarks on a phone
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-858
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Visual review of the passing iPhone sync-progress screenshot showed the reconnect toast covering the failed operation's explanation in the bottom checklist.
COST: The requested per-operation evidence was temporarily obscured exactly when it changed.
FIX: Use the visible checklist as reconnect feedback when saved work exists, cancel the lingering queue toast animation and clear its visible state when it opens, and remove its redundant completion toast. Reconnect without queued work still has its usual toast. Existing sync-step status and outbox diagnostics identify acknowledgements and failures.

## CLI launch negative control stopped reproducing its claimed fault
AREA: testing
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-859
ORIGINAL_CARD: AMUX-4417
SYMPTOM: GitHub check 103564599963 passed workspace tests and clippy, then failed the CLI launch smoke: deleting the local AMUX_API declaration still launched successfully because the CLI now also initializes it globally.
COST: The negative control no longer established an unset variable and kept the overall check red.
FIX: In the isolated mutant only, replace that declaration with an explicit unset so both local and inherited initialization are absent at the real inject. The unmodified CLI must still launch; the mutant must fail with the real unbound-variable error. The smoke output names the forced-unset precondition and its pass/failure verdict.

### 2026-09-12 — Busy composer falsely acknowledged; board recovery vetoed by current claims

User: workers still fail to drain backlog/todo/done and queued steering is not picked up. Live debug measured an empty server steering queue, but this is not provider completion evidence. The submission loop explicitly returned Confirmed for StillThereGenerating, bypassing its own bare-Enter retry. Require composer release or fresh provider transcript/native enqueue evidence instead; log generating_composer_unsubmitted.

Live board-drive showed mixpeek-cicd holding two Doing cards untouched 11–14 hours with 18 eligible todos. The current-generation exact-claim guard returned before the canonical stale-reclaim selector could execute. Permit that guarded recovery and the existing capture-shell WIP exemption; revalidate the reclaim on the serialized writer. Log stalled_claim_yields_to_canonical_pickup.

Board reminders also discarded enqueue errors and stamped cooldowns anyway. All reminder paths now check queue acceptance before recording budgets; failures log board_nudge_enqueue_failed and retry next tick. Verification was globally throttled for 24 hours after each eight-card batch; finishing a batch now re-arms the next one, without repeating an unchanged batch. Log verify_batch_queued. Dedicated driver tests and a real tmux capture replay cover these boundaries; fresh model-worker admission remains a separate live prerequisite.

### 2026-09-12 — Incoming mobile CSS guard inspected its comment instead of its rule

Integrating 5eaf25e0 made dashboard_assets fail despite the fixed positioning declaration being present. The test read only 500 characters after a long rationale; the declaration was outside that window. Inspect the mobile selector's declaration block instead. This changes the test only; the visual fix and version remain intact.

### 2026-09-12 — Cargo resource growth and cleanup could feed repeated rebuilds

The two-invocation throttle did not bound compiler/test parallelism, RSS, elapsed runtime, or target growth. Full debug data and incremental artifacts repeatedly crossed the release builder's 10 GiB cleanup threshold; unchanged failing build inputs also retried every minute. The wrapper now defaults to two compiler/test threads, disables routine dev/test DWARF and incremental output, and supervises owned Cargo groups with measured RSS/time/disk ceilings. JSON cargo_budget_* records explain refusals, stops and unmeasured probes. Idle debug cleanup moves to 32 GiB; unchanged failures back off 15 minutes and changed build inputs retry immediately (cargo_build_backoff).

The hourly stale-target sweep bypassed the shared Cargo guard with remove_dir_all, including for arbitrary old target names. Route every discovered target through that guard, preserving active leases and native lock files; cargo_reclaim_deferred identifies refusals. The consolidated lifecycle now exercises resource budgets, failed-build retries, active-target preservation and normal completion. Worker-created evidence folders and persistent profiles remain explicitly documented outside these cache quotas rather than being silently deleted.

The final entry-point audit also found unbuilt-commits.sh --build invoking bare Cargo for each historical revision. It now resolves the current safe wrapper before entering an old worktree, so historical replay receives the same budgets and diagnostics.

Make targets and the installer also invoked bare Cargo. They now use the same wrapper, with the installer's explicit target/jobs preserved. make run delegates to the committed signed atomic builder instead of overwriting the running binary from a checkout-local target. make dev bounds compilation and then runs the requested development server normally.


## Hourly cleanup omitted diagnostic folders and could mistake a failed reference query for no references
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-860
ORIGINAL_CARD: none — continuation of the user's isolated housekeeping request
SYMPTOM: Worker-created log folders had no retention (one measured 3.6 GiB); expired transcript cache entries accumulated by worker name. Storage diagnostics counted 29 deleted rotated logs while the system-job summary said zero files and zero bytes.
COST: Unbounded diagnostic output and misleading cleanup outcomes; an unavailable reference query also permitted deletion of aged uploads.
FIX: Hourly guarded diagnostic retention, descendant recency/open-file/reference checks, bounded probes, fail-closed upload references covering messages and artifacts, and transcript cache expiry. storage diagnostics expose measured/deferred outcomes and actual deletion totals; diagnostic directory retention deferred, upload retention deferred and transcript evidence cache expired announce the affected paths. Consolidated lifecycle fixtures test deletion and preservation, including a real storage tick with an unavailable reference table.

The old upload reference regex also truncated valid filenames containing spaces or Unicode. Match decoded references against actual filenames; the regression fixture keeps two such linked files and deletes an unrelated aged upload.

The live-data probe also found ordinary text mentioning “logs” would consume the reference snapshot budget (over 18 MiB in board text before filtering). Filter on normalized path separators in SQL, so plain prose cannot prevent cleanup. A large-prose control accompanies the missing/oversized-reference tests.


## Diagnostic-folder cleanup deferred because launchd could not locate lsof
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-861
ORIGINAL_CARD: none — live deployment verification for the user's housekeeping request
SYMPTOM: The first deployed sweep reported unmeasured run/evidence/audit directory cleanup with ENOENT. The service PATH omitted /usr/sbin, although lsof was available from an interactive shell.
COST: Directory cleanup deferred; 14 old log files were removed and five linked uploads were protected, but no diagnostic folders were examined.
FIX: Resolve macOS's /usr/sbin/lsof explicitly and include the executable in spawn-failure diagnostics. A native test restricts PATH to /usr/bin:/bin and checks that the probe observes a real held file; reverting to bare lsof must fail that test.

## Isolated worker peer boundary could be bypassed through Queue
AREA: workers
SEVERITY: breaks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-862
ORIGINAL_CARD: none — user requested isolated-worker consolidated lifecycle coverage
SYMPTOM: The expanded LC-ISOLATED-BOUNDARY browser case received HTTP 200 from a same-group peer's POST /steer, although the identical peer's POST /send correctly returned 403 for the isolated target.
COST: Peer messages could enter a raw worker through the steering queue, violating the same isolation boundary enforced on Send.
FIX: Share an early isolated-peer refusal across direct and queued sends before dedupe/history/queue mutation. Preserve owner and authenticated member access; an explicit peer allowance cannot bypass isolation. Each refusal emits send.isolated_refused and a WARN with verdict=isolated_target. Rust controls verify no rejected message/history rows and exactly one owner queue/history row across retries; lifecycle browsers cover same/outside groups, UI toggles, reloads and cached discovery.

## Cached board cards could not be edited offline, and retries covered Save
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-863
ORIGINAL_CARD: none — run-owned offline lifecycle fixture cards are deleted after verification
SYMPTOM: A real network-off browser run retained only two message operations out of five expected writes: the three cached card edits failed the hydration guard. After enabling complete offline snapshots, automatic retry failures painted the sync banner over the third card's Save button while the browser was still explicitly offline.
COST: The earlier 90-case outage/upload/checklist suite passed while the cold offline UI journey failed; task edits were not enqueued and a visible failure panel blocked further editing.
FIX: Persist complete authoritative task snapshots in the existing IDB mirror and hydrate offline with identity/revision guards. Refuse incomplete snapshots and explicit HTTP refusals. Avoid replay while navigator.onLine is false, then resume through the real online event. File uploads now share the per-operation acknowledgement checklist and retry scheduling. Cache failures emit card-cache-write-failed; offline hydration names its verdict, and unconfirmed file replay emits upload-storage sync-unconfirmed.

## Retrying sync erased file checkmarks that had already been acknowledged
AREA: notices
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-864
ORIGINAL_CARD: none — consolidated offline lifecycle acceptance
SYMPTOM: The cold offline run restored seven operations. Two files finished while earlier network failures retried; the next checklist showed only five synced, despite all seven operations reaching their destination. A startup history migration could add a separate import operation to that list.
COST: The final checklist did not account for every queued action. Terminal/history diagnostics also reported duplicate-history text when optimistic local message history was imported ahead of queued messages.
FIX: Retain acknowledged rows across visible retries using stable operation keys. Removed operations are skipped without an acknowledgement checkmark. Background history imports bypass the user outbox, require an actual successful response, and defer while messages remain pending. LC-OFFLINE-ROUNDTRIP verifies every stable key and server result; negative controls fail if completed-row retention or acknowledgement guards are removed.

## Completed mobile sync still showed a stale offline toast over its checkmarks
AREA: notices
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-865
ORIGINAL_CARD: none — visual review of the consolidated offline lifecycle
SYMPTOM: All 138 browser assertions passed, but the captured desktop/mobile/WebKit screenshots showed “Server unreachable — offline mode” covering rows below “7 synced” while the header showed Live.
COST: Successful server acknowledgements appeared contradictory and the phone's last two checkmarks were obscured.
FIX: Clear only obsolete connectivity/queue toasts and their active animations when the checklist opens and completes successfully; preserve unrelated failure notices. The real offline lifecycle now asserts the toast is hidden before capturing every acknowledged row, and a shipped-function test preserves a separate upload failure notice.

## A delivered mobile message stays failed, and its local copy appears beside its server copy
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-735
SYMPTOM: Owner screenshots show messages received in this conversation remaining 409 acceptance-uncertain for an hour. Steering receipt polling used the unprefixed ID although the server stores steer:<id>; both Messages and Steering independently rendered local and server representations.
COST: Repeated manual Retry, duplicate-looking rows, and a false failed-operation banner on the phone.
FIX: In progress: retain unknown acceptance and retry bounded receipt reads, use the correct steering namespace, and join display rows by transport identity. AF-736 tracks the duplicate representation; simulator verification remains outstanding.

## Loaded mobile header hid controls and its compact label escaped its button
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-731
SYMPTOM: The owner could not reach Settings beside the fleet's connection and limit labels. A first compact draft passed outer-button bounds but native Safari placed the red limited count beneath the next button.
COST: Unreachable mobile controls and an extra native verification/correction cycle after desktop geometry passed.
FIX: This candidate uses compact labels with full 44-point targets, a real count element, and measured mobile-header-clipped beacons for both control bounds and label containment. Phone-width tests and a visually inspected real iOS 26.5 screenshot cover a loaded 52-worker fleet with 18 limited; all eight targets are unobstructed. Deployment remains separate.

## Archive reason persisted but CLI reported it ignored
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-729
SYMPTOM: An isolated archive command exited 6 and warned archive_outcome was ignored, while readback showed archived=1 and the exact reason in the attributed log. The protocol consumed the key but omitted it from PATCH_CONTROL, contradicting its own successful write.
COST: The owner repeated a reason that was already saved because the acknowledgement claimed it was lost.
FIX: Register archive_outcome as a protocol key carrying log content. Refused transitions report it among discarded fields; invalid or non-applying outcome requests are explicitly refused instead of silently accepted. Structured patch_fields_ignored and archive_outcome_refused warnings name the affected card and measured population without logging supplied content. The acknowledgement regression failed first; isolated CLI reproduction and focused archive tests record the before/after evidence.

## Unreadable board acknowledgements falsely said writes were not recorded
AREA: cli
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-657
SYMPTOM: A loopback HTTP fixture returned non-JSON after accepting evidence/outcome writes. The CLI printed NOT recorded, continued to the status PATCH, and printed raw HTML for its unreadable response. Exit 1 prevented a success claim but did not tell the caller which writes were unknown; a mixed success could still invite repeating an already-applied append.
COST: The caller must rediscover whether prose and status landed independently before safely retrying.
FIX: Shared acknowledgement validation rejects malformed or non-object JSON, stops before a dependent status transition, and explicitly says the write outcome is unknown. It retains a measured board_ack_unknown event in the existing durable CLI diagnostic spool, delivered on the next invocation. Five real-CLI tests deliberately apply writes before corrupting replies, cover each stage plus success/refusal controls, and verify spool delivery; four failed before the fix and all five pass after. Existing transport checks also pass (11/11).

## Codex Terminal showed only Working while its saved conversation still existed
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-740
SYMPTOM: Mobile screenshot MSG-58672 showed the Codex Working/input footer and empty space. The structured transcript endpoint resolved 92 events, but Terminal discarded history for non-Claude alternate screens and depended on raw tmux paint otherwise. Load earlier output also bypassed the structured Codex reader. An isolated pre-fix API probe returned history absent/0 characters for a pinned, existing rollout.
COST: User reported missing logs and could not inspect earlier work from Terminal; diagnosis required tracing two provider-specific paths despite the saved conversation already being readable elsewhere.
FIX: AF-740 routes Codex/Ollama full peek and paginated earlier history through the existing provider projection, preserves independent tool results across byte cursors, keeps live polls separate, and emits measured peek_history_loaded/peek_history_unavailable signals. Native audit also found array-shaped input_text tool results were silently ignored by the shared Codex projection; those now decode alongside strings/objects, exclude image payloads, and are counted as tool_output_arrays in page logs. The terminal contract tests now initiate real wheel/touch intent before positioning earlier text: direct scrollTop assignments had left follow-bottom enabled and produced six false user-scroll failures across three engines, while native touch scrolling passed. Candidate is tested in scratch/frustrations-integration; production deployment remains pending.

## Archive reason validation rejected a flag the archive operation accepted
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-729
SYMPTOM: Broad integration validation after AF-740 caught patch_archived_round_trip_with_cross_lane_guard failing at board_api.rs:703: archived="true" plus archive_outcome returned 400. AF-729's new reason validator recognized only JSON true/1, while the existing archive mutation also accepted normalized strings 1/true/yes/on.
COST: One real compatibility regression escaped the earlier focused archive tests and prevented a clean integration gate; the existing cross-lane archive regression caught it.
FIX: Share one patch_archived_value coercion between validation and mutation, preserve authorization and exact attributed reasons, and cover accepted/rejected flag forms. The existing archive_outcome_refused WARN continues to identify rejected fields without a silent write. Corrected code is on the isolated integration branch; production adoption is still pending.

## Safari consumes the first tap on a board column card
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-741
SYMPTOM: Native iOS 26.5 Safari emitted touchstart/touchend/mouseover/mousemove on a column card but no click; the first tap revealed the previously transparent Pin button. Only the second tap opened the card. List rows opened on the first tap, so viewport-only checks missed the failure.
COST: Mobile board audit required three native reproductions to separate an incorrectly located test swipe from the real two-tap card defect. Users must tap a card twice to view it.
FIX: Limit card hover reveals to hover-capable pointers and keep touch Pin controls visible. A passive stationary-touch observer emits measured board_tap_unopened when a card tap never becomes a click, excluding scrolling and child controls. scripts/test-ios-board.mjs uses real isolated board records and native Simulator inputs; all six journeys pass after the fix. Candidate only; deployment pending.

## The bottom-follow threshold traps small upward log gestures
AREA: browser
SEVERITY: blocks
STATUS: open
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-742
SYMPTOM: User reported being unable to scroll up after reaching log bottom. Native iOS 26.5 Safari reproduced it: -25pt gesture left gap=0/following=true, whereas -350pt escaped. The scroll event and live-frame renderer independently treated being within 40px of the bottom as permission to resume following, undoing the first small upward movement.
COST: Earlier logs became unreachable with small gestures, and a correction to only the scroll handler still snapped the reader back when a live frame arrived; the new three-engine regression caught that second path.
FIX: Track scroll direction, resume only on downward movement to the actual end, and make live refresh honor the explicit follow state. Emit measured bottom-follow-paused / reader_scrolling with input kind and bottom gap. New five-pixel regression fails before and passes after, including live-frame position retention and deliberate return to bottom. Native small/large gestures both pass with changed live frames, and the full terminal browser matrix passes 72 tests. Candidate only; production deployment pending.

## Empty mobile composer clips its own working-state placeholder
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-743
SYMPTOM: MSG-58894 circles a mobile textarea whose working-state placeholder wraps to three lines and clips below its border. The user explicitly requires input/More/Send on one row; repeating the Working status and drop-file hint consumes the remaining writing width. Reproduced in the native Simulator log audit and a 375px active/idle browser regression.
COST: The empty field looks broken and obscures where to type; the user reported another screenshot despite the one-row layout already being implemented.
FIX: Use Message… with an accessible recipient label, preserving the one-row layout, drafts and send behavior. Existing keyboard-down/up geometry beacons now measure placeholder width against the actual text area and emit composer_placeholder_clipped or composer_readable. New test fails in all three engines before and passes after; native keyboard-open controls, More/mode taps and exact draft retention pass. Owner has authorized deployment; live verification follows clean integration gates.

## Helper quota errors were accepted as classifier answers
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-866
ORIGINAL_CARD: AMUX-4417
SYMPTOM: The subprocess regression returned Ok("session limit reached") after the helper exited 7. The same path accepted JSON-shaped stdout from a failed process, so failed model work could be parsed as a measured intake decision.
COST: Semantic intake failure/recovery could not be certified; quota diagnostics were misreported as invalid classifier JSON and unavailable comparison preserved extra records.
FIX: Honor process exit status before accepting stdout, retain at most 400 diagnostic characters, log distinct helper_exit_failed/helper_timeout/helper_empty_output verdicts, and reap killed children. The real-child regression matrix and before/final results are recorded in docs/lifecycle-helper-validation-2026-09-12.md. This does not claim that provider quota or native admission has recovered.

## Cargo rebuilds unchanged detached worktrees
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-867
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Consecutive helper/intake checks rebuilt amux-server for about 75 seconds each despite identical crate bytes. The tiny real Cargo regression confirmed that an unchanged detached-worktree build reported fresh=false because build.rs watched nonexistent .git/HEAD and .git/refs/heads/main paths.
COST: Repeated full server compilation during verification, with avoidable CPU and memory pressure on a host already denying new workers.
FIX: Resolve Git metadata using git rev-parse --git-path; watch the current HEAD and branch, including packed-ref transitions. The lifecycle resource case now runs a tiny real Cargo fixture proving cached repeats and correct identities after commit/branch changes. Restoring the broken HEAD watch makes the fixture fail.

## A browser schema test reached the live browser with an upload fixture
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-868
ORIGINAL_CARD: AF-745
SYMPTOM: The clean deployment suite's valid-files control called the action API and assumed any 400 was a schema failure. With Chrome running it reached the real page and returned no element matches #f; a matching input could have received the fixture.
COST: Deployment held while reproducing and separating schema validation from browser I/O.
FIX: The handler and positive control share a browser-independent validator; malformed API controls still prove validation ordering. Rejections emit browser_files_schema_rejected with measured/count fields. Negative evidence: scratch/ios-simulator-review/deploy-browser-schema-probe.log.

## A late request success cleared the phone's offline state
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-869
ORIGINAL_CARD: AF-745
SYMPTOM: The browser lifecycle regression intermittently displayed 1 sending while the browser network was explicitly offline. setOnline(true) from a previously started read could overwrite the newer offline event.
COST: Two additional browser matrix failures delayed deployment and exposed misleading queue feedback.
FIX: setOnline refuses a positive transition while navigator.onLine is false and emits connectivity_stale_success / offline_preserved. The lifecycle test deliberately injects the late success and checks offline feedback plus subsequent reconnection.

## New Brex scaffolding failed the full-suite dead-public-API gate
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-870
ORIGINAL_CARD: AF-745
SYMPTOM: Upstream f834583e introduced is_freeze and unfreeze_card without any caller. Workspace Clippy passed; no_new_unreferenced_pub_fn_in_amux_server named both as failures on the clean deployment snapshot.
COST: Deployment held for a full-suite failure invisible to the language lint gate.
FIX: Removed the two unconnected methods; the existing dead_pub_api gate remains the log signal for recurrence. No wired Brex behavior changes.

## The mounted Brex API was absent from the route boundary registry
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-871
ORIGINAL_CARD: AF-745
SYMPTOM: Upstream f834583e mounted /api/brex but omitted NATIVE_FAMILIES and its three ROUTE_TABLE paths; the clean proxy_composition test named the unclaimed route family.
COST: Another full-suite failure after the standalone Clippy gate passed.
FIX: Register the mounted native family and all three paths so the diagnostic endpoints, composition guard and route census describe the actual router. The failing test is the standing regression signal.

## CI kept failing because the debris test opted out of the harness guard
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-872
ORIGINAL_CARD: AMUX-4460
SYMPTOM: checks failed repeatedly through upstream f834583e: test-reap-amux-debris.sh lacked set -e. Its comment claimed exclusion from the guard, but the actual classifier still included it. A helper existence check did not make setup/helper execution failures abort.
COST: Multiple main-branch CI failures and another deployment gate correction.
FIX: Enable errexit for setup/helper failures while check() continues to accumulate assertion failures. Compare all eight fixture checks before and after; test-harness-guard is the standing log signal. Prior CI evidence: run 34721443418.

## New native and portable regression harnesses had no CI disposition
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-873
ORIGINAL_CARD: AMUX-4460
SYMPTOM: After fixing the earlier checks failure, pushed 638b7203 reached the next guard and reported five newly unwired harnesses: board acknowledgement, Cargo provenance, two native Simulator scripts, and Tailscale owner bootstrap.
COST: One additional failed main CI run (34723123687) despite the individual regression probes passing locally.
FIX: Invoke portable acknowledgement and Cargo provenance tests in checks.yml; record explicit local device/daemon prerequisites and commands for the three native acceptance harnesses. The existing harness-wired guard continues to report the full population and any new omission.

## Native keyboard dismissal stalled on the deployed worker's large history
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-874
ORIGINAL_CARD: AF-745
SYMPTOM: Native Safari keyboard/menu acceptance passed on small fixtures, but the real working-worker log hit a 70-second dismissal/restoration failure. The last context error masked the original dismissal failure. The toolbar lookup used XPath, whose driver path serializes the complete accessibility tree.
COST: Live deployment verification stopped; roughly 15 minutes reproducing against real data and distinguishing a retained-keyboard test setup error from the driver failure.
FIX: Use a native class-chain query scoped to Safari's toolbar Done control, keep ambiguity/visibility checks, preserve the original failure when context restoration also fails, and emit webdriver_transport_failed with operation, deadline, timeout and measured/count fields. Native live board taps now complete through the replacement query. The real Safari keyboard/menu rerun against production page data passed (1 passed, 0 failed), preserved the unsent draft, and its screenshots were inspected. The four-case refusal/dismissal/context-restoration regression passed; final deployment evidence is tracked on AF-745.

## Memory pressure ranking hides the largest compressed consumers
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AF-875
ORIGINAL_CARD: AMUX-4417
SYMPTOM: mac-health ranks only RSS. Procwarden's Python process reports about 70 MiB resident while macOS counts 24 GiB including compressed memory; Activity Monitor holds 12 GiB and fseventsd 54 GiB. Native lifecycle admission remains denied.
COST: Repeated lifecycle preflights cannot start, while the cleanup log names the wrong largest consumers.
FIX: Use bounded macOS MEM/CMPRS measurements with process IDs, explicit metric and failed-probe visibility; keep foreign application recovery under user control.

## Header notification badge intrudes into the adjacent status control
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-750
SYMPTOM: Owner desktop/mobile screenshots showed an overflowing notification badge beside an oversized red status panel and mixed emoji controls. The header diagnostic ignored desktop widths entirely.
COST: The owner requested repeated desktop/mobile visual corrections; fitting the overall header width had not ensured clean individual control boundaries.
FIX: AF-750 / AF-751, dashboard 0.9.930: contain the badge, use consistent line icons and lighter status controls, align desktop actions, preserve 44px mobile targets and fit the four primary mobile navigation labels. The existing mobile-header-clipped beacon now measures both desktop and mobile, includes the actual visible control count and detects escaping badges. A deliberate desktop badge overflow requires the real diagnostic request in the regression test.

## An own mtime observation can become "your edit record" in the commit nudge
AREA: attribution
SEVERITY: blocks
STATUS: open
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-876
ORIGINAL_CARD: AF-746
SYMPTOM: mixpeek-frustrations reported six studio paths labeled as carrying its edit record despite zero worktree edits. The current local observation store contains none of those historical studio records, so that incident's exact provenance is unconfirmed. Source inspection independently found apply_observed promoting requester-only mtimes into GuardInputs.mine when peers are visible and have no recorded claim. The nudge interprets omission from foreign/unclaimed as authorship. Its decoder also reads the boolean undecided field as an array and cannot detect paths omitted by the guard's cap.
COST: The reporter declined every remedy to avoid sweeping another lane's work. This investigation required separate guard-to-nudge boundary reproductions because the existing peer-observation regressions did not cover self-attribution. No foreign worktree changes or reported sweep occurred.
FIX: Keep observation-only paths unclaimed and committable under the existing visible-cotenant policy; preserve real writer/blind protection. Require a complete, decided guard population before the nudge derives ownership. Retain measured diagnostics and tests at the real consumer boundary. Historical six-path provenance remains unconfirmed until its original verdict is available.

## A refused pathspec commit leaves the recommended ownership check looking empty
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-12
SESSION: amux-frustrations
CARD: AF-877
ORIGINAL_CARD: AF-746
SYMPTOM: ts-gke reported overriding the ownership guard after an empty git diff --cached on a path that was not staged in the ordinary index. A real disposable Git reproduction confirms that git commit <path> gives the hook a temporary index and discards it on refusal. The emitted cached-diff hint then produces the same empty output for a legitimate append and a peer-style full rewrite. The report's eventual 66-line append was correct, but this check could not distinguish it.
COST: One reported override used an unmeasured comparison; no incorrect commit was reported. Reproducing both append and rewrite through the actual hook required a separate fixture because ordinary staged-hook tests retained the index and missed the timing gap.
FIX: Explain temporary-index lifetime in both the server refusal and installed hook. For pathspec retries compare the working tree against HEAD, the actual commit baseline; for staged commits first stage intended changes and inspect the cached diff. Require expected path/hunks and successful comparison; empty output, missing HEAD and errors are not ownership verification. Log measured review-required populations without claiming the user performed a review.


## Working iOS routes are absent from the diagnostic catalog
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-878
ORIGINAL_CARD: AMUX-4469
SYMPTOM: During AF-748 board verification, /api/browser/ios/targets returned measured:true n_considered:10 while /api/debug/routes listed zero simulator routes. The fresh route.callers_have_routes results failed for the iOS prefix and targets. The coverage test followed api/mod.rs into browser.rs but never its nested browser/ios.rs; the extended test failed on all ten omitted paths before the catalog fix.
COST: Two recurring automatic reports (AMUX-4468/4469) required another investigation; the diagnostic catalog could misclassify simulator request failures as missing routes.
FIX: Add all ten real paths with their actual verbs and descend into nested router modules in the completeness test. Require a positive nested-route canary; verify the live catalog and invariant after deployment. Existing invariant incident warnings and normalized request-log verdicts carry the operational signal. Independent review remains pending.

## A latency fixture changes the scan limit for sibling tests
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-879
ORIGINAL_CARD: AF-397
SYMPTOM: The scan-cap test sets process-wide AMUX_LATENCY_SCAN_CAP=200 while other tests use the same environment. A clean0c79fc53 rollup test given that value reproduces the reported empty findings: expected1, got0. Only20 baseline rows survive, below the detector's per-family minimum. An earlier separate window override was removed, but this fixture still changed global state.
COST: Historical CI failures were charged to unrelated pushes; this audit needed a clean controlled reproduction and a scoped134-test run before the old flaky-test card could be assessed honestly.
FIX: Pass the fixture's scan cap directly to the shared detector implementation, snapshot the production limit once, and include scan_cap with the considered/excluded population in the INFO log. Awaiting commit, clean gates and independent review.

## Manual board claims still count a capture that automatic pickup exempts
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-880
ORIGINAL_CARD: AMUX-3757
SYMPTOM: Current-source audit of the reopened manual-claim report found PATCH and the ready frontier still count an unanswered capture in Doing, while automatic pickup and status-update claims exempt it. Two isolated API regressions on bc0003b8 failed: PATCH refused with WC-1 as its sole holder, and the frontier advertised zero capacity for the ready real task. This is the manual-path recurrence, distinct from the old automatic-pickup entry retained in the archive.
COST: The reopened card remained actionable despite its earlier fix and archive; the audit required two failing API specimens and inspection of five independently maintained holder queries before the mismatch was bounded.
FIX: Share the canonical WIP-holder predicate across all five consumers, retain reshaped work as WIP, and emit measured capture-exemption and failed-query signals. Draft regressions pass; publication, independent review and live adoption remain to be recorded on the card.


## Cancelling semantic intake leaves an already recorded owner message without a card
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-881
ORIGINAL_CARD: AMUX-4486
SYMPTOM: A test holding the real intake lane lock observes the committed cmd_history row, cancels cmd_hist_record_full, and finds that original row still unlinked. Pre-fix result: 0 passed, 1 failed. The six historical messages named by AMUX-4486 also remain unlinked, but this reproduction does not prove their historical cause.
COST: The board audit cannot honestly close six original delivered-message outcomes; it required a cancellation reproduction and durable-recovery implementation instead of trusting the current-uptime invariant PASS.
FIX: Save the pending board consequence in the message transaction and resume existing semantic intake after cancellation/restart without resending commands; preserve pending rows through retention, retry failed links, and emit counted recovery/failure logs. Original six need individual reconciliation before this entry can be retired.

## Retained unlinked messages have no audited repair operation
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-882
ORIGINAL_CARD: AMUX-4486
SYMPTOM: Six exact original delivered messages survive with no card_id, but existing history append/import creates new rows and automatic semantic recapture can claim historical work as newly active. The reviewer can identify existing work but cannot record that judgment against the original source with the supported history API.
COST: The original-operand reconciliation remains blocked after the bounded cancellation fix; a missing-route negative control fails with404, and a draft retry action had to be replaced because it could create false current task claims.
FIX: Add an explicit audited PUT history/{id}/card that preserves owner/status/delivery, rejects conflicting linkage, and atomically records original source plus rationale. Transaction failure produces a measured WARN and rolls back the link. Publication and live six-message reconciliation remain pending.

## Recovery health budget is shorter than its permitted attempt
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-883
ORIGINAL_CARD: AMUX-4487
SYMPTOM: Live c6242e22/build36e3f03eb7487ab1 registers message-capture with interval30s and stale_after90s but allows120s per attempt. A read shows in_flight true, last_tick_age90.96s, last_tick_ms120003.61 and status hung; its missing catalog row also reports documented false and purpose null.
COST: The first deployment verification found a falsely unhealthy job during its own permitted runtime and an undocumented background loop, requiring a follow-up before the feature can be described as operationally coherent.
FIX: Use a90s cadence whose existing health budget240s covers both a90s idle interval and the120s attempt bound, share the job ID with its catalog and publish the real disable control. Regression checks the actual registry budget against the source constants; startup INFO records both budgets with measured/count. Pending work success and original six-link reconciliation remain separate.

## Sticky session fixture retries its first discovery race but fails its second
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-766
SYMPTOM: Final ecd36b56 Rust CI failed2376passed1failed8ignored: the idle follow-up GET returned the explicit concurrent discovery epoch500 at workers.rs2537. Only the first GET had bounded retries. A one-shot idle middleware refusal reproduced0passed1failed locally.
COST: Published corrections cannot satisfy their CI verification gate; required a fresh deterministic reproduction and another clean publication.
FIX: Use one bounded fixture reader for both phases, retain unrelated500 and exhausted-churn failures, and emit stage/attempt test-log diagnostics. Keep AF-757 quota fixture contamination separate. Validation and independent review pending.

## Browser CI reaches its job deadline without a final test population verdict
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-768
SYMPTOM: ecd36b56 job 103697706086 cancelled after 30 minutes; 1,011 tests started with two workers but no final summary or failure-only artifact upload survived. Deadline is consistent with the observed timing, not independently proven as the only cancellation cause.
COST: One 30-minute CI attempt produced no complete browser verdict and blocked honest verification of AF-762, AF-766 and AMUX-4487.
FIX: Four unchanged-population shards, a shorter runner deadline and incremental completion evidence with full-union validation; local 13/0 controls and 1,011-test disjoint union pass. Fresh complete GitHub execution and independent review remain outstanding.

## Native backlog nudge counts blocked cards that its own list excludes
AREA: notices
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-770
SYMPTOM: Native MSG-59704 says five drainable cards but lists only TG-3705. The runtime count omits blocked_on while the list excludes it; deterministic blocked-only fixture counted 1 with an empty list. ts-gke also reported separate orchestrator messages listing parked cards; those remain AF-771, not proof of this native mechanism.
COST: Inflated workload and escalation input; investigating the peer report required separating two generators before identifying the count/list disagreement. The repeated external re-measurement cost belongs to the still-open orchestrator investigation.
FIX: One shared dispatch-eligible population for native count/list/cadence, explicit display truncation and measured selection/error logs. Red control 0 passed / 1 failed; corrected gates, review and deployment pending.

## Terminal browser fixtures kept testing retired request and input contracts
AREA: gates
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-772
SYMPTOM: Full CI run34749407249 finished every test but terminal fixtures stubbed limit=200 while the client requested60, expected Tabs after a grid icon shipped, called unsupported mouse.wheel in mobile WebKit, and clicked an existing-install menu beneath a fresh walkthrough. A local all-project correction run also caught delayed history arriving before Safari dispatched the fixture's scroll event.
COST: 39 terminal-product,3 tab-label,1 wheel and1 onboarding failures in the complete CI matrix; local evidence retained1 failing history reproduction,44/1 Safari and134/1 all-project runs before the final ordering correction. No elapsed-time estimate or product regression count inferred.
FIX: AF-772 repairs scoped request/context/input prerequisites while retaining route-hit guards, attribution/buffering/visibility assertions and all3projects; measured input-method diagnostics distinguish positioned browser specimens from separately checked native Simulator swipes. Remaining browser failure families stay open under AF-748.

## Lifecycle teardown refused its own delete and hid the original failure
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-773
SYMPTOM: All6 worker-lifecycle failures in completed CI run34749407249 reported only left-worker-behind. The test asserted bareDELETE403, repeated that forbidden call in finally, then threw over the original error. Exact historical-body fault control lost ORIGINAL_MID_FLOW_FAILURE and kept its fixture worker. Corrected teardown exposed a hidden receipt-target span selected ahead of the visible worker card.
COST: Six original CI failure causes were hidden, and refused cleanup could leave test-owned provider panes on the shared host. A fresh real desktop run spent30s on the hidden locator; its cleanup is independently confirmed absent with an exact tmux control. No claim about how many historical orphans remain.
FIX: AF-773 gives exact-fixture teardown a separate budget and guarded POST, retains primary and cleanup errors, emits measured client-debug/CI evidence, and scopes the worker visibility assertion to its real fleet card. Five installed-runner controls pass; all-project product lifecycle results remain separately required.


## Lifecycle deletion screenshot passed while the worker card remained visible
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-773
SYMPTOM: Exact d5293449 lifecycle matrix reported6 passed, but Safari Haiku worker-deleted.png still showed the deleted worker as WORKING. The test waited for a function definition and absence of an unrelated modal after reload, without requiring fresh session data or the actual card to disappear.
COST: One misleading deletion screenshot in a green six-case matrix; independent visual inspection and two focused browser probes were needed before closure.
FIX: Await the current list refresh and assert exact UI/API absence, with measured deletion-view evidence in client-debug and screenshots. Frozen stale-list negative control fails expected0/received1 while API membership is false; clean matrix and independent review pending.

## Helper pipe I/O escapes the model deadline
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-13
SESSION: codex-server-sync
CARD: AF-884
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Real child regressions show a large unread prompt blocks before the timeout starts, full stdout/stderr pipes deadlock before exit, and inherited pipes block after the parent exits. All three tests failed before the transport fix.
COST: Capture/classification calls can occupy helper slots beyond their promised deadline; all three failures were reproduced without launching a native worker.
FIX: Nonblocking concurrent stdin/stdout/stderr under one deadline, isolated helper-group cleanup, explicit bounded output retention and partial-input failure; LC-HELPER-FAILURE now includes these cases.

## Offline recovery passes transport checks while its error UI still dominates Workers
AREA: ui
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: codex-server-sync
CARD: AF-885
ORIGINAL_CARD: AMUX-4417
SYMPTOM: Fresh desktop/mobile/WebKit transport tests pass, but opened screenshots show the expanded worker-list error panel that Ethan requested inside the sync modal, raw HTTP operation labels, and a five-operation count above a seven-operation checklist including files.
COST: Nine green automated cases did not establish the requested visual behavior; eight screenshots were inspected and the missing acceptance requirements recorded as LW-12. No data loss was observed in this selected run.
FIX: Pending: modal-only detailed errors with reachable retry/discard, readable operation labels, and consistently scoped pending totals; retain individual acknowledgement checks.


## Header declutter hid the only durable action inspector
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-774
SYMPTOM: AMUX-4475 removed header clutter with display:none!important on the sole interaction-feedback hub. Receipt retry/recovery continued, but users could not inspect pending/completed/refused actions or remedies; the full CI population retained18 hidden-summary failures.
COST: Eighteen failed receipt cases in the completed1011-case CI run and a separate exact Safari reproduction to distinguish hidden feedback from failed effects recovery.
FIX: Move the existing receipt inspector into Notifications with visible access, viewport bounds, scrolling and dismissal, preserving the compact header. Measure visibility in client-debug, retain receipt semantics/no-resend tests and validate native iOS; draft implementation under AF-774.


## An incoming receipt update collapses the Details section being read
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-774
SYMPTOM: After opening the last retained receipt Details in Safari and reading one effects response through the actual reconciler, the disclosure lost its open attribute. feedback.mjs replaces every article on each receipt update and did not preserve disclosure state.
COST: One failed targeted Safari regression after the inspector became reachable; a person reading the remedy would have to reopen it after updates.
FIX: Preserve expanded receipt identities across rendering, log measured retained/restored counts, and verify open Details plus reading position through a real effects read. Full matrix/native/review still pending on AF-774.


## Receipt updates discard keyboard focus while retaining open Details
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-774
SYMPTOM: Receipt rendering replaces the focused Details summary, moving focus to body while its disclosure remains open; Enter then no longer operates the receipt.
COST: Independent review rejected the candidate; two browser probes exposed a keyboard continuity gap missed by the 69-case matrix.
FIX: Restore only the retained focused receipt summary within the active panel with preventScroll; record measured focus restoration/loss and exercise real effects reads plus outside-focus/dismissal controls.


## Golden offline replay test stops at obsolete generic operation wording
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-775
SYMPTOM: All three golden offline CI cases expected 3 ops while the current banner says 3 queued, will send on reconnect, so the retained real replay/uniqueness assertions were never reached.
COST: Three persistent CI failures and loss of downstream offline replay coverage in the full matrix until this fixture was corrected.
FIX: Assert the current explicit queued state/count, retain original real UI replay and uniqueness checks, and publish measured banner/queue/operation evidence in CI and amux client-debug. Working-tree six-case golden suite passes; clean gates/review/publication pending.

## Keyboard sizing overrides the offline warning's reserved space and covers Save
AREA: browser
SEVERITY: blocks
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-777
SYMPTOM: Published ec76baf7 CI has two Chromium sw-fail-bar positive failures: the actual hit target at Save is the warning. The shared keyboard max-height rule overrides the board editor's earlier subtraction of its measured warning height. A source geometry probe additionally shows a negative modal top and partially covered button edges even where the centre remains tappable.
COST: Two failing CI cases and another browser/native audit to reconcile the keyboard and offline-warning fixes; a user can see Save while its tap area is covered.
FIX: Preserve the measured warning subtraction in keyboard-sized board boxes, and report actual partial footer coverage through the existing measured modal-layout diagnostic. Owned draft has 33 browser passes plus two native iOS 26.5 checks covering real Save/readback, fully visible warning/buttons, broken-height diagnostic and dismissal; screenshots personally inspected. Independent review, clean integration gates and publication remain pending.

## Board archive silently ignores unsupported flags and hides the card without its reason
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-729
SYMPTOM: Four authorized historical-review archives used --outcome-stdin, a flag supported by status verbs but not archive. The archive parser broke on the unknown flag, sent archived=true anyway and returned exit0. Readback found no supplied reason in any of the four logs. A regression against the actual CLI reproduced invalid-argv PATCHes; this is separate from the already-landed API archive_outcome fix.
COST: Four missing audit reasons, four readbacks, and eight corrective unarchive/rearchive operations before all exact reasons were present; the first success reports hid incomplete writes.
FIX: Refuse unknown/trailing or missing-value archive arguments before any board PATCH, retain supported flag behavior, and emit a bounded privacy-safe cli-argument-refused diagnostic. Candidate and tests are in research/archive-cli-argument-refusal-2026-09-13.md; keep open until reviewed client installation and readback.

## Mobile header wraps while its clipping diagnostic reports no problem
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-779
SYMPTOM: Owner screenshot requested a fitting top bar. At 320px parent 9550ba05 renders a 106px two-row header; the prior geometry probe returns no clipping. Compact rules end at 480px although mobile layout extends to 600px.
COST: Repeated owner report and a header consuming an extra 44px row on narrow phones; prior green clipping coverage missed it.
FIX: AF-779 one-row 44px targets with fitting edge spacing through 600px, removed redundant top padding, and measured header-row-wrapped diagnostic. Browser 9/0 and real iOS Safari 6/0; research/mobile-header-fit-2026-09-13.md.

## Structured board creation waits for a model answer that cannot affect its outcome
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-785
SYMPTOM: Creating explicit ledger fix records waited tens of seconds per request. create_item called semantic plan while holding the lane lock, then always discarded its decision when graph/gate/scheduling metadata required a separate structured record. The branch-order regression measured one comparison invocation where zero was required.
COST: The ledger mapping paused after three creates instead of repeating this cost across the remaining100 records; an attempted atomic decomposition correctly refused the manually created epic and was not bypassed.
FIX: Decide the existing structured-create policy before invoking its comparison closure; keep ordinary semantic reconciliation and WIP/ownership guards. Emit measured structured_create with model_called=false and candidate_population_measured=false; do not claim a semantic comparison ran. Red control0/1, corrected intake3/0; release and live adoption still pending under AF-785.

## Upload storage paths masquerade as repeated instructions across repos
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-888
SYMPTOM: friction_themes.py reported eight cross-lane repeated instructions, including three cross-repo groups, over 115 messages. Seven groups matched only common amux upload-path tokens in unrelated screenshot requests. Even after filtering those paths, the signal kept its literal both-repos scope for one genuine amux-only toolbar request.
COST: The daily sweep would prescribe a global rule for a class manufactured from transport metadata. Extra manual message inspection was needed to reject the signal.
FIX: Strip only amux @-upload references before phrase extraction, derive scope from all surviving evidence, and report excluded-reference counts in the signal and friction-sweep.log. Actual SQL-signal tests must retain genuine repeated instructions and ordinary filesystem prose while rejecting screenshot-only matches. Originating-session validation and resolved verification gates remain required before retirement.

## Direct message records override their own failed submission verdict in diagnostics
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-889
SYMPTOM: The frustration scan labeled MSG-59389 and MSG-59393 delivered even though each stored submit_verdict=stuck. The same shortcut in GET /api/history/{id} returned delivered beside a stuck verdict. Both readers treated direct transport selection as proof of successful submission.
COST: The message sweep was told the two repeated RTSP requests had landed, concealing the delivery failure behind a success-shaped annotation. Extra source and exact-ID checks were required before judging the repeat.
FIX: In both readers derive direct delivery from the existing durable submission verdict: confirmed/retried delivered, stuck not delivered, unverified/missing/unknown values unknown. Exercise the actual scanner query/output and actual history endpoint, with confirmed positive controls and explicit failure diagnostics. Queued steering-history inference is separate; no production send or retry is needed for this read-path correction. Originating-session validation and resolved verification gates remain required before retirement.

## A sliced installer fixture reaches an uninitialized Rust stage before the Bash guard
AREA: testing
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-891
SYMPTOM: After private Rust artifact staging shipped, tests/cli_install.py executed only installer stage3 without the INSTALL_ARTIFACT_DIR created in stage2. It tried mkdir /publish and failed before reaching the real Bash syntax-refusal guard, reddening CI despite the new Rust publication checks passing.
COST: The checks gate failed on eefc294f and could not test the Bash publisher boundary it claimed to exercise. The isolated fixture had drifted from the caller's required inputs.
FIX: Supply a private Rust stage and stub only Rust artifact validation in this Bash-boundary fixture; retain the actual guarded Bash publisher and old-installed sentinel checks. The full Rust artifact path remains covered by its separate actual-installer matrix. Existing test and CI refusal output makes a recurrence visible. Origin validation and resolved gates remain required before retirement.

## Peer collaboration is counted as board nudges and terminal closures as all progress
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-890
SYMPTOM: The nudge-no-movement theme signal counted 8 board-drive pickup messages plus 8 substantive peer messages as 16 nudges for mixpeek-security. It called zero terminal closures no queue movement even though cards moved to explicit external-wait states.
COST: A collaboration-heavy lane was presented as a stuck nudge loop, inviting an unsupported fleet mechanism diagnosis and needless inspection of peer work.
FIX: Count only the actual board-drive pickup producer toward the nudge threshold, retain peer traffic separately, and state that nonterminal movement is unmeasured. Actual SQL tests retain a ten-nudge positive control and a terminal-completion control; friction_nudge_population records both populations. Origin validation and resolved verification gates remain required before retirement.

## A corruption fixture races the status hook's durable acknowledgement
AREA: testing
SEVERITY: slows
STATUS: open
DATE: 2026-09-13
SESSION: amux-frustrations
CARD: AF-892
SYMPTOM: The status-hook durability fixture writes corrupt bytes directly into a live queue after observing the HTTP request, before the asynchronous acknowledgement is necessarily persisted. A controlled lock schedule shows the old raw write overwritten with an empty queue; the corruption assertion can then fail despite no production regression. CI5f036682 exited1 inside this section without naming its failed assertion; exact historical failing line remains unknown.
COST: The checks gate is red and the log only says exit1 after the preceding successful cell, requiring source inspection and an independent concurrency control to discriminate fixture timing from hook behavior.
FIX: Serialize corruption injection through the actual queue lock and atomically replace fixture bytes, retaining byte/schema preservation assertions. Add an ERR diagnostic naming the fixture line and command. The controlled old write loses the bytes; the corrected writer preserves them. This establishes a real fixture race, not the exact schedule of the historical CI failure. Origin validation and all resolved gates remain required before retirement.

## Pause reports completion while the worker's tools continue running
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-14
SESSION: codex
CARD: none — POST /api/board timed out after 30 seconds; the subsequent backlog read found no matching card
SYMPTOM: TubeScience Pause returned success and lifecycle=paused while running=true and the terminal badge still said WORKING. Resume re-rendered the old card without a verified runtime transition.
COST: User could not stop running work; required process-tree, queue, provider-protocol and browser regression tests.
FIX: Lifecycle integration stops owned provider/tool descendants, gates queued delivery and bootstrap, preserves the conversation reference, and acknowledges only verified transitions. Pending/failed transitions are visible and retryable. Signals: worker_lifecycle_applied, worker_lifecycle_failed, pause_process_stopped, protocol_turn_paused. Process fixtures cover Claude/Codex/Gemini and an unrelated process; browser checks cover desktop, phone and Safari.

## Opening and resizing a Claude terminal makes its history disappear
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-14
SESSION: codex
CARD: none — filing the related lifecycle incident timed out; no matching backlog card was returned
SYMPTOM: Ethan's recording shows mixpeek-finances briefly displaying history, then replacing it with a few native screen lines. TubeScience reproduces the same mostly blank terminal. Claude was on the normal screen (alternate_on=0), and the peek handler only hydrated its saved JSONL history in alternate-screen mode.
COST: User lost usable conversation navigation; required matching the recording to live tmux state and repeated refresh/resize browser tests.
FIX: Load Claude conversation history in either terminal screen mode. Signal normal_screen_history_restored identifies the recovered path. A server regression test pins normal-screen history and a browser test covers reopen, refresh and resize from desktop to phone.

## The shared-target fingerprint check cannot run from a clean checkout
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-14
SESSION: codex
CARD: AF-791
SYMPTOM: The unpublished fingerprint regression script required an ignored scratch/af791-evidence/cargo-specimen and passed an extra test filter through a wrapper that already invokes cargo test. A clean checkout lacked the specimen, and a zero-test result could satisfy its loose success check.
COST: Blocked verification while reconciling the user's request to publish every pending amux change.
FIX: Create an owned temporary, dependency-free fixture; preserve source mtime to nanosecond precision; require exactly one passing fixture test. The normal run passes both cases. Disabling the fingerprint refresh makes the preserved-mtime case fail; the mutation helper restores the exact bytes afterward.

## Haiku command intake uses both attempts without producing runnable work
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-15
SESSION: lifecycle-haiku-r3-0915 (evaluator: codex-lifecycle)
CARD: AF-904
SYMPTOM: MSG-63706 exhausted two interpretation attempts: verify with no existing ID, then invented canonical IDs. The duplicate MSG-63707 waited without another call. No command graph was committed; the subsequent execution fixtures were introduced directly and do not prove automatic intake.
COST: 10,346 measured input/cache tokens, 1,630 output tokens and a five-minute retry delay; the third trial could not demonstrate unattended command completion.
FIX: fac6452b supplies explicit identity repair instructions and retains raw attempts; deterministic regressions pass, but successful live recovery is unproven. Enforce structured output without hiding extra calls. All three authorized Haiku workers are paused; do not claim a fourth trial ran.

## The browser reaper closes a profile that CDP is actively driving
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-15
SESSION: amux
CARD: AMUX-4685
SYMPTOM: "[amux] I closed your browser on profile 'default': nothing had driven it for 6min (the activity window is 5min)" arrived three times while I was driving that exact tab over raw CDP, once mid-sweep. Activity is counted as an amux browser API verb; /chrome-cdp and skills/chrome-cdp/scripts/cdp.mjs send none, so continuous use reads as idle.
COST: Three browser restarts and one overlay sweep lost half-collected, roughly 15 minutes across an AMUX-4684 session. The kill notice names only AMUX_BROWSER_ACTIVITY_REAP_S in ~/.amux/server.env as the remedy, which needs a server restart, so a lane on a ten-minute browser task chooses between restarting the fleet's server and being interrupted.
FIX: Let the reaper see CDP: the server already stores the profile's cdp_port, so a read of /json/version on it answers "is a debugger attached" without touching the page. Or add a keepalive verb and name it in the notice, so the remedy reaches the lane at the moment it is being killed.

## The test wrapper exits 0 when the cargo budget refuses to run anything
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-09-15
SESSION: amux
CARD: AMUX-4689
SYMPTOM: `scripts/test-contended.sh -p amux-server` printed 17 lines and exited 0 with NO TEST RUN. The budget guard had refused (`{"event": "cargo_budget_refused", "target_bytes": 48094199808, "free_bytes": 356396068864, "reason": "target_size"}`) and the wrapper reported that refusal as a successful run. The contention block still printed "A failure here is NOT build contention" and the worktree block still certified the tree was clean "in this build", both statements about a run that never happened.
COST: I nearly cited it as the test evidence for AMUX-4527. The commit hook caught it instead, by a different route: "your last run EXITED 124 ... A red run vouches for nothing". Two instruments disagreed about the same run and only the incidental one was right. VERIFY.md's contract is to paste a command and its result line, and the result line here is an empty success.
FIX: Exit non-zero on `cargo_budget_refused` — a refusal is not a pass, and every caller already handles a non-zero exit. And print the remedy in the same breath: the refusal names `target_bytes` and `reason` but not `scripts/cargo-target-guard.py clear --target <root> --path <candidate>`, which exists and is invisible from there. This is the wrapper's own principle (a green must carry "and nothing was building" beside it) applied to the cheaper half, since a run that did not happen is knowable with certainty rather than inferred.

## Two report chores reached Done with incorrect required metrics
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-15
SESSION: codex-lifecycle
CARD: AF-393
SYMPTOM: Fleet validation's MF-1178 and MHC-856 reached Done with claimed passing verification, but independent recomputation found five wrong values. Boolean presence flags were counted as present even when false; missing acceptance criteria were reported as zero instead of 11 and 15. Format, row-total and upper-bound checks passed without proving the requested results.
COST: Two evaluator-assisted reopens and repeat worker execution were required to correct artifacts already presented as complete. Four other probes passed without evaluator correction; the bad values were detectable from the supplied input and were not a missing-data ambiguity.
FIX: Open. Require outcome-specific artifact checks and retain their actual results in the completion path; a link, valid JSON and a worker's PASS statement are insufficient. Correcting these two artifacts did not fix the shared gate. Existing AF-393 carries the new evidence; docs/command-lifecycle-fleet-validation-2026-09-15.md records all 17 active workers and the test's limits.

## A bulk board migration discarded a card whose own evidence said the fix was unshipped
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-16
SESSION: amux-frustrations
CARD: AMUX-4739
SYMPTOM: AF-640 (Store::read() pinning the maintenance runtime pool under load) was marked `done` at 08:36 with a real partial fix (088fa453, 5s connection_timeout) but its own evidence field said so explicitly: "What this does not fix: reads still pin a thread while they wait. The real remedy is a read_async mirroring write_async" — ~440 call sites, never done. At 09:29 a bulk migration (`bulk-migrated backlog -> discarded by api-anonymous`) moved it straight to `discarded`, with no check of whether the still-open remedy named in its own text had shipped. AF-642 (this session, same day) independently re-derived the identical mechanism from fresh log evidence, confirmed AF-640 as the owning card, and pointed back to it — landing on a card that no longer existed as live work.
COST: The read_async fix has had no live owning card since 09:29. The underlying bug kept running at ~22-27 blocking-poll warnings/minute all day (28,425 in the 17.5h since the server's last restart) with zero visibility, until it produced a user-visible symptom (two dashboard message sends failing/stalling) that had to be traced back through two already-discarded cards to find the actual diagnosis. Filed AMUX-4739 to restore a live owner.
FIX: Open. A bulk status migration that moves `done`/`backlog` cards to `discarded` should not fire on a card whose own evidence text names unshipped follow-up work — at minimum, grep the evidence for a "not fixed" / "real remedy is" pattern before a bulk discard, or require the discarding actor to read the card body rather than act on status alone. This is the same class as the STATUS meaning section already in this file's own rules (`done` != `verified`): a bulk migration over status is not blind to *label*, but it was blind to *content*.

## push-consent.sh printed "Nothing to push." while a commit sat unpushed
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-16
SESSION: amux
CARD: AMUX-4741
SYMPTOM: `scripts/push-consent.sh` defaults its tip to the local branch `main` (`TIP="${2:-main}"`). CLAUDE.md's Deploy section instructs every lane to run its gates on a DETACHED worktree (`git worktree add --detach`), and on one of those, local `main` is a stale ref unrelated to the commits being pushed. Run with no arguments on a worktree holding one unpushed commit, it printed `range origin/main..main` / `commits 0` / `Nothing to push.` and exited 0. The push itself was a different range entirely (`origin/main..HEAD`, 1 commit).
COST: A false clear on the one gate whose entire job is to stop a push that should have asked someone first. Here it cost only the minute it took to notice the count contradicted a range I already knew; the real exposure is a lane whose range holds a PEER's commits, which gets the same `Nothing to push.` and pushes them unasked, against a rule CLAUDE.md marks MANDATORY. Two instructions in the same file point opposite ways: run gates detached, and read consent from `main`.
FIX: Fixed. Tip now defaults to HEAD, which is correct in both shapes (on a checkout sitting on main, HEAD is main). The banner also prints the resolved sha and the branch or `detached HEAD`, and prints a `note` line naming local `main` whenever it is a different commit, so a reader who remembers the old default is told which range the verdict covers. The note line is computed from a rev-parse comparison, so it cannot print when it is not true.


## Worker terminal freezes until a browser refresh restarts updates
AREA: browser
SEVERITY: blocks
STATUS: open
DATE: 2026-09-17
SESSION: Ethan via Codex
CARD: AF-909
SYMPTOM: The worker terminal intermittently stops updating and resumes after refresh. Four browser regressions fail against the shipped source: a body stalled after headers outlives the cleared timeout, unrelated text selection suppresses terminal requests, a cancelled touch leaves the selection latch set, and a selection begun during a response consumes an unpainted frame's ETag/raw dedupe state.
COST: Ethan must refresh to see ongoing worker output; all four reproduced cases leave stale text visible despite later available output.
FIX: Keep the timeout through response consumption; commit ETags only for validated accepted frames; scope selection to terminal ranges and reconcile cancelled/resumed gestures. Log refresh-failed with the transport phase and selection-recovered with the event reason through client-debug. Browser regressions cover automatic recovery and retained reader position on desktop, phone width and Safari. Originator confirmation remains pending.


## Stop requests time out and multiply while the worker keeps running
AREA: browser
SEVERITY: blocks
STATUS: open
DATE: 2026-09-17
SESSION: Ethan via Codex
CARD: AF-911
SYMPTOM: Two Stop tubescience rows appeared with a 15-second timeout. The two interaction IDs were attempted five and two times; both eventually reached the asynchronous handler and emitted stopped at 13:08:46 UTC. A separate start followed. The same interval had a 65-second health probe. Policy role lookup and legacy worker routing still acquired database readers synchronously on HTTP runtime threads; Stop also typed slash commands into a busy composer and waited for its shell.
COST: Ethan could not tell whether Stop was accepted or effective, repeated the action, and waited through duplicate requests while tools continued running.
FIX: Cache the running executable hash once per process: writer-origin telemetry measured invariant recording at 97.6s and 89.9s because roughly 700 result rows each rehashed the whole executable inside the serialized write. Candidate activation hashing stays uncached so replacement binaries are still checked. Yield during policy-role/routing reads, persist and coalesce pending Stop intent across tabs, preserve Stop/Start ordering, and route Stop through the existing process-tree termination used by Pause. Record confirmed stop/failure and final interaction progress. Fault-injection tests exercise exhausted readers, browser outage/replay on desktop and mobile, and descendant termination. Originator confirmation remains pending.

## Task intake loses acceptance criteria and parked backlog has no bounded recovery
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-17
SESSION: Ethan via Codex
CARD: AF-912
SYMPTOM: Implementation tracked by AMUX-4748. Creation preserved next_action after 553c17b1 but still discarded supplied acceptance_criteria. Idle workers excluded source_ref/blocked_on backlog and missing-continuation Todo, while the continue prompt only counted blocked status. Pickup guidance explicitly encouraged handing component work to another worker and parking ordinary decisions.
COST: The active-fleet audit found 342 Backlog/Todo tasks across 15 workers, including nine idle workers; 16 cross-owner dependency edges pointed to inactive workers. Repeating a broad audit consumed turns without supplying a runnable next step.
FIX: Round-trip acceptance criteria through creation, prevent semantic intake from dropping explicit execution fields, align backlog selection with the continuation gate, and send one specific recovery per substantive blocker state through the existing durable revision-guarded queue. Allow only one outstanding blocker review per worker, enforced both before the candidate scan and atomically in the delivery queue; unrelated card identities must not stack model turns. Preserve actual holds and terminal gates. Prefer end-to-end ownership and independently executable prerequisites. Originator confirmation remains pending.

## Second instance: the same daily bulk migration discarded a card recording a live safety decision
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-17
SESSION: amux-frustrations
CARD: AMUX-4752
SYMPTOM: AF-398 (whether the dead host-memory-pressure admission gate in start_worker should be wired into the real session-start path or removed, since it currently guards 0 of ~140 live lanes) was `bulk-migrated todo -> discarded by api-anonymous` on 09-15 at 09:29 with "Tests/deployment/live evidence: not recorded. Linked assets: none recorded." Re-verified today (2026-09-17): the underlying finding is still exactly true -- /api/workers still returns 0 rows, 140 lane env files exist, session_verbs.rs still has no admission check. AF-298 (start/stop routing) depended on AF-398 and had its depends_on edge silently cleared when AF-398 left a live status, same as if the decision had actually been made. It had not. This is the same mechanism AF-640/AMUX-4739 (frustrations.md, 2026-09-16) hit one day earlier, same 09:29 timestamp both times -- looks like a recurring scheduled job, not a one-off.
COST: A real, carefully-reasoned safety decision (should 140 live lanes, including the operator's own, start refusing under memory pressure) sat invisible for 2 days with nothing tracking it, discoverable only by chance when a dependent card (AF-298) got auto-picked-up and someone actually re-read the chain instead of trusting the cleared depends_on edge. Re-filed as AMUX-4752.
FIX: Open, same fix named yesterday for AF-640: a bulk discard job should not fire on a card whose text describes an unresolved decision, and a depends_on edge should not auto-clear just because its target reached ANY terminal status -- discarded is not resolved. Two confirmed instances in two consecutive days is enough to call this a systemic gap in the migration job rather than two unlucky cards.

## CORRECTION: no scheduled job discards cards -- it is a human dashboard action, and this entry and yesterday's were both wrong about the mechanism
AREA: attribution
SEVERITY: annoys
STATUS: open
DATE: 2026-09-17
SESSION: amux-frustrations
CARD: AMUX-4752
SYMPTOM: This entry and yesterday's AF-640/AMUX-4739 entry both asserted "a daily 09:29 bulk migration job" discarding cards. `amux` (server-verified origin) corrected this: there is no script, scheduler or runtime job calling POST /api/board/bulk-migrate anywhere in the codebase (grepped). It is the dashboard's bulk-migrate control (app.js:29440), human-driven, behind a confirm() that names the count and warns there is no single undo. It records as `api-anonymous` because the browser sends no X-Amux-Session on that call, not because a daemon did it -- every dashboard bulk-migrate looks the same way. The 09:28-09:29 coincidence across two days was two separate human actions landing in the same minute, not a schedule: bursts also occurred at 04:47, 15:40-45 and 00:12, and the biggest (373 cards in one minute, 09-15 04:47) was all mixpeek-orchestrator cards -- one click on "migrate all cards from this column" against a retired lane's board, not a targeted sweep.
COST: Would have sent whoever picked up "FIX: a bulk discard job should not fire on..." looking for a script or scheduler that does not exist. The underlying remedy (re-file cards whose substance a column migration swept) was correct and stays correct; only the mechanism description was wrong.
FIX: The REAL defect, per `amux`'s finding: a 373-card destructive action with no attributable actor (`api-anonymous` on a bulk discard means nobody can answer "who cleared this column" afterward -- ethos rule 6, same unattributed-write class AMUX-1812 fixed for schedules). `amux` filed that separately. This entry exists only to correct the mechanism claim in the two prior entries; do not build or look for a scheduled-job fix, there is nothing to disable.

## Finances highlights the board-drain command while concrete execution remains in backlog
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-17
SESSION: Ethan via Codex
CARD: AF-913
SYMPTOM: MF-1231 retained its captured Prompt envelope after status appends, yet generic structured-text classification treated it as executable work and spent three advance nudges instead of requesting disposition. Five existing epics had no children. MF-1209's real 409 was gate-not-acknowledged, which the worker misreported as WIP; the capture was correctly exempt from WIP.
COST: The dashboard advertised an umbrella objective as Working now while the concrete tasks and an already-delivered cost report remained in backlog; three generic turns did not repair the state.
FIX: Use the shared capture predicate for advancement and give its one durable disposition request precedence over generic cooldown/budget. Tell workers how to reuse existing epics/tasks and to inspect refusal bodies. Repair the existing finances graph and record the actual delivered report; leave originator confirmation pending.

## Worker-owned boards regain cross-worker waits through alternate creation paths
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-17
SESSION: Ethan via Codex
CARD: AF-914
SYMPTOM: TubeScience TUBES-2461 acknowledged its Doing gate, then the next tick parked it against a Done census mis-typed as code. Old serial-order requirements survived in flat gate overrides and waiting fields after dependency edges had been removed. Active workers had 25 cross-board dependencies, including Studio waiting on paused backend work. Routed requests and peer-message capture bypassed the own-board create rule. Blocker-review identities included appended descriptions, so progress notes could rearm model turns.
COST: Workers ran without a valid highlighted card, or waited for other workers and repeated reviews instead of completing their own outcomes.
FIX: Make boards self-contained by default: refuse cross-board assignments/dependency writes, retain peer messages as coordination without minting tasks, and correct fleet guidance. Keep the explicit legacy cooperative opt-in separate from the default. Reject unready Doing transitions atomically using the pickup predicate. Suppress blocker-review repeats on note-only changes. Repair TubeScience's measured census type, preserve outcome requirements in acceptance criteria with per-column gates, and turn active workers' foreign waits into owned next actions while preserving real access, spend and customer-outbound restrictions. Originator confirmation remains pending.

## Studio repeatedly waits for approval of the same variable-path cleanup pattern
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-17
SESSION: Ethan via Codex (studio-plg screenshot)
CARD: AF-915
SYMPTOM: Studio's private-worktree resync repeatedly used rm -f "$WT/$f" after checking only existence on origin/main. Claude's native possibly-empty-path protection still prompts in bypass mode. The installed PreToolUse guard returned exit 0 with no corrective decision for the captured command, leaving the worker waiting for a person instead of revising it.
COST: Four matching Bash tool calls took 3m10, 4m32, 10m35 and 12m29 to reach their tool results. Those intervals include execution as well as any approval wait; the final prompt was captured in Ethan's screenshot.
FIX: Return a deterministic PreToolUse deny with repair instructions for unchecked variable directory prefixes in direct rm/rmdir calls. Require resolved, owned targets and exact content comparison before deleting landed copies; keep native checks and never auto-approve. Log a bounded measured correction event with a command hash. Install and replay the published hook, including all four captured commands; originator confirmation and a fresh live model retry remain unmeasured while Studio is rate-limited.

## The pre-commit cargo gate crashes with a Python traceback and refuses a valid commit
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-17
SESSION: amux
CARD: AMUX-4758
SYMPTOM: Committing at load average 121, `scripts/safe-cargo.sh` ->
  `scripts/cargo-budget.py` failed inside its OWN instrumentation, not the build:
  three `cargo_budget_unmeasured` events ("Command ['du','-sk',...] timed out
  after 20 seconds"), then `cargo_budget_stopped ... reason: probe_failed`, then
  `PermissionError: [Errno 1] Operation not permitted` from `os.killpg`, then
  `subprocess.TimeoutExpired: Command ['ps','-A','-o','pgid=,stat='] timed out
  after 5 seconds`, then a traceback out of `sys.exit(main())`. The commit was
  refused and HEAD did not move. `cargo clippy --workspace --all-targets --
  -D warnings` had passed clean on the same tree minutes earlier and passed
  again on retry once load fell to 55.
COST: One refused commit, ~11 minutes waiting for load to fall, one retry. No
  wrong conclusion shipped only because a clean clippy run was already in hand.
  The failure mode points the wrong way: a Python traceback out of the commit
  gate reads as a broken toolchain, so the next lane may go looking for a Rust
  problem that does not exist.
FIX: The script already knows how to say it cannot measure — it emits
  `cargo_budget_unmeasured` with `measured: false` three times before dying. Make
  a failed self-probe degrade to unenforced-and-say-so rather than aborting the
  supervised command, the way the staged-guard already does when it cannot reach
  the server. The budget itself should stay: AMUX-70 is real, an OOM-killed cargo
  in a shared pane scope takes the whole session down. The point is that a
  supervisor which fails exactly when the box is loaded fails exactly when peers
  are most active and a lane most needs its commit to land.

## `POST /api/board` appended to another card and the reply was shaped exactly like a create
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-17
SESSION: amux
CARD: AMUX-4776
SYMPTOM: Four creates with distinct titles, four 200s, four `id` values, zero
  cards created. Semantic intake matched all four to the open card the caller
  was working and appended each title to THAT card's `desc` as a
  "### Additional request" block; every reply returned the existing card's id.
  The dedupe decision is defensible (the titles did describe the same work, and
  its reasons are on the card's log). The caller cannot tell: same status code
  as a create, and `id` is the only field the sanctioned recipe in CLAUDE.md
  reads. The disclosure does exist — `intake.action` is "append" rather than
  "create" — in a SIBLING field nobody was told to read, which is the
  `ignored_fields` and `slim` shape already recorded twice in CLAUDE.md.
  The sharper half is that the call MUTATED A CARD THE CALLER DID NOT NAME: a
  request to create produced an edit to another record's desc, under the
  caller's attribution, with no signal.
COST: One wasted live-verification round — the bulk-migrate under test refused
  every id with `Stale { actual: "doing", expected: "backlog" }` because the
  four "new" cards were all the caller's own in-progress card — plus four junk
  blocks appended to the card being closed, which had to be noticed and stripped
  by hand before it could be read by anyone else. About 6 minutes and a polluted
  desc on a card under review.
FIX: Say it in a field the caller already reads. `"created": false` beside the
  id, or a distinct `code` on the append path. A caller that checks nothing
  must not be able to read an append as a create. Keep the dedupe.

## A provider swap strips a flag and writes the same flag back, and nothing can tell
AREA: providers
SEVERITY: wrong-state
STATUS: fixed
DATE: 2026-09-18
SESSION: amux
CARD: AMUX-4785
SYMPTOM: `PATCH /api/sessions/desktop/config {"provider":"ollama"}` returned 200
  "provider set to Ollama" and left `CC_FLAGS="--dangerously-skip-permissions"`,
  which is CLAUDE's yolo flag, on a worker that now launches codex. The swap
  really did call `strip_provider_yolo_flags` and really did remove the flag;
  the next line called `provider_yolo_flag("ollama")`, whose match had no ollama
  arm, so the default handed back the identical string. Strip and re-add
  cancelled out and the response could not say so.
COST: Nothing broke, and that is the whole entry. The ollama launch arm tests
  `PROVIDER_YOLO_FLAGS.iter().any(...)`, which matches all three spellings, so
  it emitted codex's `--dangerously-bypass-approvals-and-sandbox` and the worker
  ran correctly. The LAUNCH was right and the STORED value was wrong, so
  `GET /api/sessions/desktop` reported `flags: "--dangerously-skip-permissions"`
  for `provider: "ollama"` and every CC_FLAGS-reading view agreed with it. This
  had been shipping since ollama became a provider and was found only because a
  human happened to read the env file during an unrelated switch (AMUX-4606). A
  permissive consumer downstream of a wrong writer does not fix the writer, it
  removes the only symptom anyone would have noticed.
FIX: `"codex" | "ollama" => "--dangerously-bypass-approvals-and-sandbox"` — the
  arm is keyed on the BINARY that gets exec'd, since an ollama worker is a codex
  process. Plus the signal the two-fix rule owes: the launch arm now WARNs when
  CC_FLAGS carries a yolo flag codex would reject, naming stored_yolo_flag and
  launched_yolo_flag, so residual pre-fix workers announce themselves to a
  `/api/logs` sweep instead of waiting to be read by hand. The general shape
  worth keeping: when a function answers "which flag does X take", a `_ =>` arm
  is a wrong answer for every X nobody listed, and it cannot fail loudly.

## The session row said provider ollama and active_model claude-opus-5, in the same payload
AREA: attribution
SEVERITY: wrong-state
STATUS: fixed
DATE: 2026-09-18
SESSION: amux
CARD: AMUX-4788
SYMPTOM: `GET /api/sessions/desktop` answered `provider: "ollama"`, `model:
  "qwen3-coder:30b-65k"` and `active_model: "claude-opus-5"` at once, with
  `tokens.total: 869632` and both `model_source` and `tokens_source` reading
  `"transcript"`. The transcript was the worker's PRE-SWITCH claude
  conversation, last written at the minute of the switch and frozen since.
  `transcript_evidence` parses the Claude Code JSONL shape and had no provider
  test, so it faithfully reported a conversation that had stopped being that
  worker's two hours earlier.
COST: A wrong reading I nearly shipped. Having just switched that worker, the
  obvious conclusion from `active_model: claude-opus-5` is that the switch did
  not take — and the tmux argv said it plainly had. Establishing which of the
  two was lying meant reading three functions across two modules. The token half
  is worse and I did not measure it firing: `session_report` uses the same value
  as its context-size fallback, and the comment directly above that call says a
  wrong count there produces a forced compaction of a healthy lane rather than a
  wrong badge.
FIX: Gate the reader on the provider that WRITES the file, derived from
  `launch_base_binary` rather than restated as a second list, and return the
  honest empty otherwise. The general shape worth keeping is the disclosure
  problem, not the missing test: `model_source: "transcript"` was TRUE and
  useless. It named where the value came from and never asked whether that
  source could belong to this worker, so the field that existed to make a doubtful
  value auditable is the field that made it look accounted for. A provenance
  label is not a provenance CHECK, and the two read identically in a payload.

## A test that asserts a probe SUCCEEDED is asserting the host is idle
AREA: tests
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-18
SESSION: amux
CARD: AMUX-4787
SYMPTOM: Two lib tests ran a native host probe under a hard 5s deadline and
  treated anything else as a defect: `native_memory_snapshot_is_measured_and_
  names_its_metric` asserted `measured == true`, and `native_open_file_probe_
  works_with_launchd_path_and_observes_held_file` unwrapped the deadline and
  panicked "deadline has elapsed". Measured on this box: `top -l 1` takes 8.1s
  at load 14 and 28-36s at load 38; `lsof` takes 5.5-6.7s enumerating ~182,000
  open files. Neither is a statement about the code.
COST: Two separate investigations in one day, each to prove a red suite was not
  mine. Both times the tests appeared alongside genuine contention flakes, and
  both times they survived the isolated rerun that cleared the others, which is
  exactly the signature of a real regression. Establishing otherwise meant
  timing the two probes by hand. CLAUDE.md already warns that a red suite here
  is not automatically a regression; these two made the reader re-derive that
  from scratch every time.
FIX: Assert the SHAPE either way, and admit exactly one host excuse. The
  memory test now admits an unmeasured snapshot only when the reason equals the
  producer's own timeout constant, so a malformed parse, a non-zero exit and a
  spawn failure all still fail; the lsof test matches tokio's `Elapsed` by TYPE
  rather than by message, so every other error still fails.
  The generalisation worth keeping: the `measured` / `why_unmeasured` contract
  this repo applies to every diagnostic ENDPOINT had not been applied to the
  TESTS of those diagnostics. `memory_consumers` exists to publish whether its
  measurement ran, and its own test said a probe that could not run is a
  failure. When a module's contract says "could not measure" is a legitimate
  answer, a test that forbids that answer is testing the machine.
  AND A BOUNDED PROBE HAS TWO HOST DIMENSIONS, NOT ONE. The first version of
  this fix handled TIME only, and the very suite run meant to confirm it failed
  with "open-file probe truncated": a busy host is also a BIG host, and `lsof`
  here emits 8,565,894 bytes against an 8 MiB cap. The size dimension was
  invisible until the time one was removed. That second discovery is also
  AMUX-4791, because the same cap defers real log retention on this box every
  tick — so the test was not merely flaky, it was the only thing reporting a
  production job that has silently not run.

## `waiting_on` PATCHed with its own documented JSON-object shape silently cleared it instead
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-18
SESSION: amux-frustrations
CARD: AF-930
SYMPTOM: PATCHed `waiting_on` on a real needsyou card (AF-546) with the exact
 object shape migrations/0048 documents ({"actor":...,"type":...,"question":...,
 "unblocks":...}) and the shape `advance()`'s own Gap-4 logic writes on NeedsYou
 entry. Response was 200, `applied: true`, `waiting_on` echoed back correctly in
 that one response -- but a subsequent GET, and the card's own `log`, showed
 `waiting_on: null`, with the log line reading plainly "amux-frustrations:
 waiting_on" as if the write had landed. `set_opt`'s `body_opt_str` treats any
 non-string JSON value the same as an explicit null: `Some(v) =>
 Some(v.as_str().map(str::to_string))` returns `Some(None)` for an object,
 which is indistinguishable from a caller clearing the field on purpose. Same
 defect shape AF-711 already fixed for `acceptance_criteria` four lines above
 the unfixed `waiting_on` call site in the same file.
COST: two extra round-trips fixing the same card's `waiting_on` field before
 realizing the object shape itself was the problem, one of which briefly left
 AF-546 -- a card actually waiting on Ethan -- carrying a stale, wrong question
 because the correction attempt used the same broken shape.
FIX: 831cc0cb + a35d850f. Added `encode_waiting_on` mirroring
 `encode_acceptance_criteria`: object or non-empty string -> JSON-encoded and
 stored; null/empty -> clears; any other shape -> rejected with a 400 instead
 of silently coerced into a clear. Also fixed a second, related read-side gap:
 `snapshot_fields` decoded `waiting_on` via `serde_json::from_str(s).ok()`,
 which reports the same null for "empty" and "holds real content that failed
 to parse" -- switched to the existing `parse_json_or_raw_string` helper
 (already used for `acceptance_criteria`), so legacy non-JSON content also
 stops rendering as null. 6 new tests, mutation-verified: reverting the object
 arm to `Ok(None)` reddened exactly the 2 tests exercising that shape.

  ## A second amux-server-rs opened the shared production DB for 13h with zero warning
  AREA: instruments
  SEVERITY: slows
  STATUS: open
  DATE: 2026-09-19
  SESSION: amux-frustrations
  CARD: AF-937
  SYMPTOM: found a live, healthy-looking `amux-server-rs` process (pid 21435, port
   8823) that had been running since the prior afternoon, started manually from a
   bare Terminal.app shell with no AMUX_RS_PORT set, so it fell onto the compiled-in
   `DEFAULT_PORT` (8823, config.rs) and the default `AMUX_HOME` -- landing on the
   *exact same* `~/.amux/amux.db` the real, launchd-managed server (8824) already
   held open. `lsof` showed identical .db/.wal/.shm inodes on both pids. Both
   `/health` endpoints reported success the entire time; nothing anywhere logged,
   counted, or surfaced that two writers existed. This is the same underlying shape
   AEAB-11 reported a month earlier (2026-08-17) and it recurred with zero
   detection in between.
  COST: unmeasured but real -- the original AEAB-11 instance of this exact pattern
   dropped a batch of request-log rows to lock contention and doubled that day's log
   volume. This time nobody was watching for it; it was found by accident while
   resolving an unrelated stale board card, not by any instrument. 13 hours is a
   lower bound on how long it could silently run, since only self-adoption (an
   unrelated mechanism) kept it alive that long by re-exec'ing it onto every new
   build.
  FIX: not applied here (killed the orphan process, which fixes this ONE instance,
   not the class). Filed AF-937: Store::open (or a lib.rs startup check) should
   probe for an existing writer on the same db_path and log a loud WARN naming it,
   per ethos rule 4 -- both servers here reported "healthy" the whole time, so
   nothing about the failure was wrong-looking from either process's own vantage
   point.

## Numbered request captured as an active task named 1
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-1
SYMPTOM: MFEM1-53 was Doing with title `1` derived from `1. lets ensure ...`; separate capture producers and delivery-only holds left raw requests looking like executable work. An already-delivered advance reminder also returned before independent pickup.
COST: User could not identify active work from the board; audit required all 31 active workers and 1,298 open cards to distinguish execution from intake and stale reminders.
FIX: Consolidate capture and structured-intake predicates, strip list syntax before sentence extraction, and yield suppressed reminders to guarded pickup. Cross-board create correctly refused filing this on amux-frustrations; track on the originating board under the user's worker-ownership rule. Originator acceptance remains pending.

## Launch retries duplicate cards and disable backlog draining
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-1
SYMPTOM: Launch-created workers had three identical priority cards and AMUX_DISPATCH_BACKLOG_WHEN_IDLE=0; fan-out enabled the flag. Repeating either endpoint rewrote child env files, and title-derived identities could collide or change on retitle.
COST: Three workers each held three copies of the same priority, while the harness could not drain their backlogs after completing the current task.
FIX: Share ephemeral provisioning, reuse an identical open launch graph, retain assigned identity across retries, preserve pause/configuration, and fan out only ready independent tasks. Originator acceptance remains pending.


## Orchestrations stays blank while full board history loads and misses child follow-ups
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-2
SYMPTOM: The Orchestrations view fetched all 20,888 issues with full descriptions before rendering; existing fan-out follow-ups without epic links were absent, and To Do epics were labelled paused regardless of worker lifecycle.
COST: The owner could not see running fan-outs or assess their complete queues from Orchestrations.
FIX: Compact measured projection of existing boards, whole child queues and orphan fan-outs, actual worker pause state, explicit loading/error/retry, shared terminal predicate and current-task highlighting. Pending originating-user validation.

## Fan-out restart can discard the workspace and has no durable main integration stage
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-2
SYMPTOM: Ephemeral starts recreated detached worktrees; stop forcibly disposed worktrees. Existing fan-outs had no recorded creation base or automatic checked integration. Two legacy worktrees had empty indexes over populated commits, and two running children had no worktree directory.
COST: Completed child work had no deterministic route to main; stopping or restarting could lose uncommitted work, and malformed workspaces obstructed the board drain.
FIX: Durable per-child branches, preserve workspaces on stop/restart, whole-board integration admission, separately tested merge candidate and ordinary push, pause cancellation, explicit preserved legacy recovery. Pending originating-user validation.

## Self-contained boards could still acquire outside execution dependencies
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-3
SYMPTOM: Active-board audit found seven cross-worker dependency edges on six cards. Delegation opt-in bypassed dependency validation, missing/unassigned references escaped it, and fan-out moved a prerequisite while leaving its dependent on the parent board. The existing fan-out retry test asserted that split ownership as success.
COST: Repeated owner intervention to remove peer waits; six live cards required explicit ownership/next-action correction and a new regression covering the incoming side of reassignment.
FIX: Enforce same-board graph writes in storage and API, retain connected work on its owner board during fan-out, and preserve prerequisite evidence when repairing legacy edges. Live verification also found gate refusals recommending peer reviewer/dependency waits; those now teach local completion. No model calls are needed for enforcement.


## Idle board workers lose fallback observation after their hook expires
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-4
SYMPTOM: In the 28-active-board audit, mvs-infra and amux displayed idle while board-drive refused their expired Active reports and did not admit a current pane probe. The fallback measurement was disabled by the same hook whose evidence had expired.
COST: Two active workers with a combined 1,173 non-terminal outcomes could not cross the dispatch boundary at the measured snapshot.
FIX: Admit bounded current pane measurement when a running worker's structured report expires; require a recognized idle boundary and log measured fallback recovery. Regression includes empty, unknown and busy controls.

## Held reminders and epic containers suppress unrelated board completion
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-4
SYMPTOM: Already-delivered blocker recovery returned before verification; advancement queried 40 candidates but selected only the first; epics and raw captures blocked verification despite being exempt from pickup WIP. One unresolved verification batch member held all unoffered work for 24h. Global and amux-group Verified gates still required a different worker despite the owner's no-outside-dependency policy.
COST: Audit found 1,689 runtime Done outcomes awaiting verification, including 44 homepage, 81 gtm-engine and 328 amux-frustrations outcomes behind unchanged blocker recovery. These are measured queued populations, not all attributed solely to this bug.
FIX: Share execution-slot and reminder predicates; scan candidates past refusals; let independent verification pass held reminders; fingerprint batch output/contracts and release unoffered work on partial progress. Replace the live foreign-signoff criteria with owner reproduction and recorded evidence while retaining test/deployment/regression gates.

## Failed command interpretation leaves a request pending without an execution owner
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-4
SYMPTOM: Structured intake remained opt-in, while an exhausted two-attempt receipt stayed pending indefinitely. Active-board audit found 120 raw-capture candidates; repeated legacy launches also left three byte-equivalent full-e2e assignments.
COST: A request could consume its interpretation budget without becoming owned work, and identical fan-out copies occupied two additional Todo slots.
FIX: Default to bounded durable intake; exhausted interpretation creates one structured intake investigation on the same board using the existing dispatcher, with original errors and usage preserved. Archive only the two proven FETC duplicates against canonical FETC-3; preserve differing same-title outcomes for semantic reconciliation.

## Worker shell waits count themselves as another Git commit
AREA: workflow
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-4
SYMPTOM: Six shell commands on two active fan-out workers had waited 5–6 hours on fleet-wide pgrep -f "git commit". Their own shell command lines contain that expression, so each wait keeps itself blocked.
COST: full-e2e-test-coverage and mixpeek-fanout-eph-MF-1239 retained live shell work with no task progress despite separate durable workspaces.
FIX: Record exact PID/parent/worker evidence, terminate only confirmed self-matching wait shells after rechecking active lifecycle and child processes, and record the repair on their current boards. Document checkout-local bounded Git recovery; never delete another worker’s lock or treat process existence as progress.

## Global board render erases its current-work strip
AREA: ui
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-4
SYMPTOM: The real browser regression timed out finding global current activity even with two running fixture workers. Moving the shared strip inside its host for worker-detail scrolling also put the global strip inside the horizontal columns; every global render replaced that host and erased it.
COST: Running workers disappeared from board activity across filters and view changes, making real task execution look like non-adherence.
FIX: Mount global activity above the replaceable horizontal columns while retaining the worker-detail scrolling mount. Report active-work-missing through the existing measured UI diagnostics. Browser coverage exercises list/worker/status views, same-status task switching, filters, pause and both desktop/phone widths, with missing-strip and compressed-row negative controls.

## Supported textual criteria are misclassified as an unstructured capture
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-4
SYMPTOM: The public board API accepts text or a string array for acceptance_criteria, but has_execution_details and its SQL mirror only accepted arrays. Two active-board cards with next actions and nonempty textual criteria were consequently counted as raw captures.
COST: AMUX-4508 and MHC-808 could be sent back to intake despite already carrying the execution fields the public API accepts.
FIX: Use the same accepted text/array shapes in the shared Rust and SQL predicates, with empty/malformed/object controls. The text_criteria_recognized log names recovered captured work once per card/hour. Preserve the criteria verbatim and retain actual approval/event holds.

## A paragraph-length activity title displaces the board
AREA: ui
SEVERITY: friction
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-4
SYMPTOM: Live screenshot inspection found a legacy task with its whole request as its title. The activity strip rendered every line and stretched all neighboring cards to the same height, displacing the board on a phone despite passing presence checks.
COST: Current-work visibility consumed the space needed to see and operate the board.
FIX: Limit the activity summary to three lines, preserve the complete accessible button text and full task destination, and align cards independently. The existing UI diagnostic reports activity-summary-too-tall; the browser fixture covers a paragraph-length title and detects removal of the clamp.

## Fan-out verification accepts an unintegrated worktree
AREA: gates
SEVERITY: wrong-state
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-4
SYMPTOM: test-priority acknowledged Verified while its evidence still said push/CI pending. Its feature commit was not an ancestor of origin/main and the workspace had no creation base or integration receipt. The board accepted a textual assertion that contradicted the known artifact state.
COST: A live terminal count overstated actual completion; dependent work could consume an unintegrated outcome. Requiring the whole board before integration would also deadlock a successor waiting for a verified prerequisite.
FIX: Share a current-head/clean-worktree/integration-receipt check across Verified creation and transition, bind the async observation to the card revision/owner, and log fanout_verification_requires_integration. Integrate evidenced prerequisites before queued successors using the shared WIP predicate; active implementation and unevidenced review still refuse. Exercise real disposable Git integration and stale/dirty/missing-receipt controls through the API.

## Orchestration launch has no coordinating worker or independent fan-out model profiles
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-19
SESSION: codex-lifecycle-adherence
CARD: CLA-5
SYMPTOM: The launch form offered one shared provider/model pair and an Orchestrator (self) workspace selector. A launch created child workers without a dedicated coordinator profile; the user could not select distinct coordinator and fan-out models or see those roles in Orchestrations.
COST: One user-reported orchestration workflow blocked; coordinator ownership and model choices required manual worker setup.
FIX: CLA-5 creates a coordinating worker using the existing worker/epic primitives, separates role profiles and per-child overrides, preserves exact retry intent, and records coordinator provision/start verdicts. Browser/API validation and live deployment tracked on the card.

## Global Orchestrations lists ordinary epics and repeats fan-out workers per task
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-6
SYMPTOM: The global tab promoted ordinary epics into orchestration roots and rendered full fan-out boards as repeated worker rows. The live snapshot contained 13 fan-out workers under three parents, but 199 epics in the projection and nearly 200 displayed entries.
COST: User could not find the actual coordinator/fan-out structure in the global tab after the role/model launcher change.
FIX: Project actual tracked worker boards and linked ancestors, then group by recorded coordinator ownership. Show each worker once with expandable board tasks, active work, model and workspace state. Report included/excluded populations through orchestration_projection and API fields; preserve scoped and retired inventory.


## Verifying an unassigned card reports a false ownership race
AREA: gates
SEVERITY: wrong-state
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-7
SYMPTOM: The board details form submitted session:"" while moving a chore to Verified. The workspace preflight treated the owner as Some("") but the write normalized it to None, returning verification_observation_stale even though no concurrent edit occurred.
COST: Gate-revision acceptance failed on desktop, mobile and iOS, and legitimate unassigned tasks could not reach Verified through the UI.
FIX: Apply the transaction's nullable-owner normalization before measuring workspace readiness. Preserve the revision/owner race check, add its observed/current values to logs, and cover empty, whitespace, null and retained owners through the public API.

## An older board poll clears a newer read failure
AREA: ui
SEVERITY: wrong-state
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-7
SYMPTOM: Overlapping board reads published in response order. An older successful response could clear a newer board/status read failure and show Live while the board was unavailable; an old failure could likewise overwrite a recovery.
COST: The iOS outage acceptance test intermittently displayed Live instead of Sync error, hiding actionable failure state from the user.
FIX: Validate each response batch before publishing and order publication by read generation. Older reads may finish while a newer read is pending, but cannot replace a newer completed result. Emit board_read_superseded and test both failure and recovery with deliberately reversed responses.

## Lifecycle fixtures confuse host scheduling with product failure
AREA: tests
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-7
SYMPTOM: Full server runs failed before exercising Stop or Pause because fake tools were assumed ready after 150ms or one second. The sticky board-status fixture likewise exhausted five discovery attempts during process-wide epoch churn.
COST: Broad validation could not distinguish lifecycle failures from setup that had never reached the required state.
FIX: Wait on bounded readiness conditions, publish the fake provider PID atomically and clean up its process before reporting failure. Use the existing deadline-based real-handler discovery helper while retaining its error/refusal controls. Stop must still interrupt actual busy work within the original five-second deadline; readiness failures name the unmet condition.

## Orchestrations labels retained task links as live work
AREA: ui
SEVERITY: wrong-state
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-7
SYMPTOM: The deployed Orchestrations view showed Working now on audit-and-disable-unused and full-e2e-test-coverage while their measured runtime states were waiting and idle. It used task_board_id without the runtime activity verdict.
COST: The new orchestration view contradicted its own worker status and made retained board claims look like execution.
FIX: Share the board's runtime activity predicate, require a measured linked current task for live highlighting, and retain navigation under Current task when execution is not confirmed. Report changed activity projection counts and exercise active, idle, waiting, paused, stopped, expired, unlinked and unmeasured states in the browser.


## Layout acceptance reads different accordion renders as one frame
AREA: tests
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-7
SYMPTOM: The iPhone Paused/Archived order test queried bounding boxes in separate browser calls. Normal worker refresh replaced an accordion between element resolution and measurement, returning null while its replacement was visibly ordered correctly.
COST: The otherwise passing final Rust/browser CI run was red. A WebKit refresh diagnostic reproduced detached geometry in 38 of 60 samples.
FIX: Measure visibility, geometry and sibling order atomically in the page; check the initial frame and five actual worker refreshes. Emit measured frame counts and all rectangles in the test log, retain positive size checks, and require the entire live worker card to end above Paused.

## Successful fan-out merge leaves verification on the previous daily cooldown
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-8
SYMPTOM: An idle fan-out had three Done code outcomes and a successful current-head integration receipt, but board-drive still reported previous verification batch pending (24h retry). Verification identity covered the card and gate but omitted integrated output, so satisfying the merge prerequisite did not resume verification.
COST: The completed implementation remained unverified until an explicit continuation message; a normal board tick could not distinguish that new evidence from an unchanged wait.
FIX: Persist the successful integrated head as a durable session event and include it in verification identity. Backfill existing successful receipts at the next boundary, ignore unchanged-head retries and other workers' merges, and re-enter the existing verification/wake selector without advancing any card automatically. The regression fails on the old identity, exercises a stopped worker, and asserts repeated receipts consume no additional turns.

## A confirmed continuation remains in the coordinator input
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-9
SYMPTOM: A coordinator continuation returned confirmed after an Escape+Enter retry, but the exact instruction remained in its native composer. A later bare Enter resumed the coordinator. The retry still pressed Escape after picker-safe paste, and its final confirmation accepted a single cleared frame; a missing-UI frame also failed to reset the earlier clear observation.
COST: The orchestrator did not act on an accepted continuation until the terminal was independently inspected and the pending input submitted.
FIX: Retry Enter without Escape because picker-shaped input already uses bracketed paste. Require consecutive clear frames, including the final read, or durable provider acceptance. Emit submission_enter_retry with its actual key mode. A model-free real-tmux replay fails on the old retry bytes [Escape, Enter] and verifies the new path submits without interrupting; frame-sequence controls cover repaint, missing UI, active input and collapsed paste. Fixture cleanup uses the standard named exact tmux target so the source audit also verifies its cross-session isolation.


## Concurrent test subscribers hide board diagnostic warnings
AREA: tests
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-8
SYMPTOM: CI's parallel server tests returned the expected unreadable-WIP error but captured only the success-side INFO event; the WARN assertion failed. The board's two log-contract tests installed thread-local subscribers while sharing process-wide callsite interest with other tests.
COST: A valid release could not complete verification because the diagnostic test observed a different logging environment than production.
FIX: Execute each board diagnostic contract in its own exact-test subprocess, following the existing storage-probe contract pattern. Preserve every real production call and required diagnostic field; fail if the child exits unsuccessfully or runs zero tests. The child output is included on failure instead of retrying or ignoring a missing warning.


## Board recovery fixture mistakes a concurrent policy change for repeated work
AREA: tests
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-8
SYMPTOM: The parallel board suite queued another blocker-recovery turn after a progress-only edit. Its fixture left scoped settings unguarded while other tests changed AMUX_HOME; recovery identity correctly includes the approval policy, which differed between the real workspace and those temporary homes.
COST: The unchanged-state assertion failed for a changed-policy scenario it had accidentally constructed.
FIX: Hold the existing shared temporary-home guard in both recovery-identity tests. The policy remains fixed across progress-only edits, while real blocker/output changes must still rearm and explicit holds remain intact. Keep failed-test output as evidence rather than explaining it away as build contention.


## Worker status UI update leaves generated interaction inventory stale
AREA: tests
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-8
SYMPTOM: The combined main revision failed every browser shard before execution because the new status helpers changed the SPA function count from 2066 to 2069 but the generated interaction registry and its embedded state bundle still reported 2066.
COST: Browser verification could not run for the integration and delivery fixes after incorporating the concurrent main update.
FIX: Regenerate both artifacts using npm run build:state and verify them with lint:spa. The bundle difference is exactly the inventory count; no handlers changed. Bump the dashboard and service-worker cache versions together so deployed clients receive the matching bundle.


## Host contrast test reads a replaced node after refresh
AREA: tests
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-8
SYMPTOM: The iOS host-metrics acceptance scenario failed while parsing an empty computed color after refresh. It resolved chip handles before evaluateAll, while the completed host request replaced those elements with the next render.
COST: One browser scenario failed after the rest of its shard passed; a detached test element was mistaken for the current UI's contrast.
FIX: Resolve the current semantic chip elements and all computed colors in one browser task. Require exactly three chips, six samples, the requested theme, valid measured colors, and the unchanged 4.5 contrast threshold. Attach raw colors and frame counts, and keep the independent production contrast beacon assertion.


## Upload acceptance waits on unrelated page resources before testing uploads
AREA: tests
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-8
SYMPTOM: The upload restart scenario exhausted its 30-second test budget in page.goto waiting for load, before selecting a file. The failure snapshot already showed the rendered upload workers. Waiting for every page resource made unrelated resource completion part of the upload acceptance contract.
COST: A complete browser shard failed before reaching the upload assertions; its other 273 scenarios passed.
FIX: Wait for DOM content and the actual worker-terminal controls. Hold an unrelated image request open in the restart scenario and assert the page is still interactive while uploads recover. The old setup fails this controlled case; all 21 upload checks pass with the new readiness condition across desktop, mobile and Safari. Log upload-readiness when the pending-resource control is observed; retain every upload byte, count, timeout and cancellation assertion.

## A refused `verified` PATCH (blocked:true) read back as a demoted, wiped card moments later
AREA: gates
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: amux-frustrations
CARD: AF-942
SYMPTOM: sent `PATCH /api/board/AF-940 {"status":"verified","gate_ack":true}` against
 a card confirmed `verified` (reviewer set, evidence recorded). Response was a clean
 refusal: HTTP 409, `blocked:true`, `code:"verified_requires_gate_checked"`,
 `discarded:[]`. A GET moments later showed `status:"done"`, `reviewer:null`, and the
 entire `verification` object wiped (`state:"not_verified"`, all fields null).
 Restored the card from its own prior evidence. Could NOT reproduce on a fresh
 scratch card driven through the identical sequence (create->done->verified->same
 PATCH): that one returned HTTP 200 `applied:false` with no change, before or after.
 Reading board.rs's refusal branch, it explicitly calls `no_write()` — and AF-940's
 own durable `log` field, checked after restoring, shows NO `verified -> done`
 transition ever recorded, though every other real transition on that card is
 logged. That absence makes a genuine write-path bug the less likely of two
 explanations; a stale or racy read immediately following a refused PATCH is the
 more likely one. Neither confirmed. Spot-checked 7 other cards verified this same
 session — all clean, so this did not recur elsewhere.
COST: real alarm and ~20 minutes of investigation (a scratch card created and
 discarded, a source read, a 7-card spot-check) over what a board's own audit log
 says never happened as a write. Whether or not this is a genuine bug, a refusal
 response and a subsequent read disagreeing about a card's state — even briefly — is
 exactly the shape this repo's own instruments are supposed to make impossible.
FIX: not found. Parked on AF-942 with a concrete trigger (a clean reproduction with a
 verified immediately-before state, or a recurrence caught during a future
 verification pass) rather than continuing to chase an unreproduced anomaly.

## A fan-out validates a stale candidate and parks its own merge as an external blocker
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-10
SYMPTOM: Live MF-1238 integration repeatedly passed candidate validation, then received Mixpeek's pre-push no-ref/non-fast-forward refusal because remote main advanced during the checks. A subsequent attempt was killed at the harness's 120-second Git deadline even though repository pre-push gates take longer. TP-1 repeatedly returned to Backlog waiting for its own unmerged commit, leaving no integration candidate. The generic failure sent ordinary Git contention back to the model.
COST: Multiple successful check runs and model turns without a landed candidate; two separate fleet audits found the same self-trigger on TP-1.
FIX: CLA-10 rebuilds and revalidates up to three candidates when an observed remote ref changes, then retains an automatic retry state without another model reminder. Real unchanged-ref hook failures still require repair; hooks retain a bounded 30-minute budget and immediate lifecycle cancellation. Configuration changes invalidate in-flight checks. Real bare-remote regression tests pass and fail when candidate retry is removed; full-suite results and live adoption are tracked on CLA-10.

## 2026-09-20 — A fan-out verifier can silently leave the merged candidate
SESSION: codex-lifecycle-adherence
AREA: gates
SEVERITY: blocks
DATE: 2026-09-20
CARD: CLA-10
SYMPTOM: test-priority rewrote its validation command to cd into its original worktree, and MF-1238 used an original-worktree fallback when a file was missing from the candidate. A green command could therefore test stale source instead of the proposed main merge.
COST: Two live fan-outs could pass checks against different bytes from their main integration candidate; the incorrect command recurred after manual correction.
WANTED: Every integration check exercises the combined candidate; bad source-path configuration fails before it can publish an unverified merge.
STATUS: fixed
FIX: Reject literal original-checkout paths (including canonical and home aliases) at configuration write and at integration for persisted settings; explain candidate-relative source paths and emit fanout_verification_source_path. This catches the observed configuration error, not arbitrary shell-script behavior. A real bare-remote regression proves original/fallback commands cannot push and candidate-relative checks see both peer and child changes.

## Fully completed fan-outs retain worktrees and wait indefinitely for an idle provider to exit
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-20
SESSION: codex-lifecycle-adherence
CARD: CLA-11
SYMPTOM: The ephemeral reaper explicitly retained workspaces and required the provider process to have already exited. A clean, fully Verified and merged board could therefore retain an idle provider indefinitely; retirement did not remove its worktree. Its type-specific terminal check also allowed Done-only non-code boards to retire without every card being Verified.
COST: Completion did not reclaim worktree disk/registrations or consistently expire idle fan-outs; the user had to request another lifecycle repair.
FIX: Require a nonempty fully Verified board, fresh remote-main ancestry of the clean integrated head and an idle boundary with no queued/child work. Stop the provider, remove only that worktree without force, and expire only after confirmed removal; preserve history and restore the clean worktree if finalization loses its board/config comparison. Real Git regression tests cover the failure paths.

## Private bootstrap worker CLI routed to the default home and endpoint
AREA: cli
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-1
SYMPTOM: The server launched a private worker with AMUX_URL but without authoritative CC_HOME/AMUX_API. amux board show attempted the default CLI transport log and default endpoint; the private AAB-1 assignment was initially inaccessible.
COST: Two resume attempts stopped without the full assignment; explicit private CLI variables and a provider restart were needed to continue.
FIX: AAB-1 forwards both home and endpoint variable pairs on fresh/resumed launches and exports them after env sources. Runtime logs announce both decisions. Focused fixture and real provider acceptance evidence are tracked separately; no production changes or new cards.

## Codex linked worktrees omit writable Git metadata
AREA: cli
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-1
SYMPTOM: Parent review found the Codex launcher only adds root/.git when it is a directory. A linked worktree has a .git pointer file, leaving its common object database and per-worktree index outside workspace-write.
COST: Live project executor acceptance requires an additional launcher fix and real linked-worktree regression before executors can commit their artifacts.
FIX: AAB-1 resolves only the launched repository's git-common-dir and git-dir, adds those metadata directories, and logs codex_git_write_paths. The private parent runtime will independently validate real provider execution.

## Codex diagnostic items rejected successful project interpretation
AREA: runtime
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-2
SYMPTOM: The private live project intake exhausted receipt 6 on unexpected item: error. Exact parent reproduction exited 0 with two configuration diagnostic items, valid final JSON and usage. The helper treated every diagnostic as execution and discarded paid usage on rejection.
COST: Two failed live interpretations, a paused acceptance project and one parent diagnostic call; nested sandbox reproduction could not initialize the app server.
FIX: Recognize completed diagnostic items while requiring a successful data-only final; retain fatal/quota/execution refusals, bounded codex_helper_diagnostics logs and observed usage on rejected responses. Parent live acceptance remains pending.

## Exhausted project intake had no supported retry of the original receipt
AREA: ui
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-2
SYMPTOM: A preserved provider failure was labeled Intake needs clarification and could not be retried after its two-attempt budget without rewriting database state or replacing the request.
COST: Live acceptance could not resume after repairing the provider boundary.
FIX: Explicit operator Retry intake grants one attempt with idempotency and attempt/revision checks; retains prior result, receipt and cumulative attempts, preserves pause/budgets, logs grants and refusals, and labels exhaustion accurately. No live receipt mutation by this worker.

## Unstyled Codex footer blocked idle-boundary delivery
AREA: runtime
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-2
SYMPTOM: Codex 0.153.4 displayed a dim empty prompt followed by an unstyled model/path footer. Composer parsing labeled the footer typed input and steering held MSG-7 at not-at-turn-boundary.
COST: Parent manually delivered the bootstrap repair assignment and disabled bootstrap lifecycle; actual project executor delivery still needs live proof.
FIX: Recognize the final unstyled path-bearing footer only beside a dim empty Codex placeholder. Busy and draft controls remain blocked. Log codex_plain_footer_recognized on first observed frame; regression covers actual layout and negative cases.

## Project output arrival cannot wake an explicit operational wait
AREA: runtime
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Live PAA-3 correctly failed its candidate-relative browser gate without PAA-2 backend source, then its second attempt reported an operational wait. The free-text wait had no typed output reference or automatic continuation when PAA-2 became Verified.
COST: Existing committed Studio work stopped behind its own project output and required another harness repair; the first failed browser gate and second-attempt waiting evidence must remain inspectable.
FIX: Add an explicit same-project required-output declaration on existing task dependency edges, exact old-wait acknowledgement and idempotency. Recheck the shared planner in the writer, reserve a fresh delivery generation without another repair attempt, preserve failure/attempt history, and announce project.outputs_declared/continued/refused. Arbitrary operational and authorization waits do not auto-resume. Parent live acceptance remains pending.

## Project continuation and display need the same durable execution truth
AREA: runtime
SEVERITY: degrades
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: A project task with Doing status but a waiting execution projected into Working while its provider was idle. Review also found continuation attribution needed its own bounded claim interval rather than reopening old attempt history or relying on a historical claim fallback.
COST: The UI implied work was progressing and continuation usage could be absent or assigned outside its true interval.
FIX: Project planner projects Waiting from its waiting decision; UI consumes that phase. Durable continuation events define bounded ledger intervals with inside/before/after negative controls; prior attempt history remains immutable.

## Codex named footer displayed idle without a recognized delivery boundary
AREA: runtime
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: A resumed interrupted bootstrap pane appeared idle but blocked queued delivery. The shared adapter required the footer's final middle-dot segment to be a filesystem path; current Codex adds a session label after that path. Composer and boundary parsing disagreed.
COST: Supervisor recorded an explicit Send now intervention after observing idle.
FIX: Read the structural model/location segments with an optional session label, require no active spinner or typed draft for pane boundary, and retain measured fallback-boundary logging. Tests cover interrupted idle, working, draft and foreign-provider shapes. Live unattended acceptance remains separate.

## Project steering, retry and evidence lacked durable boundaries
AREA: runtime
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Project worker Send now re-entered legacy intake instead of its current task; treating task steering as outcome receipts inflated usage denominators. Task retry reset attempt counters. Shared parent CC_DIR left distinct Codex worktrees unowned. Report paths were inert prose and repeated criterion checks reran identical suites.
COST: A redundant legacy receipt remained pending, the original steering remained queued, actual task spend was unowned and human verification assets could not be opened after disposal.
FIX: One project routing authority with durable task steering and genuine-outcome receipt filters; operator cancellation preserves receipt history. One bounded idempotent retry grant preserves monotonic attempts. Validated active/retired workspace identity recovers only unowned exact matches. Explicit hashed passive assets are retained and registered before Verified/disposal, with read-only Projects links. Deduplicate byte-identical checks within each immutable phase. Logs: project.within_task_steering, project.legacy_intake_held, project.legacy_receipt_cancelled, project.retry_granted/refused, codex_workspace_usage_recovered, project.assets_retained, project.asset_retention_failed and project.verification_commands. Final parent Rust/browser checks and isolated live acceptance remain pending.

## Live project telemetry and resumed boot sends made false confirmations
AREA: runtime
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Private project showed 77 turns/6,536,939 tokens with estimated cost 0 despite unpriced Astra. Executor-owned steering usage fell outside attempt windows. Bootstrap resume/send returned confirmed while a timer left an unsubmitted, truncated/duplicated paste draft. Legacy normalization repeatedly attempted project-row mutation; unchanged Codex ownership repair occupied the writer for ~1.5s every tick.
COST: Unknown costs looked free, budget coverage missed execution outside attempt windows, one manual Enter intervention was needed, and unnecessary legacy/writer work repeated.
FIX: Reuse ledger prices/coverage with null unknown cost and truthful known zero; account validated dedicated executor rows separately without changing task attribution. Share project pause/budget predicates at steering claim and typing. Durable boot queue, single claim, fresh empty-frame readiness and draft preservation replace paste timers. Shared legacy normalization predicate excludes project rows; bounded indexed row-ID ownership repair skips no-op writer transactions. Logs: project.cost_unmeasured, boot_delivery_queued/enqueue_failed/start_failed, steering_draft_preserved and existing ownership/delivery verdicts. Parent Rust/native checks pending; no paid call or live project mutation performed.

## Deferred transport holds became 500 and plain Codex completion stayed blocked
AREA: runtime
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Parent broad transport suite reported 332 pass, 2 fail, 2 ignored: five deliberate boot/draft/project-budget holds lacked send_failure_status arms; finished plain Codex background frame was classified as a typed composer.
COST: Recoverable queued delivery looked like a server fault, and a completed background terminal could hold steering indefinitely.
FIX: Shared failure classifier returns actionable 409 holds with retained-message/draft guidance; real hard failures remain 500. Shared composer parser recognizes the complete ANSI-free Codex exact placeholder/final-footer layout, retaining dim-proof requirements for styled captures. Draft, multiline input, paste, foreground/background work and partially styled negative controls remain. Log signal: codex_plain_capture_placeholder plus existing HTTP status and steering_draft_preserved/held delivery diagnostics. Parent rerun pending; no live project mutation.

## Failed Review retry, canonical fixture input, and hidden pending Resume
AREA: runtime / UI / test harness
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Failed integration in Review was excluded from explicit retry. Canonical fake CLI lost long executor instruction lines although queue history reported sent. Native Resume looked enabled during pause refresh, and the offline warning overlapped Send.
COST: Genuine failed work lacked recovery, full lifecycle acceptance stopped at first delivery, and visible controls misrepresented pending/reachable actions.
FIX: Shared server/UI retry eligibility for failed Doing/Review; immutable retry history and exact identity retained. Noncanonical bracketed-paste fixture with full receipt logs and real PTY positive/negative control. Shared worker action inventory exposes pending disabled state; terminal respects --sw-fail-h. Signals: existing project.retry_granted/refused, fixture-input.jsonl exact byte receipts, lifecycle API/status diagnostics and retained SW error diagnostics. Worker PTY/syntax/diff checks pass; parent Rust/Chromium/full lifecycle reruns pending. No live state changes.

## Retry packets repeated full hook diagnostics
AREA: project execution
SEVERITY: friction
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Measured failed-integration waiting evidence contained 25,045 characters of hook output; claim retained it as last_failure and every repair packet repeated it verbatim.
COST: Repeated prompt tokens for diagnostics already retained durably.
FIX: Shared packet previous_result uses existing UTF-8-safe head/tail preview above2,048 characters, explicit original sizes/truncation and exact existing project GET/field retrieval instructions. Full state and requirements remain unchanged. Log verdict project.retry_diagnostic_preview records original size without body. Unicode/short/full-read/no-mutation regression added; parent Rust run pending.

## Project executor assignment mistaken for dependency ownership (AAB-3)
AREA: board storage / project intake
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Distinct Recheck alpha request failed cross_board_dependency_forbidden when a completed epic referenced two same-project Verified tasks assigned to retired px executors. Evidence: extractor-case-run/project-reverify-owner-failure.json and lifecycle-ui-b559-settled.
COST: Valid canonical re-verification consumed an intake attempt without progressing despite every required output belonging to the project.
FIX: One BoardOwner representation distinguishes durable project ownership from legacy worker boards. Shared outgoing/incoming checks, API validation and atomic migration/rollback ownership validation reuse it. Same-project assignment changes preserve edges; cross-project/project-legacy/missing/deleted references remain refused. Logs project_dependency_owner_validated and cross_board_dependency_refused. Real-DB actual-intake fanout/reverify and ownership negative controls added; parent execution pending. No live row rewrites, provider retries, build or deployment.

Validation (amux-astra-bootstrap, parent evidence reviewed): fixed in runtime `7ee3eb6f313b3bbbd26a7a31e692dabfff2b256b`. The 50 passing project tests include actual intake reverify after executor retirement and dependency-owner negative controls; the 11-scenario full UI includes the canonical recheck and migration rollback. Evidence: `/private/tmp/amux-astra-20260920/logs/aab3-cadence-parent2-results.json`, `aab3-cadence-preflight-negative-results.json`, and `../extractor-case-run/lifecycle-ui-7ee3eb6f313b/{results,completion-proof}.json`; see [revision validation](docs/refactors/project-lifecycle-validation.md#aab-3-validated-runtime-revision-7ee3eb6f313b). Original implementation-time pending notes and failure prose above are retained as history. This closes this measured harness defect, not the still-verifying extractor project.


## Slow legacy starts stretched project progression (AAB-3)
AREA: runtime scheduling
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: The full lifecycle fixture passed ten scenarios then exhausted its dirty-checkout observation while a rolled-back legacy source repeatedly failed provider startup. Both project packets and reports were retained; legacy sweeps stretched project ticks to roughly 14 seconds.
COST: A 90-second acceptance timeout despite successful delivery, plus diagnosis of a misleading reserved-state snapshot.
FIX: One cancellable legacy sweep with independent configured project cadence, retaining project runner exclusion and pause gates; signal project_tick_during_legacy_wait. Migration-only fixture disables pickup/standing orders via supported config and checks rollback retention without bypassing protected-source refusal. Nonbillable progress/pause/cancellation/serial-negative tests added; parent execution pending.

Validation (amux-astra-bootstrap, parent evidence reviewed): fixed in runtime `7ee3eb6f313b3bbbd26a7a31e692dabfff2b256b`. The project cadence/pause/cancellation tests pass; restoring serial cadence failed the named progress assertion (exit 101), and exact restoration passed all 50 project tests. The full UI now passes dirty-checkout retention as scenario 11. Evidence: `/private/tmp/amux-astra-20260920/logs/aab3-cadence-parent2-results.json`, `aab3-cadence-preflight-negative-results.json`, and `../extractor-case-run/lifecycle-ui-7ee3eb6f313b/{results,completion-proof}.json`; see [revision validation](docs/refactors/project-lifecycle-validation.md#aab-3-validated-runtime-revision-7ee3eb6f313b). Original implementation-time pending notes and failure prose above are retained as history. This closes this measured harness defect, not the still-verifying extractor project.


## Source-checkout command refused after expensive earlier checks (AAB-3)
AREA: project verification
SEVERITY: friction
STATUS: fixed
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Backend generation 5 reached criterion 6 before rejecting its original-checkout venv command. This was late candidate-command validation, not a generation-5 Git push failure.
COST: Earlier expensive verification commands ran before a deterministic configuration refusal that admission could already detect.
FIX: Reuse the same source-path validator at report admission and preflight all distinct checks plus the project gate before executing any. Invalid admission leaves report/status/attempt unchanged for corrected same-generation resubmission. Canonical repository identity accepts policy aliases without weakening source-path guards. Signals project.report_commands_refused and fanout_verification_source_path; API/DB and real-Git no-execution regressions added, parent Rust run pending.

Validation (amux-astra-bootstrap, parent evidence reviewed): fixed in runtime `7ee3eb6f313b3bbbd26a7a31e692dabfff2b256b`. Report admission and complete-set preflight tests plus the existing source-path guard pass; restoring late validation failed the sentinel assertion (exit 101), and exact restoration passed all 50 project tests. The full UI passes all 11 scenarios. Evidence: `/private/tmp/amux-astra-20260920/logs/aab3-cadence-parent2-results.json`, `aab3-cadence-preflight-negative-results.json`, and `../extractor-case-run/lifecycle-ui-7ee3eb6f313b/{results,completion-proof}.json`; see [revision validation](docs/refactors/project-lifecycle-validation.md#aab-3-validated-runtime-revision-7ee3eb6f313b). Original implementation-time pending notes and failure prose above are retained as history. This closes this measured harness defect, not the still-verifying extractor project.


## Owner steering could execute outside a project attempt (AAB-3)
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Parent observed owner /send input execute on PAA-2 generation 6 while execution.stage was waiting; the steering gate checked policy/budget but not the active working claim.
COST: Model/tool work occurred outside the authorized attempt boundary during real acceptance.
FIX: One shared hold predicate at queue claim and typing, original-task queue binding, current requirements/generation and policy/hold checks; retained notes wait for a sanctioned working claim. Existing queue/UI show reasons; idempotent message.held and measured project_steering_held signal once per message/reason. Real queue tests and isolated UI retry scenario added; parent validation pending. No live state or retry grants changed.

## Passing stdout prefix displayed as a failed task status (AAB-3)
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-20
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: PAA-2 failure summary showed tree-revert because the UI split retained hook output beginning tree-revert: OK at its first colon.
COST: The short status concealed the actual failed verification behind an unrelated passing check.
FIX: Harness-owned waiting_label derived from execution state and exact known reason tokens; full failure output remains unchanged in details, including exact budget labels. New transitions log project_verification_failed. Rust and browser regression cover a passing prefix before refusal; parent validation pending, no historical record rewrite.


## Stale boot idle consumed a project repair attempt (AAB-3)
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-21
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Fresh isolated lifecycle-ui-f557-held-note-boundary expected two executor calls but recorded three. PU-2's new delivery receipt was paired with old boot-idle evidence; repair followed delivery by 57ms, rejecting the valid first report and dispatching attempt 2.
COST: One unnecessary nonbillable fixture execution and failed full acceptance run; the same race could consume a real model attempt.
FIX: Current packet submission event, receipt-before-fresh-probe ordering, final writer identity/report/in-flight revalidation, and bounded stopped-before-submission recovery retaining the unsent packet. Measured project_stale_idle_held and project_current_turn_ended_without_result signals. Deterministic timing/negative tests added; parent checks pending. Full failed run and readonly fixture amux-project-ua8ko2u_ retained; source/handoff in aab3-stale-idle-{manifest,handoff}.json. No live attempt or retry grant changed.


## Send now duplicated a project owner note instead of delivering it (AAB-3)
AREA: scheduler
SEVERITY: slows
STATUS: open
DATE: 2026-09-21
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Parent source review found _steeringSendNow re-posted queued text without its identity while the project send branch ignored deliver_now and created a fresh project-owner ID. The unsupported action could duplicate a held note without sending either.
COST: One confirmed shared UI/API policy mismatch; no extra live execution trial performed.
FIX: Remove Send now for project-steering rows, retain Cancel/held reason and explain Automatic next turn. Reject project deliver_now before dedup/enqueue/history mutation with project_next_turn_only and measured project_send_now_refused log. Real send-handler regression preserves exact queue/task/history records across repeated refusals while held and working; full UI preserves explicit retry then exactly-once automatic delivery. Parent final checks pending; legacy Send now unchanged.


## Superseded unsent project packet blocked verified retirement (AAB-3)
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-21
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Parent observed PAA-4 attempt3 retaining its attempt1 execution packet with project_claim_delivery_stale. Retirement correctly refuses every pending queue row, so permanent supersession left an otherwise finished worker unable to retire.
COST: One retained real execution packet identified before retirement, plus one cleanup compile failure from an invalid IssueRow.deleted access; the failed log is preserved.
FIX: Normal steering reconciliation atomically proves supersession and moves exact packet identity/text to existing history with void:project-execution-superseded; never cancels owner/current/in-flight/foreign/unproven rows. Shared get_issue SQL excludes deleted rows in the writer; regression asserts that contract. Measured project_execution_packet_superseded/message.voided signal settlement. project_packet_reconciliation_failed retains unsafe input without vetoing unrelated queues. Helper, actual retirement predicate and injected-failure isolation regressions added; parent cleanup tests/negative controls pending in aab3-final-runtime2 handoff. No live cancellation, retry or retirement performed.


## Long verification gate exhausted fixed timeout without a checks-only retry (AAB-3)
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-21
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Parent observed PAA-4 generation3 independent verification stop at exactly 600 seconds after backend and Studio/build checks; fresh browser proof never ran. The report was retained, but the existing retry action authorized another model execution rather than rerunning checks.
COST: One real independent gate attempt failed; no further paid/provider trial or real retry was requested by this worker. Full original API snapshot paa4-g3-independent-observation.json retained.
FIX: Snapshot patch adds bounded project-configured verification_timeout_secs (default600, 1–3600), one shared per-command source/merged runner, and explicit operator Rerun checks through existing retry authority with report/input/revision/idempotency binding and unchanged model attempts/history. Failure stays waiting; no automatic repair loop from a checks-only grant. candidate_verification_command and project.verification_retry_granted emit measured evidence. Focused API/process/negative tests and isolated full UI extension await parent; no live state or accepted output changed.


## Shell startup reported started after truncated launch input (AAB-3)
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-21
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: Full isolated 424 UI run reached ten scenarios, then PRU-1 never entered its nonbillable provider. start_session recorded started despite a childless shell; its exact execution packet stayed unsent and the observer correctly held executor_stopped_before_result. Prompt polling accepted scrollback and ignored timeouts. A private tmux/Bash probe proved input loss while setup occupied the terminal: 1,916 submitted bytes lost Enter and retained only 908 of 1,800 payload bytes after an extra Enter, measured through argv rather than rendered text.
COST: One full isolated UI run failed before a report; no real project retry or model call was made for diagnosis. Original 424 failure and read-only fixture evidence retained.
FIX: Source short private temporary scripts in the existing shell, acknowledge each setup command with a unique filesystem receipt, and stop on submission failure/nonzero status/timeout before sending more input. Initial and existing fallback launches share this transport. A stopped provider cannot produce session.started; durable start_error and measured shell_start_failed expose the failure, while a live slow startup remains allowed. Private positive proof preserved all 4,096 payload bytes, cwd and environment. Focused production-helper regressions added; parent Rust/full UI validation pending. No deployment or live project changes.
Validation update (2026-09-21): runtime ab2b73576000cd5808e8b583269f9b2e144b65e5 passed startup/cwd/env/pause and 65 project tests, both entry/late mutation controls with exact restoration, workspace check and strict Clippy. Full exact-image isolated UI: 13 PASS, zero uncaught errors, 14 fixture intake/13 execution calls; restart identity and cleanup verified. Evidence: ../extractor-case-run/astra-slow-setup-results.json and lifecycle-ui-ab2b73576000/completion-proof.json. Private post-deploy audit 48/48 and retained-report UI passed; original real acceptance gates remain the 182 run. Prior 424/fb31 failures are retained. This updates measured validation only; no entry retirement or main-merge claim.


## Healthy shell profiles exceeded the submission acknowledgement window (AAB-3)
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-21
SESSION: amux-astra-bootstrap
CARD: AAB-3
SYMPTOM: The fb31 isolated UI passed two scenarios, then PU-5 held shell command receipt timed out without provider input. The short script did execute; its late receipt found the already-disposed directory. A private no-provider probe measured 11.8 seconds before script entry and 14.6 more seconds sourcing the existing profile (26.4 total). The 10-second per-command acknowledgement conflated slow shell initialization/profile execution with missing input.
COST: One fresh exact-image UI run stopped before its repair scenario; prior focused gates and long-input/stale-UI mutation results remain valid only for their scope. No real project/model retry was granted for diagnosis.
FIX: One bounded 60-second budget across shell setup/submission, separate entered/completed receipts, no advancement on entry alone, and stage/elapsed diagnostics in shell_command_entered/shell_command_settled plus durable shell_start_failed. Completion after expiry does not recreate or write into disposed receipt storage; later provider submission remains refused. Real shared-helper tests cover delayed entry, a controlled held profile, timeout stage, zero remaining budget, late completion and unchanged exact input/env/cwd guards. Private controlled probes passed; parent Rust/mutation/full UI remain pending. Final docs stay outside the checkout.
Validation update (2026-09-21): runtime ab2b73576000cd5808e8b583269f9b2e144b65e5 passed startup/cwd/env/pause and 65 project tests, both entry/late mutation controls with exact restoration, workspace check and strict Clippy. Full exact-image isolated UI: 13 PASS, zero uncaught errors, 14 fixture intake/13 execution calls; restart identity and cleanup verified. Evidence: ../extractor-case-run/astra-slow-setup-results.json and lifecycle-ui-ab2b73576000/completion-proof.json. Private post-deploy audit 48/48 and retained-report UI passed; original real acceptance gates remain the 182 run. Prior 424/fb31 failures are retained. This updates measured validation only; no entry retirement or main-merge claim.
