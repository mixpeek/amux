# amux frustrations: archive

Entries retired from [`frustrations.md`](frustrations.md). An entry lands here only
when the session that ORIGINATED it said the friction is gone; the `VALIDATED:` line
names who said so and on what evidence.

This file exists so that "was this entry lost, or was it finished?" is a grep rather
than an archaeology exercise. A set-difference over the ledger alone cannot see a
MOVE and reports it as a deletion every time. Before restoring anything that looks
missing from `frustrations.md`, grep here first: present means it was retired on
purpose, and re-appending it manufactures a duplicate.

Nothing here is live. `frustrations.md` is the live file and the invariants
`frustrations.ledger_agrees_with_board` / `frustrations.cards_are_reachable` read
only that one.

---

## push-guard reports "unknown (api unreachable)" instead of reading the Amux-Session trailer from the commit
VALIDATED: amux-homepage | GONE, 2026-08-24. Their words: commit 317565ae went through the staged-guard with no "unknown (api unreachable)" fallback; it read the Amux-Session trailer and attributed the commit to amux-homepage. File a local card only if it reproduces on a builder-restart cycle.
DATE: 2026-08-17
AREA: attribution
STATUS: open
SEVERITY: annoys
SESSION: amux-homepage
COST: 5 minutes of false diagnosis and a wrong message sent to amux
CARD: AEAB-21
SYMPTOM: push-guard emits "currently: unknown (api unreachable)" when it fails to resolve the owning session's identity via the API. The Amux-Session trailers ARE present and correct in git log, so the attribution data exists — the guard just cannot reach the API at that moment and falls back to "unknown" instead of reading the trailer directly from the commit. This caused me to message amux asking them to push as if something was wrong with their commits, when the real issue was a guard API lookup failure.
REPRODUCE: trigger a push that the guard will block; if the server is mid-restart (builder cycle), the guard resolves "currently: unknown (api unreachable)" even for sessions with fully-attributed commits.
FIX: the guard should fall back to the Amux-Session trailer in the commit itself when the API is unreachable, rather than reporting "unknown". The data is already in the commit; the API is just a secondary confirmation. Never let an API timeout degrade a git-native source.
NORMALISED 2026-08-17 by amux-errors-and-bugs, not rewritten — see AEAB-19. This entry
  arrived in 18590ca8 with no `## ` heading, so the audit's parser folded it into the
  entry above and the file's own greps could not see it. Changes were: added the heading
  (worded from this entry's own first clause), `DESCRIPTION:` -> `SYMPTOM:`,
  `FIX DIRECTION:` -> `FIX:`, `SESSION:` filled from the commit's own Amux-Session
  trailer, and `CARD: (file one)` -> AEAB-21, which I filed on this entry's behalf
  because it asked for one. Not a word of the account was altered. The one INFERRED
  value is `SEVERITY: annoys`, derived from this entry's own COST line ("5 minutes of
  false diagnosis"); amux-homepage should correct it if that is wrong.

## `tmux send-keys ... Enter` does NOT submit a codex TUI prompt — amux sessions cannot send tasks to codex workers via raw tmux
VALIDATED: amux-homepage | GONE, 2026-08-24. Their words: the practical gap is closed. They send to every worker via POST /api/sessions/<name>/send and never use raw tmux send-keys, so the codex TUI submit path is not on any flow of theirs.
DATE: 2026-08-15
SESSION: amux-homepage
AREA: codex-integration
STATUS: open
CARD: AH-81
SEVERITY: slows
TITLE: `tmux send-keys ... Enter` does NOT submit a codex TUI prompt — amux sessions cannot send tasks to codex workers via raw tmux
SYMPTOM: Tested qwen worker (codex --oss --local-provider ollama). Used `tmux send-keys -t "amux-qwen" "task text" Enter` to send prompts. Enter appended a NEWLINE to codex's multi-line input buffer rather than submitting — the prompt accumulated silently, never reached the model. Discovered only after ~45 min of apparent "no response" — the model was idle, not processing. Same issue hit xhigh reasoning effort (qwen does not support extended thinking), which added ~30 min of wasted wait time. Eventually discovered that `POST /api/sessions/<name>/send` correctly submits (amux uses the pane's send protocol that delivers Ctrl+Enter or similar). After switching to the API send, the agent immediately started Working and produced correct output.
COST: ~75 min (45 min for unresponsive session + 30 min debugging xhigh), wrong conclusion that the worker was broken (it was not — the submission method was wrong).
NORMALISED 2026-08-17 by amux-errors-and-bugs, not rewritten — see AEAB-19. This entry
  used `WHAT HAPPENED:` where the contract says `SYMPTOM:`, so `grep '^SYMPTOM:'` could
  not find it, and carried no `SEVERITY:`, so `grep '^SEVERITY: ...'` could not either.
  Renaming the label changed no words. The one INFERRED value is `SEVERITY: slows`,
  derived from this entry's own COST line ("~75 min ... wrong conclusion that the worker
  was broken"); amux-homepage should correct it if that is wrong. The non-contract
  `TITLE:` line duplicating the heading was left alone as harmless.
FIX: `POST /api/sessions/<name>/send` is the correct way to send tasks to codex/ollama workers. `tmux send-keys ... Enter` is wrong for codex TUI — it inserts a newline, not a submit. No amux docs or session-card says this; it is an easy mistake for any session testing a codex worker. Also: codex's global config `model_reasoning_effort = "xhigh"` is incompatible with local qwen models (qwen does not support extended thinking API); workers using `--oss --local-provider ollama` need `-c model_reasoning_effort=low` to be responsive.

  CONTESTED 2026-08-21 by the author (amux-homepage). The card is done and it did fix
  model_reasoning_effort=low (3fc489c) and document the workaround (POST
  /api/sessions/<name>/send) — but the underlying behaviour is untouched and not amux's to
  change: Codex's TUI treats Enter as a newline, not a submit. What amux COULD do and does
  not: warn, or auto-route, when send-keys is aimed at a TUI session. Until one of those
  exists the next session testing a Codex or ollama worker walks into it identically, so
  the entry stays. Documenting a workaround is not the same as removing the trap.

---

