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
## Dashboard's usage-limit discriminator says 'worker'; the live endpoint says 'session'
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-09
SESSION: rust-rebuild (provider adapters, RR-0043)
CARD: AMUX-2581
SYMPTOM: Porting the Claude usage probe to Rust, I took the 5h-window discriminator
  from the only in-repo consumer, loadUsage() in amux-server.py (`l.kind === 'worker'`).
  The live /api/oauth/usage endpoint returns `kind: "session"` for that window — the JS
  check never matches anymore, so the dashboard labels the 5h bar with the raw kind
  string, and the stale discriminator nearly shipped into the new Rust mapper verbatim.
COST: ~10 min re-probing the live endpoint; one step from encoding a never-matching
  filter into the Rust adapter (an ethos-7 silent probe: it would have "worked" because
  the top-level five_hour shape still mapped, masking the dead limits[] branch).
FIX: loadUsage() should accept both "session" and "worker" (the Rust mapper now does);
  better, both consumers should assert the discriminator against a recorded live
  fixture so endpoint drift fails a test instead of silently unlabeling a bar.

  VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `rust-rebuild (provider
  adapters, RR-0043)` is gone, so no author can sign this). The rust mapper accepts BOTH
  spellings — provider/claude.rs:317, `if kind_str == "session" || kind_str == "worker"`,
  with a comment naming which is live and which is older. Live check: GET /api/usage
  returns limits kinds ['session','weekly_all','weekly_scoped'], so the live spelling
  matches. The FIX section's actual ask is met too: recorded fixtures at claude.rs:404-405
  carry both kinds, so endpoint drift fails a test rather than silently unlabelling a bar.
  The dead `l.kind === 'worker'` filter is gone from the SPA.
  Probe note, since this entry is itself about a silent probe: I first called
  /api/oauth/usage and read its 404 as evidence. That is Anthropic's UPSTREAM URL
  (provider/claude.rs:51), never an amux route — amux serves /api/usage. The 404 was my
  probe missing, not the endpoint being absent, and it would have supported the wrong
  conclusion in the same direction the entry warns about.

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

## Opening peek permanently narrows the worker's tmux pane — observing changes the observed
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: peek-render agent (subagent; no $AMUX_SESSION in env)
CARD: AR-110
SYMPTOM: peek POSTs /resize to fit the pane to the viewer, and tmux pins
`window-size manual`, so the width persists after the viewer leaves. Verified live:
amux-test-claude was 220x50, one peek at a 390px viewport left it at 50x50 and it
stayed there. Across the fleet at scan time: mixpeek-autopilot 50 cols, amux 102,
amux-frustrations 94, amux-rust 94 — all real lanes emitting at a fraction of their
spawn width (220) for every later reader, because someone once peeked from a phone.
The floor is Math.max(50, ...) client-side and .clamp(50, 300) server-side, so 50 is
reachable and sticky.
COST: one wrong root-cause and a shipped CSS change that had to be reverted (see the
entry above) — the narrow pane presents exactly as "the renderer is wasting the
viewport", and nothing in peek shows the pane's column count, so the reader cannot
tell a narrow pane from a narrow render. Ongoing: any lane left narrow emits
hard-wrapped output to every future viewer and to its own transcript.
FIX: AR-110. Two parts worth separating — (1) do not let a transient viewer set a
persistent property of someone else's worker (restore on peek close, or scope the
resize to the read rather than the session); (2) surface the pane geometry in peek,
so "why is this 50 columns wide" is answerable from the instrument instead of from
`tmux list-sessions`.

  VERIFIED FIXED (part 1) 2026-08-21 (amux-frustrations; authoring lane `peek-render agent`
  was a subagent with no session, so nobody can sign this). The reported friction is gone:
  47 of 49 live tmux sessions are at 220 columns, 2 at 80, NONE at the reported 50/94/102.
  Mechanism removed both ends — runtime_jobs/pane_size.rs:207 issues `set-option -w -t <s>
  window-size latest` to undo the manual pin, and app.js:9340-9359 records the
  resize-on-peek machinery as deleted.
  PART 2 IS NOW DONE TOO — AF-128, shipped 12e8013 and live-verified.
  GET /api/sessions/<n>/peek returns no width, cols or geometry key. This entry's recorded
  COST was a wrong root cause and a reverted CSS change, because a narrow pane and a narrow
  render present identically — and that ambiguity survives the fix. Two lanes are at 80
  columns right now for unrelated reasons; the next reader who notices lands in the same
  undecidable spot.

  PART 2 CLOSED 2026-08-21: GET /api/sessions/<n>/peek now carries pane_cols and pane_rows
  (12e8013), verified on the running server rather than from the diff — amux 80x25,
  mvs-infra 80x24, amux-cloud 220x50. The two 80-column lanes are the control: the field
  tracks the ACTUAL width per session rather than reporting a constant, which is the only
  way it can settle this entry's actual question — narrow pane, or narrow render.
  No threshold and no "looks narrow" verdict, deliberately: picking a column count to warn
  at is the tuned parameter ethos.md warns about, and a reader comparing 50 against the 220
  everywhere else needs no constant. The parse returns None for every shape tmux emits when
  it cannot answer, because a fabricated 0 would answer this entry's question falsely.
  Still open, and small: the SPA peek header does not show it. The API is where every
  consumer can reach it; app.js was dirty with a peer's work at the time.

## The subagent switcher is wired end-to-end and reaches 0 of 50 sessions
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-09
SESSION: peek-render agent (subagent; no $AMUX_SESSION in env)
CARD: ARE-7
SYMPTOM: #peek-agent-nav (the ⌂/▲/▼ strip), agentNav(), the clickable .peek-agent-row
rows and the rust `agent-nav` verb are all present and byte-identical to the python
original — nothing was lost in the SPA extraction. The strip is gated on a VISIBLE
panel row (`⏺ main`/`◯ main`/`● main`/`○ main`) in the last 8 non-empty pane lines.
Running that predicate verbatim over every running session: 0 of 50 match, so the
strip is display:none everywhere, always. 46 of 50 DO show Claude's `← 2 agents`
status hint, but pressing ← (verified on an idle test session) opens the background
CONVERSATION manager — "Your conversation moved to the background · 4 awaiting input
· 0 working · 0 completed" with conversation rows — not a subagent panel with a
`main` row. Probe validated both ways first: a synthetic panel returns true, prose
returns false, so the zero is a real absence and not a broken matcher.
COST: a feature that looks complete in code review, in three layers plus a backend
verb, and that no user has ever been able to reach. Ethos rule 1 in its exact shape:
capability that exists but is received by nobody by default.
FIX: needs a live specimen of the current Claude Code agents panel to re-derive the
gate against — the `⏺ main` shape it looks for is either gone or only reachable from
a state nothing in the fleet enters. Do NOT widen the gate to the `← N agents` hint
without that: the existing comment warns that with rows hidden the nav keys open the
background-shells manager, and that is exactly what pressing ← did here. Separately,
what all 46 lanes actually have is background CONVERSATIONS, and amux exposes no
switcher for those at all — that is the reachable version of the same affordance.

  VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane was a subagent with no
  session). Resolved by DELETION plus a replacement, which is the right answer to "capability
  that reaches nobody" and better than what this entry asked for. app.js:8220 records the
  pane-driven switcher as deleted, citing ARE-7 and the 0-of-50 predicate, and names the
  replacement: a subagent list reading DURABLE transcripts via GET
  /api/sessions/<n>/subagents, with no visibility gate at all. Verified live on three lanes:
  amux 53, backend 143, amux-frustrations 1. Real data, not a matcher that might rot.
  The comment states the principle better than the entry did: "the fix for a predicate that
  matched nothing is to need no predicate, not to write a better one."
  Note on the entry's own alternative proposal: no background-CONVERSATIONS switcher exists
  (0 references in the SPA). That was a feature suggestion rather than the friction, and it
  is not what holds this entry open.

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

## Six SPA-consumed API families 404 in production and nothing anywhere says so
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-09
SESSION: amux-rust (RR-0130/0131 cutover sweeps)
CARD: AR-114, AR-115, AR-116, AR-118, AR-119, AR-120
SYMPTOM: The RR-0130/0131 live-data sweeps compared what the SPA READS against what the
  rust server SERVES. Six families the shipped dashboard calls answer 404 on the live
  server, and every one exists nowhere in `crates/`: `/api/channels/{a}/{b}/messages`
  (the DM drawer, polled every 2500ms), `/api/log-search`, `/api/memory/global`,
  `/api/observability`, `/api/review/week`, `/api/review/digest`. A seventh,
  `/api/metrics`, answers 200 with a completely different document than the SPA reads
  (`{board,events_journal,leases,queues,...}` vs the expected `data.sessions[]` +
  `data.system` + `data.server`), and the SPA calls `s.cpu_percent.toFixed()` on it
  unguarded. Nothing errored at cutover, no check went red, and the boundary registry
  (`/api/debug/boundary`) reports `proxied: []` — i.e. "everything is native" — because
  a family nobody implemented is not a family anybody proxied.
COST: These shipped broken at the python retirement and were still broken hours later;
  they were found only because someone diffed SPA call sites against live routes by
  hand. `/api/observability` is the entire Cost view, so 387,524 `token_ledger` rows
  have had no reader since cutover. Same failure shape as AMUX-2637 (board drive) and
  AMUX-2629 (submission): python-only capability, unported, invisible because absence
  does not raise.
FIX: The missing instrument is the one that would have caught all seven at once — a
  check that walks the SPA's own fetch call sites and asserts each resolves to a mounted
  route. `ROUTE_TABLE` already proves the reverse direction (claimed routes are routed);
  nothing proves the SPA's demands are met. `/api/debug/boundary` should report families
  the SPA calls that resolve to neither native nor proxied, so "unported" is a state the
  registry can express instead of one that reads as clean.

