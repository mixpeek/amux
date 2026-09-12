# amux frustrations

Friction that **amux itself** caused a session working inside it. Appended to as we
hit things; read when deciding what to fix next.

The rule for when and how to log is in
[`.claude/rules/frustrations.md`](.claude/rules/frustrations.md). The short version:
log friction the NEXT session will also hit, link a card, and record the cost in what
it actually cost.

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
## The rust request log recorded a ~15-second restart choreography as a 76ms request
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: amux-rust (lifecycle-fix subagent)
CARD: AR-111
SYMPTOM: Forensics on the amux start incident: `_amux_request_log` shows
  `PATCH /api/sessions/amux/config` at ts 19:10:35 with latency 76.26ms — but the SAME
  request wrote its "Captured before model swap" log marker at 19:10:20 and the env
  header at 19:10:35.42, i.e. the handler ran a synchronous ~15s stop/relaunch
  choreography that the request log renders as a sub-100ms call. Whatever the
  middleware stamps (completion-time ts + an inner-layer latency, or a batched flush
  clock), a long-running request is indistinguishable from a fast one.
COST: ~30 minutes of incident reconstruction chasing a phantom second actor, because
  the timeline read as "capture at :20 cannot belong to a 76ms request at :35" — the
  instrument manufactured a contradiction that had to be disproved with three other
  artifacts (env header, session log markers, session_events).
FIX: request-log middleware should stamp arrival ts and wall-clock latency around the
  WHOLE handler future; a restart choreography should be a visibly long row.

## e2e auth tests flip green->red mid-session: the server under test is rebuilt from a shared checkout that moves between runs
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: no-silent-actions agent (subagent; no $AMUX_SESSION in env)
CARD: ARE-5
SYMPTOM: three consecutive runs of `npx playwright test --config e2e/playwright.config.ts`
on the same working tree: run 1 = 83 passed / 0 failed; run 2 = 12 failed; run 3 =
5 failed, all in phase0 auth ("protected API rejects a bad bearer token" expected
401, got 200) + settings_missing_endpoint_probe. Nothing in the diff between runs
was mine — the config's webServer runs `cargo run -p amux-server`, so every run
rebuilds whatever the concurrent lane has landed in crates/ since the last one.
The 401->200 flip itself looks like a REAL auth regression landing upstream while
I was testing the SPA layer.
COST: ~15 minutes ruling out my own SPA-only changes as the cause of server-side
auth failures; and a possible live auth regression (bad bearer accepted with 200)
observed but not attributable to a commit from here (NEVER-run-git constraint).
FIX: same instrument the CLAUDE.md /health-build bracket prescribes, applied to e2e:
have playwright.config.ts record the server build hash (GET /health .build) into the
run report so a mid-session flip names "the binary moved" instead of reading as
flaky tests; separately, someone with git access should bisect the 401->200 auth
behavior on current crates/amux-server HEAD.

## Ghost-rescue can only rescue the messages that happen to carry a timestamp prefix
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: (agent, AMUX-2629)
CARD: AMUX-2629
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
CARD: AMUX-2637
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
CARD: AMUX-2644
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
CARD: ARE-10
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
CARD: AMUX-2663
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
CARD: AMUX-2647
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
CARD: AF-69 (investigation, signed off) + AMUX-3221 (the FIX, open)
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
CARD: AMUX-2701
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
CARD: AMUX-2799
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
CARD: AMUX-2841
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
CARD: AMUX-3119
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
CARD: AMUX-3249
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
CARD: DESKT-10
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
CARD: AEAB-11
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
CARD: AEAB-28
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
CARD: AEAB-36
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
CARD: AEAB-40
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
CARD: AMUX-1315
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
CARD: AEAB-47
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
CARD: DESKT-21
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
CARD: DESKT-22
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
CARD: DESKT-22
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
CARD: AF-182
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
CARD: AEAB-43
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
CARD: AF-214 (nudge skip, done) / AMUX-3668 (the `changes-requested` status, open)
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
CARD: none
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
CARD: AMUX-48
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
CARD: AMUX-48
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
CARD: AMUX-3772
SYMPTOM: A host-wide stall that RAMPS files a single-family outlier card on the scan where fewer than AMUX_OUTLIER_ROLLUP_AT (3) families have crossed the threshold. That card's verdict then says "This is not a percentile shift — it is individual requests going wrong, so look at the request, not the family", which is the exact opposite of the truth, and it names an endpoint that answered in 0.09s minutes later. The rollup that describes it correctly already exists and fires on every subsequent scan; nothing revisits the card filed at the leading edge.
COST: One lane-turn to diagnose, and the diagnosis only landed because `host_load_at_worst` was in the payload and I followed it. A reader who trusts the verdict audits innocent code. ethos.md rates a loud wrong probe worse than a silent one, and this is one: it answers, names a specific target, and is wrong.
FIX: none yet, deliberately. The obvious fix — suppress a single-family card when an open ROLLUP exists — is WRONG while a rollup card can sit parked in backlog indefinitely, because it would mute every genuine single-endpoint regression. That prerequisite is AMUX-3774 and is now fixed; this card is parked with that as its trigger. Recorded because building the wrong fix first is exactly what I did, and the order matters.