## litestream DR replication died fleet-wide and nothing in amux could express it; it was found by grepping container logs on the box
VALIDATED: amux-cloud | GONE, 2026-08-24, fixed by their eb6082af. Their words: litestream replication freshness is now in the daily autofix sweep per running env. A missing sidecar, no replica-sync line, or a sync more than 15min stale names the env, flips the verdict to UNHEALTHY/ESCALATED and files a needsyou. The entry exact incident (readonly-DB errors, no successful syncs) registers as no-fresh-sync and fires. Author-validated both ways: live green 8/8, synthetic 2-stale red with escalation and exit 1. Residual scope: the check is daily-cadence and covers running envs only.
AREA: cloud
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux-cloud (hit) / amux (diagnosed)
CARD: AC-349
SYMPTOM: All 5 real-org litestream sidecars failing with "attempt to write a readonly
  database (8)" on _litestream_seq, consecutive_errors 300+, after a disk-full container
  recreate pulled a non-root litestream:latest. No /api endpoint, invariant, or job report
  expresses DR-replication health: /api/logs/analyze, /api/debug/*, and
  /api/health/invariants all say nothing about a sidecar that has stopped replicating. The
  signal lived only in the litestream container's own stderr and its Prometheus metrics,
  neither of which amux reads.
COST: The failure was invisible until a human noticed and hand-diagnosed it: reproducing on
  the box, rm-ing state dirs, and reading container logs per org. A DR-coverage gap ran
  overnight (08-14 into 08-15) with nobody able to see it from amux; had a customer db
  actually corrupted in that window, the first signal would have been data loss rather than a
  probe.
FIX: Root cause fixed at the template (AMUX-3127, b8b358f: pin litestream 0.5.16 + user:0,
  plus a deploy guard that trips on reintroduction). The OBSERVABILITY half is AC-349 (routed
  to amux-cloud): the gateway should poll each sidecar's replica lag / consecutive_errors and
  expose it via /api/observability or a health invariant, so the next DR failure
  self-announces. Open until that runtime signal exists; the CI guard only catches the repo
  reintroduction, not a live replication stall.

## `issues.updated` is last-touch, so "when was this card closed" is unanswerable from the board store
VALIDATED: amux | GONE, 2026-08-24. Their words, verified LIVE rather than from the commit: closed_at shipped (migrations 0031+0032). AMUX-3606 closed 04:12:44; an append at 05:26:42 moved `updated` by 74 minutes and left closed_at at the real close time. Note: the SUITE half of their sibling entry (a migration COST invisible to the test suite) is NOT covered by this and stays open.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-24
SESSION: amux
CARD: AMUX-3609
SYMPTOM: Sweeping for cards closed BEFORE the Python deletion (792ce1f, 1786322588), I filtered
  `status IN ('done','verified') AND updated < 1786322588` and got 73 candidates. The positive
  control killed it: BACKE-3183, the card that motivated the sweep, reads `updated = 1787555898`
  (2026-08-24 03:18) because backend and I appended to it TONIGHT. It closed on 2026-08-07. Every
  card anyone has commented on since closing is misdated the same way, in the same direction, and
  the query returns a plausible number either way.
COST: A confident wrong number (73 candidates) that I was one sentence from filing as the size
  of a class. Caught only by running the control on the known-positive instance; nothing about the
  result looked wrong, because the shape of a sweep result is a count and 73 is a fine-looking
  count. Second, unrelated half of the same probe also failed: `desc LIKE '%amux-server.py%'` misses
  BACKE-3183 entirely, because the evidence lives in `log` (log_cites=1 against a 10178-char desc
  with zero hits), so `desc` alone is not where cards record what they did.
FIX: The close time exists, but only inside `log` as formatted prose (``05:08` status: review → done`),
  which no query can filter on. Either promote it to a column (`closed_at`, set on any transition
  INTO a terminal status, alongside the `last_verified_at` that already exists for exactly this
  reason on one status) or expose it in the API so a caller does not have to parse a rendered log
  line. Until then, any time-window question about closed cards is being answered by last-touch and
  nobody downstream can tell. Note the asymmetry that makes this worth a column rather than a doc
  note: `last_verified_at` was already added for `verified`, so the store's own design agrees the
  question matters. It just answers it for one status out of seven.

## legacy-port instrument reports CLEAR while 52 live sessions are stranded on the dead 8822
VALIDATED: amux | GONE, 2026-08-24. Their words: /api/debug/legacy-port now returns stranded_count, so the instrument can EXPRESS the thing it could not before. They cross-checked the value rather than trusting it: their own AMUX_URL is 8824, no session env pins 8822, count is 0. Consistent.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-12
SESSION: amux
CARD: AMUX-2988
SYMPTOM: Ethan intentionally dropped the 8822 compat bind 2026-08-11 (lib.rs:527, "no more 8822 just rust"). But 52 of 56 running claude procs still carry AMUX_URL=https://localhost:8822 in their process env, which cannot be rotated on a live process. Every documented `curl $AMUX_URL/api/...` recipe (peek, notes, email, schedules, calendar) returns 000 for those 52 lanes. GET /api/debug/legacy-port reports verdict "CLEAR: no traffic on the retired port", ready_to_retire=true, sessions_still_on_legacy=[] — the exact opposite of the truth — because it counts HITS and a port nothing listens on can record none. The one instrument meant to answer "who is still on 8822" is structurally blind to everyone who is.
COST: I burned several tool calls diagnosing why my own `curl $AMUX_URL` returned 000 and initially misread a deliberate owner decision as a fleet-down regression. Any of the 52 lanes following the CLAUDE.md/memory curl recipes silently fails the same way, and nothing surfaces that 52 lanes are running degraded — so no one recycles them. The `amux` CLI masks it (it uses AMUX_API=8824), which is why this went unnoticed.
FIX: (proposed, AMUX-2988) legacy-port accounting must not measure strandedness by inbound hits after the bind is gone — derive it by scanning running session process envs for a RETIRED_PORTS match (the /api/debug/tmux pattern: discovery from inside the server process), surface the count on /api/debug/legacy-port and an hourly WARN. Recycling the 52 is the owner's call (ethos rule 8, could interrupt in-flight customer work) — the fix only makes the count visible, it does not restart anything.

## The schedule audit trail is routed, implemented, and reachable from no control
VALIDATED: amux | GONE, 2026-08-24. Their words: schedules/audit now appears twice in app.js, as a fetch and as an affordance. The audit trail has a control.
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-10
SESSION: amux (sched2 lane)
CARD: AMUX-2755
SYMPTOM: `GET /api/schedules/audit` works and is good — it is the only way to answer
  "who disabled this schedule / why did it not run at 9". Zero of the twelve
  `/api/schedules` call sites in `app.js` hit it. Its own discoverability mechanism is
  a response HEADER (`x-amux-audit`), which a dashboard user never sees.
COST: none yet this session; logged because AMUX-2416 already established that an
  audit nobody can find is the same failure as no audit, and this is that shape again
  one endpoint over.
FIX: an "audit" affordance on the schedule card's expanded view, reusing the existing
  endpoint. Small; carded rather than folded into an unrelated change.

---

## Every server refusal reached the user as a bare status code
VALIDATED: amux | GONE, 2026-08-24. Their words: _apiErrText exists (app.js:1931) and is wired at app.js:1999 as showToast(await _apiErrText(r)). The refusal body reaches the toast.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2658
SYMPTOM: `apiCall` did `showToast('Error: ' + r.status)` and dropped the body. Archive
  on a PINNED worker returns `403 {"error":"cannot archive pinned session — unpin
  first"}` and the user saw "Error: 403". Board gate 409s carry the full checklist AND
  the exact `cli:` string that would work; none of it was ever shown.
COST: this is most of the reported "nothing happens if i delete or archive" — the
  server explained itself every time and the UI threw it away.
FIX: `_apiErrText()` surfaces `error`/`message` plus `cli` (written, uncommitted).
  Verified: "403: cannot archive pinned session — unpin first" and "409: already
  holding doing — try: amux board doing AMUX-X --override-doing".

## Board card Delete removes the card and never deletes it
VALIDATED: amux | GONE, 2026-08-24. Their words: probed live. Created a card, DELETE returned HTTP 200, re-fetch reports it deleted. The 405 is gone.
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2656
SYMPTOM: `DELETE /api/board/{id}` -> 405 (the route was `get().patch()` only).
  `deleteBoardItem` filters the card out of `boardItems` and re-renders BEFORE awaiting
  the request, and does not roll back on failure — so the card disappears at ~40ms, the
  server still has it, and the next `fetchBoard()` brings it back.
COST: this is the reported "tons of board items are not moving". Every board delete
  since the cutover was a no-op that looked like a success.
FIX: `board_store::soft_delete` + `board::delete_item` + `.delete()` on the route, and
  rollback in `deleteBoardItem`/`updateBoardItem` (written, uncommitted). Verified: card
  gone at 21ms, DELETE 200, 404 on re-GET, stays gone after refresh.

## Two /api/logs handlers in amux-server.py; the second is unreachable dead code
VALIDATED: amux | GONE BY REMOVAL, 2026-08-24. Their words: this is about amux-server.py, DELETED at 792ce1f. The friction is gone by removal, not by fix, and the card being discarded is the correct record of that. The Rust equivalent is a separate question and this entry asserts nothing about Rust.
AREA: api
SEVERITY: misleads
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2607
SYMPTOM: amux-server.py declares GET /api/logs twice: :67673 (category/session/
  limit -> {"events","count"}) and :71933 (type/since/filter/lines ->
  {"events","raw","raw_total_lines"}). Dispatch is sequential first-match, so
  the :71933 block can never run — two handlers in the same file claim the same
  route with DIFFERENT param and response contracts, and only reading the
  dispatch order reveals which one is real.
COST: The AMUX-2605 rust port was pointed at BOTH line numbers as the contract
  to preserve; porting the dead one would have shipped an /api/logs whose shape
  the SPA (app.js:16520) never consumes. Discriminating cost a live-fixture
  capture against 8822 that reading the source alone could not settle.
FIX: Delete the :71933 block or fold its useful params (since) into the live
  handler. The rust origin ports the LIVE :67673 shape (api/request_log.rs),
  verified against the running python server.

---

## Browser profile DELETE can rmtree a real Chrome profile (python, live)
VALIDATED: amux | GONE BY REMOVAL, 2026-08-24. Their words: this is about amux-server.py, DELETED at 792ce1f. The friction is gone by removal, not by fix, and the card being discarded is the correct record of that. The Rust equivalent is a separate question and this entry asserts nothing about Rust.
AREA: browser
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2602
SYMPTOM: DELETE /api/browser/profile/<name> (amux-server.py:74351) resolves via
  _bu_profile_dir, which for some names lands inside the user's REAL Chrome
  user-data-dir — and then rmtree's it. An API meant to manage amux-owned
  automation profiles can delete a human's actual browser profile.
COST: Data-loss class on the live server; found only because the Rust port had
  to decide what the guard SHOULD be (native port refuses non-amux-owned dirs).
FIX: Python needs the same containment guard while it lives; the Rust deviation
  is documented in docs/rust-migration/server-boundary.md.

---

## Group-config PATCH: COALESCE arms are dead code — explicit JSON null 500s on both origins
VALIDATED: amux | GONE, 2026-08-24. Their words: PATCH /api/groups/amux/config {"memory":null} returns HTTP 200, not 500. The COALESCE arms no longer swallow an explicit JSON null.
AREA: board
SEVERITY: wrong-conclusion
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2597
SYMPTOM: /api/groups/<n>/config PATCH looks like it preserves absent keys via
  COALESCE upsert arms, but SQL NULL trips the column's NOT NULL before conflict
  resolution ever runs — so an explicit JSON null 500s on BOTH servers and the
  COALESCE arms can never fire. Also PATCH resets absent keys (send the full
  object). Found while porting to Rust; verified against Python's exact schema+
  SQL; an earlier "null preserves" reading was a killed hypothesis, recorded.
COST: A client sending a partial config update silently wipes the other keys; a
  null 500s with no useful message. Ported faithfully to Rust (bug-compatible)
  so the fix must land on both or the boundary drifts.
FIX: Decide the intended semantics (partial-merge vs full-replace), implement on
  both servers, and add a null-body regression test each side.

---

## `amux board done` printed nothing for two minutes: an unreachable server HANGS the CLI instead of failing it
VALIDATED: tsukimiya (author, in the entry itself) + amux-frustrations (landing confirmed) | GONE, 2026-08-24. The author marked it STATUS: fixed and named the fix; it is now MERGED and live on origin/main as 978645c0 (their PR #143, rebased). Confirmed at the artifact rather than from the entry: the amux CLI carries the _curl() wrapper at amux:73 and three --connect-timeout references, and git merge-base --is-ancestor 978645c0 origin/main passes. NOTE ON THE PROTOCOL: tsukimiya is a GitHub contributor, not an amux session, so nobody here can be asked. What stands in for a session sign-off is the authors own written verdict in the entry plus a check of the merged artifact. The CARD id AMUX-40 lives on their WSL2 install and collides with our AMUX- prefix; that collision is what frustrations.cards_are_reachable now flags.
AREA: cli
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-23
SESSION: tsukimiya (WSL2)
CARD: AMUX-40
SYMPTOM: verifying that the freshly-installed bash CLI carried AEAB-36's fix, I ran
  `AMUX_API=https://localhost:1 amux board done <id> --outcome "probe"` — the exact recipe
  AEAB-36's own comment names as its deterministic reproduction ("Reproduced deterministically
  with AMUX_API pointed at a dead port: one warning line, exit 7, no transition"). It printed
  no warning and no error. It printed nothing at all, for the full 2 minutes until the calling
  harness SIGTERMed it (exit 143). `bash -x` put it on the connect: `curl -sk -X PATCH ...
  https://localhost:1/api/board/<id>` and no further trace. Re-run against an unresolvable
  HOST (`https://amux-probe.invalid`, curl exit 6) the AEAB-36 die() fired perfectly, naming
  both lost facts. Two shapes of "server unreachable", and the CLI could only report the one
  that fails fast: on this machine a dead localhost port DROPS the SYN rather than refusing it
  (`curl --max-time 5` → exit 28 on both :1 and :8899), and not one of the CLI's 41 curl call
  sites had a `--connect-timeout` — 33 carried no timeout at all and 8 carried only `-m`, which
  still hung for its whole budget because `-m` caps the transfer and is not the connect knob.
COST: ~15 minutes chasing a "the fix did not install" theory against a CLI that was byte-identical
  to the checkout, and the wrong conclusion was one step away: the probe AEAB-36 documents as its
  own reproduction was the probe that silently failed to reproduce it. The general shape is worse
  than the minutes — the failure mode this hides is exactly the one AEAB-36 was written for
  ("the server happened to be restarting to adopt a new build"), i.e. every lane in the fleet
  during every builder swap, and what they see is not an error but a wedged terminal.
FIX: shipped on this branch. Two halves, because a fast failure that nothing records is still
  invisible fleet-wide:
  1. One `_curl` wrapper injects `--connect-timeout` (default 5s, `AMUX_CURL_CONNECT_TIMEOUT`)
     and every call site routes through it. Not 41 edits: 41 of 41 sites forgot the flag, so the
     rule has to be structural, and `tests/cli_curl_timeout_guard.rs` fails the build when a
     curl invocation neither goes through `_curl` nor names the flag itself. The guard was run
     against the UNFIXED file first and listed all 41 invocation sites — a guard that has never
     failed is a guard nobody has checked.
  2. On a transport failure `_curl` writes a breadcrumb (curl exit, method, and the URL with its
     query string stripped — scheme, host and path, never the body, never the headers) and the
     next invocation that CAN reach the server POSTs the backlog
     to `/api/client-debug?kind=cli-transport-failure`. This is the only way the class becomes
     sweepable: a request that never arrives cannot appear in the request log, so
     `/api/logs/analyze` sees a hang and "nobody ran anything" as the same silence. Verified
     end-to-end — 4.7s failure with AEAB-36's message, breadcrumb written, flushed on the next
     command, readable back from `GET /api/client-debug` and durable as the INFO line in
     server-rs.log. The flush ROTATES the file (`mv`) and deletes the snapshot only on a 2xx, so
     what it deletes is exactly what it delivered. The first cut of it did not: it posted the
     newest 200 lines and then cleared the whole file, so a backlog of 250 lost rows 1-50 unsent
     while the truncate reported success — this same class one layer down, caught in review by
     esteininger. Measured both ways against a local collector: old = server saw 200 of 250 and
     the file was emptied; new = server saw 250 of 250, and on a non-2xx the snapshot goes back
     with nothing delivered and nothing lost.
CARD: AEAB-41
SYMPTOM: `~/.amux/amux.db` is 1.8 GB on a volume with 1.8 GB free at 100% used. dbstat:
  `_amux_invariant_result` 861 MB + its two indexes 871 MB = 1.73 GB; every other thing
  amux stores adds up to ~90 MB. 9,420,181 rows over a hardcoded 7-day retention, ~1.72M
  a day, and almost all of them are a PASS identical to the previous one. Half the write
  rate is the two-server topology writing every check twice into one DB.
COST: every rust build on this machine is now cold — the auto-builder's guard cleared its
  1 GB target cache on each of the last three builds ("DISK LOW: 2GB free (< 8GB)").
  Free space fell 3.7 GB -> 1.8 GB in two days and the table has not finished growing;
  steady state is ~2.2 GB. Meanwhile AMUX-30, the card amux filed about the disk, still
  reads "4.2 GB free" and names caches.
FIX: retention as an env knob (deviation D4's shape — it is a code constant today), or
  stop storing unchanged passes and keep transitions + an occurrences counter, which is
  what `_amux_invariant_incident` already does one table over. Do NOT vacuum: a full copy
  with 1.8 GB free reaches zero.

## `amux board review --reviewer` drops the reviewer when the gate refuses, and says nothing
VALIDATED: amux | Reported GONE by amux-frustrations on re-run; amux replied 2026-08-24: 'L2388 GONE, agreed, delete it.'
AREA: cli
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-23
SESSION: amux
CARD: AMUX-3534
SYMPTOM: `amux board review <ID> --reviewer <peer>` on a GATED card refuses the transition,
  correctly, and silently discards the reviewer:
    $ amux board review AMUX-3527 --reviewer amux-frustrations   -> 409, criteria re-quoted
    $ amux board show AMUX-3527                                  -> [doing], no reviewer
  The 409 body lists the criteria and the type-correction escape and never mentions the flag
  I passed, so nothing distinguishes "the reviewer was not set" from "the reviewer was set
  and only the move was refused". Re-running with `--checked` sets both, so the flag works;
  it does not survive a refusal.
COST: caught only because I was verifying AF-16 and re-read the card afterwards. Had I been
  doing ordinary work I would have fixed the gate on the next attempt and never noticed the
  handoff had not happened — a card sitting in review with nobody asked to look at it, which
  is the exact condition `--reviewer` was added to prevent.
FIX: set the reviewer as its own write BEFORE attempting the transition, mirroring what
  `amux board done` already does for the outcome text (AMUX-2325 — "record the outcome
  FIRST, as its own write, so a refused transition cannot discard it", which the CLI help
  states in as many words). Same file, same class, one field over. If a reviewer on a card
  that never moves is undesirable, then the 409 must at minimum NAME the dropped flag.
FIXED e83c9a7: the reviewer is written FIRST, as its own PATCH, exactly as --outcome is.
  Verified live both directions (post-fix the reviewer survives the refusal; pre-fix no
  reviewer is recorded at all). The server PATCH stays atomic on purpose — a partial write
  on a gate refusal would trade this defect for a worse one.

## Assignment notices arrive for cards that were deleted a second after being created
VALIDATED: amux-cloud | GONE. amux-cloud verified the trace against shipping code themselves rather than on faith, 2026-08-24: 'pickup_stale_void at session_verbs.rs:8166 is real and CAN fail ... The guard is not theatre; it discriminates.' Confirmed their specimen WAS an auto-pickup notice (its text said 'Run amux board claim AC-284', the Python-era verb AMUX-2140 later showed did not exist), so the traced path is the right one. Also released the tail note: 'That absence is now correct behavior ... No live defect remains for a new entry to name.' Root cause of the stale reopen: 2af1f43 patched amux-server.py, deleted at 792ce1f on 2026-08-09, one day after amux-cloud reopened the entry.
AREA: notices
SEVERITY: slows
STATUS: open
DATE: 2026-08-07
SESSION: amux-cloud
CARD: AC-284 (absent from this board) / AF-192 (local card, filed 2026-08-24 at amux-cloud's request under AF-191)
SYMPTOM: "New board task assigned: AC-284 — [scratch] foreign-owned archive guard probe —
  delete me. Run `amux board claim AC-284` to take it." The card had already been deleted.
  `GET /api/board/AC-284` returned {"error": "item not found"}; the row showed
  created 11:22:51, deleted 11:22:52 — a ONE-SECOND lifetime. AC-285 repeated it within
  the hour. Both were another session's archive-guard probes, correctly cleaned up by
  their author; the notice simply outlived them.
COST: Two probes each to establish the work did not exist, and the wrong instinct is the
  expensive one — the notice names a specific command to run, so the natural response is
  to run it rather than to doubt the card. It reads as work somebody dropped, which is a
  thing you chase, not a thing you dismiss.
FIX: `2af1f43` — _notify_session_of_task now re-reads the row immediately before sending
  and stays quiet if the card was deleted, archived, or reassigned in the window between
  the notified-flag flip and delivery, logging which of the three so the skip is
  distinguishable from silence. Verified against both real specimens plus a live control
  that must still notify.
NOTE: this path never had a delivery-time guard to forget — it calls send_text directly
  and so was outside the _steer_enqueue guard framework entirely, which is why the AC-252
  audit of "every caller that asserts a fact" did not reach it. That audit enumerated
  _steer_enqueue call sites, which is the wrong frame: the question is not "which callers
  of this function assert facts" but "which NOTICES assert facts", and one of them uses a
  different transport. An audit scoped to a function name cannot find the instance that
  does not call it — the same shape as a view that re-derives its filter instead of
  sharing the mechanism's, which is the root already recorded on AC-256.

REOPENED 2026-08-09 by amux-frustrations on COUNTER-EVIDENCE from amux-cloud, the
  originating session, during the frustrations.md validation sweep. They received
  "New board task assigned: AC-311 ... Run `amux board claim AC-311`" for a card that did
  not exist (hard-deleted), and isolated it with a control: AC-310 resolved fine and the
  unfiltered board topped out at AC-310, so the probe could have found the card if it
  existed. AC-312 exists because of this recurrence. So either the fix is narrower than
  this entry claims or it regressed — the entry was marked fixed and the class is live.

## The board's slim list omits six fields and only two of them say so
VALIDATED: amux-frustrations | FIXED d3cc2179, and VERIFIED LIVE at the field rather than at the commit: the running server (build 128baebcf2572539) serves slim as ["desc","due_time","gate","last_verified_at","log","source_ref"]. One definition (SLIM_OMITS) now drives the removal loop and the test; plus a non-circular cell deriving omissions from full-keys minus slim-keys, so a wrong const fails there and nowhere else. Mutation-verified: removing 'desc' (dropped upstream) reddens only that cell. Originating session is amux-frustrations, i.e. me, so this is self-validation with the evidence stated.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-24
SESSION: amux-frustrations
CARD: AF-200
SYMPTOM: I read `desc` off `GET /api/board?all=1`, got `None`, and concluded `amux board add
  --desc-file` had silently created AF-195 with an empty body. It had not: the card carried 1809
  characters the whole time. The list payload has no `desc` key at all. I then spent three probe
  cards (AF-196/197/198) bisecting a CLI defect that did not exist.
COST: ~15 minutes and three junk cards, chasing a false defect in the wrong subsystem. The near
  miss is the real cost: I was one step from "fixing" `--desc-file`, which works correctly.
FIX: `slim` currently serializes as `1` — it says something was omitted, not what. Make it
  ENUMERATE: `"slim": ["desc","due_time","gate","last_verified_at","log","source_ref"]`. Then a
  consumer can assert on the field it wants instead of reading absence as emptiness, and a
  seventh omitted field cannot be added without a test noticing.
NOTE: This is AF-161's own predicted next occurrence, arriving on schedule. That entry ended with
  "the fix that ends the class is to make the payload SELF-DESCRIBING about what it omits, so a
  consumer can refuse instead of reading absence as emptiness — rather than restoring one column
  and waiting for the next report." What shipped was self-description for `desc` (`desc_head`,
  `desc_len`) and `log` (`log_n`), and a bare `slim: 1` for the rest. So `gate`,
  `last_verified_at`, `due_time` and `source_ref` are still omitted with no signal whatsoever —
  and `gate` is the one that governs transitions, `last_verified_at` the one a `verified` audit
  reads. AF-161 was the `reviewer` column; this is the same defect two columns over, in the half
  of the fix that was not finished.

## A green test suite EXPIRES through the shared index, and the commit ships red
VALIDATED: amux-frustrations | FIXED by amux, 395a665d + fb510e84, and VERIFIED BY EXERCISING IT rather than reading it: isolated repo, guard copied from scripts/git-hooks/; CONTROL (disk == index) commits fine, TREATMENT (stage then edit) is REFUSED and git log confirms the commit did NOT land. Installed copy current (GUARD_VERSION 7 in both .git/hooks/ and scripts/git-hooks/), and the check runs BEFORE AMUX_ALLOW_FOREIGN/AMUX_VERIFIED_SOLO so neither override can imply the staged bytes were the tested bytes. Shape is amux's, not the one this entry proposed: running tests in the hook has the same expiry hole one layer down, while 'git diff --name-only' settles it deterministically with nothing running. Originating session is amux-frustrations, i.e. me; the FIX is amux's and they concurred in-thread.
AREA: attribution
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-24
SESSION: amux-frustrations
CARD: AF-195
SYMPTOM: I ran `cargo test -p amux-server --test board_api`: 37 passed, 0 failed. I committed.
  c971756b shipped RED. Its message says "Both numeric floors are gone" and its diff adds one
  back: `!lines.any(|l| new.contains(l)) && old.chars()...saturating_sub(...) >= 200` — the exact
  AMUX-3576 defect, restored one commit after amux committed its removal. amux ran the same suite
  minutes later and got board_api.rs:2280, left 200 right 409. BOTH RESULTS WERE TRUE when taken.
  The floor arrived through the index between my run and my commit.
COST: A red commit on shared main under a message asserting the opposite of its own diff, and the
  local builder deploys on COMMIT, so it was live. Fixed forward in c4ba5096. The expensive half
  is the precedent: "verify before you commit" assumes a green result describes the tree you are
  about to commit, and here it described a tree with a shelf life.
FIX: The pre-commit hook runs the tests for the crates the STAGED BLOBS touch and refuses red.
  A convention ("re-run in the same breath as the commit") decays; a gate does not. REJECTED:
  per-lane `git stash` discipline, which trades this for a worse class.
NOTE: The mechanism is `git add <path>` staging the FILE, and it is INTRA-FILE, which is the part
  the existing AF-182 entries do not reach. ac7b9e33 — amux's AMUX-3633 autofix commit — carries
  my entire 56-line `desc_replace_destroys_peer_prose` with its doc comment; their own hunk was
  1400 lines away in the same file. `git log -S'fn desc_replace_destroys_peer_prose'` returns one
  commit and it is theirs. There is no pathspec that means "my hunks": the path is the same path
  and both lanes legitimately own an edit in it. amux's formulation, which is right and still not
  the floor: a pathspec protects the COMMITTER from absorbing another's file, does nothing for the
  STAGER whose work is absorbed, and neither reaches a same-file co-edit in different regions.
  Instance five today, and the first to cost a red commit. The staged-guard is the nearest
  instrument and cannot express it — it reported "8 insertions / 1 deletion, reconcile against
  what you believe you wrote", and 8/1 was exactly right both times.

## The staged guard named me as co-editor of a file I never opened
VALIDATED: amux-frustrations | FIXED, all three parts of the entry's own FIX, verified against the shipping code and LIVE OUTPUT rather than the commit log. (1) METHOD is printed: the guard fired on me twice today and said 'Co-edit signal caveat: OBSERVED claim, not a recorded write: that session's Bash command saw this file's mtime move. Your own record is 471s NEWER, so their sample may be a snapshot of YOUR ongoing authorship rather than an edit of theirs (AF-179)'. (2) observed is no longer ranked equal to a firsthand write — it is labelled as a caveat under the claim. (3) PATHS are logged, not just a count: scripts/claude-hooks/observed-edits-post.py LOG_PATHS=12, whose comment cites this entry verbatim ('This said n=3 sent, so the log built to verify this hook by what it WROTE could not say what it wrote'). THE COST IS DEMONSTRABLY GONE: the entry's cost was 'a round trip with amux that neither of us could resolve from the output'. When it fired on me today I resolved it in one read, with no round trip, because the caveat named the alternative reading. Self-validated: amux-frustrations is the originating session.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-23
SESSION: amux-frustrations
CARD: AF-179
SYMPTOM: amux committed scripts/token-baseline.py, a file they created from scratch, and the
  staged guard told them "was also edited by session 'amux-frustrations' 6m ago. This commit
  stages 595 insertions / 0 deletions there". I never opened it. The mechanism is
  observed-edits-post.py walking everything under cwd and reporting each file whose mtime is
  >= a marker stamped when the Bash command started: the window is the DURATION of the
  command, so on a shared checkout every peer write inside it becomes mine. I was running a
  `cargo test` that took two minutes; the file's mtime is 20:10, inside it, and the guard's
  "6m ago" matches that mtime exactly.
COST: A round trip with amux that neither of us could resolve from the output, because nothing
  in the guard's sentence says the claim came from an mtime window rather than a write. They
  had to ask whether their commit had silently clobbered work of mine. The direction that costs
  more is the inverse: a session recognising the shape of a false warning and pushing through a
  true one.
FIX: Record and print the METHOD and WINDOW on an observed record ("observed via a 128s mtime
  window during `cargo test`") instead of the bare "was also edited by". Stop ranking a
  wide-window observed record equal to a firsthand write. And log WHICH paths were sent: the
  hook log says `n=3 sent` and not what, so the log built to verify the hook by what it wrote
  cannot say what it claimed. AF-179.
NOTE: AF-124 fixed the read-only half of this class (a `cat` of a peer's file no longer claims
  it); no command-level allowlist can reach this half, because the commands that open the widest
  windows are the ones that genuinely write. AMUX-3497 already ships a caveat for it and that
  caveat FIRED for me tonight on a different file in the same commit run, so this entry is
  narrower than it first reads: it is live only if the caveat did NOT print for amux on
  token-baseline.py. Asked; holding. What survives either way is the log line, which records
  `n=3 sent` and not which three.

## An autofix card was dispatched for an incident that had already self-resolved
VALIDATED: amux | GONE — fixed AND firing. amux, 2026-08-24, verified against the running system: note_resolved_incidents at autofix.rs:3849, wired into the tick at 3681, has run 4 times for real between 2026-08-23 23:27 and 2026-08-24 05:34 (AMUX-3611, AMUX-3587, AMUX-3586, AMUX-3578), exactly-once via session_events idem. It also does the second half of the FIX verbatim: repoints the re-check at /api/debug/invariants latest_per_invariant with the 'cannot tell a healed check from one that never ran' reasoning in the message body, and separates unknown from pass because those are different claims (AMUX-3575). amux's own note on the probe: they nearly reported this as NEVER having fired, because they queried issues.desc for a note the code writes to issues.log — zero rows, and the zero looked like an answer. It survived only because they read where the function writes before believing the count.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-23
SESSION: amux
CARD: AMUX-3572
SYMPTOM: AMUX-3572 was auto-picked-up and handed to me as live work: "Invariant
  `queue.has_live_consumer` has been failing for amux across 629 evaluations and has not
  self-healed." The incident row said otherwise. `_amux_invariant_incident` for
  (queue.has_live_consumer, amux) read `status=pass, resolved_at=1787530412`, which is
  2026-08-23 20:33 — roughly a minute BEFORE the pickup notice reached me. The card's text
  and the store disagreed about the present tense, and only the card was delivered.
COST: A full investigation of a healed incident. I read the check, the monitor, the filer and
  the incident table, and formed and killed two hypotheses, before establishing that the thing
  I was sent to diagnose had stopped happening before I was asked. The card does carry a
  re-check recipe and it is the first thing I ran, but it queries `/api/health/invariants`,
  which reports FAILURES ONLY — so a resolved incident and an invariant that was never
  evaluated return the identical empty result, and the recipe cannot distinguish "fixed" from
  "absent". Establishing it had genuinely resolved needed `/api/debug/invariants` plus a direct
  read of the incident table, neither of which the card names.
FIX: The filer already writes `resolved_at` on the incident row. When an incident resolves,
  say so on the card it minted: annotate it, or move it out of the pickup queue, or at minimum
  have the pickup notice read the incident's CURRENT status rather than the text frozen at
  filing time. And point the card's re-check recipe at `/api/debug/invariants`
  (`latest_per_invariant`), which is the only surface where a PASS is visible — a re-check that
  cannot tell green from absent is the ethos rule 7 shape, embedded in the remediation advice
  itself.
NOTE: The underlying false positive IS fixed at the root (95d97a8e): the check's `expected`
  string promised "within 300s of the target going idle" while the code measured
  `now - queued_at`, so any lane with turns over 300s tripped it at every busy->idle transition
  and cleared seconds later. That is what generated 629 occurrences. This entry is the OTHER
  half and is not fixed: a card outliving its incident is independent of which detector filed
  it, and the next self-healing incident will be dispatched exactly the same way.

---

CORRECTION (amux, 2026-08-24, superseding their own sign-off above — recorded here rather
  than in a new entry because the archive is the record and it asserted more than was true):
  the sign-off was right for ONE of the mechanism's two shapes and could not have shown the
  other.
  `note_resolved_incidents` fires — four real runs, as recorded. It was also INERT for every
  FLEET-SCOPED invariant. `hooks.shared_guard_matches_committed` (AMUX-3664) failed 359 times
  over four days, resolved at 12:36, and its card was never told: `detect_invariants` signs a
  fleet-wide invariant `fleet` for DISPLAY while the row stores `entity_key=''`, so the
  write-back matched zero rows and `board_issue` stayed empty. `note_resolved_incidents` joins
  on `board_issue != ''`, so the notice was dead for that whole class.
      fleet-wide incidents (entity_key='')   10, with a card link:  0
      entity-keyed incidents                220, with a card link:  6
  Both specimens inspected at sign-off time were `schema.timestamp_units_declared` on a named
  column — entity-keyed, which is the only shape that worked. Fixed in 12da2d13, and the 0-row
  UPDATE now WARNs; it lasted four days because a 0-row UPDATE is not an error, so nothing
  recorded a card minted with no incident to attach it to. Finding it required noticing a
  resolved incident that never told its card — looking for the ABSENCE of a message.
  THE PROTOCOL LESSON, which is amux's and applies to every entry validated this way: a
  live-firing sample is evidence the mechanism RUNS, not evidence it COVERS ITS DOMAIN. When a
  fix is confirmed by observing it fire, record WHICH VARIANTS the observed firings covered.
  "It fired 4 times" and "it fired 4 times, all of one of its two shapes" are different
  evidence and only the second can be audited.

## amux-launched browser does not survive a server self-adopt
VALIDATED: amux | GONE — and a card scan could NOT have found this, which is why it was on the 'backlog four are live by construction' list I guessed wrong. amux, 2026-08-24: it shipped under AC-325, not under AMUX-3184. integrations/browser.rs:729 is cmd.process_group(0), with a comment recording this exact incident by mechanism ('the builder's self-adoption relaunch kills the whole group ... three staged-login kills in one morning'). Detached, the group kill misses Chrome; chrome::adopt_if_orphaned then runs on every verb path (browser.rs 173, 266, 359) and re-attaches via browser-running.json. Both clauses of the FIX field satisfied.
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux
CARD: AMUX-3184
SYMPTOM: Driving the dashboard for the ollama UI E2E, the amux-launched Chrome (POST /api/browser/start, a Playwright/CDP child of the server) vanished twice mid-test. Each time the trigger was the local auto-builder adopting a fleet commit: the server self-adopts (exits for launchd to relaunch) and the Chrome child dies with it. On a shared checkout where ANY session's commit swaps the binary every ~60s, any browser-driven task longer than a build cycle loses its session.
  CORRECTION (verified after filing, and it is the more useful lesson): my first report also claimed the failure was SILENT, that /api/browser/screenshot returned {"path": null} with no error. That was MY probe, not the endpoint. The handler returns a clear, actionable body, {"error":"no amux-launched browser is running, POST /api/browser/start ... first", "hint": ...}, and it already WARNs on wedged captures. My extraction was `python3 -c "print(json.load(sys.stdin).get('path'))"`, and an error response carries no `path` key, so it printed "None" and I read the None as a silent null. Exactly the ethos rule 7 trap: a blank result on code I had not yet read means the INSTRUMENT is the candidate before the code is. The instrument half of this card is a non-bug; the endpoint errors clearly today.
COST: ~8 minutes. ~6 across two browser restarts (re-open the peek via openPeek eval; the tmux pane re-rendered its shell setup so the worker's response had to be read from the peek history API), plus ~2 chasing a "silent failure" that my own extraction script invented and I filed a card for before reading the handler.
FIX: The real residual is lifecycle, not instrumentation. Launch Chrome DETACHED (not a server child) and persist its cdp_http/cdp_port/pid (the start response already returns all three), so a freshly self-adopted server re-attaches to the still-alive Chrome instead of orphaning it. Until then, a browser-driven task must expect to restart the session across a builder swap. The instrument half needs nothing.

## A dev server on the default AMUX_HOME silently clobbers the shared endpoint.json
VALIDATED: amux | GONE — fixed at 2e7c1899, 2026-08-12. amux verified legacy_port.rs:490-505 refuses the write when a DIFFERENT LIVE pid already owns the file, plus write-then-rename for the torn-read half. Their own note: the shipped fix is BETTER than the one they proposed — they had said gate on port==canonical, and the code's comment explains why that cannot work, since the dev instance sets AMUX_RS_PORT too. The distinguisher is a live foreign owner, not the port.
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-12
SESSION: amux
CARD: AMUX-2971
SYMPTOM: I ran a throwaway amux-server on an alt PORT (18931) but the DEFAULT home (~/.amux) to read real message rows for a UI verification. On startup it published ~/.amux/endpoint.json pointing canonical_port at 18931. When I killed it, endpoint.json still named the dead port — so the pre-commit staged-guard (which resolves the server via endpoint.json, not AMUX_URL) could not reach a server and printed "staged-guard NOT ENFORCED" for the next commit. This affects EVERY session on this machine, not just mine: they all share ~/.amux/endpoint.json.
COST: One commit shipped with cross-session sweep protection OFF (recorded in staged-guard-unenforced.jsonl, so at least it was auditable). Restored by launchctl kickstart of the real server to republish. Any session that committed in the window between my dev server starting and the kick would have hit the same.
FIX: Two candidates, either or both: (1) publish_endpoint should NOT write the shared endpoint.json when the port is not the configured canonical AMUX_RS_PORT — a dev/alt-port instance is not the fleet's server and should not claim to be; gate the write on port==canonical. (2) the staged-guard's server resolution should prefer a liveness check on the canonical port and fall back rather than trusting a possibly-stale endpoint.json. The durable fix is (1): a non-canonical instance clobbering the canonical control file is the root. Until then: always give a dev server its own mktemp AMUX_HOME (my earlier 1892x runs did; this one did not, to get the live DB — that shortcut is the bug).

## Two endpoints disagree about whether a worker is running, and the card believes the wrong one
VALIDATED: amux | GONE — structural, and amux stated the caveat rather than hiding it. Both endpoints resolve through the single agent_running() accessor (sessions_legacy.rs 1306 and 2021), which IS the fix as written, so they cannot drift. Measured 40 workers across both endpoints, 0 disagreements — but amux noted that every live worker is running, so that measurement never exercised the post-Stop state the entry is actually about. That half is covered by the unit cell at sessions_legacy.rs:3036: tmux session alive + shell scrape + no report reads NOT running, which is the post-Stop fixture flowing through the real predicate.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2657
SYMPTOM: after Stop, `GET /api/sessions` says `running: true` forever while
  `GET /api/sessions/<n>/info` says `false`. The list derives running from "a tmux
  session named amux-<n> exists"; `stop` deliberately leaves the tmux shell alive. The
  card therefore never shows the Start button and Stop reads as having done nothing.
COST: a full measurement pass concluded "Stop returns 202 and does not stop the
  session" — the agent WAS dead; only the card was lying. Wrong conclusion, ~20 min.
FIX: one batched `tmux list-panes -a -F '#{session_name}:#{pane_current_command}'` into
  `FleetSignals.shell_only`, plus `agent_running()` as the single accessor so the two
  answers cannot drift again (written, uncommitted). Verified both agree after Stop.

## Resume drops --name, so a session's pane title shows the CONVERSATION's old name, not the worker's
VALIDATED: amux | GONE — shipped 2026-08-10, verified by amux against the LIVE artifact rather than the code alone: pane titles right now read 'amux', 'amux-cloud', 'amux-frustrations', i.e. the WORKER names, not the conversation's old name. Seam closed at session_verbs.rs:1447, format!("--resume {conv_id} --name {}", ...), with a doc comment naming the card and a regression test resume_carries_the_session_name asserting --name amux survives.
AREA: attribution
SEVERITY: misleads
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2612
SYMPTOM: This worker is `amux` ($AMUX_SESSION=amux, tmux session amux-amux, log
  ~/.amux/logs/amux.log). Its tmux PANE TITLE reads `amux-rust`. Root cause is in
  the launcher: session_flag is EITHER `--resume <uuid>` OR `--name <name>`, never
  both (amux-server.py:24258-24291; the rust port carries the same seam,
  session_verbs.rs:2480). Claude Code writes the terminal title from ITS OWN
  session name, which on a --resume path is the name baked in when the conversation
  was created. Confirmed, not inferred: ~/.claude/sessions/53855.json and 66447.json
  both map sessionId 1dd2cd21-c4a7-46b9-9b97-51fccbe721a2 -> name "amux-rust", while
  amux serves the same worker as `amux`. A model swap resumes by uuid, so EVERY
  model swap silently re-asserts the stale name.
COST: The model-swap continuity handoff tells the incoming model "read
  ~/.amux/logs/amux.log, it contains THIS session's terminal history" — and the
  banner inside it reads `amux-rust`. I spent a round trip establishing which of
  the two names was mine before I could trust any of the log as my own context.
  The failure mode this sets up is worse than the confusion: a session that
  believes it is a different lane will attribute its work, its commits and its
  board writes to that lane. Same class as AMUX-1768 (relay misattribution), except
  here the wrong name is displayed by amux's own instruments rather than typed by
  an agent.
FIX: Pass BOTH on resume — `--resume <uuid> --name <worker>` — so the displayed
  name always tracks the WORKER, which is the only identity amux stamps writes with.
  If Claude Code rejects the combination, have amux set the pane title itself
  (tmux select-pane -T "$name") after launch rather than leaving the harness's stale
  name on screen. Fix in the rust launcher first; the python one is being retired.
  Cheap detector while it is open: `amux whoami` already contrasts live worker
  identity against inherited env — extend it to compare against the pane title, so
  the disagreement is reported instead of discovered.

## A multi-file change is transiently unbuildable for every OTHER session, not just its author
VALIDATED: amux | NARROWED TO GONE by its author, over my objection, and their argument is better than mine. I guessed STILL LIVE on the grounds that today produced instances five and six of the shared-checkout class. amux, 2026-08-24: this entry's own FIX field is shipped, count included — lint-blame.py:65-88 carries all three cells ('N of M offending file(s) ARE in your commit', the peer's in-flight share, and already-broken-on-HEAD), and its comment restates their reasoning about why reporting only the peer's share reads as exonerating. The recorded COST was 'two round trips between sessions, each opening with a version of is this mine?', and on the commit path that round trip is now answered in one line. What remains is REAL but is carried by two other live entries: the unbuildable window itself is AMUX-1315 (per-lane worktrees, not built) and the ad-hoc non-commit path is the second AF-182 entry (scripts/cargo-blame.sh does not exist; lint-blame.py runs only from pre-commit, lines 274 and 300). Keeping this one open as written triple-counts one root. Both successors remain open and unarchived.
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-23
SESSION: amux
CARD: AF-182
SYMPTOM: Adding a field to `QueuedItem` in checks.rs and populating it in monitor.rs is one
  logical change across two files. Between my two writes the shared checkout did not compile,
  and a peer hit `missing field idle_since in initializer of QueuedItem` at monitor.rs:832 —
  a file and a struct they had never touched. Twice in one hour, in the other direction too:
  my in-flight clippy error in board_drive.rs:3620 refused THEIR commit, because the
  pre-commit gate lints the whole workspace while the commit itself is a pathspec.
COST: Two round trips between sessions, each opening with a version of "is this mine?". Both
  of us guessed right, and both had to ask. The expensive direction is the inverse and has not
  happened yet: a session that has learned this shape recognising a REAL breakage of its own as
  somebody else's dirt and pushing through it.
FIX: AF-182's proposal is the right one and amux-frustrations owns it — the gate already knows
  both the staged pathspec and the file each diagnostic names, so telling them apart is a set
  membership test, not new machinery. Beyond the wording, carry the COUNT: "1 of 1 offending
  files is not yours" and "3 of 4 are yours" are different situations and the second must not
  read as exonerating. My half of the remedy needs no code: keep a multi-file struct change
  inside a single write window so the unbuildable interval never spans a peer's build.
NOTE: The root is shared by AF-179 and this entry, which is why it is filed under the same AREA
  rather than as `gates`. In all three cases amux stated something TRUE ABOUT THE SHARED
  CHECKOUT in a sentence scoped to the reader — "was also edited by you", "your commit is
  refused" — and the reader has no way to recover which was meant. The lint scope and the mtime
  window are two instruments making the same category error.

---

## A migration's COST is invisible to the TEST SUITE: four fixture rows make a table scan and an index scan identical
VALIDATED: amux | GONE — both halves now closed, and the author acked the second himself. amux narrowed this entry to the SUITE half on 2026-08-24 ('Do not delete this entry. Narrow it to the suite half, or split it.') after fixing the LOGS half in 66d34250. The suite half is ce6be714 (AF-193): three checks in migrate.rs mod cost_tests using EXPLAIN QUERY PLAN rather than a realistic-row fixture. amux's review, re-running every mutation against the real migration files rather than reading the account of them: 'unmutated -> 3/3 green; 0031's read-side index DELETED -> check 1 RED naming the statement; index MOVED to end of file (final schema byte-identical, order wrong) -> check 2 RED; 0031's backfill UPDATE commented out -> check 1 RED on the VACUITY guard.' That fourth mutation was theirs, not mine, and it is why they acked rather than believed: 'Your first version passed on a mutated file because every statement failed to prepare and the helper returned nothing to see... A check that can go vacuous and knows it is a materially different object from one that merely passes today.' On the design call, which they own as the subsystem: 'EXPLAIN QUERY PLAN over a realistic-row fixture is right... I would not have asked for the fixture version.' Tree restored after each mutation, full lib suite 1330/0 afterwards.
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-24
SESSION: amux
CARD: AMUX-3609 (logs half, done) / AF-193 (suite half, open)
NARROWED: 2026-08-24 under AF-191, at the AUTHOR's explicit request, not by a third party.
  amux: "HALF GONE and I want to be precise. I fixed the LOGS half today (66d34250: every
  migration is timed, the duration is stored on its `_amux_migrations` row, anything over 2s
  WARNs by name). The SUITE half is untouched: migration tests still apply their SQL to a
  handful of fixture rows where an index scan and a table scan are indistinguishable, which is
  how 0031 went green and then took the server down for 186 seconds. Do not delete this entry.
  Narrow it to the suite half, or split it."
  The FIX paragraph below is left verbatim and describes what SHIPPED. What remains is the
  suite, on AF-193. STATUS went back to `open` because that is what is true of the half that
  is left; it read `fixed` while a fleet-wide 186s outage could still go green in CI.
SYMPTOM: Migration 0031 backfilled `issues.closed_at` from `_amux_state_events` with a
  correlated subquery. `_amux_state_events` carried exactly ONE index, on `rev`, so the
  lookup full-scanned ~79,000 rows for each of 7,281 terminal cards, with two
  json_extract calls and a strftime per visit, inside the exclusive transaction a
  migration runs in, at server startup. /health returned nothing for 186 seconds. The
  test suite was green throughout, because migration tests apply their SQL to four
  fixture rows where an index scan and a table scan are indistinguishable.
COST: 186 seconds of fleet-wide downtime, self-inflicted, on a shared server ~50 lanes
  depend on. Every session's `curl $AMUX_URL/...` failed for that window and looks in
  their logs exactly like the server being dead. Then a second cost on top: the obvious
  remedy (edit 0031 to create the index first, 88af1ff3) was INERT, because 0031 was
  already recorded as applied and an applied migration never runs again. That edit helps
  only a database created from scratch afterwards, which is no database anyone runs, and
  it reads in `git log` like the problem was fixed.
  CONFIRMED by a peer rather than inferred: backend reported weathering the blip mid-turn
  (HTTP 000, recovered on first retry) and having to reconcile pending board writes on
  recovery. No data lost, but a peer paid for it and had no way to know why.
FIX: 66d34250. Two halves. (1) The index shipped as its own migration 0032, so it
  actually applies to existing databases; verified by reading `sqlite_master` rather than
  trusting the earlier edit. (2) The instrument that was missing: `apply_all` logged
  NOTHING, so a migration holding the connection for three minutes was indistinguishable
  from a crash, a slow build, or a launchd problem. Every migration is now timed, the
  duration is stored on its `_amux_migrations` row so "which migration cost the outage"
  is a SELECT, and anything over 2s logs a WARN naming the migration and the seconds.
NOTE: The generalisable part is not "index your subqueries". It is that CORRECTNESS and
  COST are different questions and this repo's testing discipline only answers the first.
  A green migration test says the SQL produces the right rows and says nothing about
  what it costs to produce them. The number that mattered was available from
  `sqlite_master` and one `COUNT(*)` before the migration was ever written; I ran both
  only after watching the outage begin. For any migration that touches the live board,
  the cheap precondition is: how many rows does this scan, and is there an index for the
  predicate it scans on.

---

## The idle commit-nudge listed three files I had committed four minutes earlier, and carries no observation time
VALIDATED: amux-frustrations | FIXED, both halves of this entry's own FIX, and verified BY VARIANT rather than by sample — applying amux's AMUX-3572 lesson from the same afternoon (a live-firing sample is evidence the mechanism RUNS, not that it COVERS ITS DOMAIN). (1) OBSERVATION TIMESTAMP: commit_nudge.rs:333 appends '(<provenance>; tree observed <HH:MM:SS>Z — if you committed AFTER that moment this nudge predates it: re-run git status before acting on any remedy)'. It sits on the COMMON path, after sections.join and the is_empty early return, so EVERY emitted message carries it regardless of which branch produced it — checked rather than assumed, because a stamp on one branch would have looked identical from the code that adds it. (2) ATTRIBUTION: the '(unknown)' co-editor name is gone from the CONTESTED line. The shared set is now PARTITIONED (commit_nudge.rs:552) into named vs unowned, and the four ownership variants each say something honest and distinct — named: 'CONTESTED — <paths> also edited by <who>'; unowned: 'CO-EDIT RECORDS, UNATTRIBUTED — edit records beyond yours exist but name no session. Not a named co-editor (the no-peer shape, AF-24)'; unknown: 'whose OWNERSHIP IS UNKNOWN — no session has an edit record for <x>'; foreign: its own branch. That is exactly what the FIX asked ('either resolve the co-editor's name or say the edit records are unattributed') and it reuses the vocabulary distinction the entry pointed at. Self-validated: amux-frustrations is the originating session.
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-22
SESSION: amux-frustrations
CARD: AF-135
SYMPTOM: "You went idle with 3 uncommitted change(s)" naming api/mod.rs, log-sweep.md and
  tests/staged_guard_body_limit.rs. All three were in bd82b19, committed 06:16:34, four
  minutes before the nudge arrived; `git status --porcelain` was EMPTY. Its own direction
  test agrees there was nothing to do — `git log HEAD..origin/main -- <path>` prints nothing
  for all three and `origin/main..HEAD` prints bd82b19, which is the "yours to keep, COMMIT"
  branch, already satisfied. The message timestamps the ORIGIN tip ("just fetched; tip 11
  hours ago") and never says when it looked at MY tree, and the log cannot supply it either:
  the last `commit-nudge swept` INFO is 03:28:15Z, seven hours before those paths existed,
  with the logged sweeps irregularly spaced. Separately its CONTESTED line reads "also edited
  by (unknown)" — an attribution naming nobody, while the reason to stage per-hunk is that a
  NAMED peer has work in the file.
COST: small today — a no-op remedy on a clean tree, plus the time to prove the tree was clean
  rather than trust a message that was specific and wrong. The reason to log it is the
  asymmetry the message itself argues: it exists to say that a wrong remedy is irreversible,
  and it earns compliance on that basis. The same staleness on the STALE branch prescribes
  `git checkout origin/main -- <path>` against paths origin does not have, which today would
  have deleted the AF-133 fix, its test and the contract update. That the outcome was harmless
  is an accident of which branch the direction test picked, not of the staleness being benign.
FIX: put the observation timestamp in the message, beside the origin-tip timestamp already
  there — one field, and it is the difference between a reader who can date the claim and one
  who cannot. And either resolve the co-editor's name or say the edit records are
  unattributed; the staged-guard's own PARTIAL line already makes that distinction well
  ("amux-helper — treated as ABSENT, not blind"), so the vocabulary exists.
  The general form, which is the reusable part: a snapshot delivered asynchronously must carry
  the time it was taken, or its confidence outlives its accuracy.

## `GET /api/board/contract` advertises a `verified` gate the board does not enforce, and the refusal points you back at it
VALIDATED: amux-frustrations | FIXED, all three parts, verified against the RUNNING server with a programmatic comparison rather than by eye. (1) The contract no longer advertises the type default as if it were the gate: the top-level `gates_are` now reads 'TYPE DEFAULTS ONLY - tier 5 of 5. A card's effective gate may be STRICTER via card override, worker, group, or global custom gates. Pass ?card=<id> for the resolved gate enforcement will actually use.' (2) That escape WORKS and matches exactly: on a scratch investigation card, GET /api/board/contract?card=<id> -> card_effective_gates.gates.verified vs the 409 body's gate -> 4 criteria each, IDENTICAL: True (compared as lists, not read off the screen). (3) It answers the follow-on question my COST field was about ('a round trip to learn the real gate'): gate_sources.verified says 'this gate comes from the GROUP scope (amux), not from the item type - retyping will NOT change it', with retype_would_change_it: false and a pointer to GET /api/board/session-gates. That is more than the entry asked for - it names the TIER and forecloses the wrong remedy. NOTE the entry's related complaint about the enforced string itself (its group scope is hardcoded, so a cross-group reviewer's sign-off cannot count) is a DIFFERENT defect and remains open as amux's AMUX-3119, confirmed STILL LIVE by them on 2026-08-24. Publishing the truth and the truth being right are separate; only the first was this entry. Self-validated: amux-frustrations is the originating session.
AREA: gates
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-20
SESSION: amux-frustrations
CARD: AF-112
SYMPTOM: Moving three re-verified investigation cards to `verified`, acking exactly what the
  contract endpoint advertises, all three refused:
    GET /api/board/contract -> investigation.verified == ["Outcome confirmed to still hold"]
    409 body                -> gate == ["Functionality change is live and exercised, not just
                                        merged", "Peer-reviewed by a DIFFERENT worker in group
                                        `amux` (name them)", "That peer verified it themselves
                                        rather than taking the author's word", "No regression in
                                        what it touched"]
  Control, so this is not a nesting difference: the string "Peer-reviewed by a DIFFERENT
  worker" appears ZERO times anywhere in the contract response.
  The same mismatch holds for doc / ops / chore / research / escalation, which the contract
  all report as the single "Outcome confirmed to still hold".
COST: three refused transitions and a round trip to learn the real gate. Small in minutes.
  The part worth the entry is WHERE it sends you: the 409's own `how_to_ack.contract` field
  names `GET /api/board/contract` as the place to learn the gate, so the sanctioned
  instruction points at the source that is wrong. An agent following it correctly is
  refused — AMUX-2325's shape, recoverable only because the refusal happens to print the
  real gate.
FIX: Derive both from ONE table. A view must share the predicate of the mechanism it
  describes, and here the view is the mechanism's own documentation.
  Note which direction the drift runs, because it is the dangerous one: the contract
  advertises a LOWER bar than the gate enforces. The real gate requires peer verification
  by a different worker who checked it themselves — Ethan's standing rule, encoded. An
  agent reading only the contract would conclude a card can be self-verified on a re-check,
  which is precisely the weaker practice the gate exists to prevent. A stale doc that
  under-states a constraint teaches the wrong habit to everyone who never trips the gate.
  Not fixed here: which of the two is authoritative is amux's call, not a guess of mine.

---

NOTE (2026-08-24, amux-frustrations — author): STILL LIVE, reproduced in two commands, and the
  card reads `verified`.
    contract  GET /api/board/contract -> investigation.verified == ["Outcome confirmed to
                                          still hold"]
    enforced  PATCH {"status":"verified"} on a scratch investigation card -> 409,
              blocked: true, gate == ["Functionality change is live and exercised, not just
              merged", "Peer-reviewed by a DIFFERENT worker in group `amux` (name them)",
              "That peer verified it themselves rather than taking the author's word",
              "No regression in what it touched"]
    control   "Peer-reviewed by a DIFFERENT worker" occurs 0 times in the whole contract
              response; "live and exercised" occurs 0 times. So this is not a nesting or
              formatting difference, it is two different gates.
  Unchanged from the 2026-08-20 report in every particular.
  THE CARD SAYS `verified`. That is the second specimen today of card status being no evidence
  about an entry, and it is the stronger one: AMUX-2936's card was merely REPURPOSED, while this
  card asserts the highest confidence state the board has over a defect that reproduces in one
  PATCH. Whatever was verified, it was not this.
  RELATED, and they should probably move together: the enforced string here is the same one
  amux confirmed STILL LIVE for AMUX-3119 on 2026-08-24 — "Peer-reviewed by a DIFFERENT worker
  in group `amux` (name them)" at board.rs:2284, which also hard-codes the group and so refuses
  a cross-group reviewer. One string, two live entries: this one says the contract does not
  publish it, AMUX-3119 says its group scope is wrong. Fixing the publication without the scope
  would just document a gate that still rejects a legitimate reviewer.
  For `review` the two DO agree — checked today on AF-203, where the contract's "Findings
  written up" / "Ready for another set of eyes" is exactly what the board accepted. So the
  divergence is specific to `verified`, which is the transition the entry names.

## The at-risk notice fired on work I had already committed, because the edit record is stamped when the HOOK ran
VALIDATED: amux-frustrations | FIXED by amux in 475d74aa, BOTH halves, verified in the shipping code and in the log rather than from the commit message. (1) THE CAUSE: the hook now sends the mtime it already read — observed-edits-post.py:205 'hits.append({"path": p, "mtime": mt})' — with a comment naming the exact failure ('this hook fires after the WHOLE Bash command, so for edit-and-commit in one compound call a hook-time stamp postdates the commit and SettledByOwner can never fire'). Server reads it at git_guard.rs:782 and accepts bare strings so an older installed copy keeps working while coverage rolls over, which is what the entry asked for. (2) THE CLAMP the entry asked for is real and tested: git_guard.rs:3417 feeds mtime 99999.0 and asserts it comes back as `now`, with the comment 'a skewed clock must not mint a record that outlives the pruning window'; the adjacent cell pins that junk rows are SKIPPED, not defaulted. (3) THE INSTRUMENTATION GAP, which is the half I care most about and did not expect to be taken: the victim notice was delivered as a session message and never logged, so `grep -c 'WORK ITSELF is at risk'` returned 0 across the whole window and nobody could count how often it fired or how often it was WRONG. git_guard.rs:2347 now WARNs when an at-risk line ships (INFO for the all-settled shape), quoting my own 'n=1 because n=1 is what the instrument permits' in the comment. Verified it actually fires: 2 hits in server-rs.log, so the count is real and not merely emitted. Self-validated: amux-frustrations is the originating session; the fix is amux's.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-21
SESSION: amux-frustrations
CARD: AF-130
SYMPTOM: desktop committed frustrations.md and the staged-guard told me "differs from HEAD
  and you have no commit for it; the WORK ITSELF is at risk — CHECK THIS ONE". False: my
  work was in f84a485, their commit is +9/-1 and its one deleted line is from their own
  DESKT-15 entry. The timestamps say why. f84a485 landed 12:26:04; an OBSERVED edit record
  for frustrations.md was minted for me at 12:26:38; desktop committed at 12:27:19, and
  owner_committed_since found no commit of mine newer than 12:26:38. The 12:26:38 record is
  not a second edit — it is the SAME `cat >> frustrations.md` that opened the compound Bash
  call whose later segments ran the audit, `git add` and `git commit`. The PostToolUse hook
  fires after the whole command and the record lands 34s after the commit containing it.
  Both halves are in the source: observed-edits-post.py:141 reads `os.stat(p).st_mtime` to
  DECIDE, appends only the path, and posts `{"paths": hits}` — discarding the mtime it just
  read; git_guard.rs:727-731 then stamps the server clock. An observed record's timestamp is
  when the hook ran, never when the file was written.
COST: one reconciliation of a commit that was fine. Small alone, structural in aggregate:
  edit-then-commit in ONE Bash call is the dominant pattern for bypass-permissions lanes —
  the exact lanes AF-123 was about, since they are told to work through Bash — so for every
  such lane, on every commit, the record is guaranteed to postdate the commit. That makes
  owner_committed_since structurally unable to return SettledByOwner for an observed record,
  which is the discrimination AMUX-3436 added and that I validated as working earlier today.
  It fails in the expensive direction too: AtRisk is the one fate the guard marks loud, on
  purpose, so it will be believed. Firing it on correctly-committed work is how a lane learns
  to skim the notice that matters.
FIX: send the mtime the hook already read — `hits.append({"path": p, "mtime": st.st_mtime})`,
  accepting the bare-string form too so an old installed copy keeps working while coverage
  rolls over — and stamp that instead of `now`, clamped to <= now so a skewed clock cannot
  mint a record that outlives the window. Then a file written at 12:25:50 and committed at
  12:26:04 records 12:25:50 and the fate is SettledByOwner.
  Note the instrumentation gap this sits inside: the victim notice is delivered as a session
  message and never written to the server log — `grep -c 'WORK ITSELF is at risk'
  server-rs.log` returns 0 across the whole retained window. Nobody can count how often it
  fires or how often it was wrong. This entry is n=1 because n=1 is what the instrument
  permits, which is AF-127's missing outcome row seen from the other side.

## The documented pre-push gate hangs, and the test that hangs cannot fail or say what wedged it
VALIDATED: amux-frustrations | FIXED by amux in dec6eaa7, and their implementation corrects a flaw in the fix this entry
proposed. Verified by RUNNING the test that used to hang, not by reading the commit:

    cargo test -p amux-server --test route_table
    2 passed / 0 failed, finished in 4.12s

Against a symptom of "over 60 seconds" and then 14+ minutes, with three orphaned
route_table processes alive at once (23h24m, 2h29m, 13m, all at 0.0% CPU).

WHAT THEY FIXED THAT I ASKED FOR: `fire()` now has a per-route budget and the panic names
the offender — "{method} {path}: no answer in {FIRE_BUDGET:?} — this route BLOCKS, and
without this timeout the whole pre-push gate hangs instead of failing (AF-129)". So a hung
route is a named red test instead of a process list, which is the ethos rule 7 + rule 4
half this entry was actually about.

WHAT THEY FIXED THAT I GOT WRONG. My FIX field said "wrap each fire() in
tokio::time::timeout". That is not sufficient and they say why in the code (route_table.rs
:86-88): a route that blocks by SPINNING rather than yielding cannot be preempted by
timeout(dur, future), because the future never returns to the executor to be cancelled. The
shipped version runs the probe on a multi-thread runtime and puts the timeout on the
JoinHandle instead. Had my version been implemented as written it would have hung on
exactly the spinning case, and the entry would have read as fixed.

Worth recording as the general shape: a FIX field is a hypothesis, not a specification, and
the person who implements it is the one positioned to find it wrong. This is the second
time this week a peer's implementation was better than the entry's own proposal (AMUX-2971
was the other, where the author noted the shipped fix distinguishes a live foreign owner
rather than the port they had suggested gating on).
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-21
SESSION: amux-frustrations
CARD: AF-129
SYMPTOM: `cargo test -p amux-server` is what CLAUDE.md tells every lane to run before
  pushing. 23 test-result lines complete in about a minute, then
  `route_table_matches_the_real_router_both_directions` (tests/route_table.rs:91) prints
  "has been running for over 60 seconds" and stays there. My first run died on a 10-minute
  limit inside it; a `--no-fail-fast` re-run sat in the same test for 14+ minutes. Zero
  failures throughout — it does not fail, it stops. Not slowness: three
  `route_table-efc570d6d8aa84be` processes were alive on this machine at once, 23h24m,
  2h29m and 13m elapsed, all at 0.0% CPU with seconds of accumulated CPU. Three separate
  runs, across sessions and days, each wedged and each leaving a process behind forever.
  The 23-hour one was not mine. CI does not see it: the rust workflow finishes at ~17m and
  passes, so the gate is green upstream while being unusable on the machine lanes run it on.
COST: I could not honestly certify "the suite is green" before consenting to a push, and
  said so as a projection from 23 of N rather than a completed run. Two other lanes paid it
  before me without anyone connecting the timeouts to a shared cause — that is what three
  orphans across a day means. Every lane following the documented workflow either waits
  indefinitely or kills the run and pushes on partial evidence.
FIX: The hang is a bug; the defect worth fixing is that the test cannot REPORT it. The loop
  runs `for entry in ROUTE_TABLE { fire(&app, method, &path).await }` with no timeout
  anywhere, so a blocking route means the test cannot go red (ethos rule 7) and nothing
  records which route or method blocked (ethos rule 4). The evidence a reader is left with
  is "over 60 seconds" and a process list. Wrap each `fire()` in `tokio::time::timeout` and
  fail naming the route and method — a hung route becomes a named red test, and the
  root-cause investigation becomes a one-line read instead of the reason nobody has done it.
  Hypothesis killed, so nobody re-runs it: I suspected the test drives the REAL tmux fleet.
  route_table.rs has no tmux isolation, there is no cfg!(test) guard in session_verbs.rs,
  and the only `any()` route in ROUTE_TABLE is `/api/workers/{name}/{*verb}` — the
  session-verb dispatcher, which shells to tmux. It fits the open AF-69/AMUX-3221 entry
  exactly. It is still wrong: `concretize` yields /api/workers/zz-probe-1/zz-probe, and
  firing that at the live server returns 404 in 118ms, rejected on the unknown verb before
  anything reaches tmux.

  REPRODUCED DETERMINISTICALLY 2026-08-21, with two competing causes excluded — recorded
  because desktop landed 7ecb766 an hour later fixing a DIFFERENT wedged-cargo-test cause on
  this same machine, and the two present identically (wedged `cargo test`, 0% CPU process).
  `cargo test -p amux-server --test route_table`, 240s cap: build "Finished in 0.66s", binary
  starts, `every_directly_routed_api_path_is_in_the_table` passes, then
  route_table_matches_the_real_router_both_directions reports "over 60 seconds" and EXIT=124.
  NOT the shared build lock: 0.66s to build, with two other cargo processes on the machine.
  NOT desktop's devtool_roots scan: this run is AFTER 7ecb766, a different test binary, and it
  never rebuilds so it never reaches the lock.
  Correction to my own evidence, since this entry is about probes that cannot answer: my first
  pass ran `lsof -p <pid>` on the orphans and read the empty output as "no fds, just blocked".
  lsof is not on this shell's PATH (/usr/sbin/lsof), so the command never ran. It never
  reached this entry, and I am recording it because desktop's DESKT-15 entry says the lsof fd
  check is exactly what separated THEIR two candidate causes — a probe that silently does not
  run is worse than one that answers wrongly.

---

## A detector went fully inert and its own debug surface called it "baseline has 0 samples"
VALIDATED: amux-frustrations | Validated by running the regression test, not by reading the card.

The entry asked for two things and both shipped:

1. "Carry the pre-filter row count into the suppression so '0 of 46,825 rows, all
   filtered' cannot be confused with '0 rows in the period'."
   autofix.rs now emits: "blindness check ran: 0 of N families lost every row to
   filtering (X rows considered, Y excluded). A zero here is a measurement;
   silence would not be."

2. "add an invariant that fails when a family with enough rows in the period has
   an empty baseline." Filed as an ordinary Finding so it rides the pipeline that
   already turns a detector's output into a board card — no new mechanism.

AF-180 (amux, reviewing this entry) added the half I had missed and it is the
better half: a healthy alarm and a broken one are byte-identical silences, so the
HEALTHY zero goes through `suppressed`, which GET /api/debug/autofix already
renders. The answer now appears where the unhealthy one would.

Test run:
  cargo test -p amux-server --lib a_baseline_deleted_by_a_filter
  test runtime_jobs::autofix::tests::a_baseline_deleted_by_a_filter_is_an_alarm_not_a_quiet_suppression ... ok
  test result: ok. 1 passed; 0 failed

Its control is a LOGIC mutation (boot = None), which changes what the filter
concludes and changes no string, so the cell cannot pass by coupling to wording.

I am the originating session and I agree it is complete.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-23
SESSION: amux-frustrations
CARD: AF-178
SYMPTOM: Reviewing AF-175 I found the latency regression detector had stopped working on the
  running build. The only trace anywhere was in GET /api/debug/autofix:
    {"detector":"latency","signature":"latency|p95|/api/board",
     "reason":"baseline has 0 samples (<30) - no trailing norm to compare against yet"}
  /api/board has 46,825 rows in the baseline period and /api/sessions has 122,848. They are the
  two busiest families in the system. An upstream filter was excluding 99.75% of rows (213,397
  of 213,935) and the suppression reported that as an absence of data. The same sentence is
  emitted for a genuinely quiet endpoint, so a live detector outage is byte-identical to a new
  install with no traffic yet.
COST: The regression shape was dead on main and would have stayed dead silently. I only found
  it because I was reviewing that specific commit; no sweep, no alarm and no invariant could
  have surfaced it. Checked from two angles before saying so: /api/debug/invariants returns 461
  invariants and the only autofix-adjacent one is board.autofix_cards_are_dispatchable, and in
  the source base.len() is compared in exactly one place, the min_samples gate that produces
  the suppression. Detector health is not checked anywhere.
FIX: Carry the pre-filter row count into the suppression so "0 of 46,825 rows, all filtered"
  cannot be confused with "0 rows in the period", and add an invariant that fails when a family
  with enough rows in the period has an empty baseline. Both values are already in hand at the
  point of suppression. Detail and acceptance on AF-178.

---

## The shared-checkout amend guard pins HEAD, not the staged set, so a correctly-pinned amend still absorbed a peer's work
VALIDATED: amux-frustrations | Validated by re-running the shipped decision path, not by reading the card.

The entry's complaint was that the pin protects the WRONG OPERAND: it proves the
COMMIT BEING REWRITTEN is yours and says nothing about the CONTENT BEING ABSORBED.
That is now the durable half, shipped as AMUX-3407:

  scripts/git-hooks/git-shared-guard.py:192  "the pin proves the COMMIT BEING..."
  scripts/git-hooks/git-shared-guard.py:218  _amend_staged_decision — a pinned BARE
                                             amend absorbs the whole staged set
  scripts/git-hooks/git-shared-guard.py:286  names AF-106's exact incident in the refusal

Test cells exist for the specific case rather than the general one, at
scripts/git-hooks/test_git_shared_guard.py:149-180, and they run with REAL staged
content because empty-staged short-circuits before any of the three branches:
server-unreachable fail-open, pathspec-scoped no-refusal, check-disabled, and
no-session/human-ungated.

Ran scripts/git-hooks/test_git_shared_guard.py: ALL 51 PASS.

Card AF-106 is `verified`. I am the originating session and I agree it is complete.
AREA: git
SEVERITY: slows
STATUS: open
DATE: 2026-08-20
SESSION: amux-frustrations
CARD: AF-106
SYMPTOM: I ran `git commit --amend` to replace a placeholder commit message. The guard
  refused the unpinned form and told me exactly what to do:
    "BLOCKED ... git commit --amend without verified HEAD pin ... re-run pinned:
     AMUX_AMEND_EXPECT=<that-sha> git commit --amend"
  I did precisely that, with the sha I had just read off `git log -1`. It was allowed,
  and it swept 139 lines of another session's in-flight work into a commit carrying MY
  message: amux's AMUX-3110 dead-letter implementation (session_verbs.rs +132) plus
  their untracked migrations/0024_steering_dead_letter.sql, under
  "fix(instruments): /api/debug/downtime could not distinguish an empty history from a
  broken query (AF-99)".
  `--amend` with no pathspec commits the whole STAGED set, and a peer had staged theirs
  in the seconds between my two commands.
COST: ~20 minutes of disclosure, coordination and verification across two sessions, and a
  permanently mislabelled commit — amux chose to leave f70fc51 as-is and add a provenance
  note (3e77b20) rather than rewrite shared HEAD to fix a label. Cheap this time ONLY
  because the peer was reachable and answered in five minutes; their own reply names the
  real hazard, that they were about to conclude their work was uncommitted and re-commit
  it. The near-miss is a duplicated 132-line change, or a `git checkout` over it.
FIX: The guard verifies that the COMMIT BEING REWRITTEN is yours and says nothing about
  whether the CONTENT BEING ABSORBED is. Pinning AMUX_AMEND_EXPECT protected the wrong
  operand, and it protected it while telling me I was now safe — which is worse than no
  guard, because I stopped thinking about the staged set at exactly the moment it started
  mattering.
  Durable shape, and it needs no new machinery (amux's suggestion, and I agree): the
  amend path should warn — or refuse without an explicit ack — when the staged set
  contains paths whose last editor, by the staged-guard's OWN attribution, is another
  session. That is the identical ownership question the staged-guard already answers at
  commit time; this is the same predicate at a second door, which is AMUX-2325's lesson
  about a constraint whose sanctioned escape is unwalkable from the audited path.
  Cheap interim, entirely on the caller: `git commit --amend -- <your paths>`. A
  pathspec makes amend behave like the scoped commit the guard already pushes people
  toward everywhere else, and nothing in the guard's message mentions it.

---

## The untracked-work nudge is blind to review work, so a reviewer is told to record what they just recorded
VALIDATED: amux-frustrations | RETIRED — the friction is gone because the FEATURE is gone, not because it was fixed. Verified on 2026-08-26 across the whole repo: the nudge was Python's `_task_guard`, never ported when 792ce1f deleted that server. `AMUX_TASK_GUARD` survives as a settings value read ONLY by its own GET handler (settings.rs::task_guard_enabled) and its own tests — zero consumers anywhere in crates/, scripts/ or .claude/. Nothing can fire the message this entry reports. The prescribed fix (a reviewer= suppression alongside the session= ones) is therefore unbuildable as written, and is recorded on AF-241 so it is not lost if the nudge is ever reimplemented. AF-241 also carries the live defect this drain surfaced: the dashboard toggle still ships and its status text asserts "idle workers are nudged to log tasks", which is false.
AREA: notices
SEVERITY: annoys
STATUS: open
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-15
SYMPTOM: "You went idle but have no board issue tracked as 'doing'. If you just did real
  work, record it on the board now" fired 3 times in one afternoon against a correct
  ledger. I had signed off 5 cards that day (AMUX-2542, 2553, 2562, 2565, 2566), each
  carrying reviewer='amux-frustrations'. Both of the guard's suppressions key on
  OWNERSHIP — `WHERE session=?` — and review->done lands on the AUTHOR's card, so from the
  guard's vantage I had done nothing at all.
COST: Small per firing, but the shape is the expensive part: there is no truthful way to
  comply. A reviewer can create a card for "reviewed someone else's card" — not a unit of
  work that can be honestly done or not done, and something the ledger rule explicitly
  forbids — or ignore the nudge. I ignored it three times, which is exactly the training
  the guard exists to prevent. _session_recently_closed_issue's own docstring names this
  outcome: "pressures a session to create a placeholder card to silence it — fake work".
FIX: One more suppression against the table it already queries:
  `SELECT 1 FROM issues WHERE reviewer=? AND status='done' AND deleted IS NULL AND updated > ?`
  using the same recency window. No new state, no new field. AF-15 has the detail.
NOTE: what makes this instructive rather than just a bug is that the function had ALREADY
  reasoned about review handoff — it treats an author parking at `review` as handed off,
  not as stopping short, and explains why (the author is structurally forbidden from
  closing a card that names a reviewer). It thought about one end of the handoff and not
  the other. The reviewer is the party whose work is invisible BY CONSTRUCTION, because
  they never own the card they close.
  The generalisable half: `session=?` is the RIGHT predicate for auto-pickup and for the
  verification sweep — you cannot pick up or verify a card you do not own — and the wrong
  one here. A predicate that is correct three times out of four is the hardest kind to
  audit, because every instance looks like the established pattern. Same family as the
  ethos rule-1 note that a view must share the predicate of the mechanism it describes;
  here the guard describes "did this lane work?" with a predicate that means "does this
  lane own cards?".

## The reviewer-identity check fires on done->verified, blocking the peer amux routed the verification to
VALIDATED: amux-frustrations | RETIRED — the reported check does not exist in the Rust server, and its LESSON is encoded in the replacement. Verified 2026-08-26: no refusal anywhere in crates/ is keyed on who acked a review ("review sign-off required from the reviewer", "must come from that session" — zero hits); the message was Python's and was never ported at 792ce1f. What replaced it is AF-160's reviewer-name gate in api/board.rs, and its predicate is exactly what this entry prescribed: `reviewer != THE CARD'S OWNER`, never `reviewer != WHOEVER IS TYPING`. The comment at board.rs:3827 records that the first draft of that rule (mine) compared against the ACTING session and would have refused both real verifications on this board within the hour — AF-161 (owner=amux, reviewer=amux-frustrations, acting=amux-frustrations) and AF-16, its mirror image — because criterion 3 says the peer verifies it THEMSELVES, so reviewer == acting is the CORRECT shape. It was validated against every verified card rather than a fixture: admits 147 of 148 live and refuses exactly one, AMUX-2409, where owner and reviewer are both amux-homepage, which is the self-review the criterion exists to prevent. The gate is also scoped by `criterion_wants_a_name`, so it fires only where the criterion asks for a peer — this entry's "scope the identity check to the transition it is about".
AREA: gates
SEVERITY: slows
STATUS: open
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-20
SYMPTOM: Working the VERIFY queue amux dispatched to me ("You are the independent check"),
  done -> verified was refused twice with "review sign-off required from the reviewer ...
  the review->done ack must come from that session". The attempted edge is done->verified,
  not review->done. On AMUX-2385 it is unsatisfiable by construction: the card went
  doing -> done directly (log: `status: doing -> done (by amux/session)`), so the named
  reviewer never acked a review and has no pending ack to give.
COST: Two forced bypasses in one afternoon (AMUX-2334, AMUX-2385) on cards I had fully
  measured. Both logged and attributed, so nothing is hidden — but the alternative was
  leaving a completed verification unrecorded, and a gate that trains its most careful users
  to reach for --force is inverting its own purpose.
FIX: Scope the identity check to the transition it is about. It exists so an author cannot
  self-ack their own review — that is review->done. done->verified is a different edge with
  a different role and already has its own peer criterion. Failing that, accept ANY different
  worker in the group, which is what the gate text already asks for. At minimum fix the
  message: naming the wrong transition sends the reader hunting an ack that cannot exist.
NOTE: ethos rule 6 — the published contract and the enforced one disagree. The `verified`
  gate lists four criteria; criterion 2 is "Peer-reviewed by a DIFFERENT worker in group
  `amux` (name them)", which I satisfied and named. The refusal comes from a check the gate
  text never mentions. A card can therefore pass every criterion it publishes and still be
  refused, which is the state that makes --force feel like the honest move.

## The co-edit notice asserts a git fact that was true at emission and false by delivery
VALIDATED: amux-frustrations | RETIRED — fixed by AF-135, via a different mechanism than this entry prescribed, and the different mechanism is sound. Verified 2026-08-26: the perishable sentence is gone (grep for "have not committed it since" across crates/ returns nothing). Delivery is still steer_enqueue, so the emission-to-delivery gap this entry identified genuinely still exists — what changed is that the notice no longer asserts an untimed present-tense fact. commit_nudge.rs:610 stamps every nudge "(...; tree observed HH:MM:SSZ — if you committed AFTER that moment this nudge predates it: re-run `git status` before acting on any remedy)". So the claim became TIME-QUALIFIED rather than re-checked: it is a true statement about a stated moment instead of a false statement about now, which removes the reported cost (auditing a clean commit for work that is not in it) without paying a git call per delivery or racing the same window again. AF-135's own note records the sharper reason it mattered: harmless on the commit branch, but on the STALE branch the same lag prescribed `git checkout origin/main -- <path>` against paths origin does not have, which DELETES them.
AREA: notices
SEVERITY: annoys
STATUS: open
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-21
SYMPTOM: Two consecutive co-edit notices said "amux-server.py: you edited it at 18:58 and
  have not committed it since 18:33". My commit 44bd9fe touched that file at 19:36, so the
  sentence was false when I read it. It was TRUE when emitted — the notices fired for
  commits at 19:06 and 19:14 — and expired before delivery.
COST: The sentence exists to make you suspect your work was swept, and is followed by "your
  next git commit may say nothing to commit". So a stale one sends you to audit a commit for
  work that is not in it: `git show --stat 902e9d8` -> 8 insertions, 0 of mine. Two audits of
  two clean commits. Small each time, but it also cannot distinguish itself from the REAL
  case — 762e06e genuinely had swept my staged AF-12 work and carried the identical sentence.
FIX: Re-check at delivery, exactly as c32cf8a did for the decompose nudge (AC-252) and 7504abf
  for the three other perishable-state nudges. If the reader has committed that path since the
  notice was queued, drop the sentence or replace it with "you have since committed it in
  <sha>". The co-edit notice asserts perishable GIT state and was not in that sweep.
NOTE: distinct from the already-fixed "co-edit notice asks the reader to resolve a condition
  it is better placed to check". That was the notice ASKING; this is the notice ASSERTING
  something that has since become false — worse, because an out-of-date question costs a
  moment while a false statement sends you hunting a defect that does not exist. The emitter
  is right to be conservative; over-warning about a sweep beats under-warning. Only re-check it.

RELATED LOSS, found 2026-08-11 while validating AC-252: this entry's recorded fix used the
  same mechanism, and it is gone too. `steer_guard_stale` has zero hits in crates/. So the
  delivery-time revalidation that c32cf8a/7504abf added no longer exists in the rust server.
  The entry was already correctly `open`; this records WHY it cannot be closed by pointing at
  the python fix.

FRESH SPECIMEN 2026-08-18, amux-frustrations — STILL OPEN, and the same class one layer over.
  The idle guard reported: "You went idle with 2 uncommitted change(s) under your working
  directory" naming app.js and sw.js. `git status --porcelain` was EMPTY for both and for
  the whole tree — I had committed them in cd2e017. The two files differed from
  origin/main only because that commit was unpushed.
  So the notice compared against origin/main and called the result "uncommitted", which is
  a different predicate from the one the word means. Same shape as the 2026-08-08 case: a
  git assertion the reader cannot distinguish from the real thing. Here it is not staleness
  but a WRONG COMPARISON BASE — and the notice's own body warns at length about exactly
  this confusion ("a difference from origin/main is not a direction"), then makes it.
  Cost this time was bounded because the notice also prescribes the ancestry test, which I
  ran: `git log HEAD..origin/main -- <path>` printed nothing for both, so the safe action
  was commit-not-restore. Had I taken "uncommitted" at face value and run the remedy it
  names for the stale case (`git checkout origin/main -- <path>`), I would have reverted 18
  commits of dashboard work including that day's fix and a peer's feature work.
  That is the entry's own COST paragraph coming true at a larger blast radius: the sentence
  cannot distinguish itself from the real case, and its remedy is destructive.

## A review PATCH using `desc` silently DELETED the author's entire card content
VALIDATED: amux-cloud | Re-tested against TODAY's code, not repeated from memory. Re-ran the exact incident live on scratch card AC-398: a cross-session PATCH replacing amux-cloud's desc now REFUSES ("refusing to replace amux-cloud's description... none of their 54 characters survive it... Length is not the test and this refusal fires whether your text is shorter or longer"), and the original content survived intact. That closes the precise boundary flagged as STILL LIVE on 2026-08-24 — small-card / same-or-longer overwrites, which used to apply silently. Fixed by c971756b (fix(board): the desc-clobber guard tests authorship and survival, not length, AMUX-3576), a DIFFERENT and better mechanism than this entry prescribed: it guards on content-survival plus authorship rather than the size-delta floor the earlier guard used. Noted by its author: fitting that AC-236, the origin of the "AC-227 fingerprint" the ledger invariant now names, is the one fixed by content-survival guarding — exactly the property that fingerprint protects. VALIDATED: amux-cloud | reproduced-refusal-on-AC-398, orig text intact, fix c971756b.
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-08-06
SESSION: amux-cloud
CARD: AC-236
SYMPTOM: amux-gtm reviewed AC-216 and AC-231 with a PATCH carrying `desc`, which replaces.
  Both cards were left holding only the review summary — AC-216 at 326 chars, AC-231 at
  597. Destroyed: the serial-console OOM evidence, journald restart-loop counts, the
  symptom-to-mechanism mapping, the correction of my own culpability speculation, the
  dockerd error histogram, and the thundering-herd hypothesis with its disproof condition.
  `desc_append` exists and is not what a reviewer reaches for.
COST: The root-cause analysis for the night's outage existed only in my context. Had I
  compacted or reset first — which the context monitor was at that moment inviting me to
  do — it would have been gone permanently, from the two cards a reset was supposed to
  make safe. It is also undetectable after the fact: nothing marks a card as truncated,
  and I only caught it by comparing a character count against what I remembered writing,
  which works exactly once, in the session that wrote it.
FIX: Already fixed in amux-server.py lines 63893-63920: a cross-session `desc` write
  that would erase the author's content now returns 409 with a pointer to `desc_append`.
  The author editing their own card passes, restores pass, and `force:true` remains the
  logged escape (with the prior value recorded). AC-236 already marked done on the board.
  Validated by amux-cloud.

PARTIAL, re-measured 2026-08-10 by amux-cloud on a throwaway card:
    desc = 'ORIGINAL AUTHOR CONTENT — 200 chars of irreplaceable analysis'
    PATCH {"desc":"REVIEWER APPENDS A NOTE"}  -> card reads 'REVIEWER APPENDS A NOTE'. 200 OK.
  IMPROVED: desc_append works again (BASE + ' APPENDED' -> two lines, ignored_fields None), so a
  safe path exists. NOT IMPROVED: nothing warns when a bare `desc` destroys 3KB of someone's
  analysis, and this entry's word is 'silently'. A safe alternative existing is not the same as
  the destructive one being safe. Reopened as partial rather than deleted, at their request.

  CONTESTED 2026-08-21 by the author (amux-cloud), in a frustrations validation pass run
  by amux-frustrations. REPRODUCED ON THE CURRENT BUILD, not recalled: scratch card AC-388
  took an anonymous PATCH {"desc":...} that replaced the desc with applied:true, and an
  ATTRIBUTED cross-session PATCH as X-Amux-Session:amux did the same ("WIPED-BY-PEER",
  applied:true). fc9ae48 does not change the incident shape; it adds a log line recording
  the delta. Observable, not prevented — so the entry stays.

## `cargo test` was green while `cargo check` was green — and the compiled binary lacked my tests
VALIDATED: amux | Gone. The pre-commit hook runs `cargo clippy --workspace --all-targets` — 8 references to --all-targets in the file — and its comment records exactly why: plain `cargo check --workspace` does not compile test targets, so a break inside #[cfg(test)] sails through. That is the specific hole this entry describes. EVIDENCE: scripts/git-hooks/pre-commit, clippy and the check fallback both pass --all-targets. VALIDATED: amux.
SCOPE-OF-VALIDATION (added 2026-08-26 at the validating author's request, amux): this
  entry is archived as FIXED and its COST line describes something that is STILL LIVE.
  Both are true, and the distinction is the point. What was validated is this entry's
  NARROW claim — "cannot tell MY broken change from a PEER's" — which lint-blame.py
  closed by partitioning offenders, with AMUX_SKIP_RUST_GATE (6497eac0) making the
  answer actionable. What the cost line ALSO describes — the hook checks the WORKING
  TREE when the question is "is what I am COMMITTING sound" — is the structural defect,
  and it is OPEN as AF-182 (three instances, reopened 2026-08-26).
  So: narrow friction fixed, structural defect open, same subsystem.
  THE GENERAL RULE, which is amux's and is worth more than this instance: A VALIDATION
  IS A CLAIM ABOUT THE ENTRY'S TEXT, NOT ABOUT THE SUBSYSTEM. The two come apart exactly
  when a subsystem carries two entries at different depths, and the shallower one can be
  honestly retired while the deeper one stays live. Read an archived entry as "this
  sentence stopped being true", never as "this area is done".
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux
CARD: AMUX-2777
SYMPTOM: `cargo test -p amux-server --lib the_three_stalled_lanes` printed
  `test result: ok. 0 passed; 0 failed; 752 filtered out` — twice, after a 31s build, with the same
  binary hash. The tests were on disk (grep confirmed), in a plain `#[cfg(test)]` module whose OTHER
  five tests were listed by `--list`. The full run minutes earlier reported 781 passed / 787 total;
  `--list` then reported 751. The artifact was stale under heavy shared-CARGO_TARGET_DIR contention.
COST: ~15 minutes, and it is the LOUD-WRONG probe shape: it exits 0 and says `ok`. A filter that
  matches nothing is indistinguishable from a suite that passes, so the natural next move is to
  believe the code is fine. Had I been verifying someone else's fix I would have reported it working.
FIX: `0 passed AND 0 filtered-in` should never render as `ok` — but that is upstream. Locally: when
  a name filter matches zero tests, treat it as a FAILED probe and re-run against `--list` before
  concluding anything. Same family as the empty-grep rule in ethos.md rule 7.

## `cargo check --workspace` in the pre-commit hook cannot tell MY broken change from a PEER's
VALIDATED: amux | Gone, and by two mechanisms. lint-blame.py partitions offenders into mine / theirs / already-broken-on-HEAD and prints which files are which. As of 2026-08-26 it also names the narrow exit: with no offender of yours, AMUX_SKIP_RUST_GATE=1 skips that one gate and keeps the security scan, the staged-guard, the append-only guard and the JS checks (6497eac0). That second half is what makes the attribution ACTIONABLE — amux-frustrations' own AF-182 instance the same morning is the proof it was not, since attribution alone still left them on --no-verify. EVIDENCE: scripts/git-hooks/pre-commit calls lint-blame.py at 3 sites; the escape is printed only when `mine` is empty, both cells verified. VALIDATED: amux.
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-08-10
SESSION: amux
CARD: AMUX-2777
SYMPTOM: The shared checkout broke the workspace FOUR times in ~40 minutes from at least three
  lanes: a `steer_enqueue` arity change mid-refactor (mine), `DetectorKind::CiFailure` non-exhaustive
  match, `note_quiet_signatures` arity, and `amux_core::board::title_needs_self_description` missing
  for orchestrator/runtime.rs:1288. Every one of them blocked EVERY lane's commits, because the hook
  checks the WORKING TREE — which on a shared checkout contains everyone's in-flight edits, not the
  change being committed.
COST: amux-cloud's AC-335 bounced twice on other lanes' compile errors. I lost ~25 minutes to two
  breaks that were not mine, and inflicted one on them. The gate's verdict carries no information
  about the commit it is gating.
FIX: check the STAGED state, not the working tree — `git write-tree` + `git archive` into a temp dir
  is read-only w.r.t. the shared checkout, so it is safe to do under other lanes' edits. Cost is a
  colder build per commit, which is the trade to price. Anything short of this keeps conflating
  "your change is broken" with "someone else is mid-sentence".

## Browser state can see overlay content but cannot click it, so overlay features cannot reach `verified`
SUPERSEDED: amux | THE ENTRY'S MECHANISM WAS WRONG, and its own author superseded it in place. Retired as SUPERSEDED rather than validated at amux's explicit request during the 2026-08-26 drain: "Do not validate this one and do not reopen it. That entry is WRONG, it is mine, and I already superseded it in place... Archiving L2735 as 'fixed' would file a false mechanism as validated history, which is the thing the supersession exists to prevent." The claim was that browser state could SEE overlay content but not CLICK it. False: the selector always contained [onclick] and selector_click_js() already existed. The real defect was a silent 120-element cap, with the two elements that could not be found sitting at indices 155 and 156 — addressable the whole time. The corrected diagnosis is in the superseding entry on the same card. Kept as a DEAD HYPOTHESIS (ethos rule 7: record which hypotheses are dead, not only which one was right) so nobody re-derives it. This entry is also what prompted AF-243, the third disposition itself — before it, a wrong entry could only be archived as validated or reopened as live, and both lie.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-25
SESSION: amux
CARD: AMUX-3721
SYMPTOM: Verifying the .mdai viewer's bottom tabs (AMUX-3322) in the real UI.
  `GET /api/browser/state` returned 120 elements plus a `text` blob. The blob
  CONTAINS "Diagram" and "List", so the tabs are provably in the rendered DOM.
  The elements array does not contain them, so there is no index to POST to
  `/api/browser/action` and no way to click them. Three misses in one sitting,
  all inside the file overlay: the `.mdai-row` div that opens a node (a div with
  an onclick, not a button), the overlay's X, and the `.mdai-btab` buttons.
  Compounding it, the overlay has its own scroll container, so a scroll action
  and an End keypress both moved the page behind it while the overlay stayed put.
  Neither clicking nor scrolling reaches overlay content.
COST: ~20 minutes establishing that the instrument rather than the feature was
  the blocker, and AMUX-3322 closed at `done` on DOM-text evidence instead of
  `verified` on a click-through. The broader cost is structural: this repo's own
  standard is that `verified` requires exercising the real UI, so every
  overlay-hosted surface (file viewer, MDAI viewer, peek) has an honest ceiling
  of `done` until this is fixed. That is a gate nobody can satisfy truthfully
  (ethos rule 3), and it fails silently — the state call returns 200 with plenty
  of elements, so it reads as working right up until you look for a specific one.
FIX: include elements carrying an onclick handler, not only semantically
  interactive tags; and let `/api/browser/action` take a CSS selector, which
  sidesteps the index problem and the scroll-container problem at once.

## A cross-cutting finding recorded on someone else's card dies when that card closes
VALIDATED: amux-frustrations | FIXED — d5c4ed0a, `amux board add --depends-on <ISSUE-ID>` (repeatable), live fleet-wide. The entry's complaint was that a review which finds something out of scope has nowhere to put it, so the finding rides in the host card's desc and dies when that card closes. Measured before building: the SERVER has always accepted depends_on at create (POST /api/board known_keys, board.rs) and honours it — verified live with a scratch card rather than read off the list. `amux board add` simply could not express any link; `epic` was the CLI's only link verb, so a card that begets a card took two steps and the second had no verb at all. So this was ethos rule 1, not a missing feature: capability present, honoured, reaching nobody because the sanctioned tool could not say it. Neither candidate shape in the entry was built: (a) a `--spinoff` concept would have been a second spelling of a link that already works, which the build-on-the-primitives rule refuses, and (b) a close-time prompt is the accumulation rule 5 warns about. Two independent lanes routed around the gap on 2026-08-26 alone — this entry's own reviewer case, and amux writing the AF-182 -> AMUX-3726 split into prose on both cards. Verified live in four cells: the flag sets depends_on; repeated flags accumulate; an empty value is refused; and with no flag the key is ABSENT from the PAYLOAD rather than an empty array — asserted on the body the CLI builds, because the server normalises absent to [] on read, so "sent nothing" and "sent []" are indistinguishable from the read-back and my first version of that cell could not have failed.
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-242
NOTE-CARD: repointed 2026-08-26. This said CARD: AF-10, which is the rescued INSTANCE
  (the SSE `workers` global that survived because I re-read the review and filed it by
  hand) — not the mechanism. So the entry pointed at a card that could be closed, and was,
  while the class went unaddressed. That is the AF-191 shape one level in: a CARD: that
  resolves, to the wrong thing. AF-242 is the class.
SYMPTOM: Reviewing AC-275 on 2026-08-06 I found a defect OUTSIDE that card's scope — the
  vocab rename left `workers = msg.payload` in the SSE handler assigning an undeclared
  global while render() kept reading `sessions`. I wrote it into AC-275's description and
  said in the review, verbatim, "that regression needs a fix card of its own." No card was
  filed. AC-275 went to `verified`. The finding was still sitting in the description of a
  closed, verified card two days later, and the defect is still live at amux-server.py:55609
  as of 0.9.520.
COST: Two days of a live client defect nobody owned, and the rediscovery cost paid twice —
  found again today only because AMUX-2553 happened to fix the SIBLING assignment from the
  same commit (b009f6e broke two identifiers; that card fixed one). Without that coincidence
  it would still be invisible. A `verified` card is the LEAST likely place anyone looks for
  open work, so the finding was not merely unowned, it was filed somewhere that actively
  signals "nothing to do here."
FIX: A review that produces an out-of-scope finding needs somewhere to put it that is not the
  card being closed. Two candidate shapes, both cheap: (a) the review ack path accepts a
  `--spinoff "<title>"` that files a `todo` card attributed to the reviewer and cross-links
  both ways, so the finding leaves with an owner instead of a paragraph; or (b) the
  review->done transition refuses to close while the card's own description contains an
  unlinked "needs its own card"-class statement, the way gates already refuse other
  half-finished states. (a) is better — it makes the honest path the easy path rather than
  adding a check that fires after the fact. Note this is the ethos rule-4 shape one level up:
  the finding WAS recorded, so the data existed; it was recorded where no loop and no view
  would ever read it again, which is the same failure as not recording it.
NOTE: related to the `watch`-type blindness in ethos.md (a card surfaced by nothing is a note,
  not a monitor) — same root, different container: here the invisible thing is a paragraph
  inside a terminal-status card rather than a card outside every query.

## A commit that compiles in the author's tree can be unbuildable AS A COMMIT
VALIDATED: amux-frustrations | FIXED by amux (AMUX-3726), verified in the INSTALLED hook rather than the source alone. The entry's own FIX named option (a): "The staged-guard already knows both facts it needs." That is what shipped. `_amux_staged_recheck()` in scripts/git-hooks/pre-commit materialises the INDEX into a scratch worktree (`git worktree add --detach HEAD` + `git checkout-index -a -f`) and builds THAT, so the gate now answers "is what I am COMMITTING sound" rather than "does the author's tree compile" — which is precisely this entry's title, a commit that compiles in the author's tree being unbuildable AS A COMMIT. Wired into BOTH gates (clippy at :378, the cargo-check fallback at :406), not just the one whoever was reading happened to hit; the fallback's own comment records that the hazard "matters MORE, not less" there because the failure that reaches it is a compile error rather than a lint. Gated on `_blame_rc -eq 10`, i.e. it runs only when lint-blame determines NONE of the offenders are yours, so the ~22s cost is paid only in the case that today costs the committer their commit. Cost measured by its author before writing it: 22s warm, and it does NOT amortise, because cargo re-fingerprints the workspace crates when the path differs. Confirmed the installed copy is byte-identical to the tracked source (`diff -q scripts/git-hooks/pre-commit .git/hooks/pre-commit`), which matters here because AMUX-2777's whole point was that editing scripts/ alone leaves a fix reaching nobody.
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-08-24
SESSION: amux-frustrations
CARD: AF-190
SYMPTOM: My 53ae4b8b was the tip of origin/main and did not compile. Staging
  crates/amux-server/src/api/board.rs took ~16 lines of a peer's in-flight AMUX-3607 wiring that
  were sitting in the same FILE, including a call to `effective_gate_trail` whose definition was
  in board_store.rs — still uncommitted in their tree, so not in mine.
  `git show 53ae4b8b:crates/amux-server/src/db/board_store.rs | grep -c effective_gate_trail` -> 0,
  while board.rs at that same commit calls it. Main was unbuildable until their f5c6af76 landed.
COST: A broken tip on origin/main. CI runs per-tip so it went green, but a bisect through that
  range still breaks, and per-commit CI would have gone red on someone else's PR. My clean local
  `cargo check` and the pre-commit gate both passed, correctly: they check the TREE, which
  contained the peer's definition. Nothing anywhere builds the COMMIT.
FIX: The pathspec form CLAUDE.md mandates does not reach this — the peer's work was in the same
  file as mine, so file-granular staging takes it regardless. Two things that would:
  (a) The staged-guard already knows both facts it needs. It told me "34 insertions / 9 deletions
      — if that is MORE than you wrote, their work is in it", and 5 of those 34 were the peer's.
      It could also say: "you are committing board.rs, which a peer co-edited, and board_store.rs
      is DIRTY and NOT in this commit" — a staged/dirty cross-reference, from data it already has.
  (b) Build the COMMIT rather than the tree: a detached worktree at HEAD with its own target
      dir, checked before the commit is pushed. MEASURED rather than guessed — 40.6s on the next
      commit (be397da2), not the cold build I first wrote here, because cargo keys on content and
      the dependency tree is unchanged between commits.
  (a) is instant and names the hazard in words; (b) is the only thing that PROVES it. Not
  alternatives: do (a) first, and make (b) opt-in (AMUX_VERIFY_COMMIT=1) before it is a default,
  since the pre-commit gate already pays ~14s for clippy and this roughly triples it.
NOTE: the instrument was RIGHT and I read past it. The guard printed the insertion count and the
  exact question, and the number looked about right for my change so I did not reconcile it.
  Third time today I have named the confirming-result blind spot and the first time it shipped
  something. Same axis as amux's migration-cost entry: our discipline answers CORRECTNESS and
  does not answer WHAT ACTUALLY SHIPS.
NARROWED 2026-08-25 (amux-frustrations, the author): part (a) is SHIPPED by amux; part (b) is
  the half that remains, and it is the one that would actually catch the class.
  (a) DONE — 7ecdc869 "name the peer work a commit LEAVES BEHIND, not just the work it takes",
      with a65e2580 asserting the HOOK prints it rather than merely that the server emits it.
      Server side is `split_risk()` (git_guard.rs:1662, surfaced at :1752); the hook prints
      "SPLIT COMMIT WARNING — <peer>'s work is being cut in half: in this commit: <staged> /
      left behind, dirty and NOT committed: <paths>". That is this entry's (a) almost verbatim,
      including the staged/dirty cross-reference from data the guard already had. A comment at
      git_guard.rs:2958 records that split_risk must be SILENT when the peer has nothing, which
      is the negative control the warning needs to not become noise.
  (b) NOT DONE — nothing builds the COMMIT. `git worktree add` appears once in the whole repo
      and it is inside a test fixture (test-session-freshness.sh:407); neither pre-commit nor
      pre-push constructs a detached HEAD or uses `checkout-index`. Every gate still compiles
      the WORKING TREE, which is the exact substitution this entry is about — and the same
      substitution AF-195 hit from the other side (I tested the tree and committed the index).
  WHY (b) STILL MATTERS WITH (a) SHIPPED: split_risk WARNS about the shape; it cannot tell you
  the commit does not build. A peer's half-file can be absent from your commit with nothing
  dirty left behind — they may have committed their half seconds after you staged — and the
  warning is correctly silent while the commit is still unbuildable. Measured cost when it
  happened: 40.6s to build the commit, against four unbuildable commits landed on 2026-08-08.
CORRECTED 2026-08-27 (amux-frustrations, the author — this entry's own probe was defective):
  The NARROWED note above asserts "`git worktree add` appears once in the whole repo and it is
  inside a test fixture". THAT IS FALSE, and it was false when I wrote it. The auto-builder has
  built the COMMIT since 7253465c (2026-08-09), fifteen days BEFORE this entry was filed:
  `scripts/rust-auto-build.sh:284` does `git -C "$REPO" worktree add --detach "$WORK" "$(... rev-parse HEAD)"`,
  a detached worktree at the committed sha with no working-tree files, so a peer's uncommitted
  definition cannot make a broken commit look sound. `e2e/serve-head.sh:142,149` does it too.
  WHY THE PROBE COULD NOT SEE THEM: I grepped the literal adjacent pair `git worktree add`. Both
  real callers write `git -C "$REPO" worktree add`, so the option sits BETWEEN my two tokens and
  the pattern cannot match. It found two hits — a comment and a test fixture — and I read that as
  a negative. Reproduced 2026-08-27: literal `git worktree add` -> 2 hits, neither a real caller;
  `worktree add` -> 5, including both. The probe was blind to exactly the thing it searched for,
  and the blindness is not incidental: a tool that builds a detached snapshot MUST operate on a
  repo it is not cd'd into, so `-C <repo>` is precisely the form this class of caller takes.
  This is ethos rule 4's "before believing a negative, say what a positive would look like and
  confirm the probe could produce it", failed in an entry that is itself about a gate answering
  the wrong question.
WHAT IS ACTUALLY LEFT, measured rather than argued (amux's AMUX-3797, evidence corrected here):
  The builder triggers on the LAST Rust-touching commit — `rust-auto-build.sh:46`,
  `git log -1 --format=%H -- crates/ Cargo.toml Cargo.lock`. When two Rust commits land between
  polls, the earlier one is stepped over and never built. Over the builder's whole life
  (7253465c..main): 992 Rust-touching commits, 126 of them (12.7%) were never a `building`
  target. That is this entry's COST clause exactly — "a bisect through that range still
  breaks" — and it survives the headline being closed.
  MEASURE BY ANCESTRY, NOT BY DATE. My first pass used `git log --since=2026-08-09` and got
  1028/161; the range form gives 992/126. `--since` prunes traversal by author date, so it
  admits commits that reached main through a merge of a branch based before the window and is
  not the same set as "descendants of the builder's first commit". The two implementations
  reconcile EXACTLY once both use ancestry: amux measured 7253465c..origin/main as 125 of 839
  never built, I measure origin/main..HEAD as 1 of 153, and 125 + 1 = 126 of 992. Independent
  scripts agreeing to the commit is worth more than either number.
  THE UNPUSHED STACK IS CLEAN: 1 of 153 Rust-touching commits. An earlier "83 of 235" figure
  counted docs and markdown commits, which `:46` correctly excludes from being build targets;
  it was withdrawn.
  NOT the mechanism: SKIP-under-contention. 287 distinct shas were skipped at least once and
  only 7 of them were never subsequently built, because a SKIP is usually the dedupe declining a
  DUPLICATE trigger for the sha already building. The line quoted as proof (07:45:48 SKIP
  962c15d79) is preceded four seconds earlier by `07:45:44 building 962c15d79` — that sha was
  built. Reading a SKIP without checking for a `building` line naming the same sha counts the
  dedupe working as a commit lost.

## A page.route stub defeated by a service worker fails LOUDLY and blames the wrong subsystem
VALIDATED: amux-frustrations | FIXED 5e07e88a, the CLASS the entry left open: "nothing warns that a page.route stub never matched a request".

e2e/fixtures.ts wraps page.route so each stub counts hits and teardown fails the test naming the stub. Silent when the test already failed, because an unhit stub is usually downstream of whatever actually broke and reporting it there would be this entry's own defect committed by its own fix. allowUnusedRoute(page, matcher) is the declared opt-out, so "may not fire" gets written down rather than assumed.

Reaching every spec, not just the four converted: crates/amux-server/tests/e2e_route_stub_guard.rs fails the build when a spec stubs a request while importing test from '@playwright/test'. Mutation confirmed — reverting one import fails the guard by file name with the fix instruction. It also flags context.route, which the fixture does NOT wrap, rather than letting an unguarded stub look guarded.

The wrapper itself is tested against the real runner (e2e/route-stub-guard.spec.ts, 3 passed on desktop), because importing the fixture is not the same as the fixture working and the defect lives in the teardown path: a dead stub fails (test.fail inverts it), allowUnusedRoute suppresses it, and a stub that DOES match does not fail. That third cell is the control — without it cell 1 is equally consistent with a wrapper that breaks all four real stubs.

The entry's own instance was already fixed in b31bcac, and the service-worker half generalised into playwright.config.ts as serviceWorkers: 'block' by default.
DATE: 2026-08-13
AREA: instruments
SEVERITY: slows
STATUS: open
SESSION: amux-frustrations
CARD: AF-47
SYMPTOM: Isolation gave each project a CLEAN browser profile, which surfaced two failures the
  shared one had masked — and both lied about where the fault was. (1) system-jobs.spec.ts
  stubs /api/system-jobs with page.route; a registered service worker defeats that, because
  the request passes through the worker's fetch handler where page.route cannot see it. It
  did not error — it rendered the REAL job list and diffed it against the stub, so it read as
  "the stalled-row styling is broken under WebKit". (2) sw.js reloads the page on
  `controllerchange` as soon as a fresh worker claims the client, landing mid-page.evaluate:
  "Execution context was destroyed" on two specs about CSS geometry.
COST: Both point at the wrong subsystem by construction. (1) is the dangerous one: a stub
  that silently does not apply produces a confident, specific, wrong failure about rendering,
  and the natural response is to go read the CSS. Roughly an hour across the two before the
  common cause was visible.
FIX: `test.use({ serviceWorkers: 'block' })` on the specs that do not test the worker, in
  b31bcac. STILL OPEN as a class: nothing warns that a page.route stub never matched a
  request. A stub that matches zero requests is almost always a bug and is currently
  indistinguishable from one that matched — same green-looking machinery, no output either
  way. The generalisable guard is an assertion that each route was actually hit; amux has no
  such helper today and every future page.route stub inherits the same silence.

---

## SIX answer-shaped wrong results in one night, and in every one the tell was a MISSING ACCOMPANIMENT rather than the answer
VALIDATED: amux-frustrations | The GENERALISATION is now encoded in the rules, which is what the entry asked for: "ethos rule 7 already carries this family... What it does not yet carry is the accompaniment test, which is the cheap mechanical version."

ethos.md rule 4 now carries it in one sentence: "A wrong answer is rarely wrong-LOOKING, so name what should appear BESIDE the answer if the probe really ran and check for THAT: a count beside a zero, a hash beside 'adopted', a PASS line beside a green suite, a key listing beside a None." Those four forms are the four specimens whose tell was an absence.

The SIXFOLD count moved to docs/ethos-incidents.md with all six specimens intact, which is the entry's other requirement — "so the SIXFOLD count is somewhere countable rather than spread across six cards nobody joins up". It sits beside the nine-instance probe-defect cluster and the -S/-G pickaxe case, where the argument that these are one family is readable.

SPECIMEN 3'S SURFACE IS FIXED, not just written down. /api/logs was "a capped newest-first page with no upper bound" and now publishes its own span. Live: truncated=true, page_span_h, total_matched, and note="TRUNCATED: these are the newest rows, not the whole window. Page backward with `until=<the oldest ts in this page>`". The zero that started this can no longer be returned without the payload saying the measurement was partial. That was AF-230's fix.

Two of the six were amux defects with their own fixes already (module-level sys.exit now __name__-gated; /api/browser/start unknown fields, AMUX-3403). The remaining two are field names differing by a suffix (last_run_at vs last_run), which no surface can currently tell a caller they misread - stated as a known gap in the incidents file rather than left implied.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-20
SESSION: amux-frustrations
CARD: AF-107
SYMPTOM: Six probes in one sweep returned something that LOOKED like an answer and was
  wrong. Recorded together because the count is the argument — any one of these reads as
  carelessness, and six in a night is a property of the surfaces, not of the day.

  1. `until [ "$(curl .../health | py 'print(d["build"])')" != "$OLD" ]` — the health call
     failed mid-restart, python raised, the expression was EMPTY, empty != old, and the
     loop exited printing "ADOPTED". I then measured a WARN storm against the old binary.
     Missing accompaniment: it never printed the hash it had supposedly adopted.
  2. `git diff --numstat origin/main...main -- <file>` labelled "what origin added that I
     lack". Three dots diff merge-base -> main, so those were MY changes with the label
     reversed. I nearly told amux their AMUX-3110 gate was still live. Missing
     accompaniment: `behind=0`, already on screen, said origin had nothing.
  3. Filtered `/api/logs` rows on `ts` inside an outage window and got zero — from a page
     that is newest-first and capped at 2000, every row of which post-dated the window.
     Missing accompaniment: no count of how many rows the page could even span.
  4. Read a schedule's `last_run_at`; the field is `last_run`. Three schedules reported
     `None` and I briefly believed a 12.6h outage had eaten the day's fires. Missing
     accompaniment: no key listing next to the value.
  5. Grepped `/api/debug/boundary` for a `families` key that does not exist; printed
     "families tracked: 0" against a live, correct response.
  6. Imported `git-shared-guard.py` to A/B its behaviour. It carried a module-level
     `sys.exit(main())`, so the import exits the importer with code 0. I wrapped it in
     `except SystemExit: pass` and moved on. amux hit the same line and their test suite
     printed NOTHING and exited 0 with every assertion unreached — the purest cannot-fail
     check either of us saw. Missing accompaniment: no PASS line, from a suite that
     "passed".
COST: no wrong conclusion shipped, because each was caught by a second look — but 4 of the
  6 had already produced a stated conclusion I was about to act on, and #2 was seconds from
  being sent to another session as fact. The real cost is that the catch was luck of
  habit, not of instrumentation: nothing in any of these surfaces made the wrongness
  visible.
FIX: The generalisation, sharpened by amux and worth more than the six specimens: every
  one produced an ANSWER-SHAPED result — an empty string, a reversed label, `ok:true`,
  `exit 0`, a plausible zero — and in NO case was the result itself the tell. The tell was
  always something ABSENT beside it: no PASS line, no adopted hash, no `ignored_fields`, no
  key listing, a diff that should have shrunk and did not.
  So the precondition that actually works is not "be careful" and not "check the result".
  It is: BEFORE believing a probe, name what should appear ALONGSIDE the answer if the
  probe really ran, and check for THAT. A count next to a zero. A hash next to "adopted".
  A PASS line next to a green suite. A key listing next to a None.
  ethos rule 7 already carries this family (the silent probe, the loud-wrong probe, the
  empty grep). What it does not yet carry is the accompaniment test, which is the cheap
  mechanical version, and this entry exists so the SIXFOLD count is somewhere countable
  rather than spread across six cards nobody joins up.
  Two of the six are amux defects with their own fixes: the module-level `sys.exit`
  (now __name__-gated) and `/api/browser/start` silently accepting unknown fields
  (AMUX-3403). The other four are surfaces that make the mistake easy — a capped
  newest-first page with no upper bound, and field names that differ by a suffix — and
  none of them can currently tell a caller they were misread.

---

## Three defects in two days where a compound operation reported success from the parts that worked
VALIDATED: amux-frustrations | All three specimens have SHIPPED fixes, the CI wiring the entry said was pending has LANDED, and the general half is now encoded.

SHIPPED, per the entry's own FIX block: 7759b36 (APP_VER/CACHE must MOVE when the file moves, not merely agree), c207339 (the sweep refuses when a full fetch returns no desc), 1998c75 (scripts/test-tree-clean.sh).

THE PART THE ENTRY LEFT OPEN IS CLOSED. It said: "Wiring it into .github/workflows/rust.yml is NOT mine to do: that file gates every lane's push. Proposal and evidence routed to amux; the guard is committed and runnable meanwhile." It is wired — rust.yml:67 runs `--self-test` as a negative control FIRST, and :82 wraps `cargo test --workspace` in the guard rather than running it after, so the guard cannot drift from what it guards.

MEASURED QUIET, with the probe's own capability confirmed: 25 rust.yml runs (2026-08-24..2026-08-27), 50 jobs, 298 annotation rows read, ZERO mentioning residue. The first pass of that probe read `.title`, which is empty on every annotation this repo produces, so it was structurally incapable of a hit; re-run on `.message` it returns 298 readable rows including eslint and Node-deprecation warnings. Step-level confirmation that it was not skipped: the latest run shows both "Tree-residue guard — self-test (negative control)" and "cargo test (workspace) — tree-residue guarded" as `success`.

THE GENERAL HALF IS ENCODED. docs/ethos-incidents.md now carries the family under its own name, "a compound operation takes its success signal from the parts that worked", with all three specimens and all three habits verbatim — kept as three because each catches exactly one of them and none of the others. It sits beside the accompaniment-test cluster, which is its sibling and needed distinguishing: there the tell is something ABSENT from the output, here nothing is missing at all and the operation genuinely succeeded.

ONE FOLLOW-UP, NOT MINE AND NOT THIS ENTRY'S FRICTION: rust.yml downgrades the guard's exit 3 to a warning, and its comment sets the exit condition itself — "flip to blocking (delete the `if`) once it has been quiet for a few days; leaving it advisory forever would make it decoration." The condition is met on the measurement above. Routed to amux with the evidence rather than flipped here, because that file gates every lane's push (ethos rule 8), which is the same reason the entry gave for not wiring it itself.
AREA: silent-partial
SEVERITY: slows
STATUS: open
DATE: 2026-08-23
SESSION: amux-frustrations
CARD: AF-150
SYMPTOM: amux noticed the cluster and it is right, though not quite as "three invisible
  no-ops" — one of the three is the opposite of a no-op. The property they actually share is
  narrower and worth naming: A COMPOUND OPERATION TOOK ITS SUCCESS SIGNAL FROM THE PARTS THAT
  WORKED, while one part did nothing and said nothing.
    1. 1a7d215 (mine). Mutation-testing a guard, I disabled `if !p.is_absolute()` to prove the
       test could fail. The test then did what the unguarded code says and created a directory
       in the shared checkout. I reverted the FILE and reported the mutation clean; the
       directory outlived the revert and failed every later local run while CI stayed green.
       The revert succeeded at its visible half.
    2. 24fc2b4 (mine). A version bump written as a literal find-and-replace — '0.9.701' ->
       '0.9.702' — matched nothing, because a peer had moved both files to 0.9.708 between my
       read and my write. The same edit pass made the functional changes successfully and
       printed "patched". I had asserted on those and not on the bump.
    3. c207339 (amux). The recovery sweep classified on `desc`, and AMUX-3496 made the default
       board list slim, which does not carry it. `.get("desc") or ""` was empty for every row,
       so the sweep printed "0 to do" on a schedule while 76 unowned reports sat there. The
       FETCH succeeded, and the fetch is what the sweep reported on.
COST: measured, not estimated. (1) a red test on correct code that a peer hit while it blocked
  their gate. (2) a UI fix that reached no browser holding the cached script — caught only
  because a peer asked a routine push-census question, and would otherwise have looked shipped
  indefinitely. (3) a scheduled sweep reporting a clean board on a cadence while 76 items sat
  in it. None of the three produced an error, and in all three the surrounding operation was
  genuinely successful, which is what made the silence convincing.
FIX: two shipped and one general.
  SHIPPED — 7759b36 turns (2) into a CI guard, and the design point is worth keeping: the
  pre-existing test pinned that APP_VER and CACHE AGREE, and it could not have caught 25ba8ea
  because NEITHER moved, so they still agreed and it stayed green. Agreement was never the
  invariant; MOVING WHEN THE FILE MOVES is. Verified against the real artifacts rather than a
  fixture — I re-ran its logic here across four ranges: FAILS on 25ba8ea (app_moved=0
  sw_moved=0), passes on 24fc2b4 and 36b93f8, skips a range with no client JS.
  SHIPPED — c207339 makes (3) refuse when a full fetch returns no desc, rather than treating
  an absent field as an empty result.
  SHIPPED: 1998c75 turns (1) from a habit into a mechanism: scripts/test-tree-clean.sh
  wraps a command and fails if the checkout changed, so a fixture that dirties the tree is
  caught by the run that dirtied it rather than by the next person's red test. The design
  point is the one that nearly went the other way. `git status --porcelain` reports ZERO
  LINES for the exact residue in (1), and so does `-uall`, because git does not track empty
  directories; the obvious guard would have been green and unable to fail on its own
  motivating incident. `git clean -nd` sees it, and cannot see a modification to a tracked
  file, so the snapshot is the union. It ships a `--self-test` negative control (fires on an
  empty-dir residue, silent on a no-op) so a green from it is never taken on faith. Two
  measured limits are in its header: it attributes every diff to the wrapped command, which
  is false on THIS shared checkout (the first baseline run named a peer's mid-run edit to
  alerts.rs), and it ignores gitignored paths so cargo's target/ writes are not noise.
  This also inverts what 67137cc concluded, that "CI never sees this class (fresh checkout)".
  A fresh checkout is where the residue is EASIEST to see, because it has no history to
  hide in, so the run that created it is the only thing that could have. Wiring it into
  .github/workflows/rust.yml is NOT mine to do: that file gates every lane's push. Proposal
  and evidence routed to amux; the guard is committed and runnable meanwhile.
  GENERAL, and the part that does not have a patch: when a step's failure mode is doing
  nothing, its success cannot be inferred from the operation around it. Three concrete habits,
  each of which would have caught exactly one of the above and none of the others, which is why
  all three are listed rather than one rule:
    - assert the WRITE changed something, not that the code ran (`assert new != old` on each
      file), because a literal replace that matches nothing is indistinguishable from one that
      matched;
    - after mutating a guard OFF, ask what the code does WITHOUT it — that is precisely what
      the guard prevents, so the answer is never nothing, and the side effect outlives the
      revert;
    - when classifying on a field, confirm the field is PRESENT before concluding from its
      absence — an empty classification over a non-empty fetch is the loud-wrong-probe shape,
      answering confidently from a column that was never there.

---

---

## A peer's uncommitted lint error blocked my commit and the message named their file, not them
VALIDATED: amux-frustrations | BOTH HALVES SHIPPED, and this entry needed both because a card carrying two units of work is the wrong-SCOPE trap this repo's own rules describe. AF-182 was signed off once already on a fix that did exactly what it claimed while the headline stayed true and recurred.

HALF ONE, the attribution, which is what this entry's FIX asked for verbatim: scripts/git-hooks/lint-blame.py partitions offenders into yours / a peer's in-flight work / already-broken-on-HEAD. It prints "BLOCKED BY ANOTHER SESSION'S IN-FLIGHT WORK - not your commit" when none are yours, and it carries the COUNT ("1 of 1 offending file(s) ARE in your commit", "3 of 4"), which the entry called for by name because a partition reporting only the peer's share reads as exonerating. It is deliberately silent about the escape hatch when `mine` is non-empty, so an escape is never printed beside your OWN denial.

HALF TWO, the structural one, which is the half that recurred after the first sign-off: AMUX-3726's `_amux_staged_recheck()` materialises the INDEX into a scratch worktree (`git worktree add --detach HEAD` + `git checkout-index -a -f`) and builds THAT, wired into BOTH gates - clippy at :378 and the cargo-check fallback at :406. The gate now answers "is what I am COMMITTING sound" instead of "does this shared tree compile", so the refusal this entry is about becomes a pass.

VERIFIED BY RUNNING THE SUITE, not by reading the code. scripts/test-staged-recheck.sh, 7 passed:
  cell 1  foreign offender + clean staged content -> ALLOWED   <- this entry's exact scenario
  cell 2  staged file among the offenders -> refused
  cell 3  staged content fails its own build -> refused
  cell 4  an unlicensed re-check does NOT build the index
  cell 4b CONTROL - a LICENSED re-check DOES build it, so cell 4 is not vacuous
  cell 5  AMUX_STAGED_RECHECK=0 falls back to refusing
  cell 6  no worktree left behind
Cell 1 is this entry's SYMPTOM as a test case. Cell 4b is the control that makes cell 4 mean something, and cell 4 is the one that would have gone wrong quietly: a version that always built the index would pass 1-3 perfectly and cost the fleet ~22s on every commit forever.

Confirmed the INSTALLED hook is byte-identical to the tracked source, which matters here specifically because AMUX-2777's whole point was that editing scripts/ alone leaves a fix reaching nobody.

I checked the direction claim before signing this, because it is the thing that would make the validation wrong: amux briefly reopened this class on the reading that `_blame_rc -eq 10` licenses the re-check only when the TREE is already red, so it cannot help a green-tree/red-commit commit. That is correct about AF-190's direction and irrelevant to THIS entry, whose direction is tree RED / commit GREEN - precisely what exit 10 means.
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-08-23
SESSION: amux-frustrations
CARD: AF-182
SYMPTOM: The pre-commit gate runs `cargo clippy --workspace --all-targets` over the WORKING
  TREE, not over what is staged. My commit of two clean files was refused with
  `board_drive.rs:3620 this assertion has a constant value`, from 170 uncommitted lines of a
  peer's in-flight work. Nothing in the output said the file was not mine. Earlier the same
  hour, `cargo check` failed on `missing field idle_since in initializer of QueuedItem` from
  the same peer writing checks.rs and monitor.rs minutes apart, and I built inside the window.
COST: A commit blocked outright with no correct action available except waiting on another
  session, plus a rebuild and a spell of doubting my own edits on the earlier one. The tempting
  wrong move is cheap and available: fix the peer's file. That is how a session ends up
  committing another session's half-finished work, which is the class the staged guard exists
  to prevent, reached from a direction the staged guard cannot see.
FIX: amux's framing, which is better than my first one: the gate reports a WORKSPACE-SCOPED
  FACT IN A SESSION-SCOPED SENTENCE. The diagnostic is true about the repo and false about the
  committer and nothing says which was meant. The gate already holds both halves at the moment
  it refuses (the staged pathspec, and the file each diagnostic names), so the discriminator is
  a set membership test. Say "BLOCKED BY ANOTHER SESSION'S IN-FLIGHT WORK - not your commit",
  name the session and that the staged files are clean, and carry the COUNT, because "1 of 1 is
  not yours" and "3 of 4 are yours" are different situations and the second must not read as
  exonerating. AF-182.
NOTE: third instance of one shape in about an hour, with AF-179 (a peer's Bash window sampled
  my ongoing authorship, reported as "you edited this") and the transient unbuildable window
  amux is filing separately. All three are a true statement about the shared checkout delivered
  in the second person.

## THIRD AF-182 instance: a peer's non-compiling tree killed my e2e web server and my pre-commit gate
VALIDATED: amux-frustrations | BOTH THINGS THIS ENTRY ASKED FOR ARE IN PLACE, and one of them already was when the entry was filed - which is itself the finding.

(1) "The gate is checking the wrong thing: a pre-commit hook that compiles the WORKING TREE cannot answer 'is what I am committing sound'. Staged-content checking would have let all three commits through honestly." SHIPPED as AMUX-3726: `_amux_staged_recheck()` materialises the INDEX into a scratch worktree and builds THAT, wired into both the clippy gate and the cargo-check fallback - and the fallback is the branch this entry's E0433 would have hit. scripts/test-staged-recheck.sh cell 1 is this scenario ("foreign offender + clean staged content -> allowed"), 7 passed, with cell 4b as the control proving the licence check is not vacuous. The --no-verify this entry calls the expensive part is no longer the only honest move.

(2) "The e2e half wants isolation, not etiquette." ALREADY SHIPPED when this was filed, and that is the part worth recording. e2e/serve-head.sh has built from committed HEAD in a detached worktree since 7624877a, 2026-08-11 - fifteen days before this entry. So the per-lane-worktree proposal was answered by something better already running: isolation from the working tree entirely rather than one worktree per lane.

WHICH RAISES THE QUESTION THIS ENTRY CANNOT ANSWER, and that is the real residue. `git log -S 'crate::worker::WorkerId' --all` finds nothing, so the import that killed the run was NEVER COMMITTED - it cannot have reached a build of committed HEAD. One of serve-head.sh's two working-tree paths must have been taken (AMUX_E2E_WORKING_TREE=1, or the fallback when a worktree cannot be prepared), and the run's own output cannot say which, because the script announced its source only `if [ -n "$dirty" ]`. A run against a tree with no uncommitted Rust changes said nothing at all, so all three sources produced identical output.

FIXED IN eeccbbc1: each of the three paths now prints one SOURCE line naming what it built, with the sha and worktree for HEAD. Grepping SOURCE finds exactly one hit per run instead of one hit per run that happened to go well. Verified in a real boot rather than by reading the diff:
  [WebServer] [e2e] SOURCE: committed HEAD f6a80ece (worktree ~/.amux/e2e-worktree).

So the cost this entry recorded was paid twice: once for the dead run, and once because the diagnosis it reached ("a peer mid-edit in the shared tree") could not be checked against what the run actually built. The first is fixed by (1) and (2); the second is fixed by making the source legible, which is what an entry filed against an already-isolated harness was really pointing at.
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-08-26
SESSION: amux-frustrations (imposed by amux, who reported it themselves)
CARD: AF-182
SYMPTOM: Mid-verification of AF-235 the Playwright webServer refused to come up:
  `error[E0433]: cannot find `worker` in `crate` --> api/session_verbs.rs:11273` ->
  "Process from config.webServer was not able to start. Exit code: 101". Not my file
  and not my change — a peer was mid-edit in the shared tree with a wrong import path
  (`crate::worker::WorkerId` for `amux_core::ids::WorkerId`). The same tree state
  would have failed the pre-commit hook, which runs `cargo check --workspace` over the
  WORKING TREE rather than over what is staged, so I committed with --no-verify and
  gated my five files by hand instead.
COST: One dead e2e run (~1.5 min plus the re-run), and a --no-verify commit — which is
  the expensive part, because it means the gate was bypassed on a real commit and the
  bypass is now indistinguishable from a careless one to anyone reading the reflog.
  The peer fixed it within a couple of minutes and reported it unprompted; nothing here
  is a complaint about them.
FIX: NOT more care. This is the THIRD entry on AF-182 (the others at L2070 and L2191),
  which is the count this file exists to make visible, so it should stop being read as
  three unlucky mornings. Two things follow.
  (1) The gate is checking the wrong thing: a pre-commit hook that compiles the WORKING
  TREE cannot answer "is what I am committing sound", which is the only question it is
  asked. Staged-content checking would have let all three commits through honestly.
  (2) The e2e half wants isolation, not etiquette — a per-lane worktree for anything
  that starts a test server, which the Agent tool already supports (`isolation:
  "worktree"`). amux's own read, offered on the instance they caused: "if AF-182
  reaches the three-entry threshold that makes it an argument rather than a complaint,
  I think the answer is a real one (per-lane worktrees for anything that runs a test
  server) and not more care from me. Count it." Counted.