---
  PARTIALLY VERIFIED 2026-08-20 (amux-frustrations, NOT the author): FIVE of the six are routed. GET /api/health/invariants -> route.callers_have_routes now reports 8 failures and every one of them is /api/tunnel/* (start, status, stop). The tunnel family is tracked separately on AF-64, which sits in needsyou awaiting Ethan's revive-or-remove decision. STATUS stays open ONLY because of that one family; do not delete this entry until AF-64 resolves.

  VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust (RR-0130/0131
  cutover sweeps)` is gone).

  THIS SUPERSEDES MY OWN 2026-08-20 NOTE ABOVE, WHICH WAS WRONG. That note said "FIVE of
  the six are routed" and held the entry open on the tunnel family pending AF-64. Tunnel
  was never one of the six. I read `route.callers_have_routes` failures, saw they were
  all /api/tunnel/*, and mapped them onto this entry without checking them against the
  six families the entry NAMES three lines above. The right probe was to call the six.
  Called today, all six answer HTTP 200: /api/channels/{a}/{b}/messages, /api/log-search,
  /api/memory/global, /api/observability, /api/review/week, /api/review/digest.

  The seventh claim (/api/metrics serving a different document than the SPA reads) is
  also closed, and I nearly got this one wrong in the same direction. The payload has no
  `data` wrapper, which looks like the reported defect — but app.js:29269 assigns
  `_metricsData = data` (the raw body) and _metricsRender reads `data.sessions` /
  `data.system` off THAT, so top-level is what it wants. Live: 116 sessions, 49 active,
  and 0 active sessions lacking a numeric cpu_percent, so the unguarded .toFixed(1) at
  app.js:29427 does not throw.

  The missing instrument the FIX section asked for exists and can fail:
  route.callers_have_routes walks SPA/CLI call sites against the mounted table and today
  reports 8 failures, every one /api/tunnel/* — a different family, tracked on AF-64.
## Two rust call sites defer work to "while the Python server runs" — python is retired
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: amux-rust (RR-0131b sweep)
CARD: AR-117
SYMPTOM: `api/session_verbs.rs:5910` says `_write_claude_memory (symlink into
  ~/.claude/projects) is not ported — Python owns the memory composition during
  coexistence`, and `api/scope.rs:41` says `While the Python server runs (the migration
  soak) its next compose picks the edit up; the gap closes with the memory-compose port,
  not here.` Both are honest, well-written deviations — and both were made void the
  moment python was shut down. A worker memory write now updates
  `~/.amux/memory/<name>.md` and never composes `~/.claude/projects/<proj>/memory/
  MEMORY.md`. RR-0131b's own acceptance line ("MEMORY.md regenerated from migrated
  entries") cannot pass.
COST: Silent divergence between the memory a session edits and the memory Claude Code
  loads, for an unknown number of edits since cutover. Found only by grepping comments
  during a sweep; no test, no check and no doc references either site.
FIX: Deviations whose mitigation is "the other server covers it" need to be enumerable.
  A `GRACE:`-style marker (or a `python_covers_this` const the retirement checklist
  greps) would have turned python's shutdown into a list of exactly what stopped being
  covered, instead of a discovery process. RR-0154's shutdown criteria should include
  that grep.

  VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust (RR-0131b
  sweep)` is gone). Both NAMED comment sites are absent from crates/, and the grep
  discriminates: `git log -S` shows the strings entering at 0b156bb and leaving at
  ff6b7d1, whose subject is `fix(memory): compose MEMORY.md after worker memory writes
  (AR-117)` — the removal is the fix, not a reword. write_claude_memory now composes
  session memory into the project MEMORY.md. Live end-to-end evidence rather than a
  code read: THIS session's loaded MEMORY.md carries a composed worker-memory block and
  the fleet roster, which is the composition the fix produces.
  Note for anyone re-deriving this: a lowercase grep for `while the Python server runs`
  finds nothing because the source says `While`. The empty result is the probe missing,
  not the string being absent — check with `git log -S` before believing it.

---
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

## A CLI probe measured a connection failure and it read as the bug reproducing
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-10
SESSION: amux-rust
CARD: AMUX-2672
SYMPTOM: While reproducing AMUX-2653, every verb returned exit 1 whether piped or
  not. That reads as "the panic is everywhere". It was not: amux-rs defaults to
  https://localhost:8823, nothing listens there (8822 and 8824 both answer
  /health), so each verb died on connect before writing a byte. The real bug only
  appeared once AMUX_RS_URL was set by hand — and then only for `board list`,
  because the other verbs are too short to fill the pipe buffer.
COST: ~20 minutes and one wrong intermediate conclusion, which was then corrected
  only because 101 vs 1 did not match the card's claim. A less specific card would
  have let the wrong reading stand.
FIX: AMUX-2672 — point the default at a port that exists. The general shape is the
  one already in ethos rule 7: a probe whose failure mode is indistinguishable from
  the fault it is hunting will corroborate whatever you already believe. A
  connection error and an application error should not both surface as exit 1 with
  no discriminator.

  VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust` is gone). The
  defect was that amux-rs defaulted to https://localhost:8823, where nothing listens, so
  every verb died on connect and read as the application bug reproducing. Tested the built
  binary directly (~/.amux/rust-build-target/debug/amux-rs, since amux-rs is not on PATH):
  a bare `amux-rs board list` with no AMUX_RS_URL set exits 0 and returns 1,722 lines of
  real board data. It resolves the live endpoint on its own.

## A stderr capture moved stdout off the pipe, so nothing could break
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-10
SESSION: amux-rust
CARD: AMUX-2653
SYMPTOM: Comparing panic noise before/after the fix with
  `amux-rs board list 2>&1 >/dev/null | head -2` returned EMPTY for both binaries.
  The redirection order sends stderr to the pipe and stdout to /dev/null — so
  stdout was never attached to a pipe, no EPIPE was possible, and the pre-fix
  binary could not panic. Both looked identically silent, which reads as "no
  difference, fine".
COST: Would have certified the fix on a probe that could not fail, in the same
  session that ran the pre-fix binary and saw exit 101 ten minutes earlier. Caught
  only because "0 bytes of panic noise BEFORE the fix" contradicted a measurement
  already in hand.
FIX: Capture stderr to a FILE and leave stdout on the pipe
  (`cmd 2>err.txt | head`). Generally: when a probe reports no difference between
  a known-broken and a known-fixed artifact, the probe is the candidate before the
  conclusion is. This is the "loud wrong probe" from ethos rule 7 — it answered,
  and its answer was agreeable.

  VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust` is gone).
  Tested with the probe the entry says was botched — stdout ON the pipe, stderr to a FILE
  (`amux-rs board list 2>/tmp/e.txt | head -2`), not the `2>&1 >/dev/null` that detached
  stdout and made a panic impossible. Result: amux-rs exits 141 (128+13, SIGPIPE), which is
  correct Unix behaviour for a closed stdout, with 0 bytes on stderr and no `panicked` line.
  Not exit 101. And the control the entry's own lesson demands: unpiped, the same command
  emits 1,722 lines, so stdout really was attached to the pipe and an EPIPE panic was
  reachable — the silence is the fix, not the probe missing again.

## Five finished cards sat in `todo` and kept being auto-picked
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-10
SESSION: amux-rust
CARD: AMUX-2674
SYMPTOM: Auto-pickup handed me AMUX-2672 with "32 more queued". Five of those 32
  (AMUX-2599, 2609, 2618, 2634, 2636) were all fixed by ONE commit — e679bdb, whose
  subject literally reads "five carded defects — watchdog, the 404 trio, OSC-8,
  pane shrink, custom columns" and whose body names each card id. Their descs
  already said "DONE" and named a single remaining step (`git add`), which a later
  commit had done. Nothing moved the cards.
COST: The queue overstated real work by ~16% and auto-pickup kept offering finished
  cards, each costing a full scope-and-decide cycle to rediscover. Worse for
  anyone reading the board to see what is left: five defects looked open that were
  live in production.
FIX: The commit body already names the card ids in a machine-readable form. Nothing
  reads them. A commit trailer or body scan that flags "card named in a merged
  commit but still in todo" would have surfaced all five in one query — the data
  was there and unread, which is the same shape as AC-323's ignored_fields. Note
  the honest limit: a named card is not proof of completion, so this should
  SURFACE candidates for a human/agent check, never auto-close (ethos rule 8).

  VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust` is gone).
  crates/amux-server/src/api/commit_mentions.rs exists, cites AMUX-2674 and e679bdb by name,
  and GET /api/board/commit-mentions is routed and live — it returns 20 open cards named in
  merged commits right now, each with the sha and subject that named it.
  It also honours this entry's explicit ethos-8 caveat rather than quietly dropping it. The
  module header says so in its own heading, "It SURFACES, it never closes", with the reason:
  a card id in a commit is not proof of completion, since commits reference cards for
  context, for partial work and for reverts. The endpoint is a GET that mutates nothing.
  Probe note: my first call was /api/commit-mentions and returned 404. The route is under
  /api/board/. The 404 was my probe missing, not the feature being absent — same shape as
  the /api/oauth/usage miss recorded three entries up.

---
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

## The auto-builder ships any branch to the live fleet with no announcement
AREA: deploy
SEVERITY: blocks
STATUS: open (the live deviation is fixed; the hazard is not)
DATE: 2026-08-17
SESSION: amux-errors-and-bugs
CARD: AEAB-12
SYMPTOM: `~/amux` is the BUILD SOURCE, and the builder rebuilds on any local HEAD move
  regardless of branch; the server self-adopts in 5s. I committed a9aa7177 on a feature
  branch there at 00:02; at 00:03:43 the builder installed it and it served the whole
  fleet until 09:45 — 9h42m of an unreviewed, un-CI'd commit in production. The same
  condition left the machine 29 commits behind origin/main, so SCHED-1 ("keep me on the
  latest") fired at 09:00 and could not do its job.
COST: 9h42m of unreviewed code live, plus the owner's standing "keep me on the latest"
  request silently unmet while every indicator looked healthy. Diagnosing it took the
  first ~30 minutes of a log review that was supposed to be about something else.
FIX: Live deviation fixed — ~/amux back on main, fast-forwarded to 9d5aebf4, verified
  by build-stamp change (663a3a84 -> ec3228af), store=ok, 0 panics/0 ERRORs since. The
  hazard is NOT fixed and should not be fixed by refusing non-main HEADs: this machine
  survived weeks deliberately pinned to an unmerged fix branch, so that is a supported
  mode. The defect is that a deliberate pin and an accidental feature branch are
  byte-identical to the builder and the accidental one is announced nowhere. Wanted:
  one line in rust-auto-build.log naming the branch when the revision is off main, and
  the same fact on /health or the dashboard. Workaround that works today and belongs in
  CLAUDE.md: never develop in ~/amux — `git worktree add` and leave its HEAD on main.

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

## frustrations.md logged from ~/Developer/amux is stranded — that checkout cannot push
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-17
SESSION: amux-errors-and-bugs
CARD: AEAB-18
SYMPTOM: Two copies of this file exist and the one a session is pointed at is the one
  that cannot reach anyone. `~/Developer/amux/frustrations.md` holds 25 entries /
  43,934 bytes; `~/amux/frustrations.md` (origin/main) holds 116 / 207,952. Same file,
  same lineage — the local one is a stale revision that has ALSO diverged, holding at
  least one entry that never reached origin. CLAUDE.md and `.claude/rules/frustrations.md`
  both live in the stale checkout and say to append to "frustrations.md at the repo
  root", which for a session cwd'd there resolves to the stranded copy. The append
  succeeds. There is no error.
COST: All four frustration entries from today's log review went into the stranded copy.
  The whole argument for this file is that a single frustration is a complaint and a
  cluster is an argument — three entries sharing an AREA is the signal. That only works
  if they are in the file everyone reads. Mine were invisible to every other session and
  to any AREA tally run upstream, and would have stayed so indefinitely: the unblocker
  is the 4-unpushed-commit divergence that has been an open owner decision since
  2026-08-13.
  Distinct from that divergence rather than a restatement of it: that one is "the
  checkout cannot fast-forward", which announces itself. This one is "the documented
  place to log friction is INSIDE that checkout", so the divergence silently swallows
  new writes instead of blocking a read.
FIX: Migrated today's four entries here and verified against
  `scripts/frustrations_audit.py` — no new structural problems, all four CARD ids
  resolve on the live board. The underlying choice is open and worth making
  deliberately: (a) resolve the divergence so the checkout syncs again — owner's call,
  needed regardless; (b) point the rule at the build source, which can push, and say why;
  (c) have the rule REFUSE to append to a checkout that is behind origin, or at minimum
  warn. (c) is the one that survives the next time two checkouts drift, because this
  failure is silent by construction.

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
## A wedged disk scan could not say whether the walk or the database was stuck
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-20
SESSION: desktop
CARD: DESKT-15
SYMPTOM: A reclaim scan froze at 1,087 directories and sat there for 35
  minutes until a builder restart reaped it. `dirs_walked` stops moving in
  exactly the same way whether `read_dir` is blocked in the kernel or the
  SQLite flush is blocked on the write lock, and the row carried no phase, so
  the two hypotheses were indistinguishable from outside the process. Worse,
  the reaper I had written to make dead scans legible was clearing
  `current_path` as it marked them interrupted, so the finished row said the
  server had restarted and refused to say where. The one field that would have
  answered the question in a second was being deleted by the code whose stated
  job was to expose the failure.
COST: About 40 minutes, most of it re-walking the home directory by hand with
  a stopwatch to find what the scan already knew and had thrown away. The
  culprit turned out to be one directory: `~/Library/Mobile Documents` never
  returns from readdir on this machine (90s, zero entries, still blocked),
  while `stat` on it answers instantly with the same st_dev as $HOME, so the
  walker's cross-mount guard had no reason to skip it.
FIX: 7ecb766. Position and phase are published per directory BEFORE the syscall that
  can block, separately from the throttled write that persists them; the
  reaper preserves both and names them in its error text; a watchdog WARNs to
  server-rs.log BEFORE it touches the store, so a stall in the write lock still
  reports rather than hanging where the walker did. Stalled directories are
  recorded, and skipped by later scans once corroborated, with a Re-include
  button so the exemption is not a one-way ratchet.
  CORRECTION, 0371230: 7ecb766 made the watchdog END a scan at 45s and
  permanently exempt the directory it was on. Its first production run did that
  to ~/Downloads, which answers readdir in 2 seconds with 318 entries. The
  threshold was below the baseline — ~50 sessions at load 95, with the scan
  competing for the disk it measures — so the detector fired on contention it
  was itself producing, and its action was a silent hole in the scan. Now it
  WARNs at 45s and decides nothing, ends a scan at 300s, and routes around a
  path only after it hangs two separate scans. On the verifying run ~/Documents
  went quiet for 46s, was named in the log, and was NOT exempted. The fix
  found its own bug within the hour, which is the argument for the instrument.
  Same commit fixed a second bug found by measuring rather than by theory:
  `devtool_roots()` is a list of real absolute paths that `walk()` sized
  regardless of cfg.roots, so every unit test calling walk() on a tempdir also
  scanned ~/.cache, ~/Library/Caches and the 15GB shared cargo target dir. Two
  such tests ran 14 hours at 0% CPU and took every lane's `cargo test` hostage
  on the shared build lock. A peer read the 0% CPU as the FileProvider hang
  above, which had been proven real an hour earlier and so corroborated itself;
  lsof showing NO directory fd at all is what separated them.

---

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
## amux's own debug log is the biggest thing on a disk amux is filing cards about
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-22
SESSION: amux-errors-and-bugs
CARD: AEAB-49
SYMPTOM: `curl /health` on the live server returned `commit: 5eabfb4dc6cc` — a commit that
  exists only on my unmerged branch, never reviewed, never merged. `rust-build-provenance.json`
  said `{"sha":"23ddb8d1...","ref":"fix/push-guard-rebase-false-positive","on_main":"no"}` and
  a build of that commit was in flight. The auto-builder builds `~/amux` HEAD every 60s, and
  I had been checking feature branches out in `~/amux` all session.
COST: the fleet ran unreviewed branch code for at least one build cycle. Nothing broke, and
  that is luck rather than design — the same mechanism would have shipped a mid-edit tree just
  as happily. It also churns: putting the checkout back on main makes the next tick rebuild and
  reinstall, so the fleet takes a second unnecessary swap.
FIX: the guardrail already exists and it is a log line nobody reads — rust-auto-build.log says
  "Installing it makes it the live build for the WHOLE FLEET within ~5s, with no CI and no
  review. Intentional pin? fine. Accident? put ~/amux back on main — develop in a git worktree,
  not the build source." It printed exactly that, correctly, while installing my branch. A
  warning that fires as it does the thing is not a guardrail. `on_main:no` is already computed;
  the builder should either refuse to INSTALL an off-main build unless a flag says the pin is
  deliberate, or announce it where a session actually looks (a board card or the session
  banner) rather than only in its own log.
  The general shape, and it is the third instance today after AEAB-46 and AEAB-47: amux knows
  the dangerous fact, computes it correctly, and writes it somewhere the person who needs it
  never opens. Rule 4's second layer — a tag in a store the reader never opens is the same
  failure as no tag.

## I read `hook_outdated` as file staleness; it is not, and AF-156 is right
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-23
SESSION: amux-errors-and-bugs
CARD: AEAB-47
SYMPTOM: my own error, corrected here rather than by rewriting anyone's entry. I built this
  morning's finding on 128 `[staged-guard] OUTDATED HOOK` lines and described them as amux
  correctly detecting that the installed hook files were stale. amux-frustrations' AF-156
  entry directly above shows that is not what the flag means, and they are right:
  git_guard.rs:1586 is `let guard_version = obj.get("guard_version").as_i64().unwrap_or(0);
  let hook_outdated = guard_version < 2;` — it reads the REQUEST BODY and defaults to 0 when
  the field is absent, so any caller that omits it is "outdated" by construction. I verified
  that line myself before writing this. It is not a file check and never was.
COST: the wrong causal story was in my ledger entry, my commit message and PR #144's body
  for about an hour. It did not change what I built, which is the only reason it is cheap.
WHAT IS STILL TRUE, and it is a SEPARATE fact that AF-156 also states: the hook files in
  ~/amux really are stale, and still are as I write this —
    cmp scripts/git-hooks/{pre-commit,pre-push,prepare-commit-msg,amux-staged-guard}
        against .git/hooks/*   ->  all four DIFFER
    ls .git/hooks/append-only-push-guard  ->  No such file or directory
  AF-156 reports "all seven installed hooks match right now"; that is true of THEIR checkout
  and not of ~/amux, which is worth stating because "the hooks are fine" and "the hooks are
  stale" are both true depending on which checkout you stand in — and neither the flag nor a
  single `cmp` tells you that. Per-checkout is the unit.
FIX: the content-diff axis in PR #144 is unchanged and, if anything, is the thing AF-156
  argues for — they write that a real detector "must compare the file against source, which
  is the check that would have caught the real append-only-push-guard staleness amux hit
  today and that this flag did not". What I am correcting is the EVIDENCE I cited, not the
  fix. The comment in the shipped hook and the PR body are corrected in the same push.
  The lesson for me: I treated a log line's WORDING as a measurement. "OUTDATED HOOK ...
  Reinstall: scripts/install-hooks.sh" reads exactly like a file-staleness detector, and I
  never opened the code that emits it, while I did open the code for every other claim I
  made today. A message that names a plausible cause is not evidence for that cause.

---

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

## `amux board --help` reports the flag as an unknown SUBCOMMAND instead of printing help
AREA: cli
SEVERITY: annoys
STATUS: open
DATE: 2026-08-23
SESSION: desktop
CARD: DESKT-21
SYMPTOM: `amux board --help` -> `amux board: unknown subcommand: --help` (exit 0). Help is
  reachable only as `amux help board`. `amux board` with no args prints the whole board, so
  neither of the two things a person reaches for when a verb fails shows the verb list.
COST: minutes, and it compounds the entry above: the natural way to check "does a `desc` verb
  exist" is `--help`, and that path answers with a message shaped like a verb error, which
  reads as though `--help` itself were the mistake rather than as "here are the verbs".
FIX: treat `-h`/`--help` in the subcommand slot as a request for the same text `amux help
  board` prints, and echo the verb list in the `unknown subcommand` error rather than only
  naming what was rejected.

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

## The disk ranker cannot rank a file, so it could never have named the 1.8 GB one
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-22
SESSION: amux-errors-and-bugs
CARD: AEAB-42
SYMPTOM: `disk_candidates()` pushes only entries where `metadata().is_dir()`. Its own
  cache, `~/.amux/du-sizes.json`, holds 26 entries and all 26 are directories. `amux.db`
  would rank fourth, above `~/.claude`, and is absent.
COST: the report meant to say what is eating the volume pointed at ~/Library/Caches,
  ~/.npm and ~/.cache while the fourth-largest object was amux's own database — for as
  long as that database has existed. I only found it by running dbstat by hand.
FIX: push regular files over a size floor from the same read_dir passes; the size is
  already in the metadata so there is no extra du cost. The lesson worth keeping: AEAB-33
  taught the ranking to declare the candidates it FAILED on, and that warning can never
  declare candidates it never GENERATED — after adding a surfacing mechanism, ask what
  the mechanism itself cannot express.

## I fixed the inner loop of a noisy warning and left the outer one, at 77% of the log
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-22
SESSION: amux-errors-and-bugs
CARD: AEAB-45
SYMPTOM: 1,336 of 1,726 lines in the 24h window are one sentence naming `~/.Trash
  (du exit 1)`, a condition that cannot change, emitted every autofix tick on each of two
  servers. I wrote it in AEAB-33, and its own comment says it now fires "ONCE per run ...
  rather than once per attempt" because the per-attempt spelling "drowned the log it
  shares with real faults".
COST: it competed for attention with three real findings in the same window (AEAB-41,
  AEAB-42, AEAB-43). AEAB-13 recorded the identical shape at the identical ratio — 921 of
  1004 lines — where it buried a first-ever `database is locked` line during a log review
  that existed to find exactly that.
FIX: reuse AEAB-13's tested `stall_log_first_this_bucket` rather than writing a second
  spelling of it, keyed on the joined path list so a CHANGED skip set still logs
  immediately. The pattern: a per-run dedupe is not a dedupe if the run is on a timer.

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

## amux.service's KillMode=mixed cgroup-kills the whole fleet on every ordinary deploy
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-30
SESSION: amux (this session, catching up on the 2026-08-29 reboot-verification memory)
CARD: INIT-1
SYMPTOM: Continuing the prior session's "verify everything comes back after reboot"
  checklist, `GET /api/sessions` showed ALL 9 registered worker sessions with
  running:false — not just after the physical reboot, but again after the routine
  08:31:39 auto-builder restart that followed (commit 251cf15b, an ordinary
  feature-branch deploy). `tmux list-sessions` had nothing but a freshly-recreated
  `amux-init`; the real tmux server that held every worker's session had been killed
  outright. Root cause: `amux.service` has `KillMode=mixed` + `SendSIGKILL=yes`, and
  the tmux server lives in that unit's cgroup (spawned by ExecStartPre, never leaves
  it — cgroup membership is sticky across reparenting to PID 1 even though tmux
  daemonizes). Every restart of amux.service — reboot OR ordinary deploy — SIGKILLs
  the whole cgroup, tmux server included. `amux-worker-start.service` only fires once
  at boot (`WantedBy=default.target`), so nothing brought sessions back afterward.
  This generalizes the narrower 2026-08-29 entry ("worker session does not auto-
  restart when server restarts", CARD: none, still open) — that one suspected a
  single worker and a single restart path; this is the whole fleet, and it fires on
  every commit-triggered deploy, which happens many times a day on an active branch.
COST: The entire fleet (9 lanes) silently down for ~1h25m (08:31 restart to 09:56
  discovery+fix) with no alert anywhere — `/health` reported "ok" the whole time,
  because the server process itself was fine; only the sessions it was supposed to
  be managing were gone. Inbound Telegram messages during that window had nowhere to
  land. A separate near-miss found along the way: `amux start <name>` (no --detach)
  silently returns exit 1 with ZERO output when it can't attach to a non-existent
  TTY, even though the start itself succeeded — first read as "start is broken",
  cost a few minutes of confusion before `--detach` runs revealed it was already
  running.
FIX: `~/.config/systemd/user/amux.service`: `KillMode=mixed` -> `KillMode=process`
  (config-only; `daemon-reload` applied without disrupting the running process —
  confirmed same PID/start-time before and after the reload). `process` mode signals
  only the unit's main PID, leaving the tmux server (and its sessions) alone —
  matching what ExecStartPre's own idempotent `has-session || new-session` check
  already assumed. VERIFIED live: `systemctl --user restart amux.service` (09:59
  UTC) — PID changed, uptime_s reset, and all 8 real worker sessions (excluding the
  separately-broken `synthesia`, wrong macOS path) kept their original tmux
  `created` timestamps and came back running:true with no manual restart needed.
  NOT YET DONE (the log-signal half, tracked on INIT-1): an `invariants/checks.rs`
  check for "session expected running (standing_orders / no recorded stop event)
  but `tmux has-session` says no" — today nothing in `runtime_jobs` would have
  caught this without a human reading the dashboard; `backend::bootstrap::Bootstrap`
  only reacts to explicit Starting/ended DB transitions, and an out-of-band cgroup
  SIGKILL produces neither.

## `amux start`/`start-all` silently die under `set -e` on a tmux target-syntax bug
AREA: cli
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-30
SESSION: amux (recovering from the KillMode incident above)
CARD: INIT-2
SYMPTOM: While recovering the fleet from the KillMode=mixed incident (previous entry),
  `amux start-all` created exactly ONE tmux session then exited 1 with NO output at
  all. `amux start <name>` on any not-yet-running session behaved the same: silent
  exit 1, session left running-but-unlocked in tmux, nothing printed. Root cause:
  `cmd_start`'s window-name lock, `tmux set-option -t "=$tname" allow-rename off
  2>/dev/null`, targets a WINDOW-scoped option with a bare session-exact-match
  target — tmux looks for a window literally named "=amux-<name>", finds none,
  exits 1 — and `set -euo pipefail` (line 19) kills the function right there, with
  the only evidence routed to `2>/dev/null` on that exact line. A second, separate
  bug compounded it: `cmd_start_all` called `cmd_start "$name"` with no `--detach`,
  so even after fixing the first bug, the first session started still hit
  `cmd_start`'s own terminal-attach step, correctly failed "open terminal failed:
  not a terminal" in this non-interactive context, and `set -e` aborted the rest of
  the loop — every session after the first silently stayed down.
COST: `amux start-all` — the obvious, documented recovery command for "the whole
  fleet is down" (INIT-1) — was silently non-functional for that exact use case.
  Cost ~15 minutes of manual per-session `amux start <name> --detach` calls to
  actually recover the fleet before this was root-caused, and would cost the same
  to the next session (or the next reboot) that reaches for `start-all` expecting
  it to work.
FIX: `amux` (ships on save, already live): `-t "=$tname"` ->
  `-t "=$tname:"` on both the `set-option`/`set-window-option` lines (explicit
  window target); `cmd_start_all`'s `cmd_start "$name"` -> `cmd_start "$name"
  --detach`. Verified live: a fresh non-TTY `amux start <name>` now starts the
  session and prints the honest attach-failure message instead of silent exit 1;
  `amux start-all` against 8 fully-stopped sessions now starts all 8 in one pass
  (the 9th, `synthesia`, fails for a pre-existing unrelated reason — a macOS path
  baked into its config on this Linux box — and now says so clearly instead of the
  whole batch dying silently after the first session).

## An AF-66-style guard existed for this and had been green the whole time
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-25
SESSION: amux
CARD: AMUX-3707
SYMPTOM: `assert_cli_verbs_exist` in board_drive.rs does exactly the check that
  would have caught the above, and was written for exactly this failure (AF-66,
  where `amux board show` fell through to help and exited 2). It is called on
  ONE prompt, from one fixture: the pickup Claim prompt. The decompose nudge
  never flowed through it, so a verb it named for months did not exist and the
  suite stayed green.
COST: No wrong conclusion shipped, but the guard's existence is what made the
  gap invisible. Anyone auditing "do we check that emitted commands exist?"
  finds the helper, reads it, and stops. Reading the check does not reveal which
  call sites it covers.
FIX: c1c238b1 widens it from one fixture to a source sweep of the whole server
  crate. The general lesson is ethos rule 7's: ask where the defect would be
  INTRODUCED and confirm the fixture flows through that code, not an ancestor of
  it. A single-call-site guard is worth naming its scope in its own doc comment.

---
## `amux` died at load with a bash syntax error — every subcommand, every session, at once
AREA: cli
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-30
SESSION: amux (recovering from the KillMode incident above)
CARD: INIT-2
SYMPTOM: While recovering the fleet from the KillMode=mixed incident (previous entry),
  `amux start-all` created exactly ONE tmux session then exited 1 with NO output at
  all. `amux start <name>` on any not-yet-running session behaved the same: silent
  exit 1, session left running-but-unlocked in tmux, nothing printed. Root cause:
  `cmd_start`'s window-name lock, `tmux set-option -t "=$tname" allow-rename off
  2>/dev/null`, targets a WINDOW-scoped option with a bare session-exact-match
  target — tmux looks for a window literally named "=amux-<name>", finds none,
  exits 1 — and `set -euo pipefail` (line 19) kills the function right there, with
  the only evidence routed to `2>/dev/null` on that exact line. A second, separate
  bug compounded it: `cmd_start_all` called `cmd_start "$name"` with no `--detach`,
  so even after fixing the first bug, the first session started still hit
  `cmd_start`'s own terminal-attach step, correctly failed "open terminal failed:
  not a terminal" in this non-interactive context, and `set -e` aborted the rest of
  the loop — every session after the first silently stayed down.
COST: `amux start-all` — the obvious, documented recovery command for "the whole
  fleet is down" (INIT-1) — was silently non-functional for that exact use case.
  Cost ~15 minutes of manual per-session `amux start <name> --detach` calls to
  actually recover the fleet before this was root-caused, and would cost the same
  to the next session (or the next reboot) that reaches for `start-all` expecting
  it to work.
FIX: `/home/syseng/src/amux/amux` (ships on save, already live): `-t "=$tname"` ->
  `-t "=$tname:"` on both the `set-option`/`set-window-option` lines (explicit
  window target); `cmd_start_all`'s `cmd_start "$name"` -> `cmd_start "$name"
  --detach`. Verified live: a fresh non-TTY `amux start <name>` now starts the
  session and prints the honest attach-failure message instead of silent exit 1;
  `amux start-all` against 8 fully-stopped sessions now starts all 8 in one pass
  (the 9th, `synthesia`, fails for a pre-existing unrelated reason — a macOS path
  baked into its config on this Linux box — and now says so clearly instead of the
  whole batch dying silently after the first session).

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

## Typing at a lane disabled that lane's auto-pickup
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3757
SYMPTOM: Every prompt is auto-captured as a `doing` card whose desc is still literally `**Prompt:** <what was typed>`, and that card counted against the WIP-1 cap. An unanswered prompt is not work in progress — the decompose nudge exists precisely to make the lane dispose of it — but the pickup query could not tell the two apart. The exemption list already carried tripwire, watch, epic and needs:you for the same reason and had never been extended to the cards amux mints itself.
COST: The specimen is TUBES-2225, titled "Why are you stopping": Ethan's complaint about tubescience stopping was itself the card holding the WIP slot that kept it stopped. A frustrated re-prompt is the likeliest prompt to arrive at a stalled lane, so the loop closed on exactly the lanes already in trouble. This lane's own board held 11 capture shells in `doing` at once, all of them his prompts.
FIX: 7e4682f0 — a capture shell joins the WIP exemption, using the same `substr(desc,1,11)` form as the fold query in board.rs so the two cannot disagree about what a capture shell is. Reshaping the desc, which is the exit the decompose nudge already asks for, makes it count again.

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

## "The tests pass" is load-dependent on this box, so a green suite is a weaker claim than it reads
AREA: tests
SEVERITY: slows
STATUS: open
DATE: 2026-08-28
SESSION: amux
CARD: AMUX-3853
SYMPTOM: A full `cargo test -p amux-server --lib` run showed 8 failures, all in `opencode::structured`, in code nobody had touched. Re-run in isolation the same tests are 15 pass / 0 fail. The failures were build contention: those tests spawn a binary out of the shared `CARGO_TARGET_DIR` while another lane's build is rewriting it, which is the ETXTBSY family `2618b7d3` already added a retry for. The retry is not sufficient under the load this machine actually carries (50 lanes, a builder rebuilding on every commit, and any peer running clippy).
COST: I nearly reported 8 failures as a regression in a peer's area, and spent a cycle proving they were not. The larger cost is retrospective: every "1530 pass, 0 failed" I wrote on a card today rested on a run that happened not to contend, and I could not have told the difference at the time. A green suite here means "green, and nothing was building" — the second clause is invisible and nobody states it. That is the same shape as the 706ms latency number from the same day: a measurement taken on a machine whose load is the dominant variable, reported as if the load were not there.
FIX: none yet. The cheap instrument, not the cure: have the test run record whether a build was in flight (the builder's lock is already on disk at `~/.amux/rust-build.lock`) and print it beside the result, so a red suite says whether it was contended. The cure is either per-lane target dirs (rejected before, for disk) or serialising the spawn-a-binary tests behind the same lock the builder takes. Naming the instrument first because the wrong lesson from this entry is "ignore red suites", and a contention flag is what separates the two honestly.

---
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
NOTE THE THIRD OUTCOME, because neither party had a slot for it: this was not "you were
 right" or "I was wrong". The signal was REAL and pointed at the WRONG EVENT. An
 attribution system keyed on time rather than content will keep producing that verdict,
 and the AF-179 caveat is doing real work — it is why amux hedged instead of asserting —
 but a caveat cannot make an unfalsifiable signal falsifiable.

## A test cell that reads the ambient process ancestry cannot fail on the box that wrote it
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-31
SESSION: amux
CARD: AMUX-3962
SYMPTOM: `checks` red on main for the whole fleet, two consecutive runs. Failing step
 `test-commit-stamp.sh`, cells 1 and 2, `alpha='' beta=''` and `got ''`. Both cells ran
 the commit-msg hook under whatever process ancestry the test inherited and asserted on
 the `Amux-Agent` trailer, which the hook populates by walking its own parents for a
 `claude` process. On any dev box that walk finds the session running the test, so both
 cells pass. In CI there is no claude anywhere in the tree, the hook correctly omits the
 field, and both cells fail on an empty string. Reproduced locally by reparenting the
 test to init, which is what a runner looks like from inside the walk: 7 passed, 2 failed,
 same two cells, same empty values.
COST: About an hour of fleet-wide red CI, and the specific cost is that `checks` is the
 job every lane's `board done` evidence leans on, so a red there taxes work nobody
 involved was doing. Worse, it was invisible in the only place anyone was looking: two
 lanes independently ran the local suite that night and both read green (1665/0), because
 the local suite and the CI job were not running the same thing. The commits that went
 red were not the commits that broke it. The cells had NEVER been green in CI; run
 33396997200 was simply the first one to reach them, so the fleet-wide red landed on
 whoever happened to push next, four commits downstream of the author.
FIX: 232c212f. The two cells now build their own ancestry, the technique the later cells
 in the same file already used: one `claude` shim (a symlink, so ps sees a matching
 argv[0] basename), both hook runs under it, so ancestry is a test INPUT rather than a
 property of whoever launched the test. 9/9 with a claude ancestor and 9/9 reparented to
 init. Cell 2 got stronger on the way past: it asked `ps -p <pid>` for liveness, which
 cannot tell the right process from any live one. Mutating the hook to stamp `pid=1` is
 both invariant and live, and the old pair passed that completely clean; against the
 shim's known pid it fails.
THE SHAPE, which is the reusable part: a cell that reads the ambient environment measures
 the LAUNCHER, not the code. It is not merely untested in the other environment, it is
 structurally unable to fail in the one where it was written, so a local green carries no
 information about it at all. The tell is an assertion whose subject was not constructed
 by the test. That is ethos rule 7 with a location attached: "can your check actually
 fail" has to be asked about the environment as well as the logic, and the way to ask it
 is to run the file somewhere the ambient answer is absent.

---
## A status signal with a store, a consumer and a unit test, and no producer anywhere
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux
CARD: AMUX-4024
SYMPTOM: `subagents_live` was null for 125 of 125 lanes. AMUX-3048 shipped
  `subagent_event_post` (start/stop), a `{count, ts}` store, a reader in
  `FleetSignals::subagents_working`, an explain field, a status-history column and a
  passing unit test. No hook ever POSTed an event, so every one of those read null
  forever. The code comment deferring the count-authoritative "off" direction reads as
  a careful trade-off between two live signals; there was only ever one, because the
  other was never sent. Two more details compound it: the deferral names the producer
  as "PreToolUse:Task" and the tool is called `Agent` in current Claude Code, so the
  hook would have been inert even if someone had wired the documented name; and
  `hooks.report_hooks_wired` walks the entries that EXIST, so it structurally cannot
  fail on an event class nobody added.
COST: Two wrong lane statuses reported by Ethan in one afternoon, in opposite
  directions, both landing on the mtime fallback nobody knew was load-bearing:
  tubescience read IDLE while blocked on a background agent, mvs-pitr read WORKING
  with an AGENTS badge over an empty composer. About 40 minutes of this session spent
  designing a fix keyed on the reported count before checking whether any lane
  reported one — the answer was none, and the first fix would have been green and
  completely inert, which is the same defect a second time.
FIX: Producer wired in `scripts/hooks/hook-report.sh` (`subagent:start` / `subagent:stop`)
  and in settings.json as `PreToolUse[^(Task|Agent)$]` + `SubagentStop`; count made
  authoritative in both directions; `hooks.report_hooks_wired` extended with an
  absent-event-class arm so the next dead producer fails a check instead of reading
  as a deliberate trade-off.

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

## Delegated worker requests held the requester's WIP instead of becoming task dependencies
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-05
SESSION: amux
CARD: AMUX-4153
SYMPTOM: `amux board request` created a durable child task and terminal callback,
 but left the requester's active parent in `doing` with no `depends_on` edge. The
 requester therefore occupied its WIP slot and appeared idle until the peer finished.
COST: Delegation serialized work that should have run concurrently, hid independent
 ready work from the requester, and left the board without a durable record of why
 the requester was waiting.
FIX: ccb37879 atomically adds the delegated child to the parent task's dependencies,
 requeues the parent to `todo` to release WIP, and lets the existing ready frontier
 wake it when the child closes. `amux::task_dependency` now logs a verdict for every
 linked, standalone, ambiguous, invalid-parent, or cycle-refused peer request.

## Usage resets opened the clock gate after delivery had already been skipped
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-05
SESSION: amux
CARD: AMUX-4154
SYMPTOM: A Claude worker with a known, elapsed reset remained protocol-level
 `RateLimited` until its first subsequent prompt, while the legacy lane sweep could
 roll a stale clock banner forward to the next day. The scheduler also recovered
 rate-limited workers after command delivery and planning had already consumed a
 stale snapshot, leaving the worker idle for another tick.
COST: Workers could remain visibly idle after their provider said usage was
 available, and queued work needed a manual interaction or an extra scheduler cycle
 before it continued.
FIX: dd57e1d5 makes an elapsed reported reset immediately deliverable, recovers
 protocol workers before pumping and planning, and preserves the original reset
 through stale terminal banners. `amux::usage_reset` emits `worker_recovered`,
 `delivery_released`, and `delivery_gate_open` verdicts at each release boundary.

## Settings collapsed a multi-provider fleet into one Claude quota bar
AREA: observability
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-05
SESSION: amux
CARD: AMUX-4154
SYMPTOM: The Settings usage panel exposed only Claude's two coarse percentages even
 though the built-in registry also supports Codex, Gemini, and Ollama. Codex's API
 supplies named buckets, exact reset times, durations, credits, and plan metadata,
 but none of those facts reached the operator.
COST: Operators could not tell which provider constrained a mixed fleet, how long a
 limit would last, or whether another provider was unmetered or simply unmeasured.
FIX: ecbf4daf derives the Settings rows from the full built-in provider registry,
 retains every provider-reported window and exact reset, and distinguishes unavailable
 quota APIs from unlimited local inference. 6bdf9999 also resolves Codex through the
 same login-shell path as a real worker, rather than launchd's stale-but-executable
 shim. `amux::usage_probe` logs whenever a probe succeeds or cannot report its quota.

## Isolated workers hid confirmed owner work from the shared board
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4159
SYMPTOM: The live `amux` worker has `CC_ISOLATED=1`. Its first task prompt in this
session was recorded in `cmd_history` row 44634 as `delivery=direct`,
`submit_verdict=confirmed`, but `card_id=NULL`; there was no capture verdict in the
server log. Direct prompt capture deliberately excluded every isolated worker, and
the capture health invariant deliberately excluded the same population, so mechanism
and monitor agreed on invisible work.
COST: The user had to point at the Workers board to establish that the task ledger
was still incomplete, and the first linked implementation request had no card or
task-local evidence even though the worker had received and was executing it.
FIX: 6bce0158 removes isolation from owner-prompt capture while preserving its
harness, peer-discovery, and automation boundaries. Capture logs now include
`owner_isolated`, the health invariant evaluates isolated-owner prompts, and an exact
regression fixture proves the confirmed live prompt shape mints and links a card.

## Decomposition accepted child cards that did not say how to execute or verify them
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4161
SYMPTOM: The capture decomposition endpoint required a title, priority, dependency
 indexes, and next action, but accepted an empty description and no acceptance
 criteria. All 24 live decomposition children predate the corrected contract and
 lack acceptance criteria, yet no health invariant reported that incomplete
 population.
COST: A worker could receive a syntactically valid child without enough durable
 detail to determine scope or falsify completion; terminal evidence then depended
 on conversational context outside the board.
FIX: The endpoint now rejects vague descriptions and missing, malformed, or duplicate
 acceptance criteria atomically, and the board health sweep reports every incomplete
 live decomposition row with `measured`, `n_considered`, and per-card gap names.

## Manual claim bypassed a decomposed task's dependency graph
AREA: board
SEVERITY: corrupts
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4161
SYMPTOM: Board-drive held dependency-backed children in backlog, but the public claim
 endpoint could force a backlog child directly to doing without consulting those
 dependencies. The chaos journey reproduced the bypass by claiming plan step two
 while step one was still open.
COST: Two workers could execute an ordered plan out of sequence, consuming work whose
 prerequisite had not produced its result while the board still displayed a valid
 dependency edge.
FIX: The claim primitive now checks dependencies in the same SQLite writer transaction
 as its status compare-and-swap. A refusal returns `dependency_blocked` with the exact
 blockers and writes both a WARN verdict and a durable `claim.dependency_blocked`
 session event.

## A different decomposition retry was reported as idempotent
AREA: board
SEVERITY: corrupts
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4161
SYMPTOM: Once an epic had children, every later decomposition request returned
 `idempotent: true` without comparing the submitted plan to the committed one. A
 retried or racing caller could therefore believe its changed plan won even though
 the board retained a different child set.
COST: The API acknowledged work it did not store and gave the caller no discriminator
 for recovering after a lost response or concurrent decomposition.
FIX: Decomposition now persists a normalized plan SHA-256 on the root epic. Exact
retries return measured idempotent success; divergent retries return a measured 409
with both hashes and emit a greppable `plan_conflict` WARN verdict.

## The todo ceiling stranded a dependency successor after its predecessor closed
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4161
SYMPTOM: Live dogfooding closed AMUX-4162, but AMUX-4163 remained in backlog across
 multiple measured board-drive ticks even though its only dependency was done. The
 selector found it correctly; the shared transition engine then refused backlog to
 todo because this lane already had 122 unrelated todo cards against its 20-card
 queue ceiling, and the refusal was discarded without a log line.
COST: An accepted ordered plan could stop permanently between steps for a condition
 unrelated to that plan, while `/api/debug/board-drive` reported `promoted: 0` with
 no card or refusal reason to investigate.
FIX: Dependency-cleared promotions now carry a narrow todo-ceiling exemption while
 retaining transition, archive, and gate checks; revisit and ordinary queue additions
 remain capped. Any selected promotion still refused by the transition engine now
 emits a measured `promotion_refused` WARN naming the card and exact refusal.


## A successful trigger PATCH immediately puts parked work back in todo
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4168
SYMPTOM: The 48-hour worker audit found explicit source_ref updates retaining an older last_verified_at. GCA-157 was parked at 12:51 and auto-drained at 12:51; its new condition did not start a new parking window. The API returned success plus an advisory instead of completing the parking operation.
COST: Repeated park/drain cycles and a fleet audit to discover why the workers still looked idle over todo cards.
FIX: Record parking time in the PATCH transaction for explicit trigger writes, including reassertions and autofix diversions; preserve explicit timestamps/null. Fixed in af53a6bb (AMUX-4168). The PATCH/selector regression fails with timestamp zeroed; live AF-298, AG-39 and GCA-157 reassertions passed and emitted trigger_timestamp_repaired.

## Historical dependency prose prevents the worker from reconciling its own queue
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4168
SYMPTOM: Six idle workers had nine todo candidates refused by the prose dependency regex. MS-1253 was blocked by its own ID; MR-21's first historical blocker outranked its newer description of the remaining work. No reconciliation prompt reached these workers.
COST: Six live worker queues stayed unclaimable while their board cards still said todo; the user had to request another fleet investigation.
FIX: Keep structured dependencies authoritative and deliver ambiguous prose as a dependency-recheck prompt, with a named WARN verdict, instead of vetoing pickup. Fixed in af53a6bb (AMUX-4168). The nine-card/six-worker regression fails when the veto returns. Scheduled live deliveries reached all six workers; all nine cards were reconciled against current dependencies or handed onward.

## Stale-WIP recovery immediately assigns the same card again
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4168
SYMPTOM: BR-51 was reclaimed at 04:25 and picked again at 04:26, then reclaimed at 10:26 and picked again at 10:27. Selection ignores pickup.reclaimed_stale when applying its per-card cooldown, so recovery does not yield to the other queued work.
COST: Two six-hour recovery cycles left byo-ray holding the same WIP slot over eight/nine eligible todo candidates.
FIX: Apply the existing bounded per-card cooldown to the reclaim event too, and log stale_reclaim_yields_to_next_card. Fixed in af53a6bb (AMUX-4168). The regression fails without the reclaim event in the cooldown. The actual byo-ray snapshot selected BR-137 after reclaiming BR-51; its live current CI run remains in progress, so no live reclaim was forced.

## Worker message arrows did nothing on Codex output
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4178
SYMPTOM: Live amux terminal contained three Codex prompt lines but zero navigable message elements. Previous message did not move. Consecutive Claude prompts could nest, and terminal provenance was classified before worker-scoped history loaded.
COST: User could not navigate the conversation; reproduced a zero-target click on the live worker.
FIX: Recognize both prompt glyphs, balance ANSI and message blocks, classify from scoped history, land at message starts, and emit peek-message-nav beacons with landed/no-targets/target-not-visible verdicts. Desktop, mobile and WebKit: 9 passed; removing Codex detection fails the regression.

## A stale dependency cycle can hide a new task cycle
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4179
SYMPTOM: depends_on_cycle inspected only the first cycle anywhere on the board and ignored it if it did not contain the edited task. A second newly introduced cycle could therefore escape the write guard. Parent lineage had no cycle guard, and no periodic graph integrity check exposed malformed or dangling relationships.
COST: Plans could become structurally unbuildable without a reliable rejection or graph-wide diagnosis.
FIX: Edited-task reachability checks with real cycle witnesses, independent parent DAG validation, and a typed snapshot over existing primitives. board.graph_integrity runs periodically; dependency_cycle_rejected, lineage_cycle_rejected and task_graph_invalid identify failures. Removing the dependency guard makes the persisted-corruption API regression accept a cycle and fail.

## A refusal test read the live fleet's open sending policy
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4179
SYMPTOM: The complete server suite failed in the_cross_group_refusal_names_the_verb_that_works_without_an_existing_card because it expected a refusal while reading real worker configuration. Cross-group sending has been open by default since September 3.
COST: One false failure interrupted a 2,012-test library run before integration targets could execute.
FIX: Isolated AMUX_HOME with explicit opt-out and distinct groups, following the neighboring policy tests. The failure now names its explicit fixture if the gate unexpectedly opens; the focused test passes.

## Routine graph checks downloaded the complete 58 MB audit snapshot
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4179
SYMPTOM: The first live graph check read 57,937,366 bytes for 13,381 tasks because structural validation and full descriptions/evidence shared one export response.
COST: Every routine CLI check transferred and parsed 58 MB; the loopback export alone took 0.576 seconds.
FIX: Add /api/graph/board/verify using the same structural verifier as the periodic monitor; CLI summaries/checks use it, while --json retains the reproducible full audit export. Invalid checks emit task_graph_invalid with projection=validation; unmeasured reads emit task_graph_unmeasured. Regression compares both projections and proves a 2 MB task body cannot inflate the preflight response.

## Worker header stacked tiny message arrows and hid actions behind unexplained icons
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4188
SYMPTOM: Ethan circled the Human 0 tower, tab grid and directory link/edit icons on his phone. The navigation group measured 76px tall with 24px arrow targets; its counter secretly cycled seven message types.
COST: A second user report after the earlier scrolling fix; 30px of avoidable header height and small ambiguous tap targets remained on the primary phone surface.
FIX: One horizontal 44px-control toolbar with an explicit message-type select and on-demand Find. Search and message navigation share arrows/counters, including keyboard changes. Tabs is labeled; directory changes and copy links use named worker-menu entries. peek-toolbar-layout emits unusable-controls with measured/population/overflow/target sizes if this layout regresses. Browser checks cover desktop, 375px mobile and iOS WebKit; a negative control restores column stacking to prove the geometry check fails.

## Cached worker links opened before message state existed and never loaded scoped history
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4188
SYMPTOM: Live #peek=amux displayed Cannot access _pmSel before initialization. _peekMsgRows remained null: the initial cached-message render threw before _peekMessagesLoad reached its fetch.
COST: Live toolbar verification was blocked; direct worker links could keep Human at zero because the scoped history request never started.
FIX: Initial deep-link routing and screen restoration now run together after DOM readiness and full bundle initialization. Cached worker/history fixtures prove a real scoped request and human classification. Global action/script failures now emit client-action-error beacons as well as the existing toast. Live layout signals also exposed a zoom-coordinate false positive; geometry validation now compares layout sizes to layout thresholds and records rendered height separately.


## A completed worker-menu action left a listener that swallowed the next opening click
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4188
SYMPTOM: Physical mobile testing copied the directory link successfully, but the next Worker actions click immediately closed the menu. Its old document click listener survived because menu items stop propagation.
COST: Live verification of the new named directory actions failed; people needed an unexplained second click after using an action.
FIX: Closing the menu now retires both the pending dismissal timer and the document listener, including action and toggle paths. Browser regression tests repeatedly open Change directory, cancel and reopen the menu. An opening lost before paint emits worker-action-menu verdict=open-lost with measured and the menu-item population.

## Local multiplayer passed localhost while its Tailscale invite links were unusable
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: Codex local Tailscale multiplayer
CARD: ATE-79
SYMPTOM: The checked-in local multiplayer Playwright test passed in three browser
 projects, but it ran only on localhost. A real browser negotiated HTTP/2 on the
 Tailscale TLS listener, where authority and scheme live on the request URI rather
 than the HTTP/1.1 Host header, and Team generated an http://localhost invite link
 that no second tailnet node could open.
COST: The original green E2E result gave the wrong release verdict and about 45
 minutes went to repeating the flow at the real Tailscale origin before the protocol
 difference became visible.
FIX: ATE-79 reads authority and scheme from either HTTP version, pins the HTTP/2
 request shape in the route test, and logs `origin_fallback` when neither source is
 present.

## Local multiplayer called a member revoked before testing a reload
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: Codex local Tailscale multiplayer
CARD: ATE-79
SYMPTOM: The revocation E2E asserted that the already-open member page received 401
 after deletion and stopped. The HttpOnly cookie remained in the browser; reloading
 made its DB lookup fail, classified the request as an ordinary public shell, and
 injected the owner bearer into JavaScript. The member was therefore promoted to
 owner by the first recovery action a user would try.
COST: A revocable-invite feature carried a privilege-escalation path despite a green
 browser test, and the missing lifecycle edge added about an hour of independent
 cookie, reload, mutation, and request-log checks.
FIX: ATE-79 distinguishes absent, verified, and revoked member cookies before owner
 bootstrap, adds the reload assertion to all three Playwright projects, and emits
 `revoked_member_bootstrap_withheld` when the blocked transition is attempted.

## The PWA cache discarded a valid remote-owner bootstrap credential
AREA: auth
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: Codex local Tailscale multiplayer
CARD: ATE-79
SYMPTOM: A remote owner opened the dashboard with the correct `?_token`, but the
 service worker canonicalized every root navigation to cached `/` before the server
 could see the query. The UI loaded, reported Polling, and every Team mutation failed
 unauthorized, while direct HTTP with the same credential succeeded.
COST: Two fresh browser origins and two server rebuilds were needed to separate a
 server-auth failure from a cached-shell failure; without the browser check the new
 secure remote-owner boundary would have shipped with no usable recovery path.
FIX: ATE-79 exchanges the one-time URL bearer for a derived HttpOnly owner-session
cookie, removes it from the address bar, bypasses the canonical shell cache for the
exchange request, and pins the app/service-worker version seam.

## Message jumps mixed zoomed screen coordinates with unzoomed scroll offsets
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4192
SYMPTOM: Multi-worker live checks found 80% jumps hundreds or thousands of pixels from the selected message. At 100%, the fixed 12px landing put the first line beneath the terminal's floating controls. Search also matched serialized HTML, so entities and ANSI-split phrases could not be found, and a working worker's refresh could replace the selected search result.
COST: The preceding live check at one normal zoom did not cover these cases; Ethan requested cross-worker verification again. The baseline reproduced unreadable landings on amux-frustrations and mixpeek-general.
FIX: Convert rendered geometry to layout coordinates, reserve the actual floating-control inset, reveal nested horizontal table matches, and publish zoom/inset/scroll-error/visibility in navigation beacons. Search rendered text across inline spans as one result. Pin selected search/message nodes while newer output is buffered. Regression cases cover long/wrapped text, symbols, Unicode paths, ANSI spans, wide tables, start/end wrapping and real refreshes across desktop, 375px and WebKit.


## Saved Codex composer hints became false message-navigation targets
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4192
SYMPTOM: The six-worker sweep found four copies of Ask Codex to do anything in amux's saved output being treated as messages. The composer exclusion only applied at the very end of the entire log, so historical frames escaped it.
COST: Geometrically correct jumps still landed on terminal input hints instead of submitted messages.
FIX: Detect the adjacent model footer for each unclassified block, including historical frames; authoritative submitted-message history still wins. A browser test covers old composer frames followed by later output and the same literal text recorded as a real human message.


## Dependency completion released code at Done and returned to the child task
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4193
SYMPTOM: Code dependencies became runnable at Done before verification. The completion callback was linked to the producer child instead of the original requester task and lacked its next action. Claims and backlog promotion disagreed on missing and discarded dependencies.
COST: Ethan requested explicit end-to-end proof that verified dependency completion actually wakes and resumes the original worker. Existing unit fixtures encoded the earlier, weaker completion boundary.
FIX: Use the existing type-specific verification boundary for claims, promotion and durable callbacks. Derive the original return task from depends_on, retain graph edges, link the producer-origin message to the original task, include remaining blockers and next action, and record both task histories. Log dependency_waiting_for_verification, dependency_callback_linked and withheld or unmeasured delivery cases.

## A bottom-clamped message jump was mistaken for resuming live output
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4192
SYMPTOM: The deployed multi-worker sweep lost the selected message on amux-testing-e2e when a jump hit the bottom. The async scroll listener unlocked live updates even though refreshPeek itself preserved selected targets.
COST: One live navigation failure survived synchronous refresh tests; a working worker replaced the selected message after a geometrically correct jump.
FIX: The scroll listener now preserves the navigation lock for selected messages and search results at the finite scroll boundary. Regression tests wait for actual scroll events before refreshing and cover both trailing-output and end-of-output targets. Existing navigation beacons expose missing targets and scroll errors.

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
## A working Codex lane inherited its sibling's idle turn, and the alert blamed a discarded hook
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4220
SYMPTOM: mvs-research produced 26 retained status.agrees_with_pane failures from 04:59 to 05:23 EDT. Its actual Codex process held the research rollout open, but nearest-start association selected mvs-pitr's completed rollout for both workers. The failure evidence named a 30-hour-old stop-hook even though status-explain said report.applied=false and decided_by=codex_rollout.
COST: Roughly 20 minutes tracing the deciding signal through metadata, rollout boundaries, and kernel open-file ownership; the original alert omitted the evidence needed to distinguish a broken hook from a sibling transcript.
FIX: AMUX-4220 refuses ambiguous startup associations and missing explicit claims, excludes subagent candidates, logs resolution failures with filenames, and retains the actual derivation in invariant evidence. Existing codex_session_id metadata provides the durable recovery using kernel-proven ownership. Regression tests cover the two-worker startup collision, candidates beyond the previous scan cap, and the provider-boundary grace period. Full findings: docs/incidents/2026-09-08-codex-rollout-cross-attribution.md.

---
## A health timeout rebuilt the same revision and a later successful curl was logged as drift
AREA: runtime
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4225
SYMPTOM: Already-stamped a604412b rebuilt at 10:12:56 and self-adopted at
  10:16:06 while health, sessions and handoff calls stalled. The drift line
  printed the matching 12-character SHA, concealing earlier failed probes.
COST: One confirmed redundant release build took 3m01s; another entered 18GB
  cache cleanup. Missing measurement provenance sent diagnosis toward SHA
  abbreviation, which the original comparator already accepted.
FIX: One captured identity decision, unmeasured deferral, hash-checked install
  receipts, same-revision adoption suppression, bounded off-runtime health
  reads, and per-job slow-poll attribution. Native sample had no stacks; the
  original unannounced stop remains unproven. RCA:
  docs/incidents/2026-09-08-health-stalls-build-feedback.md.

---
## Every owner follow-up created another Doing claim and replaced the runtime's current card
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4228
SYMPTOM: During AMUX-4225 recovery, four delivered owner follow-ups each
  minted Doing and task.claimed. Runtime correctly reported five conflicting
  claims; asking to fix the conflict itself added another claim.
COST: Repeated manual reconciliation. Moving the extra cards to Todo was
  refused by the lane's existing WIP cap, so the requests were preserved in
  triggered Backlog; no unrelated work was closed to manufacture room.
FIX: Capture checks the active Doing population in the writer transaction.
  Follow-ups remain durable, already-delivered cards in triggered Backlog and
  emit task.captured, preserving the active task.claimed identity. Logs name
  capture_pending_active_claim, the existing card and population. Focused
  capture tests: 21 passed, including distinct prompt preservation, one active
  claim, a pending delivery receipt, and retry deduplication.

---
## Terminal Find discarded the message-type filter and the file menu consumed its own phone row
AREA: dashboard
SEVERITY: friction
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4229, AMUX-4227, AMUX-4230
SYMPTOM: Entering a search changed Human to a disabled Search results selector
  and searched all terminal output. The file breadcrumb's 180px flex basis
  wrapped the ellipsis onto an otherwise empty second row at phone width.
COST: Owner supplied two screenshots and had to request filtering and compact
  controls explicitly; changing message type could not narrow an active search.
FIX: Search retains an editable type filter and finds matches only inside that
  kind's message blocks. The file toolbar shrinks the breadcrumb on one row;
  the worker menu uses vertical dots. Client beacons retain the selected type,
  flag mismatched targets, and report file-toolbar wrapping/overflow. Browser
  proof: 12 passed across desktop, 375px Chromium and iPhone WebKit; screenshots
  reviewed. The browser proof used changed assets with an isolated pinned
  backend, so it required no fleet access or additional Rust compilation.

---
## Separate background tasks still held the HTTP runtime for seconds
AREA: scheduler
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4225
SYMPTOM: Newly shipped runtime_job_blocking_poll logs measured individual
  commit-mention-notes, board-drive, orchestrator-runtime and autofix polls
  holding Tokio workers for 3914ms, 2484ms, 1128ms and 1078ms. Each job had its
  own task but shared executor threads with health, TLS and worker API calls.
COST: Owner and TubeScience hit 20s coordination API timeouts; the earlier
  empty native stack sample could not name the synchronous work responsible.
FIX: Existing registered jobs use a separate process-owned maintenance runtime,
  including the two loops that previously spawned before registration. Boot
  and slow-poll logs name the runtime pool and full source/PID identity. The
  regression blocks maintenance while real health and board requests must
  return; host CPU/IO contention and the original unannounced stop remain
  separate, explicitly unproven parts of the historical incident.

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
## Status sorting mixed working and waiting workers and let idle pins lead
AREA: ui
SEVERITY: confuses
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4237
SYMPTOM: Ethan reported mixpeek-general positioned oddly and requested status ordering by default. The default was Recent activity, while the Status comparator was identical to it: working and waiting shared one priority and pins outranked status. Grouped rendering also discarded its sorted bucket when only one group remained.
COST: A worker's position did not reliably communicate its displayed state; recent traffic and pins could split the expected status order.
FIX: Default to Status and share the displayed status keys across ranking, grouping and frozen-order capture. Working, needs-input, API-error, rate-limited, idle and stopped remain distinct; pins and activity order within each state. A rendered-DOM worker-status-order beacon reports measured population and status-order-violation to the durable client-debug log. Browser regressions exercise mixpeek-general, status changes, single-group sorting, freeze, explicit preference preservation and a deliberately inverted DOM: 9 passed across desktop, mobile and iOS Safari.

---
## Publication gate mistook match patterns for failed sends
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4225
SYMPTOM: The exact published 765e9ea2 suite reported two unclassified HTTP 500 outcomes containing only doing. Both were (false, "doing") match-arm inputs in the recovered prompt attribution path, not returned messages. Separately, the guard-default test inherited AMUX_TASK_GUARD=1 from host settings or another test's config load; the same executable passed alone with a clean environment.
COST: Correct production behavior failed the publication gate, obscuring the distinction between a regression, ambient test state and an extractor error. The full run also reached an unrelated real-home filesystem probe that stayed blocked beyond its budget; its interruption and the integration-target continuation are recorded separately under ~/.amux/logs/amux-4225/.
FIX: Exclude tuple literals followed by a match arrow while retaining actual failure returns and the existing positive controls. Emit gate_probe with measured population, excluded match patterns and unclassified count. The guard-default fixture explicitly clears its two effective values in its private server.env so live process settings cannot override the defaults under test. No runtime refusal policy or fleet state changes.

---
## A bounded process snapshot failed a guard for adding useful fields
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4225
SYMPTOM: The published recovery build retained one bounded ps snapshot outside the fleet loop, extending its parent-only projection with PID, state and command fields. sessions_probes_are_bounded required the old exact argument string and called the new snapshot absent.
COST: A false missing-liveness-probe verdict obscured a passing bounded-process implementation during the final fleet preservation gate.
FIX: Verify the single external ps invocation, full-population flag, parent identity field and bounded call with its diagnostic identity; allow additional projected columns. The no-subprocess-inside-the-loop assertion remains. The gate emits bounded_fleet_process_snapshot with its measured population and actual arguments.

---
## A local invite proved identity but granted the owner’s entire API surface
AREA: auth
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: Codex local Tailscale multiplayer
CARD: ATE-79
SYMPTOM: Every accepted local/Tailscale invite became a verified member, but the
 member row carried no resource boundary and auth returned immediately after the
 cookie check. An invite intended for one worker could read and mutate the whole
 fleet, board, settings, and membership API.
COST: Local multiplayer could not be shared safely with an external collaborator;
 the only authorization choices were full owner-equivalent data access or no access.
FIX: Persist global/group/worker scope on invites and members, resolve it on every
cookie-authenticated request, filter fleet and board reads, refuse cross-scope
worker/card access, keep org administration owner-only, and make rescoping take
effect on the existing cookie. The browser E2E now transitions one live member
global → group → worker and proves both permitted work and cross-scope 403s.
Member-authored cards, edits, worker creation, sends, and request-log rows derive
their author from that verified cookie, so client-supplied creator or worker
headers cannot rewrite multiplayer history.

## Dashboard calls repeated authorization refusals Connecting and loses its own diagnostics
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux
CARD: AMUX-4246
SYMPTOM: Owner screenshot shows Connecting/Polling with no workers. The last-hour request log has 777 worker-list 401s and 38 rejected client-debug reports from a remote client. The old error body cannot distinguish missing credentials from an invalid bearer or member cookie; exact screenshot origin remains unconfirmed.
COST: A blank owner dashboard and roughly 30 minutes tracing a responding server before finding the client ignored authorization error objects.
FIX: AMUX-4246 adds an explicit access/failure state, a bounded fresh-bootstrap recovery under existing auth rules, structured refusal reasons in request logs, and a deferred browser diagnostic after access recovers. Historical credential presence cannot be reconstructed.

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
## Migration cost guard called a sibling write an unindexed read
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-09
SESSION: amux
CARD: ATE-79
SYMPTOM: The full server library gate failed four times on 0060_org_teams, claiming
  its post-backfill team_id indexes appeared after statements that read org_members
  or org_invites. The cited statements were UPDATEs of the opposite sibling table;
  neither statement read the indexed table at all.
COST: One false red in a 2,159-test, 292-second run, and the first apparent remedy
  was to move write-side indexes before their backfills—the opposite of the cost
  rule documented by the test itself.
FIX: The ordering guard now verifies a prior DML actually names the indexed table
  after FROM/JOIN and uses the index's leading column, while continuing to exclude
  indexes on the table being written. A planted 0031-shaped late read-side index
  still fails and alternating sibling writes are the positive control.

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
## A pool outage erased pending browser writes while the editor reported Saved
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-09
SESSION: amux-testing-e2e
CARD: AF-640
SYMPTOM: During the ENOSPC/pool incident, board PATCHes failed with timed out
  waiting for connection. The editor still displayed Saved and discarded its
  draft. Sync cleared localStorage before replay, then could stay at 0/1 on an
  unbounded request; reloading lost that in-flight intent. An SSE connection
  could keep the header Live while board reads/writes failed. A cold card open
  left the previous card's controls saveable, and saves omitted expect_rev.
COST: Gate/status edits disappeared after reload and the interface claimed
  durable success while the store was unavailable. The user had to diagnose
  the discrepancy in the live browser and request coordinated recovery.
FIX: b724cdff retains each write until exact-card acknowledgment, bounds
  requests, preserves failed/newer drafts, serializes replay across tabs, and
  pins editor identity/generation/revision. 30/30 outage browser cases passed
  in the final fixture run, including server/device ENOSPC and conflicts.
  Live on 69490b05/build 9f259f186724b394: an AF-640 note showed Not saved,
  retained queue id b68ae07c-c4e3-4b93-bfdb-68ee4062bb5b across reload, then
  acknowledged 1 synced; the durable card contains the note exactly once.
  Server read failure showed Sync error with SSE connected, then recovered
  to Live. This validates the browser contract; AF-640 server recovery remains
  open and the stale-blocker Rust patch is still paused.


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
## Cross-tab delivery coverage reached an uncounted context route
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-09
SESSION: amux-testing-e2e
CARD: AF-640
SYMPTOM: Exact-commit CI for 69490b05 failed e2e_route_stub_guard: the real
  cross-tab delivery regression used context.route, while the fixture counted
  only the default page's routes. The behavior test passed, but an unused
  context stub still had no named failure signal. Additional context.newPage
  pages likewise bypassed the default-page wrapper.
COST: A passing browser suite left a gap in whether its response stubs ran,
  and the Rust import census correctly refused that coverage claim.
FIX: Keep the census's context-route prohibition and register the shared
  interception handler on both known pages. Wrap every page the context
  creates, so second-tab stubs receive the same hit counter and named failure
  as the default page. The expected absence of a second delivery is explicit
  at its allowUnusedRoute call; the shared write counter still rejects two
  deliveries. Matched and dead-stub controls exercise context.newPage through
  the real fixture. Candidate asset plumbing still applies to the context.
  Verification: route-stub-guard.spec.ts + outage-recovery.spec.ts across
  desktop/mobile Chromium and iOS Safari -> 45 passed, 0 unexpected/skipped.
  JSON receipts confirm every new-page negative control failed by its exact
  dead-new-page-probe matcher; matched controls and cross-tab delivery pass.


---
## Broad terminal and offline checks retained obsolete UI and response contracts
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-09
SESSION: amux-testing-e2e
CARD: AF-640
SYMPTOM: Full browser CI still required a 40px navigation inset for controls
  that now occupy a sibling row, and required a pending-count pill while a
  failed read correctly had priority as Sync error. A pre-existing cold-worker
  fixture returned peek data without its required worker name, so the identity
  guard rejected it; the same test still selected a retired filter combobox.
COST: Valid new terminal geometry and honest outage state read as regressions,
  while the old anonymous peek specimen never exercised cold-start rendering.
FIX: Assert that the message lands near the scroller start and below the actual
  controls. Keep the exact three-operation banner and durable replay checks
  while accepting Sync error in the pill. Supply the real worker identity and
  exercise the current source-filter dialog. The existing identity-discard
  beacon and named geometry/state assertions expose the next contract drift.
  terminal-message-navigation + worker-toolbar-boot + golden -> 57/57 passed
  across desktop/mobile Chromium and iOS Safari. With unchanged worker-config
  controls included the run was 59/60; its mobile snapshot poll exceeded 5s
  after the config write was logged successful. Exact fresh-home rerun ->
  1 passed (20.9s), no source change. This is not a green full-browser-CI claim.

## An unresolved merge was published as the fleet's Bash CLI
AREA: cli
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-09
SESSION: amux-testing-e2e
CARD: ATE-136
SYMPTOM: mixpeek-general measured conflict markers in ~/.local/bin/amux at lines 1527/1535/1563; every subcommand failed parsing. The shared checkout was mid-merge. install.sh installed only the Rust binaries, leaving no supported Bash publication path, while the freshness hook prescribed a worktree symlink. The exact manual copier is not established; this lane did not start or change the shared merge.
COST: Fleet-wide CLI outage requiring a peer to restore the committed origin/main script; the installer recommendation initially pointed at a path that did not exist. Incident evidence is retained on MG-1716.
FIX: make install-cli and install.sh now use one publisher: snapshot beside the destination, reject unmerged source/conflict markers/invalid Bash, then atomic rename of those validated bytes. Refusals and publication failures preserve the installed client and emit stage/reason to stderr and logs/cli-install.log. The grid helper is included and validated before either file is published, so replacing a symlink preserves that command. Freshness guidance uses the guarded publisher. Sixteen temporary-fixture tests cover ENOSPC, publication failure, open readers, source races, concurrent installs and installed grid dispatch; syntax and in-place-copy mutations fail named tests. This retires the publication mechanism only; resolving the separate shared merge remains its owner's work.

## TubeScience earlier output rendered terminal redraw fragments as conversation
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-adhoc-fixes
CARD: AMUX-4352
SYMPTOM: Ethan's screenshot showed rows such as `+ e n 4` and `* d i`, followed by individual spinner glyphs. The same missing-letter text and redraw fragments exist in tubescience.log; the worker's 220x50 live pane and structured transcript are readable. Load earlier output used the lossy pipe-pane log as if it were conversation history.
COST: Ethan could not read the TubeScience worker's earlier output and had to report a screenshot for diagnosis.
FIX: Claude earlier output now pages complete structured transcript records using absolute byte cursors and conversation identity. It replaces the initial overlapping history tail and preserves new live output. Live verification exposed different blank-line normalization between peek and record pages; overlap matching now mirrors peek normalization, verified against 119,599 characters of real TubeScience output. Raw log downloads remain available. The server emits conversation_history_page with source, record count, bytes and remaining cursor, and warns on unavailable/read-failed conversation history.

## TubeScience unsent paste fragments appeared as delivered chat
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-adhoc-fixes
CARD: AMUX-4359
SYMPTOM: The current Claude composer held adjacent [Pasted text] placeholders and AMUX-INJECT-END tails, and the dashboard classified that unsent input as an Unclassified chat message. CLI transport retries also lacked msg_id and fell back to direct terminal injection after an ambiguous timeout.
COST: Ethan had to report another screenshot because transport debris still occupied the chat after the history fix.
FIX: The live renderer separates the framed worker input into a collapsed, inspectable draft excluded from message navigation; raw input remains intact. CLI retries share one msg_id and no longer paste into a terminal after missing acknowledgments. send_delivery_unknown is recorded locally in logs/send-failures.jsonl; the dashboard emits composer_excluded_from_messages when separating the input.

## Queued-work badge names a blocker but gives no way to manage the queue
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex (Ethan report)
CARD: AMUX-4361
SYMPTOM: mixpeek-general showed MG-1295 +5 queued behind MG-1743; the badge opened only the first holding card, hid the other queued cards, and gave no loading feedback. The existing browser test checked only a changed hash, not usable task controls.
COST: Ethan could see six waiting tasks but could not act on the queue from the header.
FIX: v0.9.868 opens a measured queue inspector with every current and ready card, direct access to existing status/worker editing, retryable load failures, and worker-queue loaded/load-failed/open-task beacons. Task queue is also available in Worker actions while the worker is active. Browser regression opens the real card editor and preserves the worker draft on return.

## Low-context automation piles compact reminders into the worker input queue
AREA: notices
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4366
SYMPTOM: byo-ray showed repeated /compact plus prose at 12%, 11%, 7%, and 4% remaining while its native input queue grew. The dashboard toggle wrote auto_compact_enabled, but Rust never read it. Queuing a reminder emitted session.auto_compact without evidence of compaction.
COST: Owner had to intervene over context maintenance that Claude Code already performs automatically; repeated reminders consumed input and obscured the task.
FIX: Delegate to Claude Code's native compaction, remove duplicate steering and the ineffective toggle, and record measured session.context_low events without claiming completed compaction. Endpoint regression covers falling readings and recovery without adding a message.

## Subagents have a toolbar list but no terminal navigation
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4368
SYMPTOM: The terminal lost its agent arrows; the replacement toolbar button only displayed descriptions, without any way to read a selected subagent's output.
COST: Owner could not navigate subagent work where they were reading the terminal and had to request the controls again.
FIX: Two arrows beside Copy cycle current main/subagent output, preserving the main draft and view. Read owned structured child transcripts, never inject navigation keys. Missing outputs and list failures announce themselves through subagent-navigation diagnostics. Desktop/phone navigation, no-child, failure, and parent restoration tests. Live verification also caught a parent loading indicator surviving a quick child switch; clear it on selection and cover that race.

## Sending cleared a draft before Amux confirmed delivery
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The worker composer cleared text and attachments before /send returned; permanent refusal lost the visible retry draft. A second send could race the pending request.
COST: The lifecycle failure-path test reproduced loss of the only visible message draft.
FIX: 2ba4ec5b retains text and files until server or durable outbox acceptance, disables duplicate sends, preserves concurrent new drafts, and emits composer-delivery/unconfirmed with draft_retained to the local client-debug log.

## A dedicated CC_HOME still routed Bash commands to production
AREA: cli
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The Sonnet lab workers used their dedicated CC_HOME but the Bash CLI resolved the production endpoint.json and its legacy verbs defaulted to port 8824.
COST: The live pair queried the wrong board and required an explicit AMUX_API correction before coordination could continue.
FIX: 2ba4ec5b resolves endpoint.json from CC_HOME and initializes the API base for every legacy verb; the URL diagnostic names the endpoint file. Eight shell routing tests passed.

## An idle hook sent Escape into an active Claude tool turn
AREA: scheduler
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The live Sonnet pair showed interrupted tools when an idle-hook callback trusted the hook over a fresh active terminal frame and sent a leading Escape.
COST: The real peer-review cycle needed recovery after callbacks interrupted work.
FIX: 2ba4ec5b classifies the fresh frame after the send lock: active frames paste without Escape and live selectors wait. Queue admission still trusts the hook so background agents cannot strand delivery. Six steer tests passed; local verdicts idle_hook_live_activity_paste and idle_hook_live_selector_wait identify the paths.

## Freeze reordered pinned workers instead of freezing the displayed list
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The status-order test pinned a worker and then Freeze moved it into a computed status bucket instead of preserving the order on screen.
COST: The broad visual test exposed a visible jump when enabling Freeze.
FIX: 2ba4ec5b snapshots rendered card order, appends remaining workers, and emits worker-freeze-order/captured-rendered-order locally. The nine ordering tests passed on all three browser targets.

## Files menu Download failed after a successful upload and rename
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: An actual Files-tab upload/preview/rename succeeded, but Download was canceled. Its direct anchor used the JSON preview endpoint and omitted bearer authentication.
COST: The new exact-byte roundtrip test could not retrieve the file that the UI had successfully uploaded.
FIX: The menu now shares the authenticated raw-byte download helper with the preview toolbar. Failures emit file-download/failed locally. Upload, preview, rename, exact-byte download and deletion passed on desktop Chromium, mobile Chromium and WebKit.

## A just-created worker accepted keystrokes in its launch shell
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: A real Sonnet worker was still executing its launch shell when Send pasted a long prompt. The send later reported not_submitted. The test had mistaken --model sonnet in the launch command for the ready provider banner.
COST: The fresh two-worker acceptance run could not begin its review cycle; the retained draft made recovery possible.
FIX: During the startup window, send waits for a positive provider UI frame before typing and refuses without typing if readiness times out. Local verdicts send_waiting_for_boot_ui and send_boot_ui_not_ready expose both outcomes. The live suite requires the actual Sonnet version/footer, and pair/upload are serial so a failed pair cannot reset the author for upload.

## Settings promised a Notes folder that never loaded
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The settings suite carried a skipped test for a Notes folder section that displayed an ellipsis forever; no client code populated it and no backing endpoint existed.
COST: The consolidated mobile audit could not verify the advertised feature and reported skipped coverage.
FIX: Removed the unsupported section and replaced the skip with an assertion that the misleading UI is absent. Settings now run at mobile width on its separate server; failures and screenshots are retained in the lifecycle report. There is no remaining Notes-sync action to emit a runtime event.

## The route catalog advertised a nonexistent host-metrics endpoint
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The route-table integration test received 404 for OPTIONS /api/metrics/host even though the catalog advertised GET. At the tested revision, the real metrics router implemented /api/metrics, /fleet and /replay, while the host panel was not yet exposed.
COST: The merged lifecycle verification failed its route-table consistency gate.
FIX: Initially removed the unsupported catalog entry. Incoming main commit 1b22fd21 subsequently implemented /host and exposed its UI, so the integrated change preserves that implementation and restores its catalog entry. The bidirectional route-table check verifies agreement; the consolidated browser suite now opens Host, checks a real measured response, refreshes, and follows Disk Cleanup and System navigation. Host measurement failures retain the existing measured=false diagnostic.


## Find did not land when its first match arrived with terminal history
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The live Sonnet screenshot showed an unrelated earlier message while Find displayed CROSS_ACK and a match count. The query had been entered before the full history response; only Locate armed a deferred jump. A DOM-visible assertion did not prove the match was inside the scrolling terminal.
COST: A user could find a message in the count while still being shown unrelated output.
FIX: Typed Find now arms the same one-shot jump when its current frame has no match; arriving history lands it and emits peek-message-nav/deferred-search-landed with measured target geometry. Existing selected-result buffering and a reader's deliberate scroll remain intact. The regression covers late history and scrolling away; live checks now require the selected match to be in the viewport.

## A saved board title clipped after switching from desktop to phone width
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The phone screenshot of a completed queue task showed only its shared run prefix; the distinguishing task suffix was hidden. The readonly textarea kept its desktop height after its width changed.
COST: Different completed tasks appeared to have the same title on a resized or rotated display.
FIX: Observe title width and recalculate its content height in the next animation frame without editing the text or causing a ResizeObserver loop. The local board-detail-layout/title-resized-after-wrap event names measured corrections. The real board-create/read/export scenario now checks the complete title after desktop-to-phone resizing.


## The extra terminal action crowded the phone filter caption
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: After the new Subagents action arrived on main, the phone toolbar squeezed the word Filters into the match count. All outer buttons were still large enough, so the old geometry check missed the overlapping inner caption.
COST: The live peer-message screenshot had an unreadable filter caption despite a passing outer-toolbar check.
FIX: Phone layouts use the filter icon and count, retaining the full accessible name and selected-source description. The toolbar diagnostic now reports clipped_filter_caption, and the viewport regression checks the inner label as well as button bounds.


## The Subagents dialog opened behind the terminal
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The newly added toolbar button activated a same-z-index overlay placed before Peek in the DOM; its data loaded but Peek covered its contents and close button. The overlay-card/head classes also had no styling.
COST: The new action could not be inspected or dismissed through its own controls while the terminal remained open.
FIX: The incoming main change replaced this dialog with read-only terminal arrows, removing the covered overlay entirely. Preserve that replacement and verify the old dialog and launch button remain absent; the consolidated suite includes subagent-arrows.spec.ts for output, retry, navigation and parent restoration. Existing subagent-navigation diagnostics expose failures; no dialog runtime path remains to diagnose.


## Host status chips were unreadable in light mode
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The new Host panel passed navigation tests, but the actual phone and desktop screenshots showed near-black CPU/Memory/Disk labels on a hard-coded dark chip background in light mode. The status text also used bright dark-theme colors.
COST: A user could see colored dots and values without being able to read which host dimension they described.
FIX: v0.9.874 uses the existing card/text and semantic status theme colors. The real Host lifecycle toggles both themes through Settings and checks computed label/status contrast against 4.5:1. Local host-analysis-contrast diagnostics report readable/low-contrast, actual foreground/background colors, ratios, theme and six considered samples after rendering.

## Claude opens a diff sidebar in the worker terminal
AREA: terminal
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4372
SYMPTOM: mixpeek-general displayed Claude's diff sidebar and /diff to hide diff, compressing the conversation and showing empty diff panels.
COST: Owner had to request removal of a terminal layout they never want.
FIX: Close observed active sidebars once (89 panes checked, zero left open; native close controls preserve drafts), seed Claude's native diffSidebarOpen=false on worker launches (including already-trusted folders), and remove /diff from amux command suggestions. Log preference persistence success/failure; regression tests cover preference reset, idempotence, unrelated config preservation, and slash-command discovery.


## The send-retry regression existed without a CI invocation
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The published lifecycle audit failed GitHub checks because test-send-retry-identity.py, already on main, was not invoked by any workflow or hook.
COST: Stable retry identity and refusal-without-terminal-injection had a regression file but no continuous coverage, and the required harness-wiring ratchet blocked checks.
FIX: Invoke the real Python harness in checks.yml next to the existing send-retry tests. Its outage/recovery/refusal cases and the harness-wiring ratchet pass locally. The existing test-harness-wired.sh diagnostic names any future unwired harness in CI and locally; no product runtime path is involved.

## Image attachments stay at zero percent
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4374
SYMPTOM: Two image.png chips stayed at 0%; upload fetch and response-body reads had no deadline. Live start requests took 24.6s and 30.8s despite ultimately returning200. A hung request could hold one of four slots forever. Queued uploads also captured the global peek array, so switching workers before their start could misroute attachments.
COST: Owner could not reliably attach screenshots or send the blocked draft.
FIX: Bound each upload phase, retry transient failures up to three attempts with fresh upload IDs, and retain failed chips with an explicit Retry button. Show queued/starting/uploading/finishing/retrying states. Create placeholders immediately and capture the originating attachment array; retries share the concurrency limit. Record phase, attempt, bytes, HTTP status and outcome in attachment-upload diagnostics. Desktop/phone regression tests cover stalled fetches and bodies, retries, server restarts, cancellation and worker switching.


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

## Startup message refusal inherited HTTP 500
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: Hosted refusal census found one unclassified send failure: a worker still starting correctly declined typing but appeared as a server crash.
COST: Failed hosted Rust check after the lifecycle push.
FIX: Classify the startup refusal as 409 with a wait/retry next step. The literal census and state-refusal regression diagnose future classification drift.

## A populated torrent panel threw after the upload status change
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: SPA lint caught undefined f in the torrent progress renderer, copied from attachment status rendering. Empty torrent fixtures had not reached it.
COST: Blocked follow-up SPA check; populated downloads would disappear behind a console error.
FIX: Restore torrent percentage rendering and bump app/service-worker versions together. LC-TORRENT verifies populated progress and pause/resume/remove on all three viewports; the existing no-undef gate identifies the offending binding.

## Parallel legacy discovery test discarded the retryable response body
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: Hosted legacy sessions test returned 500 during parallel roster mutations and only printed the status, omitting its diagnostic body.
COST: Failed hosted Rust check requiring a separate investigation.
FIX: Preserve the body in the status assertion and retry at most four times only for the exact documented discovery-revision invalidation error. All other errors still fail immediately.


## Torrent control names exposed symbols instead of actions
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The populated-panel browser check found buttons named only pause/stop glyphs; title tooltips did not supply an accessible action name.
COST: Three viewport failures after progress rendering was repaired.
FIX: Explicit aria-labels name Pause, Resume, Remove and Stop & remove. LC-TORRENT uses accessible action names and verifies each resulting state, so missing labels cannot silently regress.

## Two-image roundtrip test downloaded without its authenticated context
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: Both UI uploads finished, but the new direct download assertion returned 401 in all three browsers; its APIRequestContext omitted the dashboard bearer token.
COST: Three failed checks in a 45-case upload/composer run; earlier progress comments overstated that case before the final summary.
FIX: Use the existing auth helper for downloads and assert the actual status with the requested URL. Keep the real byte equality check and preserve the original failed report.

## Provider usage disappears during account throttling
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4375
SYMPTOM: Settings showed Claude Unavailable after HTTP429 despite an active subscription. The last successful report existed only in a short-lived route cache; deployments erased it. Routing and reserve consumers bypassed that cache and made competing probes. The whole-envelope stale fallback also rewound healthy Codex/Gemini rows.
COST: Owner lost visibility into remaining plan capacity and had to reopen Settings to discover recovery.
FIX: Share one in-flight Claude probe across server consumers, respect Retry-After with bounded exponential backoff, and atomically persist credential-scoped usage snapshots and cooldowns. Show historical readings with their observation time and scheduled refresh; never route work from stale percentages. Preserve other providers' current rows. Settings refreshes while open and retains readings through network interruptions. Log fresh, last_known, retry_scheduled and cache_write_failed outcomes. Tests cover concurrent consumers, restart/backoff/account isolation, stale routing exclusion, provider independence, and desktop/phone recovery.


## Torrent controls were too small to tap on a phone
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: Opening the passing populated torrent screenshot showed tiny adjacent pause and stop glyphs with correspondingly small targets at 375px.
COST: A visual review found friction that successful click assertions missed.
FIX: Give every torrent action a bordered 44px target and readable theme text. LC-TORRENT records measured dimensions and fails below 44px while retaining viewport-fit and effect checks.

## An in-flight send reservation is reported as an accepted retry
AREA: messages
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: send_dedup inserted an identity before attempting delivery, then treated any duplicate row as proof of delivery. Steering reserved before validation and did not release an archived-target refusal. A retry could therefore receive ok/deduped while nothing had been accepted.
COST: The local-outbox acceptance audit found a server receipt that could remove pending user intent prematurely; isolating reservation versus acceptance required a separate persistence and routing regression pass.
FIX: Store the confirmed response ID separately from the reservation. Pending attempts cannot acknowledge delivery, legacy unknown rows remain uncertain, failed identity storage refuses an untracked send, and validation refusals reserve nothing. Record the original queue/send ID only after actual acceptance. New message_acceptance tests and the real steering route test validate the distinction; amux::message_acceptance logs unconfirmed receipt states.