## Discarding an autofix card as a "duplicate" deletes the only thing suppressing the re-file
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-28
SESSION: amux
CARD: AMUX-3849
SYMPTOM: A live outage (`/api/browser/start` 502) produced FOUR cards in three hours. I hand-filed AMUX-3842 with the diagnosis, then discarded the two autofix cards as duplicates of it, twice, and a fourth arrived anyway. `open_card_for_fault` suppresses on `source_ref LIKE 'autofix:<ident>|%'` for any card not done/verified/discarded — so a HAND-FILED card carries no signature and can never suppress, and discarding the autofix ones removes the only cards that could. The two look identical on the board: same title shape, same status vocabulary, no visible difference between a card the detector will honour and one it cannot see. `discarded` not suppressing is DELIBERATE and correct (it is what lets a genuinely new occurrence file after a judged one), so every individual piece behaved as designed while the composite guaranteed a re-file loop.
COST: Three discards, four cards, and the wrong conclusion available at every step — the obvious reading is "the dedupe is broken", which is what I would have reported if I had not gone and read `fault_identity`. The detector was right and I had deleted its memory. Also self-inflicted noise on a shared board while the underlying outage sat correctly parked in `needsyou`.
FIX: none yet. Immediate workaround, applied: copy the autofix signature onto the hand-filed card's `source_ref`, which makes it suppress (verified against the LIKE). Two candidate real fixes, cheapest first: (a) `amux board discard` warns when the card carries an autofix signature AND is the last non-terminal card holding that ident — a discard that turns the detector back on should say so; (b) `board add` for a fault already carded by autofix is the wrong move entirely and the honest path is folding the diagnosis INTO the autofix card, which nothing currently suggests. The transferable shape: a card's suppressing power lives in a field nobody looks at, so two cards that read identically to a human behave oppositely to the detector.

## `git commit -a` in a shared checkout swept three lanes' in-flight work into one lane's commit, twice in four hours
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-30
SESSION: amux
CARD: AF-342
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
CARD: AMUX-3954
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
CARD: AMUX-99
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
CARD: ATE-10
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
CARD: AMUX-4083
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

## A process killed before it can log leaves the fleet no diagnostic surface for the failure that removes the diagnostic surface

AREA: instruments
SEVERITY: wrong-conclusion
STATUS: open
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-458
SYMPTOM: the server is in a launchd crash loop and NOTHING in its own logs says so.
 macOS SIGKILLs it at exec for `Code Signature Invalid` / `Launch Constraint
 Violation`, so it dies before any of our code can write a shutdown line. Both
 StandardOutPath and StandardErrorPath point at ~/.amux/logs/server-rs.log, and the
 last line before each death is an ordinary WARN. The only honest record is
 ~/Library/Logs/DiagnosticReports/*.ips plus `launchctl print`, where `runs` went
 10 -> 18 -> 23 in about two minutes and `properties` reads "needs LWCR update".
COST: this is the flap the whole fleet is hitting, and it presents as five unrelated
 problems. It forced gtm-engine's send onto the unstamped fallback (see the two
 entries above), made `amux board retitle` exit 7 with no message, broke a `git
 commit` with "unable to write new_index file", and made two /api/board reads
 return empty. Each looks like its own bug. Worse, the log carries an ERROR-level
 line 24 seconds before a death — "migration VERSION COLLISION at 35" — which is
 loud, adjacent, and irrelevant: migrate.rs:636 documents it as deliberately
 non-fatal ("this reports rather than refuses ... a gate with no truthful path,
 ethos rule 3") and it appears identically on runs that stayed healthy. A wrong
 cause was one step away and I nearly filed it. Fifth AF-445-shaped near-miss in
 this session.
FIX: not actioned — the remedy touches a launchd agent and ~/Dev/CLAUDE.md requires
 explicit owner approval ("This machine runs 24/7. Do NOT restart launchd agents").
 One-shot is `launchctl bootout gui/501/com.amux.server-rs` then `bootstrap`, since
 the binary itself verifies clean on disk and it is launchd's cached Lightweight
 Code Requirement that is stale. The durable fix is the builder re-bootstrapping the
 agent after it swaps the binary; until then every deploy on this box reopens the
 window. The INSTRUMENT half is the part that belongs here: a process killed before
 it can log needs its death reported somewhere a lane already looks. /health going
 unreachable and `/api/debug/*` being unreachable at the same moment means the fleet
 has no diagnostic surface for exactly the failure that removes the diagnostic
 surface.
NOTE: gtm-engine independently confirmed this from the other end and bounded it
 (origin-stamped, 2026-09-03). They closed five cards inside a flap window trusting
 a "-> done" line, re-read all five at the FIELD, and found two gaps that were their
 own omissions rather than the crash loop. Their conclusion: "on this lane the flap
 degraded loudly every time and silently never." Every symptom seen so far is
 fail-loud (curl rc 7, empty body, refused index write, a verb exiting non-zero with
 no message); nothing yet shows a write that REPORTED success and did not land. So
 the failure mode is availability, not silent corruption, which is the difference
 between a degraded fleet and one whose records are suspect. Not a reason to leave
 it running; it is a reason not to re-verify every board write made today.
NOTE: CAUSE CORRECTED, 2026-09-03, same session. The codesign SIGKILL is real
 (crash report 160828.ips) but it is NOT what drives the climbing run counter, and
 I recommended a fix that would not have worked. Three facts I should have checked
 before recommending anything: only ONE crash report all day against 76 runs (a
 codesign kill writes one per death), the binary unchanged since 16:10 so there is
 no swap-kill-swap cycle, and `codesign --verify` clean right now. What is actually
 happening is a port race: an agent session started `AMUX_RS_PORT=8824
 amux-server-rs` by hand in a gemini-shell background job (pid 20191, parent a
 /bin/bash -c with `trap 'jobs -p > "$_bgpids_file"' EXIT`), it holds 8824, and
 launchd's managed copy cannot bind, exits cleanly with 78, and KeepAlive respawns
 it forever. Clean exit, hence no .ips. So `bootout`/`bootstrap` would have resumed
 losing the same race. The entry's INSTRUMENT argument survives intact and is if
 anything stronger: a process that exits before binding logs nothing either, both
 halves of `runs`-climbing-with-a-silent-log look identical, and I distinguished
 them only by counting crash reports, which is not a thing any lane would think to
 do. The deeper problem this exposed: the fleet's live server is an UNSUPERVISED
 background job that dies with its parent shell, while the supervisor that should
 own it is locked out of the port.

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
CARD: AMUX-4142
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
CARD: AC-416
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
CARD: AC-416
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
CARD: AMUX-3249
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
CARD: MOS-33
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
CARD: AMUX-4203
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
CARD: AMUX-4203
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
CARD: AMUX-4203
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
CARD: AMUX-4225
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
CARD: MR-174
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
CARD: ATE-128
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
CARD: ATE-130
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
CARD: AF-640
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
CARD: AMUX-4362
SYMPTOM: GitHub's e2e job failed the shipped-function Node test because it still expected a failed outbox write to make the connection badge read Sync error. The current product deliberately reports connection/read health separately and shows pending operation failures in the outbox.
COST: The stale assertion stopped the browser CI job before its browser cases could run.
FIX: Align the assertion with the documented connection behavior, retain the checks for pending counts and read/auth errors, and explicitly assert the failed operation still displays its error. The existing named Node assertion is the local/CI diagnostic; no runtime behavior is changed.
## A dead database writer left health green while browser sync failed
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4416
SYMPTOM: During host ENOSPC, heartbeat repeatedly reported "writer thread is gone" and request-log rows were dropped, while /health returned store:"ok" from a read-only probe. The browser retained 26 queued operations. Failure injection also showed journal and COMMIT errors poisoning the next transaction.
COST: Hours of failed writes could look healthy to the watchdog and an empty request-log analysis; cache deletion did not free snapshot-retained blocks. Recovery required explicit snapshot-reclamation approval and an API restart. Separate reader-pool exhaustion during recovery is not attributed to a specific borrower by these tests.
FIX: Guard every write transaction through commit, catch mutation unwinding without killing the writer, emit failure verdicts, and include a bounded no-op writer transaction in health. Four baseline regressions failed before the change; panic, journal, commit, unwritable-writer and stalled-writer cases now cover recovery. Keep the detailed read failure in the Sync error modal and update it on recovery. See docs/incidents/2026-09-11-offline-sync.md for the causal limits.

## Network-first bypass reintroduced a blocking composer and noisy sync sequence
AREA: ux
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4416
SYMPTOM: Send/Queue bypassed local persistence and waited on the API, then fell back into Queued/Syncing. An optimistic follow-up cleared draft text and uploads before durable acceptance.
COST: A slow mobile connection became a composer delay; failed local storage could lose the working draft. Automatic retries opened delivery progress during ordinary sends.
FIX: Restore both modes through the existing durable local outbox, clear only the accepted draft/files, retain newer edits, and run automatic replay quietly. Tests exercise held responses, refusal, quota failure, reload and retry on desktop/mobile/WebKit. Two contract controls reproduce the old behavior on 091bc3a9. Historical causes are recorded in docs/incidents/2026-09-11-offline-sync.md.

## Gemini idle terminal cannot receive the worker's first queued task
AREA: scheduler
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Fresh Gemini CLI 0.58 authenticated and displayed an empty composer, but /api/debug/steering held its first task at not-at-turn-boundary. Workers displayed idle. The captured-frame regression returns empty status rather than idle; the thin-rule input box is also unknown to the delivery verifier.
COST: The new worker's lifecycle acceptance could not begin for more than ten minutes; no deliverables were produced.
FIX: Recognize Gemini's provider-owned footer and current input box, preserve active/picker/pending-input controls, and emit idle_display_without_delivery_boundary when the display and delivery disagree. Rerun the live provider suite before closing.

## Completion callbacks ask the requester to notify themselves again
AREA: coordination
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Gemini's same-group run completed implementation, review and handoff but kept creating review/capture tasks after completion receipts. The server appended its default "Notify the requesting worker" instruction to the callback already addressed to that requester.
COST: Repeated reviews, acknowledgement messages and capture cleanup consumed turns while the complete-board acceptance remained red.
FIX: The automatic callback is the notification. Do not add another notify instruction; suppress the old generated instruction on existing rows, preserve explicit custom callbacks, and identify receipts that need no acknowledgement. callback_echo_instruction_suppressed logs legacy rows. The regression inspects the real durable callback queue. Live loop reduction is not yet claimed.

## Removed attachment reappears after immediate mobile reload
AREA: messaging
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The consolidated mobile regression failed on desktop and mobile Chromium: a removed attachment returned after immediate reload. The chip vanished before its asynchronous IndexedDB deletion committed.
COST: Cancelled files could be unintentionally reattached. Two of 33 mobile/offline checks failed; the 256 MiB interrupted upload and checksum checks passed.
FIX: Save a per-attachment cancellation intent before removing the chip; suppress restoration and recover deletion after reload. Keep the attachment when saving cancellation fails. upload-storage reports cancellation-intent and recovery failures. The regression holds deletion forever before reloading and checks actual stored bytes are then removed.

## Gemini peer messages disappear from the terminal Workers filter
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The real Gemini pair finished the review/revision cycle and its tasks, but terminal search for REVIEW_APPROVED with the Workers filter returned zero. Messages history contained the confirmed receipt. The renderer only recognized Claude/Codex prompt glyphs, while Gemini echoes input with >.
COST: The live pair case failed after 9.9 minutes; three dependent upload/cross-group/queue cases could not run.
FIX: Recognize Gemini input glyphs only for Gemini workers, preserve multiline provenance and exclude the actual input placeholder. The regression uses the real terminal filter/search buttons and verifies non-Gemini > lines remain unclassified. Navigation diagnostics now include provider beside considered prompts and match results.

## Browser caches leave no room for the mandatory local message outbox
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Seven studio-plg composer failures on Safari 0.9.900 reported only unconfirmed. The live Safari WebApp localStorage held 5,193,082 bytes, chiefly command history and reproducible board/schedule/HTML caches; its outbox was empty. The client attempted the failed local write twice and replaced the specific quota error with a terminal-confirmation message.
COST: Messages could fail before reaching the server despite a healthy connection. A real WebKit quota reproduction against pre-fix source returned failed instead of queued.
FIX: User-intent writes reclaim only reproducible HTML/board/schedule caches and retry the same atomic write, preserving other drafts, operations, attachment journals and the offline worker list. Local refusal returns once with its storage reason; outbox-storage logs measured byte counts, browser capabilities and the failure category without content. Quiet background replay no longer announces queued-operation completion. The original seven failures lacked a reason field, so their exact exception cannot be recovered retrospectively; quota is reproduced against the observed storage condition.

## Watchdog restarts a progressing database after short health deadlines
AREA: server
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Production watchdog logs explicitly issued kickstart -k at 13:16:19 and 13:29:32 on September 11 after three health responses with measured:false / probe_deadline_exceeded. launchd recorded SIGTERM, not an application crash. The 250 ms health deadline detached its ongoing writer/read probe but discarded its later success, so each slow sample could imply a hung store despite intervening progress.
COST: The monitor itself disconnected clients and restarted the server; both restarts were followed by more slow probes rather than durable recovery.
FIX: Retain monotonic completion and in-flight ages for real probes after HTTP timeout. Readiness remains unmeasured/503; the watchdog defers a restart only with recent successful progress or bounded initial work. Real writer failures, pool exhaustion, absent listeners and stale progress retain recovery. slow_probe_completed and watchdog restart-deferred logs expose the decision. Rust exercises a blocked writer twice and requires the detached first probe's receipt during the second timeout; Python tests cover actual HTTP 503 classification and both restart/no-restart loop controls.

## Gemini New conversation restarts the old provider conversation
AREA: workers
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The real Gemini upload acceptance run clicked New conversation and received a successful config response, but the native terminal exited with Invalid session identifier. The handler cleared only the Claude conversation key, and Gemini/Codex launch paths ignored skip_conv_id, so the supposedly fresh launch still used --resume.
COST: Upload acceptance could not start; the worker remained at a shell while the UI reported a reset.
FIX: Fresh resets clear all provider resume keys and both hookless launch paths respect the fresh flag. New Gemini identities are UUIDs with random leading bytes, matching the CLI's documented --session-id contract and avoiding time-derived filename prefixes. conversation_recycled now logs provider identity. Tests exercise the config handler and a stale Gemini identity followed by fresh launch and exact subsequent resume.

## Gemini uploads stop at native read approval before submission
AREA: workers
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The real UI upload launched a fresh Gemini session, then its @uploaded-file prompt opened a native read approval outside the checkout. Amux's send verifier returned stuck. Gemini was launched with the log directory included but not the uploads directory.
COST: The user-uploaded file could not reach a completed receipt task and the composer reported a send failure.
FIX: Include the Amux uploads directory in the Gemini workspace, using its supported repeated --include-directories option. The real upload case must read the attached bytes, produce a matching JSON receipt, finish its board card and expose the delivered message and terminal on desktop and mobile.

## Fresh conversation accepts a message into the retiring process
AREA: workers
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The real Gemini upload run received reset acceptance at 18:18:24Z, delivered its prompt at 18:18:26Z, and only launched the replacement process at 18:18:46Z. The test saw the previous terminal's banner while reset was still stopping that process.
COST: A following send could appear accepted and then lose its native conversation when the asynchronous reset killed the old process.
FIX: Acquire the existing per-lane send boundary before accepting a running reset and retain it through stop/start. Ordinary sends during that interval persist immediately into steering, with an acceptance receipt; interactive commands refuse without an effect. Holding the HTTP request itself through restart was disproven by a mobile timeout and pending duplicate receipt. Other lanes remain independent. conversation_restart_send_boundary and existing lane-send-serialised logs expose the ordering. The live upload scenario deliberately sends through the UI immediately after reset, then requires real attachment data and completed board evidence.

## Board recovery hides the full assignment from the sanctioned CLI
AREA: workers
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Gemini recovered an auto-captured upload task through amux board show. The card preview ended at `row`, before `row_count` and the attachment path. The API already returned the full linked source message, but the Bash CLI dropped messages entirely. The worker searched logs and produced `rows` rather than the required `row_count`.
COST: A completed receipt had the wrong schema despite the original request remaining in durable history; recovery spent tokens searching terminal logs for context the API already provided.
FIX: Board show exposes linked message IDs and a supported --messages option for full assignments. Structured recovery explicitly reads that option for captured prompt previews, while ordinary board reads remain compact. A fake-transport regression uses the real CLI with a requirement beyond the 300-character preview and requires it only on the explicit full-message read.


## Queued delivery observation reads an unstamped command receipt
AREA: testing
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The real Gemini upload reached steering history with outcome sent, produced its correct file and completed LG1A-7, but the acceptance helper timed out waiting for cmd_history.delivered_at, which the steering drain does not stamp.
COST: A delivered message was reported as undelivered, stopping the remaining acceptance cases.
FIX: Expose the existing steering outcome and submission verdict, and the exact queue ID for restart acceptance. The observer checks this delivery instrument and excludes dead-letter rows despite their timestamps. The real handler regression covers confirmed, retried and discarded histories; existing steering-delivered logs remain the operational signal.


## Messages normalization discards recorded delivery metadata
AREA: ui
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The real Gemini mobile upload screenshot showed direct? on MSG-80 although its API receipt recorded queued. Both the shared history cache mapping and _msgNorm discarded delivery metadata before the shared renderer read it.
COST: New messages looked like legacy records, and failed submission indicators could disappear from all three message surfaces.
FIX: Preserve recorded delivery, queue timestamps, wait duration and submission verdict through the shared normalizer, and use it for initial history loading too. A browser regression fetches controlled direct, queued and stuck API rows through the actual scoped loader, renders all three message surfaces and requires their real labels. The existing server delivery logs and exposed steering outcome remain the diagnostic signal.


## Delivered message still appears locally unsent while its response is pending
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The user's 11:55:17 screenshot showed a homepage request in Claude's native queue while Messages said not yet delivered. Its exact MSG-55405 server record had direct/confirmed delivery at 11:55:12. Local pending state was tied to the entire POST response, including downstream board processing, instead of the durable acceptance already recorded.
COST: The client contradicted the terminal and offered cancellation as if an already-attempted message could still be prevented from sending.
FIX: Expose a read-only, non-cacheable receipt lookup scoped by session and msg_id. During an in-flight send, a bounded lookup can acknowledge the exact durable receipt without repeating delivery or cancelling the original handler's board work. Persist attempted state before transport, label uncertainty as Awaiting confirmation, and refuse local cancellation once attempted; legacy entries without attempt provenance are conservative. acceptance_receipt_read and outbox_acceptance_receipt expose reconciliation. Tests hold the original POST open, reject wrong-ID/unaccepted receipts, and check the real handler never reserves or sends on a lookup.


## Rapid input inherits retry backoff and full-history terminal refreshes
AREA: messaging
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: After confirming the stale queued banner was gone, the user reported a slight delay before input appeared in the terminal. A message appended during an in-flight replay missed its snapshot and inherited retry backoff. The deterministic counterexample selected an 8000 ms timer. The post-input UI also launched two full-history refreshes; five read-only production samples were approximately 128 KB each versus 5 KB for a live frame.
COST: Rapid messages waited unnecessarily and mobile terminal updates transferred scrollback to display newly arrived input.
FIX: Newly added, unattempted operations resume on the next tick after the active replay, retaining FIFO delivery and receipt checks. Replace overlapping full refreshes with one bounded live-frame loop: first tick at 40 ms, then 100 ms intervals for 1.5 seconds, with normal cadence afterward. Remember pending turn-end history refreshes. The executable latency regression and its attached dispatch/render measurements detect recurrence; disabling immediate continuation makes the counterexample fail.


## Send button mistakes a second rapid press for the first tap's click echo
AREA: messaging
SEVERITY: blocks
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The rapid-send lifecycle case failed on desktop, mobile and iPhone WebKit: the first local send cleared, but the second distinct message remained in the composer after Send. _btnFire suppressed every activation within 350 ms instead of only the synthesized echo of one gesture.
COST: A legitimate new message required another tap and made the local-first composer appear stuck.
FIX: Reset per-button echo suppression on a new pointerdown/touchstart and allow distinct keyboard activation. Keep the same gesture's pointerup/touchend/click echoes deduplicated. The rapid-send UI case exercises two different messages and verifies two unique IDs, immediate continuation and terminal rendering; the event contract verifies duplicate echoes still fire once. Existing send-fire diagnostics retain the pre/post composer length and event sequence.


## Semantic intake acceptance listed worker-message coverage but only exercised the board API
AREA: testing
SEVERITY: slows
STATUS: open
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: LC-SEMANTIC-INTAKE posted candidate tasks directly to /api/board. The canonical case also promised captured worker messages, but no executable scenario sent those messages through a composer and checked their surviving task links.
COST: A direct-board semantic pass could be mistaken for proof that ordinary new messages avoid near-duplicate board tasks.
FIX: Add LC-SEMANTIC-MESSAGES to live discovery: six composer messages must produce three tasks, four linked source messages on one survivor, measured append/update decisions and preserved requirements. Follow source links in desktop/mobile details. Record live prerequisites separately: the first attempt failed worker admission under host memory pressure before sending, so it is not a semantic pass. Preserve the dedicated run's health and trace evidence.

## Phone composer squeezed the draft beside a misaligned Queue button
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The phone composer put the textarea, top-aligned more button and bottom-aligned Queue button on one row. Removing the corrected full-width rule reproduces a 204px input in a 363px row. The expanded test also found the attachment menu 16px above the viewport in landscape.
COST: Another user screenshot and a failed landscape acceptance run before the clipping was corrected.
FIX: This commit gives phones a full-width input and a separate aligned 44px toolbar, bounds the long draft and attachment menu, and adds inputW/actionDelta to the existing layout diagnostic. Source-built LC-COMPOSER-LAYOUT, LC-LATENCY and LC-RECEIPT: 9 passed across desktop, mobile and iPhone WebKit; 32 outbox contracts passed. The CSS negative control fails on input width (204.34375px versus at least 362px).

## Automatic quota resumption was labelled needs input
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-11
SESSION: codex
CARD: AMUX-4420
SYMPTOM: Ethan's mixpeek-frustrations screenshot showed NEEDS INPUT over Claude's usage limit with automatic resumption at 6:10pm. Preview cancellation text overwrote the provider state; the sweep discarded this banner's reset clock. The wider audit found ready-composer events overwriting quota/error states and missing Starting/Error badges.
COST: User had to inspect the terminal and report a question that did not exist; independent state projections disagreed.
FIX: Current provider-footer classification, clock-preserving observation, typed state projection, idle-prompt event semantics, and explicit dashboard badges. Controlled provider/model and browser chaos regressions; diagnostic verdicts preview_quota_over_input, provider_auto_resume_quota, and ready_composer_idle.

## Worker terminal opened in the middle of its history
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-11
SESSION: codex
CARD: AMUX-4421
SYMPTOM: Ethan opened mixpeek-general and landed midway through old terminal output instead of at the latest output.
COST: Each open required finding and scrolling to the worker's current output.
FIX: Preserve bottom-follow intent through asynchronous history/live rendering and resizing, cancel it on deliberate reading/navigation, and flush buffered output on resume. Desktop/phone race tests and bottom-anchor-restored diagnostics.

## Accepted details message survived as a partial card draft
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-11
SESSION: codex
CARD: AMUX-4424
SYMPTOM: Ethan sent a message to amux from worker details, but an earlier partially typed copy remained in the worker card. The 250ms draft mirror lagged; exact-match acceptance left the partial copy alive, and lifecycle DOM harvesting could save it again. Fullscreen edits and separate browser contexts also missed draft synchronization.
COST: User could mistake already-submitted text for unsent work and submit it twice.
FIX: Immediate per-worker draft updates across card/details/fullscreen and same-origin tabs/grid; revision-bound acceptance preserves newer edits, lifecycle events never overwrite storage from stale DOM, and failed storage retains text with a visible warning. Server client-debug verdicts composer_locally_accepted and composer_draft_storage_failed. Regression reproduced on pre-fix source; desktop, phone and WebKit coverage alongside durable outbox tests.

## Offline banner promised to retry permanently failed edits
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: A blocked 409 board edit displayed as queued and promised to send on reconnect while offline. The regression reproduced that exact text before the fix.
COST: The user could wait for an automatic retry that will never occur.
FIX: This commit separates failed and pending counts in offline mode, preserves review/dismiss actions for failed-only queues, and adds LC-BLOCKED-OUTBOX across desktop/mobile/WebKit. The final focused run passed 9 cases including gate revisions and linked records; screenshots were opened.

## Stale failed-row dismissal deleted an edit resumed in another tab
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: A stale failed-row action removed its operation by ID even after durable storage changed its state to pending. The regression lost the resumed entry before the fix.
COST: Potential loss of a pending edit when two tabs act on the same outbox.
FIX: This commit checks blocked state inside the shared storage lock, refreshes the UI and emits outbox_dismiss_ignored when the action is stale. The contract verifies pending work survives both individual and bulk failed-only dismissal. All 34 outbox contracts passed.


## More-specific mobile flex rule narrowed the composer again
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-11
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: After integrating the latest toolbar change, LC-COMPOSER-LAYOUT failed on all three projects: the 320px phone input shrank to 155px instead of its available 308px.
COST: Long drafts become difficult to read beside More and Queue.
FIX: Remove the conflicting ac-wrap flex override, retain compact chrome and aligned action controls, and preserve the full-width mobile writing row. Existing composer-layout diagnostics record inputW and actionDelta; the browser case checks short/long drafts, Send/Queue, narrow/landscape viewports and attachment-menu reachability.

## Uncertain native submission was deleted and counted as synced
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: An uncertain 409 send response removed the durable message and marked its progress row done.
COST: The only recoverable intent disappeared while the UI reported a success.
FIX: Keep the original message ID, text and attachment references in a blocked outbox row; outbox_retry_failed reports the rejection. The new uncertain-submission contract rejects false checkmarks.

## Steering preview depended on expiring in-memory text matches
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Pending steering used a temporary map cleared by matching text or a two-minute expiry, while actual intent lived in durable storage.
COST: A reload or delay could erase the preview; identical messages could be conflated.
FIX: Render pending steering directly from durable outbox entries with stable IDs. Distinct identical requests survive reload and age. steering_accept_failed identifies acceptance failures.

## Worker-card file picker was missing on touch screens
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Details offered Attach file while the worker card relied on drag/drop.
COST: Phone users could not select files from the worker-list composer.
FIX: Add a 44px card file picker using the same durable upload pipeline. card_files_selected logs file counts; lifecycle covers real upload/download bytes and Send/Queue from both surfaces.

## Mobile attachment menu overflowed after compact composer layout
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: At 320px and iPhone WebKit the attachment menu extended 10–16px beyond the screen edge.
COST: All three layout runs failed their reachability check.
FIX: Right-align the menu with its More control. Desktop, phone and WebKit layout checks now pass at narrow, landscape and keyboard heights; existing composer geometry diagnostics expose bounds.

## Disabling browser idle expiry disabled hard and activity lifetimes
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The reaper returned immediately when idle expiry was zero, skipping independently configured hard and activity TTLs.
COST: Browsers could retain processes indefinitely despite configured age limits.
FIX: Evaluate activity and hard expiry before the idle-only switch. Existing reaper warnings report the actual expiry arm. The real-stop contract exercises both lifetimes with idle expiry disabled; notices now name AMUX_BROWSER_IDLE_REAP_S correctly.

## Automatic browser capture could raise a user window
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Screenshot retry restored the browser and invoked bringToFront; omitted API headless settings also launched a visible window.
COST: Background automation could interrupt the foreground application.
FIX: Default API automation to headless and remove all screenshot focus recovery. capture_failed_without_focus logs a failed capture; explicit headed sign-in remains available. Real browser lifetime and foreground checks are tracked in LC-BROWSER-BACKGROUND.

## Reconnect hid individual progress and failed-step evidence
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Reconnect requested a quiet sync and failures immediately hid the step list.
COST: Users could not follow which saved operations had succeeded.
FIX: Reconnect shows per-operation progress, only acknowledged changes receive checkmarks, and failed steps remain reviewable. LC-SYNC-PROGRESS holds three real edits at successive boundaries, injects one conflict and verifies explicit recovery.

## Composer cleared before durable local acceptance
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The recent fire-and-forget path cleared the editor before local persistence and restored text later on refusal.
COST: A failed local write or intervening edit could create misleading success feedback.
FIX: Clear only after the fetch interceptor durably accepts the intent; delivery remains asynchronous. Existing composer_locally_accepted and composer_unconfirmed diagnostics identify the boundary. Newer text and files survive refusal.

## Quoted Gemini picker blocked an idle worker's steering boundary
AREA: steering
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The existing current_questions_survive_but_quoted_questions_do_not replay failed: a quoted boxed Gemini selector above a newer empty Claude prompt classified the worker as waiting.
COST: Automatic steering could refuse an idle boundary based on historical output.
FIX: Keep Gemini's live picker-over-placeholder behavior but disregard a boxed selector preceding a newer bare prompt. stale_picker_ignored emits a debug verdict. The existing cross-provider replay is the pre-fix failure; native completion remains separately blocked by host admission.

## Browser reaper reported disabled while its hard lifetime was running
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: The real browser-lifetime probe expired Chrome successfully, but its follow-up system-jobs assertion found no enabled reaper: setting one expiry arm to zero marked the entire job disabled.
COST: Operators could not distinguish a disabled lifetime arm from a stopped cleanup loop.
FIX: The catalog no longer treats arm-specific zero values as job-level disable switches. Actual per-job and global isolation still report disabled. LC-BROWSER-BACKGROUND requires an enabled reaper and disabled unrelated loops before launching, then observes real expiry; system-jobs exposes the corrected status and tick count.

## Reconnect toasts covered the sync checkmarks on a phone
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
SYMPTOM: Visual review of the passing iPhone sync-progress screenshot showed the reconnect toast covering the failed operation's explanation in the bottom checklist.
COST: The requested per-operation evidence was temporarily obscured exactly when it changed.
FIX: Use the visible checklist as reconnect feedback when saved work exists, cancel the lingering queue toast animation and clear its visible state when it opens, and remove its redundant completion toast. Reconnect without queued work still has its usual toast. Existing sync-step status and outbox diagnostics identify acknowledgements and failures.

## CLI launch negative control stopped reproducing its claimed fault
AREA: testing
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-12
SESSION: codex-server-sync
CARD: AMUX-4417
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
CARD: none — continuation of the user's isolated housekeeping request
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
CARD: none — live deployment verification for the user's housekeeping request
SYMPTOM: The first deployed sweep reported unmeasured run/evidence/audit directory cleanup with ENOENT. The service PATH omitted /usr/sbin, although lsof was available from an interactive shell.
COST: Directory cleanup deferred; 14 old log files were removed and five linked uploads were protected, but no diagnostic folders were examined.
FIX: Resolve macOS's /usr/sbin/lsof explicitly and include the executable in spawn-failure diagnostics. A native test restricts PATH to /usr/bin:/bin and checks that the probe observes a real held file; reverting to bare lsof must fail that test.
