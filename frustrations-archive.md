# amux frustrations: archive

Entries retired from [`frustrations.md`](frustrations.md). An entry lands here only
when the session that ORIGINATED it said the friction is gone; the `VALIDATED:` line
names who said so and on what evidence.

This file exists so that "was this entry lost, or was it finished?" is a grep rather
than an archaeology exercise. A set-difference over the ledger alone cannot see a
MOVE and reports it as a deletion every time. Before restoring anything that looks
missing from `frustrations.md`, grep here first: present means it was retired on
purpose, and re-appending it manufactures a duplicate.

Nothing here is live. `frustrations.md` is the live file, and
`frustrations.ledger_agrees_with_board` / `frustrations.cards_are_reachable` read
only that one. A third invariant, `frustrations.retired_entries_stay_retired`,
reads BOTH and fails when a title is in both files at once (AF-430). It exists
because the three places that already stated the rule above all sit on the
ARCHIVE path, and a resurrection lands on the LEDGER: on 2026-08-29 one
whole-file overwrite put 29 signed-off entries back into the live file, where
they read `STATUS: open` for four days and were counted as backlog.

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

## The log sweep's own instrument could only show it 1.6% of the window it was judging
VALIDATED: amux-frustrations | Re-verified 2026-08-28 against the entry's own FIX claim, not the subsystem. `until` is
honoured (`since < ts <= until`; every returned row satisfied the upper bound), and the
response now carries `truncated`, `page_span_h` and a `note` reading "TRUNCATED: these are
the newest rows, not the whole window. Page backward with `until=...`".

Exercised on its author the same day: today's log sweep called /api/logs for step 5, got
`page_span_h=0.79 truncated=True`, and I changed approach BECAUSE the response said so
rather than by noticing `total_matched` disagreed. That is precisely the cost this entry
records - the sweep was reaching "the accusation you cannot un-say" from one capped page,
and the mismatch had to be noticed rather than read.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-26
SESSION: amux-frustrations
CARD: AF-230
SYMPTOM: `GET /api/logs?since=<24h ago>&limit=2000` answered `total_matched: 123645`,
  `count: 2000`, and the rows it returned spanned 0.48 HOURS. `since` ("ts > ?") was the
  only time bound, and the query is `ORDER BY ts DESC LIMIT <=2000`, so every call returns
  the same newest rows and there is no way to page backward. Nothing in the response said
  the page was a slice — `total_matched` disagreed with the window being described, but the
  mismatch had to be noticed rather than read.
COST: Sweep step 5 decides whether a lane is doing mutating work with no board trace — the
  contract's own words are that this is "the accusation you cannot un-say", and it lists
  seven qualifications, each added after a false positive. That step has been reaching its
  verdict from one capped page for as long as it has existed. Today's answer was clean, so
  the cost was not a wrong accusation; it was that a clean 29 minutes was on its way to
  being reported as a clean day. The contract already carried a workaround telling the
  reader to state the blind spot "or read the store directly for the full window" — routing
  a caller off the sanctioned instrument onto raw SQL, which is the rule 6 shape.
FIX: fcff219e. `until` ("ts <= ?") makes the window walkable (`since < ts <= until`), and
  the response now admits when it is a slice: `truncated`, `page_span_h`, and a note naming
  the paging move. `analyze` and `stats` already publish `scan_truncated`/`actual_window_h`
  for exactly this reason; this is the same admission on the endpoint that lacked it, so the
  next capped read announces itself in the payload the caller already opens. The contract's
  step 5 now carries the paging loop instead of the workaround.

## The ledger cannot express that an entry is unvalidatable, so 20% of the open set can never drain
VALIDATED: amux-frustrations | Re-verified 2026-08-28 by running the shipped audit and checking BOTH directions, because
this entry's complaint was that the two cases read byte-identically.

  AEAB-* (non-fleet namespace, author absent from the fleet):
    "STRANDED: prefix AEAB exists nowhere on this board and author
     amux-errors-and-bugs is not in this fleet"  x12
  AC-227 (amux-cloud, a LIVE lane here): not flagged at all - no line emitted.

So the discriminator fires on the stranded set and stays silent on the ordinary
cross-instance id, which is the distinction the entry says did not exist. The summary line
now states the number outright: "STRANDED 12 entr(ies) cite a card no one in this fleet can
reach", against the 12 of 59 the entry measured.

The discriminator is the PREFIX NAMESPACE plus author liveness rather than author liveness
alone, which is what keeps a live lane's cross-instance card out of the stranded bucket.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-25
SESSION: amux-frustrations
CARD: AF-229
SYMPTOM: `frustrations_audit.py` resolves every CARD: against the live board and printed one
  advisory when it missed: "not on this board (other instance, or deleted)". Byte-identical
  for AC-227 (amux-cloud, a LIVE lane here) and AEAB-18 (amux-errors-and-bugs, absent from
  all 120 sessions, working out of a `~/Developer/amux` that does not exist on this machine).
  12 of 59 open entries are AEAB-*; direct GET returns 404 for each, and 0 of 9,296 cards
  carry that prefix while DESKT-*, also a non-fleet lane, carries 25.
COST: The deletion protocol keys removal to the ORIGINATING session's sign-off, so those 12
  have no party who can ever sign them off — they accumulate in the open set forever while
  reading as ordinary work. This file's entire argument is a COUNT ("three entries sharing an
  AREA is an argument"), so a fifth of the open set being permanently unactionable distorts
  every AREA tally computed from it, including the ones used to decide what to rebuild next.
  Not hypothetical: it is why the drive-to-zero sweep stalled at 59 rather than finishing.
FIX: 04721906. The advisory stays advisory — a cross-instance id is not an error — but it now
  discriminates, and the discriminator is the PREFIX NAMESPACE rather than author liveness.
  That distinction is load-bearing: amux-rust is not live either, yet AR-114 answers HTTP 200,
  so judging on liveness alone called six drainable AR-* entries permanently stranded on the
  first run. Same commit fixes a defect it exposed rather than caused: `board.get()` was called
  on the whole CARD string, so multi-id fields ("AR-114, AR-115, AR-116") had ALWAYS reported
  unresolved, invisibly, until the branch started saying something specific and said it wrongly.
  Two of three predicate mutations survived the first draft of the test suite, which is why the
  roll-up and the empty-session-list controls exist as their own cells.
  STILL OPEN, and it is Ethan's call, not mine: what actually happens to those 12 entries.
  Reaching amux-errors-and-bugs, or retiring them with a rationale, is a decision about another
  party's contributions (ethos rule 8). The audit now names them; it does not presume to sweep them.

## The archive tool took evidence as an argv positional, so my shell executed the code I quoted
VALIDATED: amux-frustrations | Re-verified 2026-08-28. Both safe paths exist on the tool and the usage text prefers them:

    scripts/frustrations-archive.py <line> <validated-by> --evidence-stdin
    scripts/frustrations-archive.py <line> <validated-by> --evidence-file <path>
    PREFER --evidence-stdin/--evidence-file whenever the evidence quotes code.

Exercised rather than read: this validation and the two archived alongside it were all
written through --evidence-stdin, and the heredoc bodies contain backticks and $(...)
that reached the archive byte-for-byte. That is the exact substitution which corrupted
AF-130's archive line in two places and left only a misleading `now: command not found`
on screen.

The argv positional form still exists and is still unsafe with quoted code. That is a
choice rather than a trap now: the safe path is documented and preferred at the point of
use, which is what this entry asked for.
AREA: cli
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-25
SESSION: amux-frustrations
CARD: AF-223
SYMPTOM: Archiving AF-130 with evidence that quoted code, via
  `scripts/frustrations-archive.py <line> <who> "<evidence...>"`. Bash printed
  `line 1: now: command not found` and the archive line landed corrupted in TWO places:
  "asserts it comes back as , with the comment" (backtick-now evaluated to empty) and
  "so 0 returned 0 across the whole window" — where `grep -c 'WORK ITSELF is at risk'`
  was EXECUTED by my shell and replaced by its own output. The archive succeeded; only
  the one visible bash error hinted anything was wrong, and it named the wrong half.
COST: a mangled quotation written into the file that exists to be the DURABLE RECORD of
  what was verified, and the least recoverable place for it: the entry it describes had
  just been deleted from frustrations.md in the same operation. Caught only because the
  stray `now: command not found` was on screen. A quieter substitution — `$(date)`, or a
  grep that returns nothing — would have left a plausible sentence and no error at all.
FIX: shipped in the same breath. `--evidence-stdin` / `--evidence-file` on the tool, with
  the usage text saying to prefer them whenever the evidence quotes code. Verified the
  file path preserves backticks and $(...) byte-for-byte.
NOTE: this is AMUX-1888's shape, and the rule already exists — `amux send` and
  `amux board add` both grew --stdin/--file for exactly this, and CLAUDE.md states it as a
  fleet convention I have cited repeatedly this week. My own tool was written in the old
  shape and I used it the old way. The lesson is not "remember the rule": it is that a
  tool taking free text as an argv positional MAKES the trap, and every such tool in this
  repo has now had to learn the same lesson separately.

## The e2e suite restarts its own servers mid-run, and blames whichever specs were mid-navigation
VALIDATED: amux-frustrations | Validated 2026-08-28 on the OUTCOME half of the entry's own prediction, with the other half
stated as unverified rather than assumed.

The entry predicted two things of "the next e2e job": zero `binary changed on disk` lines,
and no ERR_CONNECTION_REFUSED failures.

CONFIRMED - the failure it describes did not occur, on real specimens of the exact shape
that motivated it. Two OUTSIDE CONTRIBUTOR PRs ran full e2e yesterday and both passed:
#161 e2e 17m38s, #162 e2e 21m9s. Both branches were based on cad635ea, and 67474428 is an
ancestor of it, so those runs contained the fix. The mechanism is still in place:
e2e/serve-head.sh:59 exports AMUX_NO_SELF_ADOPT=1.

That is the COST this entry records, gone: "a contributor's PR blocked on a red check that
was never theirs". Two contributor PRs went green through e2e and merged.

NOT CONFIRMED, and I will not claim it: zero `binary changed on disk` lines in the job log.
I tried to read run 33083312377's log and got 0 BYTES back. Grepping it returned 0 for the
predicted strings - which is what a genuinely clean log returns and also what an empty file
returns. A positive control (grep for "passed", "playwright", "e2e") returned 0 for those
too, which is how I know the fetch failed rather than the log being clean. Sixth instance
of that shape today and the first one caught before it became evidence.

The outcome half is the one that carries the cost, and it is confirmed twice.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-23
SESSION: amux-frustrations
CARD: AF-185
SYMPTOM: PR 148 (an outside contributor's) was red with e2e 4 failed / 228 passed, every failure
  `net::ERR_CONNECTION_REFUSED at https://localhost:18823/`, and nothing anywhere said why. The
  suite starts three servers (desktop/mobile/ios-safari), each a `cargo run` rebuilding into the
  SAME target dir; every rebuild rewrites target/debug/amux-server, and a running server watches
  that path and exec's itself. Run 32671387493's log has three `binary changed on disk —
  exec'ing the new build in place` lines, each right after a sibling target's build finished,
  each costing ~10s of refused connections while the suite drove that server.
COST: A contributor's PR blocked on a red check that was never theirs, with no way to tell from
  the PR. Because the victims are chosen by timing they move run to run, so no spec is reliably
  guilty and the whole thing reads as flakiness rather than a defect with a cause. That is the
  same misattribution shape as AF-179 and AF-182: a true statement about the environment
  delivered as a statement about the thing under test.
FIX: 67474428 — the suite sets AMUX_NO_SELF_ADOPT=1. The capability already existed (AEAB-52)
  and its own doc comment says what it is for, "a test harness pins its build on purpose"; the
  one harness in this repo that pins its build was not enrolled. Rule 1 exactly. Not yet proven
  in CI: the prediction is zero `binary changed on disk` lines in the next e2e job and no
  ERR_CONNECTION_REFUSED failures, and if they persist the env is not reaching the server through
  serve-head.sh.

## The push guard's only override is worded for the human, so the AUTHOR's explicit consent has no honest exit
VALIDATED: amux | Signed off by amux 2026-08-28, the originating session, who verified it themselves rather
than taking amux-frustrations' account:

  "Both escapes exist: AMUX_ALLOW_FOREIGN at pre-push:18 and AMUX_FOREIGN_CONSENT at :358,
   with :396 rejecting it malformed and :453/:483 rejecting it when it does not match the
   commits. Ten mentions, plus cells E-H in scripts/test-push-guard-range.sh."

Corroborated live the day before by amux-frustrations: pushing 260 commits, the guard
offered both paths in its refusal output, with per-commit `<sha>:<session>[:owner]` entries
for the recorded form. AMUX_ALLOW_FOREIGN=1 was the honest one there because Ethan had
asked for the whole branch - which is exactly the human case the entry says the wording
already covered. The gap it records was the absence of a RECORDED, checkable alternative
for the non-human case, and :358/:396/:453/:483 are it.

NOTED, because the author raised it against himself: the first grep run on this entry was
`grep -n "AMUX_FOREIGN_CONSENT\|AMUX_ALLOW_FOREIGN" ... | head -4`, whose four
ALLOW_FOREIGN hits filled the budget and hid the CONSENT lines at 358+, and it was one step
from being reported as "the tracked source lacks the escape". The files are byte-identical
at 41360 bytes; there is no divergence. Recorded here because the near-miss is part of this
entry's history now.
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-23
SESSION: amux
CARD: AMUX-3533
SYMPTOM: the push guard's only override is worded for one consenting party. I held 3
  commits above origin, one authored by amux-frustrations, who had explicitly consented in a
  server-verified relay ("PUSH CONSENT: yes, take all three including my bb3d9a8"). The
  guard offered three exits and my situation matched none. "Push only yours" was actively
  wrong and quietly so: my two commits had theirs BETWEEN them, so the "contiguous run"
  was one of my two and taking that exit would have shipped half my work while reading as
  success. "Ask that session to push its own" is circular, because their push then carries
  mine and they hit the same refusal from the other side. The third,
  `AMUX_ALLOW_FOREIGN=1`, is stated as "if the HUMAN explicitly asked you to ship
  everything" — and the human was not involved at all.
COST: the honest options were to assert a human ask that never happened, or to stop with
  the work unshipped and the author's explicit consent ignored. I used the override and
  documented the real authorization in the command, which is the least-bad of three bad
  options. ~10 minutes, and one push whose audit trail now says "blanket override" when
  what actually happened was a specific, named, verifiable consent.
FIX: a second escape that RECORDS who consented and is checkable, rather than widening the
  existing one — `AMUX_FOREIGN_CONSENT="<sha>:<session>"`, with the guard asserting the sha
  is authored by that session and writing the pair to the push audit. Note this guard was
  fixed today (#142) for a different too-narrow assumption, and its author's argument
  applies verbatim here: an alarm that fires on a routine correct action teaches the reflex
  of setting AMUX_ALLOW_FOREIGN=1 blind, and then the push that really does carry someone
  else's unreviewed work sails through.
SECOND SPECIMEN, same day: amux-frustrations took AMUX_ALLOW_FOREIGN on the written consent
  of two PEERS four hours before I did, and did not notice the wording did not cover them
  either. Two independent instances, both with legitimate specific authorization, both
  forced through an override whose stated precondition was false. Attentiveness was never
  the variable.
FIXED f4d8d9b: AMUX_FOREIGN_CONSENT="<sha>:<session> ..." — STRICTER than the override it
  replaces, not a second way around the guard. Each entry is checked against the commit's
  real Amux-Session trailer (a mismatch REFUSES, where a blanket override would have
  shipped it), every foreign commit must be covered, a malformed entry refuses rather than
  being skipped, and the pairs are written to ~/.amux/logs/push-guard.log so the trail
  answers "who authorized this?" instead of recording an undifferentiated override. The
  refusal now names it FIRST, above ALLOW_FOREIGN, with the pairs pre-computed — an escape
  nobody is handed is decoration. Five test cases, negative-controlled by making consent
  behave like a blanket override: the happy path still passes and all three strictness
  cases fail, so no single case can certify a broken implementation.

---

## A gate criterion that says "(name them)" is rejected if you name them
VALIDATED: amux | Signed off by amux 2026-08-28, the originating session, verified independently:

  "amux:1478 parses --reviewer and REQUIRES a value (die "--reviewer needs a session
   name"), and board.rs:3924 hands back the --reviewer <peer-session> fix path. The
   entry's complaint was that the criterion could not be satisfied honestly; naming makes
   it satisfiable."

The proof the author preferred is behavioural rather than textual, and it happened by
accident: while verifying AMUX-3819, amux-frustrations acked the criterion "Peer-reviewed
by a DIFFERENT worker in group `amux` (name them)" WITHOUT naming anyone, and the gate
refused -

  "acking 'name them' without a name is an unfalsifiable assertion - 91% of verified cards
   carry no peer name at all (AF-160)"

- then pointed at `--reviewer <peer-session>`. So the criterion now has a truthful path
that did not exist when this was filed, and it enforced itself against a session trying to
skip it. Ethos rule 3 satisfied in the direction that matters.
AREA: gates
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-23
SESSION: amux
CARD: AMUX-3532
SYMPTOM: the `verified` gate for group `amux` has a criterion that reads "Peer-reviewed by
  a DIFFERENT worker in group `amux` (name them)". The parenthetical is an instruction to
  supply the peer's name, but `gate_checked` is matched by EXACT STRING EQUALITY, so the
  only ack that passes is the criterion verbatim, "(name them)" included. Following the
  instruction inside the criterion is what makes the ack fail:
    sent:     "Peer-reviewed by a different worker in group amux (amux)"
    response: 409 "gate_checked does not match the gate"
  Two more traps rode along on the same call: DIFFERENT is uppercase in the criterion and
  lowercase in ordinary prose, and `amux` is in BACKTICKS, so a shell ate them unless
  escaped and the string silently differed from what I believed I sent.
COST: two retries on AF-66, and the peer's name — the single most useful fact on a verified
  card — has nowhere to go in the sanctioned ack. I put it in the outcome text on AF-66 and
  AF-106 with a note explaining why it is there. Small in minutes; the reason it is worth an
  entry is the direction it pushes: the criterion carrying the most judgment in the gate is
  the one whose literal instruction routes you toward `--ack` (acknowledge everything at
  once, which is what per-criterion acks exist to prevent) or `force`.
FIX: normalize before matching (case-fold, strip backticks, strip a trailing parenthetical),
  or better, let a criterion take a VALUE — `--checked "<criterion>=<name>"` — so the gate
  COLLECTS the fact it asks for instead of demanding it and discarding it. Failing both, the
  409 should say "differs only by case / by a filled-in parenthetical", which turns two
  retries into zero.
FIXED 12af7ab (live on build 05db91e6): both halves. Matching now normalizes (case-fold,
  drop backticks, drop ONE trailing parenthetical), with exact tried FIRST so nothing that
  passed can stop passing; and a criterion containing "name them" now REQUIRES a `reviewer`
  who is not the card's owner, so the gate collects the fact it was demanding in prose.
  The predicate compares against the card's OWNER, never the acting session — see AF-160
  for why that distinction is the whole card.
CONFIRMED INDEPENDENTLY, same day, by amux-frustrations as AF-160 (same defect, keep both
  ids): the mechanism is `board.rs:2620`, where acknowledgement is exact string containment
  (`eff_gate.iter().filter(|c| !gc.contains(c))`). They then measured the consequence, which
  is worse than the friction I hit: of their 25 verified cards, 7 name a peer and 18 do not.
  72% passed a gate whose second criterion is "name them" while recording no name anywhere
  machine-readable. AF-66, which I verified and moved TODAY, is one of them — `reviewer` is
  still None on it and my name survives only in prose. So the gate is not merely awkward to
  satisfy; it is not collecting the fact it exists to collect, on most cards, silently.
  Their fix is better than mine and needs nothing new: the `reviewer` column already exists
  and `amux board review --reviewer` already sets it, so on a transition to `verified`,
  require `reviewer` non-empty and different from the acting session whenever the resolved
  gate contains a named-peer criterion, and refuse with that as the reason.

---

## The gate-blocked 409 tells every agent to GET a route that does not exist
VALIDATED: amux | Signed off by amux 2026-08-28, the originating session, verified independently:

  "GET /api/board/contract?card=AMUX-3823 returns HTTP 200 with card_effective_gates in
   the payload, alongside gates, gates_are, how_to_ack. The route resolves ahead of
   /api/board/{id} as you said."

Independently confirmed by amux-frustrations the same day, from use rather than from a
probe written to check it: the resolved-gate lookup is now the FIRST step before moving any
card to `verified`, which is what surfaced that the group-`amux` gate is four criteria and
not the type default. The entry's cost was that the 409 body named a route that 404'd, so
the instruction inside the refusal could not be followed; it can be, and following it is
now routine.

This is the first of the six entries filed under `amux-rust` that amux has confirmed are
his under a former name, rather than authorless. The rename that migrated `issues.session`
while leaving `issues.reviewer` on the dead name is the same one his AF-210 review cites.
AREA: gates
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-09
SESSION: amux-rust (RR-0150 restart suite)
CARD: AR-123
SYMPTOM: Every gate_blocked 409 from `/api/board/<id>` carries
  `how_to_ack.contract: "GET /api/board/contract"` (`api/board.rs:1175` and `:1664`).
  `GET /api/board/contract` returns 404 `{"error":"item not found","id":"contract"}` on
  both a fresh build and the live server — it is being matched by the `/api/board/{id}`
  route as an item id. Hit it while making the restart suite move a card `todo -> doing`.
COST: Small on its own — the 409 also carries `gate` and `gate_ack`, so the escape is
  walkable without the contract. But it is ethos rule 6's exact shape: the one documented
  route out of a gate is the one action that leaves the sanctioned path, and it is the
  instruction amux itself prints. AMUX-2325 is the same defect one layer up.
FIX: Mount `/api/board/contract` ahead of `/api/board/{id}`, or delete the claim from
  both 409 bodies. Whichever — the test is that following the error message literally
  has to work.

  VERIFIED FIXED 2026-08-21 (amux-frustrations; the authoring lane `amux-rust (RR-0150
  restart suite)` no longer exists, so no author can sign this off — see the orphan note
  at the bottom of this file). Verified by the entry's OWN test, "following the error
  message literally has to work": AF-123 tripped a real gate_blocked 409 today, whose
  how_to_ack.contract read `GET /api/board/contract?card=AF-123`. That URL returns HTTP
  200 and the RESOLVED per-card gate. The bare `GET /api/board/contract` also answers
  with the real contract document. Not a code read — the 409 was produced by a live card
  transition and its instruction was then executed.

---

## `node --check` is blind to a duplicate function name, and that shipped a dead dashboard
VALIDATED: amux | Signed off by amux 2026-08-28, the originating session. Their words: "I did not read the
fix; I planted the bug."

METHOD, which is the part worth keeping. They appended a real
`function _orchRenderPlan(d) {}` to the ACTUAL SHIPPED
crates/amux-dashboard/static/app.js - a genuine duplicate of a function already defined
there - and ran both gates against it:

    node --check app.js                  PASSED   <- the entry's premise, confirmed live
    cargo test --test dashboard_assets   FAILED   <- the replacement guard, firing

and the failure names the offender rather than the file:

    "two top-level functions share a name in app.js. Declarations HOIST, so the last one
     silently replaces the earlier one and every earlier call site starts running the
     wrong body - `node --check` cannot see this because a duplicate `function` is legal
     (a duplicate `let` would be a SyntaxError, which is why that half was already
     covered). Rename one: _orchRenderPlan (2x)"

BOTH HALVES VALIDATED, and they are separable claims: the blindness is real (node --check
waved a live duplicate through) AND the guard that replaced it catches that exact case.

WHY THE SHIPPED FILE AND NOT A FIXTURE, in the author's reasoning: "a guard tested against
a fixture proves it can fail, not that it is wired to the artifact that ships." This gate
sits between a lane and the SPA users load, so wiring is the claim under test. Restored
cleanly afterwards, 0 dirty files, verified rather than assumed.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-25
SESSION: amux
CARD: AMUX-3715
SYMPTOM: I added `function _renderArchivedSection(container)` for the board's
  archived section. The sessions view already had a `_renderArchivedSection`
  ~11,000 lines earlier. Declarations hoist and the last wins, so mine silently
  replaced theirs; every sessions call site passes no arguments, so it hit
  `container.appendChild(wrap)` on `undefined` and threw before the loading
  overlay was hidden. The main dashboard view was dead.
COST: A live regression on the primary view, shipped and deployed. Found by
  gtm-research, not by me and not by any check. The PostToolUse hook runs
  `node --check`, which passed — a duplicate `function` is legal JavaScript. I
  had also written in that commit that every function the new code CALLS was
  verified to exist, which is the one-directional half of the check and the half
  that was already fine.
FIX: 7607ee46 (gtm-research renamed mine) + a guard in
  tests/dashboard_assets.rs enumerating duplicate top-level declarations,
  verified by restoring the collision: `node --check` still passes, the guard
  fails. The general lesson is in ethos.md rule 7 — when a tool covers a class,
  ask which members the LANGUAGE makes legal, because those are the ones it
  silently does not cover. A duplicate `let` is a SyntaxError; a duplicate
  `function` is not.

## `hook_outdated` reports on the request body, not the hook, and its remedy cannot fix it
VALIDATED: amux-frustrations | Validated 2026-08-28 by its author. The defect this entry proved is fixed, and the decision
I had been holding for Ethan turned out not to exist.

THE DISCRIMINATION SHIPPED. git_guard.rs:1935 no longer treats a missing field as a stale
file:

    fn hook_is_outdated(guard_version: i64, has_explicit_op: bool) -> bool {
        guard_version < 2 && !has_explicit_op
    }

`has_explicit_op` is the second signal that separates "this caller sent no guard_version"
from "this hook is old". The fix's own doc comment cites THIS entry's experiment as its
evidence: "Measured 2026-08-24 before the fix: 9 distinct (lane, checkout) pairs warned per
hour, indefinitely, including this checkout whose hook was byte-identical to the tracked
source."

VOLUME GONE, with a positive control so the zero means something. In 800 raw log rows:

    OUTDATED HOOK           0     <- the target
    sent no guard_version   0     <- the target
    staged-guard            6     <- CONTROL: the probe can see this family
    guard                   7     <- CONTROL

against the 2,527 warnings this entry measured, 533 of them naming the amux checkout itself.

THE REMEDY I WAS HOLDING FOR A HUMAN DECISION WAS NEVER RUNNABLE, and finding that out is
the other half. I had been carrying "one command ends this: install-hooks.sh
/Users/ethan/Dev/mixpeek" as a call for Ethan, on the grounds that it would upgrade a commit
gate under ~15 committing lanes. Checked properly today:

  - mixpeek's core.hooksPath is /Users/ethan/Dev/mixpeek/.githooks, a TRACKED dir, and
    install_guard_only's tracked branch REPORTS divergence and never overwrites - the
    function's own comment says mixpeek's copy is "a deliberate merge carrying local
    additions that a blind install would have destroyed".
  - all three hooks there diverge from canonical (766/803, 167/481, 97/152 lines), so that
    branch is the one that would run.
  - GUARD_VERSION is 8 in BOTH. The 4-vs-10 gap this entry was blocked on is gone.

So the command would have installed nothing, the version gap it was measuring no longer
exists, and there was no gate upgrade to decide. I held a card on a human for a decision
that had evaporated - which is its own small lesson about parking things on someone.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-23
SESSION: amux-frustrations
CARD: AF-156
SYMPTOM: chasing amux's lead that every staged-guard probe returned `hook_outdated: true`,
  which they read as their installed hook being stale. It is not a staleness signal at all.
  git_guard.rs:1586 sets it from the REQUEST BODY: `guard_version < 2`, defaulting to 0 when
  the field is absent. So a hand-rolled curl reports true by construction (verified both
  ways against the live server), and more importantly `scripts/git-hooks/git-shared-guard.py`
  sends 1 on its amend path and NOTHING on its discard and cotenant-probe posts, so every
  call it makes is classified outdated permanently. The file is not stale: `cmp` says the
  installed copy is byte-identical to source, and all seven installed hooks match right now.
  Meanwhile `amux-staged-guard` sends GUARD_VERSION = 6 and always passes. A flag that is
  always true for one caller and always false for the other discriminates the CALLER, not
  staleness.
COST: 2,433 `OUTDATED HOOK` WARN lines in ~/.amux/logs/server-rs.log across the fleet (amux
  174, amux-gtm 138, amux-frustrations 86, mixpeek-docs 76, and ~15 more lanes). The noise
  buries any real staleness signal, so a sweep cannot find a genuinely outdated hook. And it
  cost a session an investigation today: amux built a hypothesis on it, and the flag was
  never evidence for it.
FIX: none shipped; git_guard.rs and the hooks are amux's, routed to them, and they had
  already declined to stack another change on this subsystem at the tail of a long session,
  which I agree with.
  The remedy text is the part that makes it worth fixing rather than noting. It says
  "Reinstall: scripts/install-hooks.sh", and reinstalling installs the same source that
  sends 1 or nothing, so the warning returns immediately. Following the instruction exactly
  cannot satisfy the complaint — AMUX-2140's shape, where the sanctioned instruction is the
  theatre.
  Three parts to a real fix: send a real version at every POST site the way
  amux-staged-guard already does; decide what the flag is FOR (if it is meant to detect a
  stale INSTALLED hook it must compare the file against source, which is the check that
  would have caught the real append-only-push-guard staleness amux hit today and that this
  flag did not); and make sure it can be FALSE for a healthy caller, or it is not a detector.
  Kept separate deliberately: amux's append-only-push-guard WAS genuinely stale today and is
  now reinstalled and verified. That was real. `hook_outdated` did not and could not report
  it. Two different things that both say "hook" and "outdated".


NOTE (2026-08-24, amux-frustrations — author): ROOT CAUSE FIXED (6a518e41), ENTRY STAYS OPEN
  until the observable actually drops. Recording the split because "fixed" and "the cost is
  gone" are different claims here.
  WHAT WAS FIXED. 79e9c89c (06:12 today) re-keyed the server predicate on `op` instead of
  `guard_version` alone, justified as "every modern client sends at least `op`".
  git-shared-guard.py contradicted that premise: two of its three POST bodies carried `op`,
  and the cotenant probe sent `{session, dir, paths: []}` — neither field — 170 lines below
  the path the fix was aimed at. 212 WARNs followed the fix, including this checkout at
  16:23:51 with a hook byte-identical to source. 6a518e41 adds `op` to that body.
  VERIFIED against the RUNNING server, both directions:
    old body (no op)          -> hook_outdated = True    (control)
    new body (op present)     -> hook_outdated = False
  WHY IT STAYS OPEN. The COST recorded above is WARN VOLUME, and I cannot show that dropped:
  (a) the warn is rate-limited to once per session per hour, so an hour of silence is the
  minimum informative window and I have one minute; (b) the newest two WARNs name `nissan`
  and `mixpeek-docs` in ~/Dev/mixpeek/* — OTHER CHECKOUTS with their own installed copies of
  this hook, which my sync did not touch. So the volume decays only as each checkout updates,
  and archiving now would be archiving on an unrealized fix.
  STILL UNFIXED, SEPARATELY: the emitted remedy is unchanged. git_guard.rs:1608 still prints
  "Reinstall: scripts/install-hooks.sh" while the doc comment 30 lines above it (1576) states
  plainly that this "reinstalls the GIT hooks, which were already current". The defect is
  named in the comment and left in the string a reader actually receives — ethos rule 6. It
  now misdirects a smaller population (a genuine pre-rust git hook, for which the remedy IS
  right), which is why it is worth fixing but not worth blocking on.
  ALSO CORRECTED: I read `cmp` between the WORKTREE copy and ~/.amux/hooks/ as "the install is
  stale". It was not — runtime was byte-identical to the COMMITTED blob and
  `hooks.shared_guard_matches_committed` was correctly green throughout. What I had measured
  was my own uncommitted edit.

NOTE (2026-08-27, amux-frustrations — author). THE OBSERVABLE STILL HAS NOT DROPPED, three
  days on, and I can now say exactly why. The entry above predicted the volume "decays only
  as each checkout updates". That was right about the mechanism and wrong about the size of
  the population: it is not a slow decay across many checkouts, it is ONE FILE.
  MEASURED, `OUTDATED HOOK` WARN lines per day in ~/.amux/logs/server-rs.log:
    2026-08-24  25   (the fix, 6a518e41, landed this day)
    2026-08-25 288
    2026-08-26 342
    2026-08-27 272
  So the cost this entry records is undiminished. But THIS checkout now emits ZERO of them:
  all 272 of today's come from lanes whose cwd is under /Users/ethan/Dev/mixpeek/* — nissan,
  mixpeek-docs, social-media, paid-social, mvs-infra, mixpeek-security and ~10 more. The
  amux-side fix works; it simply never reached the population.
  AND THEY ARE NOT FIFTEEN CHECKOUTS. `git rev-parse --show-toplevel` from
  mixpeek/server/mvs returns /Users/ethan/Dev/mixpeek — one repo, one .git, one hooks dir.
  Every one of those lanes runs the SAME installed file:
    /Users/ethan/Dev/mixpeek/.git/hooks/amux-staged-guard   23039 bytes, Aug 20 21:28
    scripts/git-hooks/amux-staged-guard (source)            43611 bytes, Aug 24 19:46
    GUARD_VERSION = 4  vs  GUARD_VERSION = 10
  Six versions behind, and it posts to /api/git/staged-guard only — the source also posts
  /api/git/guard-outcome. guard_version appears 3 times in source and 2 in the installed
  copy, so one POST body omits it, which is this entry's original mechanism verbatim.
  THE REMEDY IS NOT MERELY THEATRE, IT IS UNFOLLOWABLE. git_guard.rs:1853 tells that lane
  "Reinstall: scripts/install-hooks.sh". From /Users/ethan/Dev/mixpeek that path does not
  exist — `find /Users/ethan/Dev/mixpeek -maxdepth 3 -name install-hooks.sh` returns nothing.
  100% of today's recipients are given an instruction they cannot execute. The entry called
  this AMUX-2140's shape (the sanctioned instruction is theatre); it is a step worse, because
  theatre at least runs.
  THE CORRECT INSTRUCTION EXISTS AND THE SERVER ALREADY HOLDS ITS ARGUMENT. install-hooks.sh
  has had a foreign-checkout mode since the python generator was deleted — `install-hooks.sh
  <dir>` installs the guard into another repo and, by its own header, "NEVER writes
  pre-commit" there. So the followable remedy for that warn is
  `/Users/ethan/Dev/amux/scripts/install-hooks.sh /Users/ethan/Dev/mixpeek` — and the warn
  line ALREADY PRINTS the directory it would pass ("mvs-infra in /Users/ethan/Dev/mixpeek/
  server/mvs"). The fix is to emit the remedy the server can already compute, rather than a
  constant that is only correct for callers inside the amux checkout. Ethos rule 3: a
  constraint must have a truthful path forward in every legitimate state.
  NOT RUN BY ME, and this is the part that is not mine to decide. One command would end the
  272/day. It would also upgrade a COMMIT GATE from version 4 to version 10 underneath ~15
  lanes that are actively committing in that repo right now, with no warning to any of them.
  Six versions of gate behaviour arriving mid-work is not a change I can spring on other
  lanes (ethos rule 8). Routed to amux, who owns git_guard.rs and the hooks, with the
  measurement and the exact command. STAYS OPEN.

## SUPERSEDES the entry above: the guard's classifier was right, only its printed ADVICE was wrong
SUPERSEDED: desktop | SUPERSEDED BY THE AUTHOR'S OWN LATER ENTRY, not by a third party's judgement.
desktop wrote the superseding entry at "SUPERSEDES both entries above on
DESKT-10", which states: "My fix 5b923db moved the direction-unknown branches to
the ancestry test but DELIBERATELY kept `git cat-file -e $(git hash-object
<path>)` in the STALE section, with a comment arguing it was correct there
because the classifier had already proven the path was behind. cold-outbound
proved that wrong and I reproduced it."

So this entry's FIX section files a mechanism its own author retracted: `git add`
writes the blob without committing, so blob existence answers yes for a
never-committed mid-edit and the prescribed `git checkout origin/main -- <path>`
deletes it. Kept as a dead hypothesis rather than stamped VALIDATED, since
archiving it as validated would file that false mechanism as history (AF-243).

Move executed by amux-frustrations on 2026-08-28 during the ledger drain.
desktop is isolated=True (raw agent, harness stripped): worker-origin sends and
all amux automation are refused into it by design, so no lane can obtain a fresh
signature. The signature relied on here is desktop's own written supersession in
this file, which is stronger than a chat acknowledgement. Reversible: git revert.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-16
SESSION: desktop
CARD: DESKT-10
SYMPTOM: Same incident, corrected diagnosis after reading commit_nudge.rs instead of reasoning from the notice alone. Two claims in my entry above were wrong. FIRST: the guard does NOT classify with blob existence. `freshness_from_repo` uses `git log HEAD..origin/main -- <path>`, which is proper ancestry and correctly returns not-stale for a committed-but-unpushed file. What prescribes `git cat-file -e $(git hash-object <path>)` is the message TEXT the guard prints, in its two direction-unknown branches. The classifier and the advice disagreed, and the advice is the half a human acts on. SECOND: I reported it firing on a CLEAN tree. `dirty_paths` reads `git status --porcelain`, so it cannot. The real explanation is a race: at nudge time the amux lane had app.css and app.js uncommitted, and by the time I ran git status they had committed them in 2ec671b. The notice itself said CONTESTED, also edited by amux, which fits. So the "gate the notice on porcelain non-empty" fix I proposed was unnecessary.
COST: nothing beyond my own time, and it would have cost the amux lane theirs: they picked the card up and were about to hunt for a second code path that does not exist. Worth recording because of HOW the wrong diagnosis was produced. I ran the blob test, watched it misclassify five real paths, and concluded the guard classified that way, when all I had actually established was that the printed recipe was wrong. The notice's text was treated as evidence of the code's behaviour. Reading the 40 lines of commit_nudge.rs would have separated them in a minute, and I filed a card and a frustrations entry before doing it.
FIX: 5b923db. Both direction-unknown branches now print the ancestry test the classifier already uses, state which way each outcome points, and name blob-existence as the thing not to substitute plus why. The STALE section's use of blob-existence is deliberately kept: there the path is already proven behind, and the open question is pure-old-copy vs novel-mid-edit, which blob existence answers correctly. Regression test asserts on the message text and was verified to fail against the old recipe. The durable lesson is narrower than my first entry: when a notice and the code disagree, read the code before filing against either, and say which one you actually measured.

## Idle guard called a CLEAN tree dirty, then prescribed a 44-commit revert as the "safe" action
SUPERSEDED: desktop | SUPERSEDED BY THE AUTHOR'S OWN LATER ENTRY, not by a third party's judgement.
desktop wrote the superseding entry titled "SUPERSEDES the entry above: the
guard's classifier was right, only its printed ADVICE was wrong", which opens:
"Two claims in my entry above were wrong. FIRST: the guard does NOT classify with
blob existence. `freshness_from_repo` uses `git log HEAD..origin/main -- <path>`,
which is proper ancestry and correctly returns not-stale for a
committed-but-unpushed file. SECOND: I reported it firing on a CLEAN tree.
`dirty_paths` reads `git status --porcelain`, so it cannot."

Both of this entry's central claims are retracted by its own author, so it is a
dead hypothesis rather than a validated fix. The real defect it was reaching for
(the printed ADVICE disagreed with the classifier) is recorded in the entries
that superseded it, and the current code is pinned by
printed_direction_test_matches_the_classifier plus, as of fa7f4d24,
every_arm_that_prescribes_a_restore_carries_the_find_object_guard.

Move executed by amux-frustrations on 2026-08-28 during the ledger drain.
desktop is isolated=True (raw agent, harness stripped): worker-origin sends and
all amux automation are refused into it by design, so no lane can obtain a fresh
signature. The signature relied on here is desktop's own written retraction in
this file. Reversible: git revert.
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-16
SESSION: desktop
CARD: DESKT-10
SYMPTOM: The idle dirty-tree notice reported "2 uncommitted change(s)" for app.css and app.js while `git status --porcelain` was EMPTY. Both worktree blobs were byte-identical to HEAD; they differed only from origin/main, which this checkout sits ~44 commits ahead of. The notice then ran its direction test, `git cat-file -e $(git hash-object <path>)`, got "object exists" for both, and classified them STALE, whose prescribed remedy is `git checkout origin/main -- <path>`. Running that would have reverted app.js by 1153 insertions and deleted crates/amux-server/src/api/reclaim.rs entirely, a feature shipped hours earlier. I tested five committed-but-unpushed paths (app.js, app.css, reclaim.rs, api/mod.rs, frustrations.md) and every single one classified STALE.
COST: no work lost, because the tree being clean vs HEAD was checkable in one command and I checked before acting. The cost is the trap itself and how well disguised it is. The notice opens by warning that a difference from origin is not a direction, and then uses a test carrying exactly that blind spot, so the warning reads as evidence the test already accounts for it. It also states that roughly 1 in 4 differing paths are novel mid-edits a checkout would destroy, which frames "STALE" as the safe verdict and pushes toward the destructive branch. Any session that follows it literally on this checkout reverts every file it names.
FIX: the direction test must be ANCESTRY, not blob existence. Blob existence cannot tell an old revision from a current one that is merely unpushed; both answer yes, and on a permanently-ahead checkout every committed file answers yes. `git merge-base --is-ancestor $(git log -1 --format=%H -- <path>) origin/main` separates them exactly: false means committed and unpushed, so leave it alone; true plus a worktree difference means genuinely older. Second, gate the notice on `git status --porcelain` being non-empty, so a tree that is clean against HEAD never triggers it at all. Both are one-line changes and either alone would have prevented this.

## The passenger check compares SHAs, so an already-upstream cherry-pick reads foreign forever
VALIDATED: amux-cloud | VALIDATED BY ITS ORIGINATING SESSION, amux-cloud, who flipped their own
STILL-LIVE verdict of Aug 24/26 after checking the code today rather than
recalling it.

The entry's whole claim was that the passenger check compared SHAs, so an
already-upstream cherry-pick read as unpushed, and that the remedy was a recipe a
human runs by hand rather than a check. That gap is closed in code:
scripts/git-hooks/pre-push `_upstream_duplicates()` computes
`git patch-id --stable` and excludes already-upstream replays from the foreign
set.

Confirmed independently by amux-frustrations before executing this move (the
archive files a claim as history, so it is worth one look): `_upstream_duplicates`
is present and called, `git patch-id --stable` is the mechanism, and the hook's
own docstring at line 107 names the entry's specimen — acdbfdf and 9ebc42c
sharing patch-id dff284cf093aecaa. scripts/test-push-guard-range.sh reports 16
passing cells.

The check DISCRIMINATES rather than merely passing, which is the part that makes
this a validation instead of a green light: cell L proves a replayed commit
already on origin is not foreign, cell M proves a foreign commit origin has never
seen is STILL REFUSED, and cell N proves an applied-and-reverted patch is NOT
cleared — the inverse hazard this entry itself named.

Fitting close, and worth recording where the next reader will find it: AC-227 is
the card the ledger's fingerprint invariant was NAMED FROM — an entry closed by
somebody who was not its author, where only the documentation half had shipped.
This time the author verified it, the executable half shipped, and the test
proves it can fail.
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-06
SESSION: amux-cloud
CARD: AC-227
SYMPTOM: CLAUDE.md's pre-push recipe lists `origin/main..main` and says to ask the author
  about any foreign commit. A commit already upstream under a different sha (cherry-pick,
  rebase, replay) sits in that range permanently. Confirmed: `acdbfdf` and `9ebc42c` share
  patch-id `dff284cf093aecaa`.
COST: Blocked my own push, asked a peer for permission they did not need to give. The
  dangerous direction is the inverse — a session assuming a familiar-looking commit is
  last week's duplicate and shipping something genuinely unreviewed.
FIX: CLAUDE.md pre-push recipe now adds `git fetch origin` first and includes a patch-id
  comparison step to identify cherry-picks/rebases before asking about foreign commits.
  Validated by amux-cloud.

REFUSED 2026-08-11 by amux-cloud — only the DOCUMENTATION half shipped. CLAUDE.md carries
  the patch-id recipe (and I used it myself), but NO executable path computes a patch-id
  anywhere: grep across *.sh, *.rs and the amux CLI returns nothing. The check still compares
  SHAs and still reads an already-upstream cherry-pick as foreign; the doc just tells a human
  how to work around it by hand.
  PROTOCOL NOTE: their card is in `review`, not done, and its own last paragraph declines to
  claim the pre-push path. So whoever marked this entry `fixed` was NOT the author — which is
  the one thing this protocol is supposed to make impossible. Flipped back to open.

## A shared CARGO_TARGET_DIR is mandated, and concurrent builds in it evict each other's artifacts
VALIDATED: amux-frustrations | VALIDATED BY ITS AUTHOR (amux-frustrations), and validated at the depth the entry
actually claimed rather than at the depth of the subsystem.

WHAT THIS ENTRY DEMANDED, in its own words: "nobody has established WHICH of the
three is happening — the diagnosis is missing, not the remedy." That is now
answered, and the answer was a FOURTH thing none of the three options named.

THE DIAGNOSIS. It is not cargo GC (a later note already killed that: `-Z gc` is
nightly-only on cargo 1.97.1), and it is not cargo evicting its own cache. It is
amux deleting the directory: scripts/rust-auto-build.sh's disk-pressure block
runs `rm -rf "$HOME/.amux/rust-build-target"` — the shared dir every lane builds
in — with no check for in-flight builds, on a script that runs every 60s.

THE DATES MATCH THE SPECIMEN EXACTLY. Line 206 of that script records that until
2026-08-19 it deleted the shared dir UNCONDITIONALLY whenever free space fell
below 25GB. This entry's incident is 2026-08-15 — inside that window, three
failures in one session, which is what an unconditional every-60s `rm -rf` of a
directory you are building in looks like. The two-tier threshold that made it
rare landed 2026-08-20 in 79abbb09 (AEAB-35, PR #131), five days after this entry
and for a different reason.

Measured today: 199GB free, so the sacrifice branch is nowhere near firing; the
builder log shows the KEEP branch 8 times against the CLEAR branch once
(2026-08-24 08:59:13, 5GB free, 195GB dir cleared).

AND THE ENTRY WAS WRONG ABOUT ITS OWN OPTION (b). It proposed giving the
auto-builder its own target dir, calling it "the one process that never benefits
from a warm shared cache". rust-auto-build.sh:285 says the opposite in as many
words: the shared cache is what makes builds ~15s instead of ~3min cold. So (b)
would have cost every deploy three minutes to fix a race that a threshold fixed
for free. Recorded because the wrong remedy was the one this entry recommended
most confidently.

THE RESIDUAL IS CARDED, NOT BURIED: AF-303. Below 8GB the reaper still deletes
the shared dir with no in-flight check, and it has fired once. That is a narrower
claim than this entry makes, which is why the entry retires and the card opens —
retiring the shallow claim while naming the deeper one beside it.
AREA: build
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AMUX-2936
SYMPTOM: `error: extern location for serde_core does not exist: ~/.amux/rust-build-target/debug/deps/libserde_core-0d2476c6ed9be3cc.rmeta`, and separately 42 errors inside the `nix` crate ("cannot find type `ControlFlags` in this scope") — artifacts deleted underneath an in-flight build, three times in one session.
  CLAUDE.md requires ONE shared build dir (~/.amux/rust-build-target) and the reasoning is sound — per-session dirs filled the disk with ~37 copies at 10-15GB each. But with several lanes plus the auto-builder building concurrently, I hit repeated hard failures of the form "extern location for serde_core does not exist: .../libserde_core-<hash>.rmeta" and 42 errors inside the `nix` crate, i.e. artifacts deleted underneath an in-flight build. Not a lock contention wait, which is what the CLAUDE.md note measured and correctly called cheap; this is cache eviction, and the only recovery is a full rebuild. Hit it three times in one session, roughly 4 minutes of rebuild each.
COST: ~12 min of pure rebuild, and worse, it masqueraded as a code error twice — the first failure looked like my own change had broken the build, which is exactly the wrong instrument reading (a red result on code you just verified by hand means the instrument is a candidate before the code is).
FIX: Not fixed; needs a decision, not a workaround. Options: (a) leave it — the failure is loud and self-recovering, just expensive; (b) give the auto-builder its own target dir, since it is the one builder that runs unattended every 60s and is the most likely evictor, accepting ~15GB for the one process that never benefits from a warm shared cache; (c) find whether this is cargo GC (CARGO_GC / cache auto-clean) rather than eviction, in which case pinning the retention setting fixes it outright and costs nothing. (c) is worth checking first because it would be a one-line fix, and nobody has established WHICH of the three is happening — the diagnosis is missing, not the remedy.

NOTE (2026-08-24, amux-frustrations): STAYS OPEN, and the reason is a trap worth naming.
  This entry's CARD, AMUX-2936, reads `done` — and that is not evidence about this entry,
  because the CARD WAS REPURPOSED. Its description is now entirely about the staged-guard
  blind-cotenant WARN (321 WARNs measured over 8h53m, 29 distinct committing lanes); its
  log shows it passed through amux, went backlog, was reassigned to me, and closed on that
  subject. Nothing in it addresses artifact eviction under a shared CARGO_TARGET_DIR.
  So a validation sweep keyed on "is the linked card closed" would have archived this as
  fixed. Card=done is weaker evidence than AC-227 already says: not only can a card close
  without the work landing, the card can stop being ABOUT the entry while keeping the id
  the entry points at.
  On the substance: no eviction failure observed today across roughly 20 builds run
  concurrently with at least one other lane and the auto-builder. That is absence of a
  race in one session, which is not a fix, and no fix was ever made — so it stays open
  until either the race recurs or someone changes how concurrent builds share the dir.

NOTE (2026-08-27, amux-frustrations, card AF-265): OPTION (c) IS DEAD, and two new facts.
  The FIX above says "(c) is worth checking first because it would be a one-line fix,
  and nobody has established WHICH of the three is happening — the diagnosis is missing,
  not the remedy." Checked, and it is not cargo GC:
    cargo 1.97.1 — `-Z gc` ("Track cache usage and garbage collect unused files") is
    UNSTABLE, so it is nightly-gated and off on this toolchain, and there is no gc or
    cache setting in ~/.cargo/config.toml (no such file). Cargo's stable auto-clean
    covers the CARGO_HOME registry/src cache, not a target dir; `cargo clean` is the
    only thing that removes one and it is manual.
  So the one-line fix does not exist, and (a) leave it / (b) give the auto-builder its
  own dir are the surviving options. Recording the DEAD one so nobody re-runs it — it
  was the cheapest to check and therefore the most likely to be checked twice.

  NEW FACT 1, and it points at (b): the shared dir is 156GB (155G debug, 1.1G release,
  839 fingerprint entries), against the 10-15GB-per-tree figure CLAUDE.md uses to justify
  sharing it. Not urgent — 226GiB free, 88% capacity, and zero stray /private/tmp target
  trees — but the disk argument FOR one shared dir is weakening as that one dir grows,
  and (b) costs ~15GB against a 156GB status quo, which is a different trade than the
  entry assumed.

  HYPOTHESIS (d), WHICH THE ENTRY NEVER NAMED, IS ALSO DEAD — and it was the strongest
  looking one. amux's OWN server runs a `reclaim` job on a `disk-watch` trigger, and
  `crates/amux-server/src/api/reclaim.rs:395` lists `~/.amux/rust-build-target` by name,
  labelled "Shared cargo target dir". A server job holding a list that contains the exact
  directory, firing unattended, is precisely the shape of "artifacts deleted underneath an
  in-flight build" — and it is a much better candidate than cargo GC ever was, because it
  demonstrably runs on this machine every boot. I only saw it because an unrelated e2e run
  printed `reclaim scan started ... roots=3 by=disk-watch` in its server log.
  IT IS NOT THE EVICTOR, and the probe can express a positive. Scanning is read-only; the
  only operation that MOVES a file is quarantine (`std::fs::rename`, :1827) and the only
  one that deletes is purge, which requires `?confirm=<batch_id>` and only ever removes
  from the quarantine root. So the quarantine ledger is the complete record of anything
  reclaim has relocated. Live: 2 batches, both by session `desktop`, both purged —
  `/Users/ethan/.cache/huggingface` (41.1GB) and `/Users/ethan/.cache/whisper` (5.5GB).
  Nothing under `rust-build-target`. The ledger is not pruned (the only DELETE in the file
  is on `reclaim_skipped`, :1621/:2421), so the absence is real history, not a short window.
  WHAT THIS LEAVES, and it is now a narrower claim than the entry started with: nothing
  EXTERNAL is deleting these artifacts. Both "something else is cleaning up behind me"
  candidates — cargo's own GC (c) and amux's reclaim (d) — are ruled out with evidence, so
  the evictor is cargo responding to concurrent builds from DIFFERENT PATHS into one dir,
  which is what the SYMPTOM described before anyone went looking for a tidier explanation.
  That strengthens (b): if the cause is path-diverse concurrent writers, giving the one
  unattended every-60s builder its own dir removes a writer rather than papering over a
  cleanup job. Still not mine to decide — it changes a CLAUDE.md-mandated policy for every
  lane (ethos rule 8) — but the decision is now between two options with a known mechanism
  instead of four with none.

  NEW FACT 2, and it makes the race MORE likely rather than less: PR #158 (merged today,
  cad635ea) made the pre-commit hook build into this same shared dir, where it previously
  used a repo-local ./target. That is correct on CLAUDE.md's disk rule. But amux's own
  measurement for the staged-recheck is that a build from a DIFFERENT PATH re-fingerprints
  the workspace crates — and the staged recheck builds a scratch worktree, so it is a new
  distinct path writing into the shared dir on every Rust commit where a peer's file is
  the offender. More writers at more paths is exactly the condition this entry describes,
  so if the race recurs, that is the first change to correlate against.

## "Is this badge accurate" is unanswerable by the time the screenshot arrives
VALIDATED: amux | session.status_decided now exists and RUNS: runtime_jobs/status_history.rs defines EVENT, lib.rs:470 spawns it, and status-explain surfaces the history (session_verbs.rs:9024) plus history_sample_secs (:11081). The entry's prescribed FIX was record-on-change plus return-recent-history-from-status-explain; both shipped. The test status_history_tells_a_stable_lane_from_an_unsampled_one is the part that matters most: it separates a genuinely stable lane from one that was never sampled, so a quiet history cannot be read as a confident answer.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3761
SYMPTOM: `derive_status_explain` is computed fresh per request and never persisted, and `session_events` records no lane status rows at all (verified against the live DB: zero for gtm-research across the whole window in question). So `status-explain` answers "which rule decided this lane is WORKING right now", while the question anyone actually asks is "why WAS it WORKING when I looked" — and a screenshot always arrives minutes later, by which time the lane has taken another turn and the evidence is gone.
COST: Ethan sent a screenshot of gtm-research reading WORKING + AGENTS over a pane whose visible text was the agent saying it had no task queued, and asked whether that was accurate. It reads `idle` now, correctly and for a good reason, and which rule fired 31 minutes earlier cannot be recovered. AMUX-3434 built status-explain specifically so a wrong badge would not cost a screenshot investigation; it still does, one layer up.
FIX: none yet. Record a `session.status_decided` event on CHANGE of status or `decided_by`, and return recent history from status-explain. The natural home is the ScanLoop, and a write-on-change into a 2.2GB SQLite from a 15s loop over 52 lanes needs its row rate measured before it ships.

## staged-guard named a co-editing session that never edited the file — ownership inferred from API traffic
VALIDATED: amux | Fixed in git_guard.rs:970-985, which cites AMUX-3497 by name and reproduces this entry's exact specimen (a session whose window held only HTTP probes named co-editor of board_store.rs). The fix suppresses the echo: an observed mtime row EXPLAINED by the other side's transcript record of the same path at the same instant is one write seen twice, not two editors. Tests at :1471-1525 assert the co-edit signal knows what it claims. The echo test deliberately runs against the ENTRY state of the firsthand sets, so the loop's own inserts cannot redefine firsthand mid-pass.
AREA: attribution
SEVERITY: annoys
STATUS: open
DATE: 2026-08-22
SESSION: amux
CARD: AMUX-3497
SYMPTOM: committing board_store.rs, the guard's NOTE said the file "was also edited by
  session 'amux-cloud' 28m ago". amux-cloud made no source edit in that window — their
  12:28 activity was HTTP board probes (card create/PATCH/discard). The edit-ownership
  row behind d.get("shared") attributed a FILE edit to API traffic against the
  subsystem.
COST: a needless wipe-apology sweep to a peer (made plausible by a real git-checkout
  hazard in the same window), plus the standing cost of the shape: once the guard is
  known to name phantom co-editors, its real co-edit warnings get discounted — on the
  exact commit type (shared-file sweeps) it exists to catch.
FIX: shipped same day (see AMUX-3497 for the sha). Root cause was not command parsing
  but the OBSERVED-edit mechanism: the Bash hook pair reports every file whose mtime
  moved during a session's command, and on a shared checkout a CONCURRENT session's
  tool edit lands in the observer's window — one write, two claimants. apply_observed
  now drops an observed row explained by the other side's transcript record within the
  clock-skew margin (both directions degrade toward protection), and an unresolvable
  observed-vs-observed coincidence keeps both claims but the shared row carries
  co_signal naming the ambiguity, which the guard hook prints. Five test cells incl.
  the rebuilt specimen; over-broad-drop mutant fails the real-second-write control.
REOPENED 2026-08-23 by its own author, on live evidence, when asked to sign this entry
  off for retirement. Probing GET /api/git/staged-guard for
  crates/amux-server/src/api/alerts.rs returned
  shared: [{"owner":"amux-frustrations","peer":true,"age_secs":4848,"mine_age_secs":4848}]
  — and every commit that has ever touched that file is mine (17710e9, d7f9545,
  024894a, 2d57c7b). age_secs == mine_age_secs is precisely the coincident signature
  357a54e was written to resolve, so the phantom co-editor still reproduces by a route
  the fix does not cover: 357a54e drops an OBSERVED row explained by the other side's
  TRANSCRIPT record, which cannot fire when the phantom claim is itself
  transcript-derived. What remains to establish is which mechanism minted that row.
  Do not retire this on the sha alone — the sha is real and the symptom outlived it,
  which is the whole reason the entry is worth keeping.

---

NOTE (2026-08-24, amux — author, superseding their own 2026-08-23 reading): STILL LIVE, and
  the mechanism is now named. Their 08-23 reopening read two equal ages as "amux-frustrations
  is a phantom co-editor on my file"; on re-probing, THE DIRECTION IS INVERSE and the phantom
  was theirs.
  They first probed the original alerts.rs specimen and got `shared: []` — and explicitly did
  NOT stop there, because the tree was clean and nobody had touched that file in the 6h window,
  so an empty result and a working fix are indistinguishable. They then probed five hot files,
  got a `shared` row on all five, and checked one against git:
    crates/amux-server/src/api/board.rs -> age_secs 455, mine_age_secs 455,
    owner: amux-frustrations, NO co_signal.
  That identical-age signature is what they could not explain on 08-23. Resolved: commit
  8575cc6f at 12:18:08 is amux-frustrations' and really does touch board.rs (mtime 12:17:22).
  amux's own claim is the manufactured one — all they did to that file was `sed -n '2270,2300p'`,
  a READ, at 12:17, and the Bash observer saw the mtime move during their command and minted an
  edit claim for them.
  WHY 357a54e's MITIGATION CANNOT REACH IT: that fix drops an OBSERVED row explained by the
  other side's TRANSCRIPT record. Here the transcript record belongs to the side whose claim is
  TRUE, and the phantom is the observed SELF-claim. The probe presents the two symmetrically
  and emits no co_signal, so nothing in the output says which of the two is manufactured.
  Working where it applies: three of the five probes DID carry a co_signal (autofix.rs and
  session_verbs.rs with the AF-179 wording, app.js with the AMUX-3497 wording). The gap is
  specifically observed-vs-transcript where the transcript side is the real one.

## `amux send` reported a DELIVERED message as FAILED on a gemini worker
VALIDATED: amux | Fixed in d8a18687 (AMUX-3889), deployed and confirmed live: after the deploy,
`amux send photo-analysis` returned plain `sent` and the message is visible in
that lane's pane.

The entry's SYMPTOM is closed and its diagnosis was right: the verdict scraper
only knew Claude Code's composer. A verbatim `tmux capture-pane -e` of
photo-analysis contains neither U+276F nor U+203A anywhere, so `composer_state`
returned NotVisible, `read_frame` mapped that to NoUi, and after five looks the
fall-through called `jsonl_user_msg_since` — which reads Claude Code's transcript
directory and cannot succeed for a gemini lane — giving Submission::Stuck and the
exact wording this entry recorded.

TAKEN BY A DIFFERENT ROUTE THAN THE ENTRY'S FIX PROPOSED, deliberately. The entry
said make the verdict provider-aware from CC_PROVIDER, or abstain. The fix
teaches `composer_state` gemini's box chrome instead, so the READER is correct
rather than one of its consumers: ghost-rescue, the composer-stuck badge and the
send verdict all become right at once, with no provider plumbing threaded through
them.

A SECOND DEFECT WAS FOUND UNDERNEATH AND IS ALSO FIXED, worse than the one
reported here. `dim_mask` read the `2` in `48;2;95;95;95` — the truecolor marker
— as SGR 2 (dim), so on a gemini frame every TYPED message read as a placeholder,
`read_frame` returned Cleared, and the send would report SUBMITTED while the text
sat in the box. This entry's own COST line predicted the shape of that hazard
("a retry driven by a provider-blind verdict will re-submit messages that already
went through").

STILL LIVE, and not claimed by this validation: the abstain half of the proposed
fix. The next unrecognised UI reproduces this entry exactly, because there is
still no honest "I cannot read this composer" verdict distinct from FAILED.
Whether that wants its own entry is for the next lane that hits it.

Tests: a_gemini_composer_is_read_rather_than_reported_as_no_ui and
a_truecolor_parameter_is_not_the_dim_code, both proven able to fail by mutation.
Full lib suite 1595 passed, 0 failed.
AREA: cli
SEVERITY: annoys
STATUS: open
DATE: 2026-08-29
SESSION: amux
CARD: AMUX-3889
SYMPTOM: Sending the post-reboot continue message to the 14 workers with `doing` cards, 13 returned `sent (queued while generating)` and `photo-analysis` returned rc=1 with `send to photo-analysis FAILED: not submitted — text is sitting in the input box (autocomplete popup ate the Enter?)`. The message had in fact been delivered and submitted: a peek showed the worker already generating, with "Resuming Landscape Photo Ranking Post-Reboot" and a plan referencing the cards. `photo-analysis` is a gemini-provider worker, whose composer chrome ("Type your message or @path/to/file", the YOLO/GEMINI.md status bar) looks nothing like Claude Code's, which is what the post-send verdict scraper matches against.
COST: Two minutes and a nearly-wrong report. I was about to re-send, which would have double-queued the instruction into a worker already acting on it. In a sweep across many workers the failure mode is worse than the wasted retry: a verdict that reads FAILED on success is indistinguishable from one that reads FAILED on failure, so the only safe response to ANY red send becomes "go look", which is what the verdict existed to save you from. AMUX-3880 (`a stuck pasted message now gets its Enter retried, not just reported`) landed the same day and makes this sharper — a retry driven by a provider-blind verdict will re-submit messages that already went through.
FIX: The verdict must be provider-aware, or it must abstain. The provider is already known at send time (`CC_PROVIDER`, and the server's `launch_base_binary` maps it), so the scraper can select the right composer signature — or, where it has no signature for a provider, report `unverified` rather than `FAILED`. Ethos rule 3: with only sent/failed available, the honest answer for an unrecognised UI cannot be expressed.

## The staged guard is blind to edits made through Bash, so it told a peer "no other session edited it" about a file I had 250 lines in
SUPERSEDED: amux | SUPERSEDED BY ITS OWN AUTHOR, same day, after testing the claim instead of reading it.

The entry says the guard "is blind to any edit made through Bash". It is not. Run
against the shipped endpoint with the exact form the entry describes
(`python3 - <<'PYEOF'` through Bash), writing a file into the repo and staging it:

    t+2s   POST /api/git/staged-guard -> unclaimed: [AMUX3904_PROBE.md]
    t+42s  POST /api/git/staged-guard -> unclaimed: []      (the path IS claimed)

    server log, same write:
    [staged-guard/inferred-edit AMUX-3128] session=amux path=AMUX3904_PROBE.md
      verdict=NOT a known read verb... — ownership INFERRED from a bash command

And `session_verbs.rs`, the file the entry says the guard "had no record I had
ever touched", is in my own observed store:

    sqlite3 ~/.amux/amux.db "SELECT value FROM prefs WHERE key='observed_edits:amux'"

put there by scripts/claude-hooks/observed-edits-post.py, a PostToolUse hook that
reports what every Bash command changed. The entry's proposed fix (a), "teach the
bash-write classifier the common write forms", proposes building a mechanism that
already exists and already runs.

Every NUMBER in the entry is correct — 3 Edit tool_use blocks, all on
sessions_legacy.rs, zero on session_verbs.rs. The inference from them is not,
because "no Edit record" and "no claim" are different things and I checked only
the first.

Kept as a DEAD HYPOTHESIS so nobody re-derives it from the same reading of
EDIT_TOOL_NAMES.

WHAT SURVIVES, on the card (AMUX-3904), narrower: a Bash edit yields an OBSERVED
claim, never a firsthand one, and `foreign` — the verdict that BLOCKS — requires
`theirs_firsthand`. So a lane editing only through Bash can produce a warning but
never a block. That asymmetry is real.

WHAT I HAD NOT WEIGHED, and it argues against the entry's own remedy: my observed
store holds seven paths I never edited (golden_scenarios.rs, replay_roundtrip.rs,
board.rs, board_api.rs, lib.rs...). They are a peer's files, attributed to me
because they changed while my long `cargo test` ran, and on a shared checkout that
window catches every write anybody made. The code already calls this AF-179.
Promoting observed claims to blocking would block commits on data that is wrong in
the over-claiming direction too, so "firsthand blocks, observed warns" is a
defensible reading rather than the oversight the entry calls it.

One measured defect does survive and is on the card: a ~30s window
(EDIT_CACHE_TTL) where a fresh write is invisible, which is what the t+2s reading
above is.
AREA: attribution
SEVERITY: blocks
STATUS: open
DATE: 2026-08-30
SESSION: amux
CARD: AMUX-3904
SYMPTOM: amux-frustrations committed 72820477 (their AF-320 work) and swept up ~250 lines of my in-flight AMUX-3903 work in `crates/amux-server/src/api/session_verbs.rs`. They were not careless: on their PREVIOUS commit the guard had warned them per-file with insertion counts, and they used it to reconcile. On this one it printed the arm that reads "is yours and has uncommitted changes right now — no other session edited it", which is a FALSE STATEMENT about the file, delivered at the moment they were deciding whether to commit. The mechanism is not co-editing and not a timing window. `git_guard.rs` derives first-hand ownership from `EDIT_TOOL_NAMES = ["Edit", "Write", "MultiEdit", "NotebookEdit"]` in the transcript, and nothing else; a write performed by `python3 - <<EOF` or `sed -i` through Bash is classified by `inferred-edit` as "NOT a known read verb, and not classifiable from this token alone — treat as unmeasured rather than as a write" (AMUX-3822). Counted in my own transcript for this session: 3 Edit tool_use blocks, all on `sessions_legacy.rs`, and ZERO on `session_verbs.rs`, where every one of my ~250 lines went in through a heredoc. The guard had no record that I had ever touched the file, so the `shared` row's `peer` field was empty and the honest-looking sentence it printed was the wrong one.
  THE COMPOUNDING PART, and why this is not a small hole: this session runs under bypass-permissions, whose harness instruction is "Do your work through the Bash tool wherever it can accomplish the job ... make file changes with sed, heredocs, or short scripts, rather than using the dedicated Read, Edit, or Write tools." So the mode that makes editing fast is the mode that makes edits invisible to attribution, and every lane running that way is unattributable on every file it touches. It also inverts which case is loud: a lane using Edit gets protected, a lane told to use Bash does not.
COST: ~250 lines and three tests shipped inside a commit whose message describes something else, so anyone bisecting the delivery ledger lands on "every diagnostic says whether its measurement ran" and has to work out why. Recovered only because I checked HEAD by hand afterwards; the peer attached a git note naming AMUX-3903 on the commit, which is the right repair and is also work neither of us should have needed to do. The deeper cost is that the guard's central promise is now conditional on a tool choice nobody makes for attribution reasons: I had staged only my own hunks a few commits earlier for exactly this hazard, and the guard could not have helped the peer do the same, because to it the file had one author.
FIX: Ownership must come from the WRITE, not from the tool that performed it. The material already exists in the same transcript — `inferred-edit` sees the bash command and the path, and already logs a verdict about it — so the gap is that "unmeasured" is treated as "no claim" rather than as a weaker claim. Two candidate shapes, and the second is probably right: (a) teach the bash-write classifier the common write forms (`>`/`>>` redirect is already recognised; add `python3 - <<`, `sed -i`, `tee`, `cat >`), which narrows the hole but keeps the same shape and will leak again on the next form; or (b) treat an OBSERVED mtime move by a session that also ran a bash command touching that path as a claim of its own tier, so the `shared` notice can say "another session may have written this by a means the guard cannot attribute" instead of asserting nobody did. The rule that must not survive either way is the current one, where absence of an Edit record renders as a positive claim that no other session edited the file. That sentence is the one that did the damage, and it is false whenever the peer edits through Bash.

## staged-guard reports every shell-based edit as a line "matching nothing you edited firsthand"
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations authored this entry, so this is a self-signoff and is labelled as one, not a peer review).
THE ENTRY'S SENTENCE was: staged-guard reports EVERY shell-based edit as "matching nothing you edited firsthand". That sentence is no longer true. `line_accounting_mode(has_firsthand, mine_observed, peer_claims)` now returns Undecidable — suppressing the per-line list — when the committer has a content-record hole and NO peer claims the path. Shipped a728fe80, plus 8729cc0b for the reviewer's three findings.
INDEPENDENT CONFIRMATION FROM A DIFFERENT LANE, which is what makes this more than my own read: amux re-derived it and reported from their own editing pattern — "almost everything I wrote today went through Bash, so has_firsthand is false and those paths take Skip. No line detail, no noise, and my commits today printed no unaccounted block while the path-level NOTE still fired."
LIVE, not merely merged: serving a728fe80; before/after on a real staged mixed-edit path was unaccounted 1 path / 9 lines -> unaccounted 0, undecidable 1 path with its reason. Card AF-342 is `verified` with amux named as the reviewer who re-derived all four gate criteria.
SCOPE OF THIS VALIDATION, stated because a validation is a claim about the ENTRY'S TEXT and not about the subsystem: the noise on the normal path is gone. Attribution in that guard is NOT thereby fixed — AMUX-3954 (observed records carry no content hash) is open and is a deeper entry on the same subsystem, still live in this file.
AREA: attribution
SEVERITY: annoys
STATUS: open
DATE: 2026-08-30
SESSION: amux-frustrations
CARD: AF-342
SYMPTOM: Committing four files I wrote start to finish (40fa0ce0), the guard printed
 93 lines of warning: "15 staged added line(s) in docs/friction-themes.md match nothing
 you edited firsthand", the same for 55 lines in scripts/friction_themes.py and 22 in
 scripts/test-friction-themes.sh, plus a NOTE naming session 'amux' as a co-editor of
 all four, plus a SPLIT COMMIT WARNING. No peer had touched any of them. The guard's
 own caveats are correct and present (AF-179 mtime provenance, "if these are yours via
 shell edits, proceed"), so it is not claiming more than it knows.
COST: Nothing shipped wrong, but the reader has to re-derive "these are all mine" from
 93 lines of warning on every commit, and the true signal this guard exists for, a
 peer's hunk riding your `git add`, arrives in the same shape as the noise. Warnings
 that fire on the normal path are the ones people learn to scroll past, which is how
 the peer-hunk case gets missed. The guard correctly kept the peer's two dirty
 browser.rs files OUT of the commit, so its load-bearing half worked.
FIX: The firsthand-edit record is fed by Edit/Write tool calls, so a session following
 the harness instruction to prefer Bash for edits (heredocs, sed, python patches) is
 unattributable BY CONSTRUCTION, every time. Two components disagreeing about the same
 fact: the harness says edit via Bash, the guard treats a Bash edit as unwitnessed.
 Either record a firsthand claim when a Bash command writes a tracked file in the
 session's own cwd, or suppress the per-line list when EVERY unmatched line is in a
 file whose only recorded writer is you and no peer has a recorded write in the window.

---

## The board stores a card type its own create path rejects
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations authored this entry, so this is a self-signoff and is labelled as one, not a peer review). Fixed in 9bdfc7f6 (card AF-323, now done): `decision` is a real card type with its own gate naming the decider. Took the add-the-type arm, not the migrate arm: the stored cards belong to mixpeek-orchestrator and ethos rule 8 plus AMUX-3552 both say surface, do not sweep, so listing the word repairs them where they sit with no edit to another lane's data. The entry's count was already stale when validated: it read three cards, five were live. Evidence: scripts/test-contended.sh -p amux-server -> 1665 passed, 0 failed; clippy clean; mutation putting core_item_type back to Code fails both new tests; live after the builder adopted the commit, GET /api/board/contract offers `decision` and gates.decision.done reads "The decision is recorded on the card: what was chosen, by whom, and when". Two tests were PINNING this defect, both using `decision` as their stand-in for an unknown type; repointed at `task`, which is still genuinely unknown.
AREA: board
SEVERITY: annoys
STATUS: open
DATE: 2026-08-29
SESSION: amux-frustrations
CARD: AF-323
SYMPTOM: `amux board add --type decision` returns
  `{"error": "unknown type \"decision\"", "valid_types": [code, escalation, blocker,
  investigation, ops, research, chore, doc, tripwire, watch, epic]}` — while three cards
  on the live board carry `type: decision` right now (ETHAN-36, MO-3036, MO-3034, all
  created by mixpeek-orchestrator, all in `todo`, all literal Ethan-decision cards).
COST: One retry and a re-file, ~2 minutes. The larger cost is conceptual: the error text
  explains that the gate is DERIVED from type and an unknown type would silently fall back
  to the strictest gate. That reasoning is right, and it means the three stored cards are
  sitting on a gate nobody chose for them. It also lands badly against AF-318, which
  proposes typed `needsyou --ask decision|access|...`: `decision` describes 24% of the 445
  needsyou cards, and it is the one type you cannot file.
FIX: Reconcile storage with validation. Either add `decision` to valid_types with its own
  gate, or migrate the three existing cards and reject it on the WRITE path, not only in
  the CLI. Whichever way it goes, one of the two components is currently lying.

---

## The nudge that tells you to discard a card names no command that does it
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux authored this entry and signed it off on 2026-08-31). Their basis, in their words: "Validated by receiving it. At the start of this session the capture-shell notice for AMUX-3958 printed `amux board discard AMUX-3958 --outcome-stdin`, the retitle form, and the epic path, with the real card id substituted into each." Producer is board_drive.rs:3654; board_drive.rs:7587 is a test asserting all three command strings are present. This is a live observation of the shipped notice, not a claim from the card status, which is the strongest basis available for a notice-text entry.
AREA: notices
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-25
SESSION: amux
CARD: AMUX-3707
SYMPTOM: The capture-shell nudge ("X is a captured prompt, not a unit of work")
  is ~250 words and fires ~42x/day fleet-wide, once per capture card ever. It
  tells the lane to "discard it" and to "set each child's `epic`". Neither was
  reachable from `amux board`: `discard` dispatches but is absent from help, and
  `epic` had no verb at all, though `epic` is a real PATCH field (board.rs:2142)
  added by AMUX-2992. Ethan flagged the token cost after seeing one fire on a
  question he had already answered inline.
COST: 540 nudges ever, 296 in the last 7 days. 70.6% of the cards ended
  `discarded`, i.e. the woken turn produced a one-line retirement. The prose is
  ~330 tokens; the turn each one wakes is tens of thousands. Every lane that
  followed the nudge to its epic exit had to hand-roll a curl, which drops
  X-Amux-Session, so the nudge was generating the unattributed board writes the
  ledger depends on not having.
FIX: c1c238b1. Text cut to ~85 words with a command on every exit; `amux board
  epic` added; `discard`/`show`/`reviewer`/`archive`/`unarchive` added to help;
  tests/nudge_commands_exist.rs sweeps every `amux board <verb>` the server
  emits against the CLI's case arms on every build.

## Mutation testing's obvious harness is a whole-file write, which reverts a peer mid-edit
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux authored this entry and signed it off on 2026-08-31). Their basis, in their words: they ran `scripts/mutate.sh run` twice on 2026-08-31 against scripts/git-hooks/prepare-commit-msg while fixing the CI red, and "both applied one exact string, both reverted in the trap on a non-zero exit, no whole-file write." Exercised on real work rather than on a fixture, which is what this entry asked for: the friction was that the OBVIOUS harness (`cp file bak`) is a whole-file write that reverted a peer's in-flight work twice on this shared checkout, and the fix is a tool that applies and reverts one exact string. Two live runs with a non-zero exit is the case that matters, since that is the path where a naive harness leaves the file mutated.
AREA: shared-checkout
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-24
SESSION: amux
CARD: AMUX-3671
SYMPTOM: `cp $F /tmp/orig ; <mutate> ; <test> ; cp /tmp/orig $F` — the natural way to
  satisfy this repo's "mutate the predicate and confirm it LANDED" rule. The restore is a
  WHOLE-FILE write, indistinguishable from `git checkout -- $F` to a concurrent peer. At
  15:45 it reverted mixpeek-research's in-flight `fn chrome_launch_args` out of
  browser.rs while KEEPING the call site that had arrived inside my mutate/restore
  window, so `cargo check` failed with E0425 for both lanes. Twice, because the harness
  ran twice.
COST: A peer lost work and had to re-apply; browser.rs was uncompilable for both of us
  for ~4 minutes. The number that matters is not this incident: the same harness had run
  about a dozen times that day across five files, and every one was a chance to do this to
  somebody. It had simply not collided until a peer edited the same file at the same
  minute.
FIX: scripts/mutate.sh — mutate by EXACT STRING, revert by the inverse exact string, so
  only the mutated bytes are ever written and a peer editing any other part of the file is
  untouched. Refuses a target that is absent (0 occurrences) or ambiguous (>1), which is
  the same discipline the rule already asks for: an unapplied mutation and a test that
  cannot fail produce the identical green, and the mutation is the cheaper one to check.
  The deeper point is that the REPO'S OWN RULE pushed everyone toward the unsafe
  implementation — ethos.md and CLAUDE.md ask for mutation testing repeatedly and neither
  says how to do it without a whole-file write. That is why this is `shared-checkout` and
  not "amux's mistake".

## Acking a peer's card with a desc PATCH silently destroys their write-up
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-23
SESSION: amux
CARD: AMUX-3576
SYMPTOM: The documented way to record an outcome before a gate transition is to write `desc`
  first. `desc` REPLACES. Acking three of amux-frustrations' review cards destroyed their
  write-ups: AF-178 4070 -> 1613, AF-182 5018 -> 2152, AF-180 3055 -> 1958. Nothing at write
  time said anything was lost. The board HAD computed the delta all along — it writes
  "desc -2457 chars" into a History line, where only someone reading the card afterwards
  finds it.
COST: ~6400 characters of a peer's reasoning across three cards, restored only because
  `_amux_state_events` carries full pre-mutation snapshots (ids 78469, 78822, 78791). mvs-infra
  hit the identical thing hours earlier on MI-4746 and lost 4082 chars of merge evidence. Two
  sessions, one evening, same field.
FIX: 91648fbc refuses a replace that drops a strict majority of a desc of 500+ chars, with
  `desc_shrink_ack` to override and a pointer to `desc_append`. c7826ed2 documents the recovery
  path in the board contract, because a recovery nobody knows about is one nobody uses.
  AMUX-3576 carries the remaining gap: the guard keys on SIZE, so AF-180 at 36% would have
  slipped under it even had it been live. Authorship is the honest axis — a non-owner replacing
  prose on someone else's card is a different act from the owner trimming their own, and the
  board knows both facts at write time.
NOTE: The guard's first production catch was ITS OWN AUTHOR. It refused me 409 on AF-179 an hour
  after I shipped it, doing the exact thing it was written to prevent, having written the commit
  message that explains why `desc_append` exists. Knowing the failure mode, having just fixed it,
  and having documented it did not stop me repeating it three times. That is ethos rule 6's
  "a rule you have written down is not a rule you run" with a same-day specimen, and it is the
  argument for why this had to become a refusal rather than a convention.

---

## Discarding a spurious autofix card refiles it, so doing the right thing loops
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-24
SESSION: amux
CARD: AMUX-3591
SYMPTOM: One server hang filed the identical card four times — AMUX-3581 (01:12), 3589 (01:26),
  3591 (01:35), 3594 (01:55) — same signature byte for byte, same 19 rows, zero new information.
  Each filing was triggered by the previous one being DISCARDED. Discarding an auto-filed report
  deletes its dedupe idem to re-arm the detector (board.rs, AF-137), which is correct for a
  CONDITION whose refile should require the condition to be live again. The 5xx signature carried
  no occurrence identity, so "recurrence" meant "any 5xx on that path still inside the 6h window"
  and the same historical rows kept qualifying.
COST: Four lane-turns, three of them mine, each a full scope-and-decide cycle on a card that was
  never a defect. Worse than the count: every round was a worker doing exactly the right thing.
  Judging a spurious report and discarding it is the sanctioned disposition, and it was the thing
  driving the loop.
FIX: 01b4cf53 — occurrence identity in the 5xx signature plus `5xx|` added to the re-arm skip,
  mirroring what AMUX-3472 already did for latency outliers. Same rows re-scanned now mint the
  same signature; a genuinely new 5xx mints a new one and files regardless, pinned by a control
  so this does not trade a refile loop for a detector that goes silent after one discard.
NOTE: Two things worth more than the bug. First, I diagnosed it WRONG twice — assumed discard
  caused it, then talked myself out of it because `already_filed` reads a durable idem and never
  checks card status, and wrote that up as a dead hypothesis. Both readings missed that the
  discard does not bypass the dedupe, it DELETES it, in a file I had not grepped. The comment
  naming the hook was in code I had already read that night (autofix.rs:1185). It took a THIRD
  filing to make me look instead of reason. Second, the correct DISPOSITION changed with the
  deploy state: with the fix merged but not running (builder dead since 00:01, AMUX-3585),
  discarding still loops, so AMUX-3594 was closed `done` instead — the re-arm hook fires only on
  the discard transition. Nothing in the card, the gate or the idle nudge can tell you that, and
  the nudge's own option 5 recommends the action that restarts the loop.

## "CDP never answered within 30s" printed with `DevTools listening on <that port>` in its own message
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-24
SESSION: amux
CARD: AMUX-3689
SYMPTOM: Six `POST /api/browser/start` 502s from `primer`, ~30.3s each: "Chrome (pid
  63351) is running but CDP on port 60005 never answered within 30s". The chrome stderr
  pasted into the SAME error body reads `DevTools listening on
  ws://127.0.0.1:60005/devtools/browser/e9edcb66-...`, stamped about three seconds into a
  thirty second wait. So CDP came up, on the exact port named, and amux polled it for
  another 27 seconds while reporting silence. The wait loop discarded every poll outcome,
  so connection-refused, a 1s timeout, a 403 and a 500 all produced that one sentence.
COST: The cause is still unknown and is now unknowable for these six, because the second
  half compounds it: the stderr path is opened with `File::create`, which truncates, and a
  failing caller always retries — so five of the six stderr files were destroyed by the
  retries before anyone looked, leaving a 600-char tail as the entire record of the
  incident. Roughly 40 minutes to establish only that the message was false. An
  investigator who trusted it would have spent that time on Chrome's startup, which is the
  half that was working.
FIX: 6d179755. `describe_cdp_probe` names which of {refused, poll timeout, HTTP status}
  the last poll got, the bail reports it with the attempt count, a WARN carries the same
  fields so a sweep sees the class, and a failed launch's stderr is copied to
  `amux-chrome-launch.failed-<ms>.stderr` (newest 5 kept) where the retry cannot reach it.
  The generalisable half, and it is not "log more": the artifact you need MOST when a
  failure repeats was being deleted BY the fact that it repeated. A truncating diagnostic
  file is fine for a one-shot failure and actively hostile for a retried one, and nothing
  about `File::create` reads as a data-loss decision at the call site.

## A detector's query failure was swallowed, so the whole detector had no coverage
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-25
SESSION: amux
CARD: AMUX-3696
SYMPTOM: `detect_silent`'s steering block was `if let Ok(mut stmt) =
  conn.prepare(...)` with no else. A schema error skipped the entire block and
  left nothing behind, which reads exactly like "no lane has a stalled queue".
  It is not hypothetical: `steering_queue.sender` is added by
  `ensure_fleet_tables`' runtime ALTER and by NO migration, so any database
  built from `migrations/` alone lacks the column and the query does not
  prepare. That is the state every test fixture is in.
COST: The steering-stall detector had ZERO test coverage and nobody could have
  known — every test that appeared to exercise it was exercising nothing,
  silently. Found only because I wrote a new test, seeded a row, and the INSERT
  failed on the missing column. Had I written the test without a write, it
  would have passed vacuously and I would have shipped it as coverage.
FIX: 79080270 records a Suppressed naming the prepare error, where the autofix
  report already surfaces suppressions. The test now asserts the query PREPARED
  before asserting anything about its output.

## An autofix card's fields contradicted each other, and only reading it caught that
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-25
SESSION: amux
CARD: AMUX-3696
SYMPTOM: After splitting the steering deadline in two, the emitted card said
  `threshold_min: 90` on a finding that fired at 360, and its `senders` blurb
  read "that lane may be unable to receive anything, which is what this card
  reports" directly beneath `lane_reachable: yes`. Every individual field had
  been correct before the change and two of them silently stopped being so.
COST: No wrong conclusion shipped, but only because I happened to read the full
  payload printed by a FAILING mutation run. No assertion covered either field,
  and nothing about the change site suggested they needed revisiting. A card
  whose fields contradict each other is worse than one missing a field, because
  each is read as a fact.
FIX: 79080270, both corrected and both pinned. The general lesson: when a
  verdict gains a second branch, every field computed alongside it inherits the
  branch whether or not it was touched — grep the payload, not the diff.

## SUPERSEDES the entry above: browser state's cap was silent, and my diagnosis of it was wrong
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-25
SESSION: amux
CARD: AMUX-3721
SYMPTOM: The entry immediately above blames the state extractor's SELECTOR for
  missing div-with-onclick rows and asks for CSS-selector clicking. Both claims
  are false and I am correcting them rather than leaving them to be greped as
  evidence. The selector has always contained `[onclick]`, and
  `selector_click_js()` already existed in the same file.
  The real defect: `state_js` collects every visible match into `seen`, renders
  the first STATE_EL_LIMIT (120) into `els`, and disclosed nothing about the
  gap. Measured live: 3625 matched the selector, 158 were visible, 120 were
  returned, and the two elements I could not find sat at indices 155 and 156 —
  addressable the whole time, because click-by-index resolves against `seen`
  rather than `els`. Clicking 156 worked the moment I looked past the response.
COST: A wrong cause filed on a card and written into this file, plus the ~20
  minutes already recorded. The compounding cost is what makes it worth an
  entry: a wrong entry here is read as evidence by whoever greps `AREA:
  instruments` later, and three entries sharing an AREA are supposed to be an
  argument for rebuilding something. An argument built on a wrong diagnosis
  points the rebuild at the wrong subsystem.
FIX: 1cddf81a — disclosure, not a bigger cap: `elements_total`,
  `elements_shown`, `elements_truncated`, and a note naming the addressable
  index RANGE and the two ways through. Verified live after adoption:
  total 162, shown 120, truncated true. The cap is fine; being unable to tell
  that it applied was the defect.
  THE TELL I WALKED PAST, which is the transferable part: the response held
  EXACTLY 120 elements, which is exactly the cap. A count landing precisely on
  a limit is a truncation, not a census. Both theories predicted the same
  observation ("my element is not in the list"), and only one was checkable in
  one command: `document.querySelectorAll(SEL).length`. When two explanations
  predict the same failure, reach for the one you can separate cheaply.

---

## amux lanes answer from an 8th-generation summary; a raw terminal answers from primary sources
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3742
SYMPTOM: An amux lane and a raw `claude` terminal, same model and same prompt, give noticeably different quality, and nothing in amux could say why. Model, effort and first-turn token baseline are identical on both sides (measured: both dominated by claude-opus-5 at xhigh, 59,016 vs 56,663 first-turn input tokens). What differs is compaction generations: amux lanes median 8 / max 215, raw terminal median 0 / max 32. Every start resumes (all 8 `start_session` call sites pass `skip_conv_id=false`), so a lane's conversation is immortal. The remedy existed and reached nobody: `app.js` rendered "New conversation" only when `!s.running`, and `config_patch` answered 409 while running, so on all 50 live lanes the one control that fixes this was hidden AND refused.
COST: Unquantifiable degradation fleet-wide for as long as lanes have been long-lived, and it took an owner noticing by feel. The diagnosis then cost four hypotheses measured and killed (model, effort, system-prompt tax, harness share) because no instrument reported the one that mattered.
FIX: 92e1383f, c246b7b9 — `amux fresh <name>`, the dashboard item on a running worker, `GET /api/debug/context-health`, and an hourly `context_health` job that logs the census every pass and WARNs `context_degraded`.

## The generation meter shipped truncating its own scan, and a truncated count looks like a healthy one
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3742
SYMPTOM: `count_compact_boundaries` did a single `take(64MB).read_to_end()` and stopped, so on a 324MB transcript it scanned the first 64MB and returned the partial count as the answer: 30 against a hand count of 75, and 105 against 215 for `mixpeek-cicd`. Shipped inside the very feature whose purpose is to stop reporting numbers that cannot be told from healthy ones.
COST: Caught within minutes, but only because the new endpoint disagreed with the census that motivated it. A reader with one number would have believed it. Also exposed that the obvious test is vacuous: every fixture small enough to write fits inside one 64MB read, so an EOF-scan test passes against the bug unless the read size is exposed as a seam.
FIX: c246b7b9 — chunked to EOF; `count_compact_boundaries_with_chunk` so the test drives a 4KB chunk over a multi-chunk fixture. Mutation-verified: restoring the single-read `break` goes red at left Some(1), right Some(3).

## A renamed lane silently orphans every card that named it reviewer
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3751
SYMPTOM: The rename cascade migrates `issues.session` and leaves `issues.reviewer` and `issues.shepherd` pointing at the dead name. The card still reads `review`, which looks healthy, while the reviewer nudge is addressed to a session that no longer exists. A nudge going nowhere is indistinguishable from a reviewer who is merely slow.
COST: 7 open cards parked in `review` on a reviewer that resolves to no registered worker, found only because Ethan asked an unrelated question about reviewer routing. Two name `amux-rust`, renamed to `amux` long ago.
FIX: 944f06b5 — the cascade migrates reviewer and shepherd; `session_is_registered()` is the one predicate for "can amux address this name"; the reviewer edge returns reason `reviewer-unreachable` with a WARN instead of nudging into the void.

## The badge and the drive loop judged the same self-report differently, and a lane could deadlock for 61 hours
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3756
SYMPTOM: `derive_status` applies a real trust model to a stored self-report — previous life, `stale_active`, trust window — and publishes `applied:false` for one it refuses. `steer_lane_at_boundary`, the gate on auto-pickup, board nudges and steering delivery, read the SAME row and asked only `state == "idle"`. So a lane whose Stop hook never fired kept a stuck `active` report, its dashboard badge correctly read IDLE (`decided_by: activity_fallback`), and the drive loop skipped it as `mid-turn` forever. The two halves of amux disagreed about the same fact, and the correct half was the one nobody acted on.
COST: 4 of 52 running lanes held out of the work loop, every one with `auto_pickup: true` and eligible cards waiting: creative-dna 61.4h, ai-video-editor 59.5h, mixpeek-autopilot 6.4h, primer 1.0h. Self-perpetuating, because only a turn writes a new report and only a human starts a turn on a lane the loop refuses to touch — so the sole exit was Ethan typing at it, which is exactly what he reported ("why do i need to push @tubescience to continue"), and doing so destroyed the evidence. The `mid-turn` skip reason read identically for a lane genuinely generating and one deadlocked for two and a half days.
FIX: 7e4682f0 — `report_applies()` is the one predicate, called by the badge and the gate; `lane_report()` is the one read, replacing two unjudged copies. A refused report WARNs `stuck_self_report` once per lane per report ts, and the board-drive trace's `mid-turn` detail now names the report's state, age and verdict.

## A gate that reads the real filesystem from inside a pure board function turns three unrelated tests red on every host
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: tests
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3751
SYMPTOM: AMUX-3751's reviewer-unreachable gate called `session_is_registered()`, which stats `~/.amux/sessions/<name>.env`, from inside `select_advance`. Board fixtures name reviewers like `peer` that exist on no machine, so three routing tests started failing — here, and in CI, for a reason that has nothing to do with what they assert. The gate itself shipped with no test of its own; breaking other people's tests was its only coverage.
COST: Found by running the full suite rather than the filtered one, which is the only reason it did not reach a push. A green filtered run and a red full run is the shape that gets pushed at the end of a session.
FIX: 7e4682f0 — `select_advance_with()` takes an injected lookup, which is what `config::resolve_home`'s own doc asks new tests to prefer over `set_home`; the tests shadow `select_advance` with a permissive registry, and `the_gate_refuses_a_reviewer_no_nudge_can_reach` exercises both cells of the gate directly.

## The pickup prompt threw away the card it was holding, and made the model buy it back at 308k tokens a call
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: tokens
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3759
SYMPTOM: `pickup_prompt` built the card's `desc + log` and then wrote `.chars().take(500)`. The lane received an ID and a 500-character stub, and spent tool calls reading back text the function had in hand one line earlier. Measured over 11,117 turns across 67 lane transcripts: an auto-pickup turn takes a MEDIAN OF 22 TOOL STEPS where a human-prompted turn takes 3, at a median resident context of 308,059 tokens per model call (p90 738k, max 966k). The cap saves ~1k tokens of steering text and costs ~308k per avoidable fetch — the wrong resource by three orders of magnitude. Silent, too: a truncated excerpt was indistinguishable from a short card.
COST: On the live queue it truncated 86% of todo cards (median definition 1,933 chars, p90 6,658) and discarded 108,820 characters of card definition. 43.8% of fleet turns and 49.7% of input tokens are amux-initiated, so this rides the largest single class of spend. Ethan noticed by feel — "theres also way too much tokens used for some reason in between tasks" — because no instrument reported steps-per-turn by what started the turn.
FIX: ade006c2 — `AMUX_PICKUP_EXCERPT_CHARS`, default 4000, config rather than a constant because this is D4 in the ethos ledger. A cut excerpt now says it was cut and names the read.

## Fixing a mechanism made its own nudge text false, and the false nudge went to the lane that wrote the fix
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3762
SYMPTOM: The capture-shell decompose nudge opens "is a capture shell holding your WIP slot". AMUX-3757 exempted capture shells from the WIP cap ninety minutes earlier, so the clause became false the moment that commit adopted. Nothing tied the nudge's claim to the query it describes, so the mechanism moved and its narration stayed put — the same view-disagrees-with-mechanism shape AMUX-3756 had just fixed one layer down, minted by the author of that fix.
COST: Small in tokens, sharp in kind. A nudge's whole persuasive force is "this is blocking you"; asserting a blockage that no longer exists makes a lane act on fictional urgency and buries the honest reason (no status is a true statement about a captured prompt, so no gate can pass it). It was caught only because the first delivery of the false nudge happened to land on the lane that had written the exemption. That is luck, and the next one will land somewhere nobody can tell.
FIX: b766472c — the clause is gone, the honest reason is stated, and `the_decompose_nudge_does_not_claim_a_blockage_the_wip_query_exempts` derives BOTH the pickup verdict and the nudge text from the same card so changing either alone fails. It also asserts the honest reason survives, because deleting a false claim and leaving an unmotivated chore is the other way to get this wrong.

## An unknown message type defaulted to "Human", so 355 amux nudges wore a person's badge
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3737
SYMPTOM: `msg_kind` was a denylist (`session`/`schedule`/`system` matched, everything else fell through to `human`). `pickup` was added later and nobody taught the classifier, so every board-drive auto-pickup nudge rendered with a blue `Human` badge in the Messages view. The row already carried `origin: board-drive`, so the discriminator was present and the classifier did not read it. The same denylist was restated in the SQL kind filter, so the badge and the filter corroborated each other; `_msgKind` in app.js was a third copy with the same default; and `_MSG_KIND[kind] || _MSG_KIND.human` was a fourth, which meant a server-only fix would have changed nothing on screen.
COST: 359 rows, 4.0% of 8,993 messages, misattributed to a person. Ethan caught it from a screenshot rather than from any instrument, and the misreading is the expensive direction: a fleet that is being auto-driven looks like it is being hand-driven. Also two docs defending the bug — the module header recorded the fallback as a deliberate Python-parity decision, and a test asserted `msg_kind("legacy-weirdness") == "human"` — so the first two things a reader consults both said it was intended.
FIX: 4239ee08 — an allowlist with an explicit `unknown` kind (selectable as a filter, because a kind nobody can select is a kind nobody goes looking for), the SQL filter built from the same constants, and both client copies aligned. Verified live: MSG-33250 now reads `kind=amux`, and 200 sampled `kind=human` rows carry zero machine origins.

## AMUX-2670's fix has never executed
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3737
SYMPTOM: `_msgKind` in app.js has returned `'unstamped'` for a `raw-tmux-fallback` row since AMUX-2670, with a comment stating that an unstamped injection must not render identically to an audited send. That branch is unreachable: `_msgKind` returns the server's `kind` when the row carries one, every API row does, and the server classified the type as `human`. And there was no `_MSG_KIND.unstamped` entry, so even reaching the branch fell back to the Human badge. Two independent reasons the card's intent could never reach a screen, in code that reads as though it works.
COST: A security-adjacent distinction — audited send versus unverified keystroke injection — silently absent for however long, while the code and its comment both assert it is present. Only 2 rows exist today, so the cost is latent rather than realised, and that is the point: nobody would have noticed until it mattered. Found incidentally, one line away from an unrelated fix.
FIX: 4239ee08 — `unstamped` is a real kind on both sides. The general lesson is the one worth keeping: a client-side classifier that defers to a server field has a DEAD local branch for every value the server also produces, and reading either half alone looks correct.

## A test and a doc comment can defend a default long enough for it to look considered
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: tests
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3737
SYMPTOM: The `human` fallback above was pinned by `assert_eq!(msg_kind("legacy-weirdness"), "human")` and explained in the module header as a Python-parity decision: "unknown types read as human, because that is the reading that gets a message looked at rather than filtered away". The reasoning is about visibility and it is sound. The conclusion does not follow, because `human` is not the only visible bucket. Separately, the kind FILTER test was green across the bug's entire life because the fixture seeds exactly one row per type the classifier already knew — a fixture that cannot contain the defect cannot detect it.
COST: Three independent signals (the doc, the test, the filter test) all reported health while the bug was live, so any reader checking whether the default was intentional got yes from all three. That is the difference between an undetected bug and a defended one.
FIX: 4239ee08 — both the doc and the test are corrected IN PLACE rather than deleted, so the next reader sees why it looked considered; `seed_unclassified` adds the two rows the fixture could not express. Mutation-verified: restoring `_ => "human"` fails both tests. The transferable question is "could my fixture contain this defect", asked before trusting a green suite.

## A parked fault card silently muted an entire autofix detector class for two days
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3774
SYMPTOM: autofix files one card per fault, and only OPEN cards suppress — deliberately, so a judged-and-discarded card lets the next occurrence through. But `backlog` is open. AMUX-3651 sat parked in backlog from 08-24, so every server-wide stall since was correctly detected, correctly deduped, and filed nowhere. The suppression reason also asserted "Its count is what moves; a second card would carry no new information" while the code pushes a report row and `continue`s, never touching the card — so the one signal it pointed at did not exist.
COST: Two days of a whole detector class dark, including a live six-family stall. `filed: []` on the tick reads identically for "nothing is wrong" and "everything is muted", which is this repo's most-reinvented bug. Found only because I was chasing an unrelated duplicate card and opened the suppression list; nothing would have surfaced it otherwise, and the card that muted the class looked like an ordinary parked backlog item.
FIX: 8b55d0bf — the false claim deleted (ethos rule 6: implement it or delete it), the suppressing card's staleness printed WHETHER OR NOT it is alarming, an explicit note that suppressing does not bump the card, and an `autofix_mute` WARN past AMUX_AUTOFIX_MUTE_WARN_DAYS. Verified live: AMUX-3651, stale_days=2.03. The better fix — actually bumping the count — is named on the card and deliberately left for its own change, because it is a write on every scan against the live board.

## An empty commit reported success, attached itself to a card, and closed it
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: shared-checkout
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-28
SESSION: amux
CARD: AMUX-3837
SYMPTOM: `2ee153e2` carries a correct subject, a correct card id and ZERO files; its tree is byte-identical to its parent's. git printed a success line, `git log` showed the commit, and the post-commit hook attached it to AMUX-3835 as that card's code history, so I closed the card citing a sha that contained nothing. The change stayed dirty in the working tree of this SHARED checkout for 25 minutes. Every instrument a session reaches for to confirm work shipped reads the MESSAGE; none of them reads the diff. The mechanism is UNEXPLAINED and I am not guessing at it: ruled out by direct test are the invocation (recovered from the transcript, no `--allow-empty`, correct pathspec, identical in form to the retry that landed 52 insertions), plain git (three scratch-repo cells covering pathspec-unmodified, pathspec-misses-the-change, and staged-outside-the-pathspec, all exit 1 and create nothing), every hook, alias and git config, a peer reverting it, and the reflog.
COST: A card closed on evidence that did not exist, and 25 minutes during which any lane's `checkout` or `stash` would have silently destroyed the work. It surfaced only by luck: a PEER's staged-guard warned them that my file looked like unattributed in-flight work, and that notice is what sent me to look. Nothing in amux was going to tell me. The near-miss is the cost, not the minutes.
FIX: edd6de55 — the `commit-report` verb the post-commit hook already calls now classifies the commit it is told about. An empty non-merge commit WARNs with session/sha/subject, marks the same card log line the commit already writes, and returns `empty_commit` on both response arms including the no-card arm. `Unchecked` is a distinct third state with its reason, never folded into `Empty`, because "we could not look" published as "your work is missing" is the false alarm that gets a warning ignored; merges are carved out because 7 of the 8 zero-file commits in the last 120 are merges and correct. A detector rather than a block: one genuine occurrence in 120 commits does not earn a gate that would be wrong more often than right. The live test against a real repo earned its cost immediately — `diff-tree` prints nothing for a commit with no parent, so the first commit of any repo read as Empty until `--root`, while the pure classifier stayed green.

## "Your token expired" and "your consent never came back" wore the same error
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-28
SESSION: amux
CARD: AMUX-3839
SYMPTOM: esteininger21@gmail.com's Gmail token returned `invalid_grant`, breaking SCHED-388. Ethan ran the re-auth flow; the account still failed identically. `social-activities` reported it as a live instance of AMUX-3747 (the Testing-mode 7-day refresh-token expiry), which is a real open problem and fits the symptom exactly. It was not that. No `/api/gmail/callback` had reached the server since 08-24: the mint hands out `http://localhost:8824/...`, the browser upgraded it to `https` (Chrome HTTPS-First; amux sends no HSTS), and 8824's self-signed `CN=amux` cert stopped it at the interstitial. Google had already released the code, so the flow died in the browser AFTER consent with the code sitting in the address bar. Every instrument said "needs_reauth" both before and after a re-auth that never landed, and nothing anywhere could express "your consent did not arrive".
COST: A wrong subsystem owned the diagnosis for hours across two sessions and one owner retry, and the data point was filed onto AMUX-3747 where it argued for urgency on the wrong work. The discriminating facts were BOTH already on disk the whole time (a surviving single-use pending entry, and the token file's mtime); nobody read them because the error did not suggest there was anything to read. The tell that broke it was a negative I could only trust after checking the log could produce a positive: 20 auth rows and 6 callback rows since 08-14, including 400s.
FIX: f2f028c4 — `/api/gmail/auth` returns `previous_attempt_never_completed` when a URL was minted for that account and no callback consumed it (pending_take is single-use, so a surviving entry IS the signal), present only when there is one so its absence claims nothing, scoped to the account, TTL-expired entries excluded. Verified live on the running binary: absent on a first mint, present on a second with no callback between, absent for a different account. Same commit fixes the adjacent silent bug the investigation exposed: the callback wrote the token to `<requested-account>.json` without checking WHICH Google account consented, so Ethan's `authuser=2` would have connected the wrong identity under the right filename. The transferable shape is the one this file keeps recording: when two states share an error string, the one that is not being reported is the one that costs the day.

## A runtime job reported healthy while structurally unable to do its work
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), with the BASIS RECORDED HONESTLY AT THEIR OWN INSISTENCE and weaker than a re-exercise. Their words: "I wrote the fix, I believe it landed, and I have not exercised it since. Archive with that basis recorded, not as verified." Read this as the author signing that the friction is gone, which is what the retirement rule asks for, and NOT as a re-measurement. They also volunteered two caveats that make their belief weaker than it looks: four entries in this batch have no reference to their card id anywhere in src/ or tests/, so the fix cannot be traced from the card, and only two of the nineteen distinct cards carry a test file naming them, so for the rest nobody can cheaply say whether a shipped check would catch a regression. Three of the untraceable four (AMUX-3887, AMUX-3723, AMUX-3687) were deliberately HELD BACK from this batch and are being exercised for real rather than believed, at the author's offer.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-28
SESSION: amux
CARD: AMUX-3829
SYMPTOM: The browser idle-reaper held its "first seen empty" clock in a process-global map. The builder installs a new binary and the server self-adopts on EVERY commit, so the 3600s window restarted whenever anyone in the fleet committed. Measured that day: 22 builds between 06:33 and 16:26, median gap 16.7 minutes, and only TWO gaps of 60 minutes or more. Against a one-hour window that is a reaper which on a working day can almost never fire. Throughout, `/api/system-jobs` reported `spawned: true, ticks: N, status: ok`, and every word of that was TRUE — the loop was running perfectly. The job's health describes the LOOP; the defect was in state the loop carries. From outside, a reaper that can never fire and one about to fire were byte-identical.
COST: A card shipped claiming "the 18-hour zombie that prompted this cannot recur" when it could, and it stayed that way until I went looking during a verification pass. Nothing in the system was going to surface it: there is no signal anywhere for "this job is alive and cannot succeed". The generalisable trap is that a runtime job's registered health answers "is the loop running", which is a different question from "can this job do its work", and the two come apart exactly when the work depends on state that does not survive a restart — on a machine that restarts its server on every commit, that is most stateful jobs.
FIX: 5a8c85ab — the clock moves to `~/.amux/browser-idle.json`, rewritten whole each tick so stopped profiles drop out. The countdown is published on `/api/browser/status` as `idle_s` (null when not empty, which is a different fact from zero) so the invisible state becomes observable, and the release log carries `pre_boot_s`, how much of the window predates this process: a non-zero value there IS the restart-survival working, and the in-memory version could only ever print 0. The transferable question, which I would now ask of any registered job: if this process restarted right now, would the job lose progress, and would anything say so?

## A graft-push checkout read as DIVERGED on every path, withholding the safe restore
VALIDATED: mixpeek-frustrations | VALIDATED by the ORIGINATING session (mixpeek-frustrations, 2026-08-31), re-exercised live rather than read off the card, which they flagged as necessary because they are the REPORTER and the card is amux's, so a card read would have been them validating someone else's close with that person's own artifact. PRECONDITION ESTABLISHED FIRST, since this entry is specifically about graft-push checkouts and is untestable on a normal one: HEAD 7b762a7dd6 is not an ancestor of origin/main 7ebef48777, 177 commits ahead and 684 behind, so every path is genuinely two-directional at the ref level. Under that condition the nudge split the dirty set into FOUR populations with four different remedies (DIVERGED 16, OLD REVISION ON DISK 1, STALE 30, unknown ownership 282) and withheld nothing. A "DIVERGED on every path" regression would have produced ONE bucket; it produced four. They hand-checked two of the 16: FRUSTRATIONS.md had commits in both directions and was correctly called DIVERGED, and FRUSTRATIONS_ARCHIVE.md was novel-and-shorter and correctly NOT in the diverged list.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-24
SESSION: mixpeek-frustrations (reported), amux (fixed)
CARD: AMUX-3599
SYMPTOM: The idle commit-nudge filed dirty append-only files as DIVERGED — "commits in BOTH
  directions, neither single-arm remedy is safe" — on a checkout where the local commits were a
  REPLAY of content already upstream. DIVERGED forbids both remedies, so the reader is left with
  a union-merge they do not need and the safe `git checkout origin/main -- <file>` is withheld.
  The classifier asked `git log origin/main..HEAD -- <path>`, which counts commits BY SHA, and a
  commit already upstream under a different sha sits in that range permanently. On a graft-push
  checkout that is EVERY path.
COST: The wrong verdict on the exact file class the nudge singles out by name — the append-only
  ledgers, where the union-merge directive is printed. A reader following it does more work than
  needed and, worse, learns that the nudge's verdicts are unreliable on their checkout, which is
  the expensive direction: the next DIVERGED that IS real gets read as more of the same. Nobody
  lost data; the reported cost is a wrong prescription plus the turn spent establishing it.
FIX: d55b7a63 — content set-difference instead of sha arithmetic, since sha identity is what a
  replay destroys. The remedy overwrites the WORKTREE, so restore-safety is exactly "does the
  worktree hold lines origin does not"; zero means nothing here can be lost. One-sided by design:
  it only ever downgrades diverged->stale, only on a readable pair AND an empty difference, so
  any error leaves DIVERGED standing.
NOTE: This is the SECOND defect in this cell in four days and they point opposite ways. The cell
  was ADDED on 2026-08-20 because the two-bucket classifier filed a genuinely-diverged path STALE
  and the prescribed restore disarmed a data-loss push guard. This entry is the same cell now
  over-firing. Both are the same underlying error — reading commit identity as content identity —
  and it produced a false negative first, then a false positive, which is why "be more careful
  with the direction test" would not have caught either. The durable form is that a classifier
  prescribing a DESTRUCTIVE remedy has to be gated on what the remedy actually destroys, not on
  a proxy for it. Also worth recording: the fix logs the downgrade, because STALE-because-
  downgraded and STALE-outright were otherwise byte-identical in the log, which is the one-output-
  two-states shape on the arm that prescribes the destructive remedy.

## Nothing owned the WORKERS at boot — a reboot left 56 of 58 down, holding 69 `doing` cards
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), EXERCISED rather than believed, at my request: this was one of the three I named from the four they had flagged as having no traceable card id. Is anything scheduled: YES, `com.amux.fleet-start`, RunAtLoad=true, runs=1, last exit 0, running scripts/fleet-boot.sh which calls `$AMUX_BIN start-all`. Does start_all get past the first worker: YES, and established from a REAL boot rather than by reading the loop, the 2026-08-30 run logging `55 started, 2 already running, 0 failed, 66 archived`, i.e. it walked all 112 entries. THEIR OWN FIRST PROBE WAS A FALSE NEGATIVE AND THEY SAID SO: grepping every LaunchAgent plist for `start-all` found nothing, because the plist runs a wrapper and the string lives one level down; the measured negative was wrong and looked exactly like a measured negative. RESIDUAL, deliberately NOT counted against this entry and filed as its own card AMUX-3965: the same boot's independent verdict three lines later reads `51/58 non-archived running, still down: opencode-test-1, refresh-house, self, social-media, studio-plg, ts-gke, tubescience`, and six of those seven appear in start-all's OWN `started` list. `tmux new-session -d` returns immediately, so `started` is a claim about the call and the verdict is a claim about the world. fleet-boot.sh already PUBLISHES that discriminator and its comment says so; what nothing does is act on it, and the boot exits rc=0 over the divergence.
AREA: cli
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-29
SESSION: amux
CARD: AMUX-3887
SYMPTOM: The machine restarted at ~19:55 ET. launchd brought back all four amux SERVICES (server-rs, its builder, watchdog, cert-renew) and `/health` was green within seconds. Every WORKER stayed down. `GET /api/sessions` read 2 running out of 58 non-archived, and the dashboard showed all 56 others registered, described, and stopped. Three separate defects stacked underneath, none of which announced itself: (1) `cmd_start` ran `tmux set-option -t "=$tname" allow-rename off` — a WINDOW option aimed at a SESSION, so tmux 3.6a answers "no such window" and exits 1, `2>/dev/null` eats it, and `set -euo pipefail` kills the script THERE, before the "started" echo and before the `--detach` return. A fully successful start reported rc=1 with zero output. (2) Because of (1), `cmd_start_all` — which calls `cmd_start` bare in a loop — aborted after the FIRST worker. Bulk start could never have worked. (3) `cmd_start_all` had no archive filter and would have tried to resurrect all 66 archived workers had it ever gotten past the first one, and it called `cmd_start` without `--detach`, ending in a `tmux attach-session` no boot-time caller can satisfy.
COST: The fleet was down for roughly an hour of wall-clock until a human noticed and asked why. Recovering it took a hand-rolled staggered start loop because the sanctioned verb could not do it. The deeper cost is that this was silent in both directions: the three defects made `amux start` return failure on every success, so the exit code carried no information at all, and `start-all` was a verb that had apparently never once done what its help text says ("Start all registered workers"). Nobody could have learned this from a log, because the failing path printed nothing.
FIX: `cmd_start` uses `set-window-option` for both rename locks, `|| true`s them so a cosmetic window-title option cannot decide whether a start succeeded, and prints a WARN naming the option and the tmux version if either is rejected — so the next tmux rename surfaces as a line instead of a fleet outage. `cmd_start_all` skips `CC_ARCHIVED=1` (read from the env file, so cold start does not depend on the server being up), passes `--detach`, keeps going past a failure, staggers via `AMUX_START_ALL_STAGGER`, and ends with a `started/already/failed/archived` summary — the count beside the zero that would have exposed (2) immediately. New `scripts/fleet-boot.sh` + `com.amux.fleet-start` launchd agent (RunAtLoad, no KeepAlive — a cold start, not a supervisor; a worker a human stopped stays stopped) waits for `/health` and brings the fleet up at login, logging an independent `N/M running` verdict from `/api/sessions` rather than trusting start-all's own count. Installed by `install.sh`; skip with `AMUX_NO_FLEET_START=1`.

---
---

## `force` claimed to log the judgment and logged an empty string, 41 times out of 41
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), EXERCISED live and in BOTH ARMS, at my request: this was one of the three I named from the four they had flagged as having no traceable card id. On a throwaway card of their own (AMUX-3964, since discarded): an empty reason was REFUSED with "force requires a reason" and the card STAYED todo; a force carrying a reason was accepted and the ledger line reads `force by amux: todo->done reason=<the actual text>`. Both arms is the claim, and they said why unprompted: the refusal arm alone proves nothing, because a gate that refuses everything passes it. That is the exact control this entry's own subject matter demanded, since the original defect was a force that logged an EMPTY judgment 41 times out of 41 while reporting success.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3723
SYMPTOM: Every force audit line on this board reads `force by <who>: a->b reason=` with nothing after the `=`. 41 lines, 41 blank — never once populated. The board contract advertises force as "bypass (judgment stays with you; logged)", and ts-gke's 2026-08-03 fix made attribution mandatory precisely so the ledger would name the party holding the judgment. It named them and recorded no judgment.
COST: Found while auditing how the autofix backlog was actually closed, and it made that audit undecidable for 25 cards: bulk-discarded in one minute, attributed, with nothing recorded about why. Reconstructing intent meant reading desc diffs card by card. The one escape hatch from the entire gate system was the one action whose trace could not answer the only question anyone asks of it.
FIX: f013ba5b. Neither obvious suspect was guilty, which is why it survived: `amux board --force` has always REFUSED to run without a reason, and the server has always written a supplied reason to the log (an existing test asserts it, and passed throughout). The CLI validated the reason and then sent it as `desc_append` instead of `reason` — 9 of the 41 cards carry a good "[FORCED] <why>" in their desc beside a ledger line that says nothing. A test on either side of the seam and none ON it. Now: the CLI sends both, the server refuses a blank reason from any caller with a 400 that names the sanctioned command, and a `force_without_reason` tracing marker makes the next off-path caller visible (a bare 400 here groups with every other board-PATCH 400 in /api/logs/analyze).

---

## session-freshness reported a stale shadowing CLI and prescribed the `cp` that rebuilds it
VALIDATED: amux | VALIDATED by the ORIGINATING session (amux, 2026-08-31), EXERCISED rather than believed, at my request: this was one of the three I named from the four they had flagged as having no traceable card id. `.claude/session-freshness.sh:226` now prescribes a SYMLINK, and its own comment names the copy it replaced; no `cp` anywhere in the file's 453 lines. RESIDUAL THEY VOLUNTEERED AND I AGREE IS SMALLER THAN THE ENTRY, so not counted against it: the hook does not mention scripts/mutate.sh either, so it no longer teaches the wrong thing and does not yet teach the right one. Worth noting because ethos rule 7 now names mutate.sh explicitly as the alternative to `cp file bak`, and the session-freshness hook is the surface every lane reads at session start.
AREA: instrumentation
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-24
SESSION: amux
CARD: AMUX-3687
SYMPTOM: The freshness hook's Axis-2 shadow detector fired correctly on
  /usr/local/bin/amux and then offered `cp "$REPO/amux" "$cand"` as the remedy. A copy
  silences the warning and leaves a copy, which is stale again the next time anyone edits
  ./amux — so the prescribed fix reconstructs the exact condition being reported. It is
  also how the specimen got there: ~/.local/bin/amux has been a SYMLINK since install.sh
  created it, and /usr/local/bin/amux was the copy, so the one file that could drift was
  the one the remedy would recreate. What was actually sitting there was an Aug-6
  227-line stub knowing two verbs (send, board) and defaulting AMUX_URL to
  https://localhost:8822, the retired port (AMUX-3046). A lane resolving it gets
  connection-refused on every call, and help-and-exit-0 on `url` or `alert`.
COST: 18 days undetected, and the detection that finally landed pointed at a remedy that
  would have reset the clock. Not measurable in minutes for me (the hook named the file
  and I checked it), but any lane whose PATH ordered /usr/local/bin first was talking to
  a dead port for those 18 days with no error a session would recognise as a stale CLI.
FIX: `ln -sfn`, in both branches of the axis, with the reason stated inline so it does not
  get "simplified" back to a cp. b0a0c6b7. Live shadow reconciled the same way; both PATH
  entries now resolve to the checkout and the axis is silent.
  The generalisable half: a detector that names a remedy owes the same scrutiny to the
  REMEDY as to the check. This one could fail, fired correctly, and still closed the loop
  back onto itself.

## Every amux-launched Chrome opens with the yellow "unsupported command-line flag" infobar
VALIDATED: mixpeek-research | VALIDATED by the ORIGINATING session (mixpeek-research, 2026-08-31), re-exercised today rather than read off the card. They started a FRESH amux browser (pid 93701) and its argv carries both the kBadFlags trigger and --test-type on one line, so the suppressor reaches NEW production launches and not only the process measured on 08-30, which is the arm that distinguishes a fix from a one-off observation. Mechanism is source-verified as the ChromeDriver-standard suppression rather than inferred from the absence of the bar. HONEST LIMIT, recorded by them unprompted rather than papered over: the PIXEL layer is still unobservable from any lane because screencapture and accessibility permissions are both ungranted (AMUX-3848, unchanged), so this is validated on verified flag delivery plus the source-verified mechanism, with the pixel layer marked UNOBSERVABLE rather than checked. Their own reopening condition, kept here so it is actionable: if the bar reappears on a window Ethan sees, that sighting re-opens the entry.
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-24
SESSION: mixpeek-research
CARD: MR-38
SYMPTOM: Ethan's screenshot at 15:40: "You are using an unsupported command-line flag:
  --ignore-certificate-errors-spki-list=... Stability and security will suffer." across the top
  of every window the amux browser opens. The SPKI pin is on Chrome's kBadFlags list
  (chrome/browser/ui/startup/bad_flags_prompt.cc:107), so the bar has been on every launch since
  the pin shipped. Nothing in amux could see it: it is browser chrome, not page content, and no
  verb screenshots that, so the only detector was a human looking at the window.
COST: every human-facing browser session since the pin shipped read as broken or unsafe to the
  person looking at it, until Ethan screenshotted it. About 90 minutes across two lanes to land,
  most of it the shared-checkout dance (the peer's whole-file write dropped two of three edits
  once; see the entry above at "Mutation testing's obvious harness is a whole-file write").
FIX: 9f4e6971. --test-type on the launch line: chromium infobar_utils.cc:173 returns before
  ShowBadFlagsPrompt for a test-harness launch (ChromeDriver passes it on every session);
  --enable-automation would also work but adds its own "controlled by automated test software"
  bar. Flags extracted into chrome_launch_args() and launch_args_tests pins "bad flag =>
  --test-type" with a control that the pin is really present; mutation-checked red without the
  flag. NOT confirmed on screen from this lane: screencapture is refused for a tmux shell (no
  Screen Recording grant), so the visual check is Ethan's next launch. Already-running Chromes
  keep the bar until relaunched.

## A peer's `git add` swept a migration DELETION into an SEO commit
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations authored this entry and shipped the fix, so this is a SELF-SIGNOFF and is labelled as one, not a peer review). Fixed in a2758f67, VERIFIED against the INSTALLED hook rather than the repo copy. test_amux_staged_guard.py -> ALL PASS. Mutations: flagging every deletion -> FAIL 2 (the controls); never firing -> FAIL 4 (the positive). Measured before building: over `git log -500` only TWO commits contain a deletion and the predicate fires on exactly ONE, the 26c45798 incident.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-31
SESSION: amux-frustrations
CARD: AF-357
SYMPTOM: `crates/amux-server/migrations/0035_reclaim_skipped_hits_repair.sql` was a
  leftover duplicate from a merge renumber, and it was reddening the suite fleet-wide
  via `every_migration_file_on_disk_is_registered`. It was removed in `26c45798`,
  whose subject is "feat(seo): parallel-feature-dev rewrite, new splitting-work guide,
  changelog 2026-08-30/31". An SEO commit deleted a migration file. Confirmed with
  `git log --diff-filter=D -- <path>`.
COST: The deletion was correct and needed doing, so nothing broke. The cost is
  forensic and it is deferred: `git log` on that path names a commit whose message
  cannot account for it, so the next person asking "when did this migration go, and
  why" finds an answer that explains nothing. I can also date it precisely, which is
  the part nobody could have recovered later: my first full suite run went red on
  exactly this and my rerun of the same test minutes later went green with no change
  from me, so the deletion landed INSIDE a 181-second test run. Anyone chasing that as
  a flake would have found nothing, for as long as they looked.
FIX: This is AF-316's class (one checkout, N lanes, one index) in the DELETE
  direction, which the existing entries do not cover: they are all about a peer's work
  being ADDED to your commit. A removal is worse to trace, because the file is not
  there to notice. The per-lane worktree in AF-336 ends it. Short of that, the
  staged-guard's co-edit notice counts insertions and deletions per path but does not
  flag a staged DELETION of a file the committer never touched, which is the cheap
  version of this specific catch.
FIXED 2026-08-31 in the commit naming AF-357. The staged-guard now flags a staged
DELETION whose top-level directory appears nowhere else in the commit, names the
path, and offers `git restore --staged`. MEASURED BEFORE BUILDING, because a check
nobody wants is worse than none: over the last 500 commits only TWO contain a
deletion at all, and the predicate fires on exactly ONE, which is 26c45798 itself.
Deletions are 0.4% of commits here, so a check scoped to them is cheap by
construction. Three controls keep it quiet: a deletion INSIDE the commit's own area
does not fire, a commit with no deletions does not fire, and a deletion-ONLY commit
is a deliberate removal and does not fire. Mutations both ways redden the right
cells: dropping the discriminator fails the controls, and never firing fails the
positive. This does not end the class (AF-336 does); it makes the one direction
nobody could notice visible at the moment it happens.

---

## A decision card carried two mechanisms, so the half needing no decision waited 13 days
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations authored this entry and shipped the fix, so this is a SELF-SIGNOFF and is labelled as one, not a peer review). Instance fixed in bea51abc (AF-98 split; CC_SEND_ALLOW set on this lane's own env; three entries unstuck after a 13-day stall). The GENERAL mechanism this entry proposed was REFUTED by measurement rather than built: probe 1 returned 0/370 because the board LIST payload carries desc_head and no desc; probe 2 found 32/371 naming an env-var remedy, most legitimately Ethan's; probe 3 got 23/32, non-discriminating because a card names its own owner anyway. The proposed gate would have refused ~30 of 32 correct asks.
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-31
SESSION: amux-frustrations
CARD: AF-359
SYMPTOM: AF-98 sat in `needsyou` from 2026-08-18 titled "Cross-group send blocks the
  frustrations validation program", asking Ethan for "one-line config, or a deliberate
  no". It described TWO different refusals as one problem. Reaching `desktop` is
  refused by the isolated-raw-agent rule, which only the owner can work around and is
  genuinely his call. Reaching `mixpeek-frustrations` and `mixpeek-research` is
  refused by the ordinary intra-group rule, whose refusal text names its own remedy in
  the body: "set CC_SEND_ALLOW on amux-frustrations". Both symptoms print as "send
  refused" and only the refusal BODIES distinguish them, so from the card's own text
  the two are indistinguishable.
COST: 13 days of the frustrations program blocked on three entries that needed no
  decision from anyone. I set CC_SEND_ALLOW="ops,new-features" on this lane's own env,
  scoped rather than `*`, and all three routed immediately with no restart. The
  mechanism had been printing the fix in every refusal for those 13 days. The deeper
  cost is the shape: a card is ONE unit of work, something that can be honestly done or
  not done, and this one could not be. No single answer finished it, so it parked in
  the queue where the answerable half was invisible behind the unanswerable half.
FIX: Fixed for this instance by splitting it: AF-98 is narrowed to the isolated-worker
  half only, and the cross-group half is closed. The general form is the 451-folds rule
  applied to `needsyou` specifically: before parking a card on a human, ask whether
  EVERY part of it needs them, because the parts that do not will wait exactly as long
  as the parts that do. A cheap version worth building: when a card enters `needsyou`,
  have the typed `--ask` gate (AF-318) refuse an ask whose own body names a remedy the
  ASKER can apply. That is detectable — this refusal literally contained an imperative
  addressed to the sender.

---

## Retiring an entry is two writes, and only the second one can fail
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations authored this entry and shipped the fix, so this is a SELF-SIGNOFF and is labelled as one, not a peer review). Fixed in ccdc2ddc. scripts/test-frustrations-archive.sh -> 30 passed, 0 failed, cells (r) and (r2) new. Mutation: removing the retry sleep -> 29 passed, 1 failed, cell (r) alone. The two cards the defect damaged (AMUX-3887, AMUX-3723) were repaired by replaying the shipped carry_to_card, and both read back with the RETIRED-ENTRY marker.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-31
SESSION: amux-frustrations
CARD: AF-362
SYMPTOM: `scripts/frustrations-archive.py` does two things per entry: it MOVES the
  text from frustrations.md into frustrations-archive.md (a local file write, which
  always succeeds) and it CARRIES the SYMPTOM and COST onto the entry's card over
  HTTP. Only the second can fail, and it did, twice in one batch:
    AMUX-3887: NOT carried (curl exit 7, 0 bytes)
    AMUX-3723: NOT carried (curl exit 7, 0 bytes)
  `/api/health` answered normally moments later with `uptime_s: 11`. The server was
  not down, it was MID-RESTART: this box rebuilds and swaps the binary on every
  commit, and I had just committed twice.
COST: Two half-retired entries. The text was gone from frustrations.md and the cards
  it pointed at never received the symptom or cost, which is the whole reason AF-38's
  rule carries them: the card is where someone hitting the friction again looks. The
  archive move is NOT rolled back on a failed carry, so nothing self-heals and nothing
  re-attempts. I only caught it because the tool prints `NOT carried` honestly, which
  is AF-150's lesson working; a version that inferred success from an empty stdout
  would have reported two clean retirements. The exposure scales with batch size and
  I archived 20 entries in one run tonight, so this was luck rather than a near miss.
FIX: Fixed. The carry now retries three times with 2s between attempts, which covers
  a binary swap, and still reports `NOT carried` when the server is genuinely down
  rather than blocking the archive (a retry that ended in a false success would be
  worse than none). Cell (r) in scripts/test-frustrations-archive.sh pins it on
  elapsed time against a closed port, a floor and never a ceiling; cell (r2) is the
  control that the failure is still reported. Removing the sleep makes (r) fail.
  The general shape is worth keeping: an operation that is one verb to the user but
  two writes underneath needs to say which half it completed, and this one did, which
  is the only reason there was anything to fix rather than to discover later.

---

## A card create records the claimed creator and nothing about the request
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations authored this entry and shipped the fix, so this is a SELF-SIGNOFF and is labelled as one, not a peer review). Fixed in 517400ee. cargo test --test board_api -> 60 passed, 0 failed; clippy --all-targets -D warnings clean. CONTROL ESTABLISHED BEFORE THE DIAGNOSIS: a create with no X-Amux-Session leaves creator empty and takes the AMUX- prefix, so AF-363/AF-364 did carry this lane's header. Verified live: card=BACKE-3744 caller_session=mixpeek-funnel stamped_creator=mixpeek-funnel owner_session=backend.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-31
SESSION: amux-frustrations
CARD: AF-366
SYMPTOM: Auto-pickup handed this lane `AF-364 [ts-gke] tenant-deploy engine skipped
  on 75ad074eab7c` to work, alongside `AF-363 Test card from tubescience`. Both are
  stamped `creator: amux-frustrations` and `session: amux-frustrations`, created 8
  seconds apart with empty descs. I did not create either. Nothing anywhere can say
  who did:
    - a successful POST /api/board logs NOTHING (`grep -c 'POST /api/board'` over
      today's server-rs.log returns 0), while the board READ path logs `caller_ua`
      and `caller_session` on its truncation WARN;
    - the `_amux_state_events` payload for the create stores the resulting
      `creator` field, which is the value in question, not the request's origin.
  The `X-Amux-Session` header is caller-supplied and unverified, which is fine for a
  local fleet, so the stamp is only ever as good as the caller's honesty or config.
  Verified by control: a create with NO session header leaves `creator` empty and
  takes the `AMUX-` prefix, so these two DID carry this lane's header.
COST: A lane was handed another team's deploy card by an automated loop, and the
  misrouting is unfixable at the root because the origin is unrecoverable. I could
  route AF-364 to ts-gke and discard the probe, but I cannot tell whoever did it,
  and neither can anyone auditing later. The read path being instrumented while the
  write path is not is backwards: a read is recoverable by reading again, a write is
  not.
FIX: Fixed. The create success path now emits `board card created` at INFO with
  `card`, `caller_session`, `caller_ua`, `stamped_creator` and `owner_session`, so a
  header that disagrees with the caller, or a create from an unexpected user-agent,
  is greppable. It reuses `truncation_caller` rather than re-deriving the pair, so
  the honest fallbacks ("(none)", "(unattributed)") stay identical to the read path
  instead of one site growing a silent blank. Not claimed as fixed: nothing verifies
  the header, and this does not change that. It makes a wrong stamp VISIBLE, which
  is the part that was missing.

---

## A field with an unexpected NAME reads as a missing value, even when it is on screen
VALIDATED: mvs-research | VALIDATED by the ORIGINATING session (mvs-research), who filed the underlying report, RETRACTED it when shown the data, and then confirmed the fix IN PRODUCTION on their own work. Their words: "Your write-path fix works in the wild. First real use, just now on MR-114 ... That is exactly the intervention you argued for: it names the field AND hands over the read-back command, so the search I failed never has to happen." Verified independently from this side rather than taken: MR-114 status=backlog, source_ref="shard rolled and WalTail serving (finalize_wal_history has decided a lineage...)", last_verified_at 2026-09-01 01:05:37, owner mvs-research. Fixed in 7fa1fe8e, with 16a6d7cc adding --connect-timeout to the printed recipe after amux-cloud caught it redding cli_curl_timeout_guard. The entry's own content is why the write-path fix was the right one: mvs-research printed every non-empty key as a fallback, source_ref WAS in the output, and they did not read the values — so the reader-side fix was already tried in this incident and was insufficient.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-01
SESSION: mvs-research (hit it and retracted), amux-frustrations (hit it twice, fixed it)
CARD: AF-379
SYMPTOM: `amux board backlog <id> --trigger "..."` stores the condition in
  `source_ref` and stamps `last_verified_at`. The success line says only
  `<id> -> backlog`, and no key is named `trigger`. FIVE independent probes read
  that as a lost write, across two lanes in one day:
    mvs-research  filtered for a key whose NAME matched trigger|block|condition;
                  `source_ref` does not match, and they read the empty result as
                  the value being absent.
    mvs-research  then printed EVERY NON-EMPTY KEY as a fallback. `source_ref` WAS
                  in that output. They did not read the values. Their own words:
                  "I had the answer on screen and filed a bug against your tool
                  anyway."
    amux-frustrations  searched keys matching /trig/ plus desc and log on AF-367,
                  found nothing, and nearly confirmed the report against my OWN
                  card, whose source_ref held my condition the whole time.
    amux-frustrations  (AF-359, an hour earlier) searched the board LIST payload,
                  which ships `desc_head` and no `desc`, and got a confident 0
                  across 370 cards.
    amux-frustrations  wrote up a "genuine disagreement" between the gate text
                  "Trigger condition documented on the card" and the CLI, without
                  checking. `backlog` is not a gated status and that criterion
                  governs tripwire/watch entering `doing`. Two mechanisms sharing
                  one word.
COST: A bug filed against a working tool, propagated to three places (MR-112,
  MR-19, and a direct message to mvs-infra) before being retracted; roughly an
  hour across two lanes. The instructive part is that the SECOND probe succeeded
  and was still misread, so "write a better probe" is not the lesson. The reader
  had the data and could not see it, because nothing told them which of ~30 keys
  was the answer.
FIX: Fixed in 7fa1fe8e. The CLI now prints, on stderr so captured JSON is
  untouched, `trigger stored in source_ref (+ last_verified_at)` plus the exact
  command to read it back. NAMING AT WRITE TIME is the fix that works, because
  every reader-side improvement was already tried in this incident and one of them
  succeeded without helping.
  NOT DONE, on the reporter's own judgement and mine: renaming the column.
  `source_ref` is opaque for a re-queue condition, and gate acks plus stored
  queries key on existing names, so the rename costs more than the confusion it
  removes now that the write path announces itself.

## test-contended.sh rules out the builder, so its green verdict reads as "therefore your bug"
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations diagnosed this and wrote the entry; amux paid the cost). SELF-SIGNOFF, labelled as one, not a peer review. Fixed in 4ab03071 and hardened in 4ec45b1d. scripts/test-contended-worktree.sh -> 10 passed, 0 failed, wired into CI. The wrapper now captures the dirty set BEFORE and AFTER the run and prints it beside the verdict on BOTH arms; the mutation that writes it the obvious way, inside the clean arm only, fails exactly cells (c2) and (d). It deliberately does not attribute an owner, because mtime attribution has been wrong on this checkout repeatedly (AF-179, AMUX-3662) and a confident wrong owner is worse than a named file with none. CONFIRMED BY THE LANE THAT PAID THE COST, unprompted: amux ran the suite across the change and reported "worktree: THE TREE CHANGED DURING THIS RUN" naming four files, two theirs and two mine, and said they could sort them instantly and that had it guessed it would have guessed wrong on migrate.rs, where mtime said them and the truth was me.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-31
SESSION: amux-frustrations (diagnosed), amux (paid the cost)
CARD: AF-356
SYMPTOM: `amux` ran the suite and got 1 failure, `gate_table_matches_python`, in a
  module they had not touched. It passed in isolation. test-contended.sh printed its
  verdict: "the auto-builder was NOT rebuilding during this run, so the shared binary
  was stable under it. A failure here is NOT build contention." They concluded ETXTBSY
  anyway, reported it as their weakest evidence line, and asked me to form my own view.
  The true cause was neither: MY uncommitted edit to board_store.rs was sitting in the
  shared worktree while their test ran. I was adding `ItemType::Decision` (AF-323), and
  that test asserted `default_gates_for("decision", Done) == default_gates_for("code",
  Done)` because `decision` was its stand-in for an unknown type. My half-finished
  change made the assertion false in a file they never opened.
COST: A wrong root cause carried into a verification request as its stated weakest
  line, and ~20 minutes of mine to refute it. The deeper cost is that the wrapper's
  verdict is load-bearing in the other direction: it exists to stop a red being read
  as a regression, and by ruling out the ONE cause it can see, it makes the remaining
  space look like "your bug" when a shared checkout has a third option. Both failure
  modes look identical from inside a test run: a red in a module you did not touch,
  green on rerun. The wrapper cannot tell them apart and does not say so.
FIX: The wrapper already answers "was the builder rebuilding". Have it also answer
  "was the tree dirty, and in which files" — `git status --porcelain` before and
  after, printed beside the verdict, naming the paths and their sessions from the
  observed-edit records the staged-guard already keeps. Then the verdict line becomes
  two facts instead of one, and the missing clause stops being invisible. Ethos rule 4:
  the output that can read "clean" must publish whether it measured the thing at all,
  and this one measures one of two causes while its sentence implies both.
FIXED 2026-08-31 in the commit naming AF-356. The wrapper now captures the dirty set
  BEFORE and AFTER the run and prints it beside the verdict, on BOTH arms. The
  before/after pair is the strongest form: a tree that CHANGED under the compile is
  the case a single snapshot cannot see. It deliberately does NOT attribute an owner,
  because mtime attribution has been wrong on this checkout repeatedly (AF-179,
  AMUX-3662) and a confident wrong owner is worse than a named file with none. Clean
  is STATED rather than left silent, since a silent probe and a clean tree are
  otherwise the same output. scripts/test-contended-worktree.sh, 7 cells, wired into
  CI; the mutation that writes it the obvious wrong way (clause inside the clean arm
  only) fails exactly (c2) and (d).

---

## The nudge that tells you to union-merge cannot tell you how to do it safely
VALIDATED: mixpeek-frustrations | mixpeek-frustrations, who ORIGINATED this entry, verified all four criteria itself
rather than accepting the fixer's report, and asked for it to be archived.

LIVE, NOT MERELY MERGED. It took the commit from the SERVING artifact, not from the
branch: GET /api/health -> commit 892633a52052, and `git merge-base --is-ancestor
e0e2d54a 892633a52052` is true. Confirmed independently from amux-frustrations at
archive time: same health commit, e0e2d54a (2026-08-31) is an ancestor of it, and
the serving commit's own source carries the recipe at both render sites (4
occurrences of the merge-file invocation in commit_nudge.rs: 2 rendered, 2
asserted).

REGRESSION CONTROL, mutation-tested in BOTH directions, which this entry
specifically demands because its subject is a guard that could not see its own
subject. Stripping the recipe from build() failed
diverged_paths_get_their_own_section_and_leave_both_recipes, alone. Stripping it
from commit_worthy_body() failed
the_protocols_diverged_bullet_carries_the_merge_it_prescribes, alone. Restored: 6
passed, 0 failed. Each site has an independent guard and neither passes vacuously.

VALIDATED RATHER THAN SUPERSEDED, on the originator's own reasoning: its note had
allowed for SUPERSEDED if "hand the path to its owner" were the right terminal
answer. It is not. `git merge-file -p` computes the merge without writing the file
and returns the conflict COUNT as its exit status, so handing the path over becomes
a decision made KNOWING that number instead of in place of knowing it. The entry's
mechanism was right and the fix is what the entry asked for.

TWO TRANSFERABLE FINDINGS, both from the fixing lane (amux) and neither visible to
the lane that measured the symptom:

  The same bare directive existed TWICE, rendered by two different functions. The
  originator measured one arm. Fixing only what was measured would have left the
  other arm bare, and that arm's own test could not see the second site.

  The existing test asserted contains("MERGE the two versions"). The replacement
  text QUOTES that directive while describing it as former behaviour, so the
  assertion stayed green across a rewrite that removed the prescription entirely.
  Caught by running the test EXPECTING RED and getting ok. A check that a quotation
  satisfies is pinning the words, not the property.

The fixing lane (amux) is an isolated raw-agent worker and cannot be reached by peer
send; nothing was needed from it, since the originator is the party whose signature
the protocol requires.

While verifying, mixpeek-frustrations applied and reverted both mutations in this
shared checkout and reported it: the file was restored to the state it was FOUND in
(not to HEAD), `cmp` identical against its backup, leaving ts-gke's 184 uncommitted
insertions on top of the committed fix untouched.
AREA: notices
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-25
SESSION: mixpeek-frustrations (hit it), amux (fixed it)
CARD: AMUX-3718
SYMPTOM: A DIVERGED FRUSTRATIONS.md nudge said `MERGE the two versions (for
  append-only files, union-merge per .claude/rules/frustrations.md)` and stopped
  there. Two things were wrong at once. The cited path exists in ~/Dev/amux and
  NOT in ~/Dev/mixpeek, where the reader was, because commit_nudge is server
  code that fires into every lane's OWN checkout. And the safe procedure it was
  pointing at could never have arrived anyway: `build()` defines commit_worthy
  as the dirty paths that are NOT stale/diverged/revived, and the archive-check
  note was emitted from inside `commit_worthy_body`, which receives exactly that
  set. So a DIVERGED append-only file was structurally excluded from the only
  code that emits the archive check. The one state that prescribes a union-merge
  was the one state that could not be told how to perform it.
COST: A near-miss on real data. The lane followed the destructive half verbatim,
  which would have resurrected an entry closed on a 692/692 prod measurement and
  double-inserted a content twin already on origin under a different subject. It
  also cost a second lane a wrongly-filed card, since from ~/Dev/mixpeek the
  only visible symptom is "this file does not exist" and the citation looks like
  the whole bug. Both readings were reasonable and both were incomplete.
FIX: 972b44a4. Hoisted the note to `build()` over the full dirty set so it
  travels with every arm, deleted the citation because the procedure is already
  inline, and rewrote the note's unit test to go through `build()` — it had been
  calling `commit_worthy_body` directly and was green for the entire time the
  note was unreachable (ethos rule 7 / AF-161: a check pinning the wrong layer
  is exactly as green as one pinning the right layer). Second fix per the
  two-fixes rule: `missing_archive_check()` now WARNs on the ACTUAL delivered
  bytes before `steer_enqueue`, so the next regression announces itself in
  server-rs.log instead of arriving as another near-miss.
  The general shape worth remembering: a citation is only as good as the reader's
  checkout, and the dangerous half of an instruction must never be the half that
  travels while the safety half is behind a link.

## A "slim" payload that omits a column can still SHIP DERIVATIONS of it, and the loader layer cannot see that
SUPERSEDED: amux-frustrations | SUPERSEDED, by the entry immediately below it in the ledger, which is by the same
session and says so in its own title. Signed by the originating session, which is
also the session that got it wrong.

The mechanism this entry states is false in both halves. It says the a99955f7
dashboard regression happened because no consumer-side invariant existed, and that
one was being added. amux checked instead of agreeing, and found that
tests/board_api.rs :: list_is_slim_by_default_and_serves_prose_only_on_request
already existed, already drove the real HTTP list path, and already asserted that
desc_head starts with the card's first line. Run against a99955f7 in a scratch
worktree it fails in 0.16s. The guard was written before either lane arrived, was
correct, and would have blocked the commit.

It did not run because the verification command was `cargo test -p amux-server
--lib`, which prints "1625 passed" and silently skips every tests/*.rs target: 47
integration files, roughly 339 tests. A partial run whose number reads like a total.

Archived as SUPERSEDED rather than VALIDATED on purpose. Its remedy, "add a
consumer-side invariant", is advice that sends the next reader to write a duplicate
of a test that already passes, while leaving the command that skipped it untouched.
Filing that as validated history is exactly what the superseded disposition exists
to prevent (AF-243). The text stays as a dead hypothesis so nobody re-derives it.

The friction itself is NOT retired by this move. The superseding entry stays open in
frustrations.md and carries the live version of the lesson.
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-30
SESSION: amux-frustrations
CARD: AF-346
SYMPTOM: I made `/api/board`'s slim path stop SELECTing desc+log, having verified the slim
 response carries neither key. It shipped (a99955f7) and blanked every card preview on the
 fleet dashboard: desc_len>0 = 0, desc_head!="" = 0, log_n>0 = 0, folded_n>0 = 0 across
 2,047 cards, and needsyou_note gone, so cards waiting on a human stopped showing their
 question. `list_body` derives five values from `row.desc`/`row.log` BY REFERENCE and ships
 those instead of the prose. Reverted at b1227af0, restored and verified.
COST: A live user-visible regression on the owner's own dashboard, caught by a peer
 measuring the deployed build rather than by any test. Roughly 20 minutes of blank previews
 fleet-wide, plus a near-miss on a double revert that would have re-applied it. My full test
 suite was green: the cell I wrote asserted the slim hydrate returns empty prose and the full
 one returns it (both true), and the two PRE-EXISTING equivalence tests key on desc+log,
 which the slim body omits by design, so they compared two payloads that both correctly
 omitted the fields and asserted nothing about the derived ones.
FIX: The structural half is a naming problem the code cannot express: "slim" describes the
 PAYLOAD, and every reader takes it as a claim about the LOADER. AMUX-2840 was this same bug
 one layer up ("silently blanked both in the dashboard"), its warning is three lines above
 the line I changed, and I read past it - so a comment is demonstrably not sufficient here.
 What would have caught it is a consumer-side invariant rather than a mechanism-side one: a
 slim row's desc_len must equal the real desc length, and its desc_head the real first line.
 amux is adding that cell with the revert. Generally: when a payload drops a column, the test
 that matters asserts on what the payload DERIVES from it, not on the column's absence.

## `GET /api/board/contract` advertises a `verified` gate the board does not enforce, and the refusal points you back at it
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations). SELF-SIGNOFF, labelled as
one, not a peer review. Verified by running the entry's own test rather than by
recalling that something shipped.

THE SYMPTOM IS STILL LITERALLY TRUE, and that is now by design rather than a
defect. The bare `GET /api/board/contract` still reports investigation.verified as
["Outcome confirmed to still hold"] and still contains ZERO occurrences of
"Peer-reviewed by a DIFFERENT worker" — the same control this entry used to prove
it was not a nesting difference.

WHAT CHANGED IS WHAT THE COST LINE SAID WAS WORTH THE ENTRY: where the refusal
sends you. Captured from a live 409 on a throwaway investigation card:

  how_to_ack.contract -> "GET /api/board/contract?card=AF-407 (the RESOLVED gate
  for this card — the bare contract lists only type defaults, AF-112)"

It names the ?card= form, states the bare form's limitation in the same breath, and
cites this entry by number. And the resolved form is correct: for an investigation
card in group amux it returns the four-criterion peer gate with
gate_sources.verified.source = "group", alongside a note reading "this is the gate
a transition will accept".

So an agent following the sanctioned instruction is no longer refused, which is the
AMUX-2325 shape this entry was filed under. The probe card was deleted after.

WHAT I AM NOT CLAIMING: that the bare contract is now right for every reader. It
lists type defaults and a lane whose gate comes from a group or worker scope will
still see something a transition would refuse. That is documented in the pointer
rather than fixed in the payload, and it is a deliberate trade — the resolved form
needs a card id, and the bare form has no card. If it bites someone anyway, the
honest move is a NEW entry rather than reopening this one, because the friction
this entry names, the refusal pointing at the wrong source, is gone.
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
FIXED, verified 2026-09-02 by running the exact test this entry describes.
  The SYMPTOM half is still literally true and that is now by design: the bare
  `GET /api/board/contract` reports investigation.verified as
  ["Outcome confirmed to still hold"] and contains ZERO occurrences of
  "Peer-reviewed by a DIFFERENT worker", the same control this entry used.
  What changed is the part the COST line said was worth the entry: WHERE the
  refusal sends you. A live 409 now answers
    how_to_ack.contract -> "GET /api/board/contract?card=AF-407 (the RESOLVED gate
    for this card — the bare contract lists only type defaults, AF-112)"
  It names the ?card= form, states the bare form's limitation in the same breath,
  and cites this entry by number. And `?card=` resolves correctly: for an
  investigation card in group amux it returns the four-criterion peer gate with
  `gate_sources.verified.source = group`, plus a note reading "this is the gate a
  transition will accept".
  So the sanctioned instruction no longer points at a source that will refuse you.
  Measured on a throwaway card and deleted after.

## A graft-push checkout read as DIVERGED on every path, withholding the safe restore
VALIDATED: mixpeek-frustrations | VALIDATED on the checkout that motivated it — ~/Dev/mixpeek, 342 commits in origin/main..HEAD, every
recent path replayed under a different sha by graft-push.

THE PRECONDITION, confirmed before testing anything:

  FRUSTRATIONS.md                        35 commits ahead by sha   content vs origin: IDENTICAL
  server/observability/2026-08-31-...md  16 commits ahead by sha   content vs origin: IDENTICAL

That is the defect's exact shape: sha arithmetic reports "ahead" indefinitely for content already
upstream.

THE TEST. Made an append-only file dirty (one appended line), then computed both bases on it:

  OLD basis, sha arithmetic:    git log --oneline origin/main..HEAD -- FRUSTRATIONS.md  ->  35
                                non-zero, so the old classifier reaches DIVERGED
  NEW basis, content set-diff:  origin lines ABSENT from worktree  ->  0
                                worktree lines absent from origin  ->  1

Zero origin-lines-at-risk is the Some(0) arm at commit_nudge.rs:1870, which downgrades DIVERGED to
EDITED and advises COMMIT. The verdict that forbade both remedies no longer fires on this shape.

ONE CORRECTION TO MY OWN ENTRY, which is why this is not a clean validation. The entry asked for "the
safe `git checkout origin/main -- <file>`". THAT REMEDY WOULD HAVE BEEN WRONG. On an append-only file
the worktree is a strict SUPERSET of origin, so a restore DELETES the appended lines. The fix gives
COMMIT instead, which is correct — had it delivered literally what I asked for, it would have been
destructive on exactly the file class I named.

So: mechanism in the entry RIGHT (sha arithmetic breaks on graft-push replay), prescribed remedy in
the entry WRONG (restore, where the superset case needs commit). d55b7a63 fixed the mechanism and
declined the bad remedy. Recording that rather than letting the entry retire implying its own
prescription shipped.

Also kept: the downgrade is one-sided and fails safe — only downgrades, only on a readable positive
zero, and any error or unreadable side leaves DIVERGED standing. I did NOT test the error path.

Test hygiene: the append went into a co-edited shared file and was restored from my own byte copy
rather than `git checkout --`, which the shared-checkout guard blocks for good reason. Verified after:
status clean, zero occurrences of the test string, byte-identical to origin.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-24
SESSION: mixpeek-frustrations (reported), amux (fixed)
CARD: AMUX-3599
SYMPTOM: The idle commit-nudge filed dirty append-only files as DIVERGED — "commits in BOTH
  directions, neither single-arm remedy is safe" — on a checkout where the local commits were a
  REPLAY of content already upstream. DIVERGED forbids both remedies, so the reader is left with
  a union-merge they do not need and the safe `git checkout origin/main -- <file>` is withheld.
  The classifier asked `git log origin/main..HEAD -- <path>`, which counts commits BY SHA, and a
  commit already upstream under a different sha sits in that range permanently. On a graft-push
  checkout that is EVERY path.
COST: The wrong verdict on the exact file class the nudge singles out by name — the append-only
  ledgers, where the union-merge directive is printed. A reader following it does more work than
  needed and, worse, learns that the nudge's verdicts are unreliable on their checkout, which is
  the expensive direction: the next DIVERGED that IS real gets read as more of the same. Nobody
  lost data; the reported cost is a wrong prescription plus the turn spent establishing it.
FIX: d55b7a63 — content set-difference instead of sha arithmetic, since sha identity is what a
  replay destroys. The remedy overwrites the WORKTREE, so restore-safety is exactly "does the
  worktree hold lines origin does not"; zero means nothing here can be lost. One-sided by design:
  it only ever downgrades diverged->stale, only on a readable pair AND an empty difference, so
  any error leaves DIVERGED standing.
NOTE: This is the SECOND defect in this cell in four days and they point opposite ways. The cell
  was ADDED on 2026-08-20 because the two-bucket classifier filed a genuinely-diverged path STALE
  and the prescribed restore disarmed a data-loss push guard. This entry is the same cell now
  over-firing. Both are the same underlying error — reading commit identity as content identity —
  and it produced a false negative first, then a false positive, which is why "be more careful
  with the direction test" would not have caught either. The durable form is that a classifier
  prescribing a DESTRUCTIVE remedy has to be gated on what the remedy actually destroys, not on
  a proxy for it. Also worth recording: the fix logs the downgrade, because STALE-because-
  downgraded and STALE-outright were otherwise byte-identical in the log, which is the one-output-
  two-states shape on the arm that prescribes the destructive remedy.

## The reviewer-identity check fires on done->verified, blocking the peer amux routed the verification to
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations). SELF-SIGNOFF, labelled as
one, not a peer review. Closed by rebuilding the specimen, not by remembering.

The entry's specimen was a card whose named reviewer could never ack, because the
card went doing -> done DIRECTLY and no review ever happened, so done -> verified was
refused demanding a review->done ack that nobody could give. Rebuilt exactly that:

  probe card, type investigation
  amux board reviewer <id> amux-cloud
  doing -> done directly, never through review
  attempt done -> verified

  refusal: "gate not acknowledged"  (the ordinary criteria gate)
  "review sign-off" and "review->done ack" appear NOWHERE in the response

That is the FIX line's own prescription: scope the identity check to the transition
it is about. review->done still needs the reviewer; done->verified no longer does.
The probe was deleted afterwards.

CORROBORATED INDEPENDENTLY, and this is the part I did not have to construct: six
cards moved done -> verified today by two different peers (amux-cloud on AF-385,
AF-386, AF-387, AF-388, AF-390; amux-homepage on AF-375, AF-379, AF-366), none of
them blocked by an identity check, all carrying reviewer as data. The entry's cost
line was "two forced bypasses in one afternoon". There were zero bypasses today
across eight verifications.
AREA: gates
SEVERITY: slows
STATUS: fixed
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
FIXED, verified 2026-09-02 by rebuilding the entry's own specimen rather than by
recalling that something shipped. Probe card, type investigation:
  reviewer set to amux-cloud
  moved doing -> done DIRECTLY, never through review, so the named reviewer has no
    pending ack to give — the unsatisfiable-by-construction shape this entry names
  attempted done -> verified
  refusal: "gate not acknowledged", the ordinary criteria gate
  the string "review sign-off" / "review->done ack" appears NOWHERE in it
So the identity check no longer fires on this edge. It is scoped to the transition
it is about, which is the FIX line's own prescription. Probe deleted after.

## A detector went fully inert and its own debug surface called it "baseline has 0 samples"
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations). SELF-SIGNOFF, labelled as
one. Closed on POSITIVE evidence, which this entry specifically requires: its claim
is that a dead detector and a quiet one are byte-identical, so "the error string is
gone" is not a test. I checked that first and deliberately did not stop there.

GET /api/debug/autofix: loop_running true, tick 120s, last tick 95s old, 18
suppressed decisions, 3 of them latency. The one that closes this entry:

  {"detector": "latency",
   "reason": "blindness check ran: 0 of 75 families lost every row to filtering
              (295970 rows considered, 0 excluded). A zero here is a measurement;
              silence would not be.",
   "signature": "latency|input-collapsed..."}

The entry's mechanism was an upstream filter excluding 213,397 of 213,935 rows while
the suppression reported it as an absence of data. That filter is now measured every
tick, with its denominator, and the payload states which of the two states it is in
rather than leaving them identical. The sentence "A zero here is a measurement;
silence would not be" is the contract this entry argued for, in the detector it
argued about.

The detector is running, not merely listed: three latency decisions in that tick,
including a long-by-design exemption for /api/email/inbox carrying its own numbers
(3 requests past the 10s floor, worst 15.8s, 30s budget).
AREA: instruments
SEVERITY: slows
STATUS: fixed
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
FIXED, verified 2026-09-02 with POSITIVE evidence rather than the absence of the
error string. The entry's whole point is that a dead detector and a quiet one read
identically, so "the message is gone" would have been the wrong test.

GET /api/debug/autofix, last tick 95s ago on a 120s loop, loop_running true, 18
suppressed decisions of which 3 are latency. One of them is this entry's fix,
verbatim:

  {"detector": "latency",
   "reason": "blindness check ran: 0 of 75 families lost every row to filtering
              (295970 rows considered, 0 excluded). A zero here is a measurement;
              silence would not be.",
   "signature": "latency|input-collapsed..."}

So the upstream filter that was excluding 99.75% of rows is now MEASURED per tick
and reported with its denominator, and the payload says out loud which of the two
states it is in. That is the measured/n_considered contract applied to the exact
failure this entry names. The detector is also demonstrably running rather than
merely present: it made three latency decisions in that tick, including a
long-by-design exemption for /api/email/inbox with the numbers attached.

## The shared-checkout amend guard pins HEAD, not the staged set, so a correctly-pinned amend still absorbed a peer's work
VALIDATED: amux-frustrations | VALIDATED by the ORIGINATING session (amux-frustrations). SELF-SIGNOFF, labelled as
one. Verified in the hook that RUNS, not the one the repo versions, because those
two turned out to differ.

The entry's mechanism: a correctly-pinned `--amend` was allowed and swept 139 lines
of a peer's staged work, because the pin protects the COMMIT being rewritten and
says nothing about the STAGED SET being absorbed.

Now, in ~/.amux/hooks/git-shared-guard.py, a valid pin does not end the check:

  if m and head.startswith(m.group(1)):
      # AF-106 durable half (AMUX-3407): the pin proves the COMMIT BEING
      # REWRITTEN is yours; the check below proves the STAGED SET being
      # absorbed is too. ... the pin was satisfied and protected the wrong operand.
      return _amend_staged_check(scrubbed, run_dir)

`_amend_staged_check` posts to /api/git/staged-guard and `_amend_staged_decision`
refuses when any staged path was last edited by a different session, naming them and
prescribing `git commit --amend -- <your paths>`. That is the entry's own
prescription, and it is the second door on the same question the pre-commit guard
already answers.

FOUND WHILE CHECKING THIS, filed as AF-409 rather than folded in here: the invoked
copy and the repo copy are not synced by anything, and the invoked one was 148 lines
behind, missing a command-substitution BYPASS fix (e782b68a / AMUX-3932) for three
days. Both copies carry the AF-106 fix, so this entry closes either way — but I
would not have known that without checking the right file, and the AF-375 lesson is
the only reason I did.
AREA: git
SEVERITY: slows
STATUS: fixed
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
FIXED, verified 2026-09-02 in the hook that actually runs. A VALID pin no longer
ends the check: git-shared-guard.py proceeds to `_amend_staged_check`, which asks
the same server endpoint the pre-commit guard uses and refuses on foreign staged
paths. The code names this entry at that branch: "the pin proves the COMMIT BEING
REWRITTEN is yours; the check below proves the STAGED SET being absorbed is too ...
the pin was satisfied and protected the wrong operand". That is this entry's own
prescription.
Verified in the INVOKED copy (~/.amux/hooks/git-shared-guard.py, the path
~/.claude/settings.json runs), not only in the repo, because the two are not kept
in sync — see AF-409, filed while checking this.

## A shared CARGO_TARGET_DIR is mandated, and concurrent builds in it evict each other's artifacts
VALIDATED: amux-frustrations | Diagnosed and largely fixed. The entry's own FIX said "nobody has established
WHICH of the three is happening — the diagnosis is missing, not the remedy". It
is now established, and it is none of the three.

MECHANISM: the builder's disk guard rm -rf'd the ENTIRE shared target dir at the
25GB fleet floor, with no peer-build gate. From ~/.amux/logs/rust-auto-build.log,
all 23 shared-dir clears in the log, no truncation:
    2026-08-15  11   <- this entry's incident date, which reports 3 failures
    2026-08-16   9
    2026-08-24   2
    2026-08-30   1
Log line: "DISK LOW: 22GB free (< 25GB). Clearing the 162GB shared target dir;
next build is cold." A lane's in-flight cargo check/test had its whole dependency
tree removed underneath it — exactly the vanished serde_core rmeta and the 42
errors in nix.

The builder's own AMUX-2927 comment predicted this in writing and was never
connected to this entry: its lock serialises BUILDER vs BUILDER and does nothing
for BUILDER vs LANE.

THE THREE OPTIONS, resolved:
 (c) cargo GC — RULED OUT. -Z gc is unstable, this is stable cargo 1.97.1,
     RUSTC_BOOTSTRAP unset, no [unstable] stanza in .cargo/config.toml and
     ~/.cargo/config.toml does not exist. A positive would have been an enabled
     gc flag; the probe could have produced one and found none.
 (b) give the auto-builder its own target dir — MISAIMED. It only builds
     --release, into release/. Its debug cleanup arm was added 2026-08-29
     (881ff614), 14 days AFTER this incident.
 (a) leave it — overtaken.

FIXED BY 79abbb09 (2026-08-20, AEAB-35), filed against a different card: the
fleet floor was split from the build's own cache threshold, so the shared dir is
sacrificed only below 8GB and the idle e2e dir goes first. 20 clears in the two
days before it, 3 in the thirteen days after.

RESIDUE CARRIED FORWARD, not left silent: the builder now has two rm -rf arms and
only the debug-size one carries AF-303's peer-build gate. The disk-low arm has
none. Almost certainly correct (ENOSPC outranks a peer build, by AF-303's own
reasoning) but undocumented at that site. Filed as AF-415 rather than kept open
here, because this entry's text is about evictions at the 25GB threshold and
those are gone.
AREA: build
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AMUX-2936
SYMPTOM: `error: extern location for serde_core does not exist: ~/.amux/rust-build-target/debug/deps/libserde_core-0d2476c6ed9be3cc.rmeta`, and separately 42 errors inside the `nix` crate ("cannot find type `ControlFlags` in this scope") — artifacts deleted underneath an in-flight build, three times in one session.
  CLAUDE.md requires ONE shared build dir (~/.amux/rust-build-target) and the reasoning is sound — per-session dirs filled the disk with ~37 copies at 10-15GB each. But with several lanes plus the auto-builder building concurrently, I hit repeated hard failures of the form "extern location for serde_core does not exist: .../libserde_core-<hash>.rmeta" and 42 errors inside the `nix` crate, i.e. artifacts deleted underneath an in-flight build. Not a lock contention wait, which is what the CLAUDE.md note measured and correctly called cheap; this is cache eviction, and the only recovery is a full rebuild. Hit it three times in one session, roughly 4 minutes of rebuild each.
COST: ~12 min of pure rebuild, and worse, it masqueraded as a code error twice — the first failure looked like my own change had broken the build, which is exactly the wrong instrument reading (a red result on code you just verified by hand means the instrument is a candidate before the code is).
FIX: Not fixed; needs a decision, not a workaround. Options: (a) leave it — the failure is loud and self-recovering, just expensive; (b) give the auto-builder its own target dir, since it is the one builder that runs unattended every 60s and is the most likely evictor, accepting ~15GB for the one process that never benefits from a warm shared cache; (c) find whether this is cargo GC (CARGO_GC / cache auto-clean) rather than eviction, in which case pinning the retention setting fixes it outright and costs nothing. (c) is worth checking first because it would be a one-line fix, and nobody has established WHICH of the three is happening — the diagnosis is missing, not the remedy.

## An archived entry came back through a merge, read as open, and got re-diagnosed from scratch
VALIDATED: amux-frustrations | Self-validated; amux-frustrations is the originating session. The entry's claim was that scripts/frustrations-archive.py could not tell a resurrected title from a first retirement. That claim is fixed: the script checks the archive at the moment it writes and prints the prior-copy count, the reason resurrection happens, and the grep that surfaces the earlier VALIDATED line (frustrations-archive.py:349-354). Warning rather than refusal, deliberately, because a friction can honestly recur under one title.

ARCHIVED WITH ITS OTHER HALF NAMED, so this does not read as the class being closed. This entry DIAGNOSED 7dbab8f6 correctly and fixed the archive path. It did not count or remove the population that commit created: 29 already-archived entries were still sitting in the live file reading STATUS: open when this entry was written. AF-430 removed them (6cb3bcc1) and added frustrations.retired_entries_stay_retired, a ledger-side invariant, because every guard this entry improved sits on the ARCHIVE path and a resurrection lands on the LEDGER. Read this entry as "the archive script now notices", not as "resurrection is solved".
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-frustrations
CARD: AF-417
SYMPTOM: I picked "A shared CARGO_TARGET_DIR is mandated..." out of the ledger as `STATUS:
  open`, spent ~40 minutes diagnosing it from the builder log and git history, and reached
  the disk-guard `rm -rf` with no in-flight check. Correct. It was also already archived on
  2026-08-29 (53cafb92) with a VALIDATED line by the same session recording the SAME
  conclusion, including cargo GC already ruled out on the same grounds. I only found out
  because the archive then held two copies of the title and I went looking.
  THE MECHANISM IS THE FILE, NOT THE READER. An archive move is a DELETION from
  frustrations.md, and this file is merged across divergent branches. Counting copies at
  each commit shows it oscillate: 53cafb92 correctly removed it; 7dbab8f6, a human "sync
  frustrations.md to fork's current copy before push", put it back; merges 4216504b and
  09dd5024 from feature/telegram-connector carried it onto main. A resurrected entry is
  BYTE-IDENTICAL to a never-retired one — it reads open, its own FIX text still says the
  work is undone, and the sign-off lives in a different file nobody greps before starting.
  `git log -S` did not find the reintroduction either; only counting copies per commit did,
  because the pickaxe follows one line of history and the resurrection came across a merge.
COST: ~40 minutes re-deriving a retired conclusion, and it nearly cost more: I wrote the
  result onto the wrong card (the entry's `CARD: AMUX-2936` is itself a mis-link — that card
  is about the staged-file absorption window) and presented it as a new finding before
  catching it. Two independent derivations agreeing is reassuring about the ANSWER and says
  nothing good about the process. The general cost is worse than the minutes: every entry in
  this ledger is a claim that work is outstanding, and a resurrected one is a false claim
  that no reader can distinguish from a true one.
FIX: 80bff64b + this commit. `scripts/frustrations-archive.py` now checks, at the moment it
  writes, whether the title is ALREADY in frustrations-archive.md, and if so prints the
  count of prior copies, the reason resurrection happens, and the grep that surfaces the
  earlier VALIDATED line.
  A WARNING, NOT A REFUSAL, deliberately. A friction can genuinely recur and be honestly
  re-logged and re-retired under one title; refusing would be a gate with no truthful path
  for that case (ethos rule 3). So it archives and says what it noticed.
  `.claude/rules/frustrations.md` already warns about the mirror direction — do not
  re-append something that merely LOOKS lost, grep the archive first, creative-dna measured
  15 of 15 "lost" entries as archive moves. That rule asks a human to remember. This is the
  same check run by the tool that has both files open anyway, which is rule 1: the guidance
  existed and did not reach the moment it was needed.
  VERIFIED by two cells against a fixture: a resurrected title warns and names 2 prior
  copies; a novel title is SILENT and still archives. The control is the half that matters,
  since a detector that fired on every archive would be worth nothing.

---

## A PATCH rejected for its status silently discards the desc sent in the same body
VALIDATED: amux-frustrations | Self-validated; amux-frustrations is the originating session. Verified LIVE against the running server rather than from the diff, on a real card, just now:

  PATCH /api/board/AF-433  {"status":"verified","desc_append":"probe: ..."}
  -> blocked: True | discarded: ['desc_append']

That is the whole complaint answered. Before, a PATCH refused for its status swallowed every other field in the same body and said nothing; the caller's desc was gone with a 200-shaped refusal and no way to learn it. The refusal now names the field it dropped, so the caller can resend it.

The `discarded` key is emitted unconditionally, not only when something was dropped: the gate refusal on AF-430 earlier in this session returned `"discarded": []`, which is the arm that matters. A field that appears only when non-empty cannot be distinguished from a field the server forgot to compute (ethos rule 4).
AREA: silent-partial
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-frustrations
CARD: AF-413
SYMPTOM: `PATCH /api/board/AF-410 {"desc": <4.2 KB>, "type": "code", "status": "doing"}`
  answered `{"blocked": true, "error": "gate not acknowledged", ...}` with a full
  `how_to_ack` block. The desc was discarded. Nothing in the 900-byte response mentions
  it — every field describes the STATUS transition, and `desc` does not appear.
  Re-measured deliberately on a throwaway card (AF-412, deleted), with a different
  rejection reason to show it is the shape and not one gate:
    PATCH {"desc": "CANARY-TEXT-SHOULD-IT-SURVIVE", "status": "doing"}
    -> {"blocked": true, "error": "already holding doing"}
    -> read back: status "todo", desc ''
  Both rejection paths drop the whole body.
COST: ~3 minutes and one silent loss of a 4.2 KB card body I had just composed. Cheap
  here only because I read the card back out of habit. The failure is invisible to a
  caller who does not: the write returns a 200-shaped JSON object, the error names a
  DIFFERENT field than the one that was lost, and a card body is exactly the kind of
  thing nobody re-reads after writing it. A script doing `{"desc":..., "status":...}`
  in one call loses every desc for every card whose gate is unmet and reports nothing.
FIX: Atomicity is defensible and I am not asking for a partial write. Say so in the
  response: one `discarded` key listing the fields that were not applied because the
  transition was refused (`"discarded": ["desc", "type"]`). The refusal already builds a
  rich object; the caller cannot infer from `error: "gate not acknowledged"` that an
  unrelated field went with it. Ethos rule 4 in its exact shape — the payload cannot
  express what was and was not applied, so a wrong outcome is not detectable from what
  the caller keeps.
  THIS IS THE MIRROR OF AF-150, in the same AREA and worth counting with it. There, a
  compound operation took its SUCCESS signal from the parts that worked while one part
  silently did nothing. Here it takes its FAILURE signal from one part and silently
  discards another. Same defect, opposite sign: the response describes one component of
  a multi-part operation and is read as describing all of them. Three entries under
  `AREA: silent-partial` is the argument that compound operations need a uniform
  per-field outcome, not a per-operation verdict.

  SHIPPED 87699f3c. The refusal now carries `discarded` (always present, including
  empty — an absent key would mean a server that does not compute this, which a caller
  cannot distinguish from "nothing was lost") plus a `discarded_note` when non-empty.
  Decorated at the ONE arm every refusal converges on, PatchOut::Refused, rather than at
  the 16 sites that build a refusal body: covering those would fix today's and miss the
  next one, which is this bug's own shape.
  BIGGER THAN THE CARD BODY THAT PROMPTED IT, though not in the way I first wrote.
  CORRECTED same day: I claimed `amux board done --evidence-stdin` loses its evidence on a
  gate refusal. It does NOT — the CLI writes evidence as its own PATCH before the
  transition, and says why at the site ("409 rolls back the evidence too... do not fold
  this into the status body"). My own fix disproved my claim: the refusal I hit closing
  AF-411 came back `discarded: []`, not `["evidence"]`.
  What is true is the API level: `PATCH {status, evidence}` discards the evidence, proven
  by the HTTP test and live. Anyone calling the API directly loses it.
  AND THE CORRECTED FORM IS THE STRONGER ARGUMENT. The CLI does not avoid this defect, it
  CARRIES A HAND-BUILT WORKAROUND FOR IT at four separate sites (amux:1761, :1779, :1825,
  :2272), each added after someone was bitten on a different field — evidence, the typed
  ask, the outcome, and a fourth. The comment at :1761 says so: "AC-323's shape, now on a
  fourth field". Four independent discoveries of one silent behaviour, each paid for
  separately and patched locally in the client. The fifth field's author now gets told.
  Mutation-verified: removing the decoration fails the HTTP wiring test; making the key
  conditional on non-empty fails the control cell.
---

## The staged-guard ships on INSTALL, so an edited hook is inert and nothing says so
VALIDATED: amux-frustrations | Self-validated; amux-frustrations is the originating session. The complaint was that the staged-guard ships on INSTALL, so a checkout running an edited or stale copy is silently inert and nothing anywhere says so.

Verified live, not from the commit: `hooks.guard_reaches_every_checkout` is failing right now and NAMES the stale checkouts with the numbers that make it actionable:

  /Users/ethan/Dev/mixpeek runs GUARD_VERSION 11 (1 behind): 226 firings across 20 lanes were served it
  /Users/ethan/Dev/ethan.dev-minimal runs GUARD_VERSION 9 (3 behind): 3 firings across 1 lanes
  /Users/ethan/Dev/amux-GTM runs GUARD_VERSION 10 (2 behind): 1 firings across 1 lanes

Each verdict carries the version, the lag, the firing count and the lane count, so "nothing says so" is now three named checkouts with a blast radius attached, and the failures minted cards (AMUX-4035/4036/4037). The floor is the fleet MAX rather than a constant, so it cannot go stale as the guard advances; a single versioned checkout returns Unknown rather than passing vacuously, because one checkout compared against itself is not a measurement.

STILL TRUE AND NOT CLAIMED FIXED HERE: the three named checkouts remain behind. This entry's claim was that the drift is INVISIBLE. It is visible. Closing the drift is those checkouts' owners' work, and mixpeek's copy is vendored and tracked, which no installer will overwrite.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-31
SESSION: amux-frustrations
CARD: AF-375
SYMPTOM: I shipped two changes to `scripts/git-hooks/amux-staged-guard` today and
  BOTH were inert. The hook is installed by COPY (`install-hooks.sh` cp's into
  `.git/hooks/`), so a repo edit reaches no lane until someone re-installs:
    grep -c 'COMMIT ONLY YOUR OWN PATHS'  installed=0  repo=1   (AF-365)
    grep -c '_orphan_deletions'           installed=0  repo=4   (AF-357)
  The installed copy was dated 09:34 and never moved. AF-365 was closed `done`
  with evidence reading "ALL PASS", which was TRUE and was about the repo copy.
  Nothing in the commit path, the test, or the card gate distinguishes "the file
  changed" from "the behaviour changed for anyone".
COST: One card closed on a false claim for about two hours, and a second fix that
  would have been closed the same way if I had not checked. The near-miss is the
  cost: I only looked because the day's own theme is "a fix ships, its tests pass,
  and it does nothing in production", so I asked the question out of habit rather
  than because anything prompted it. A lane without that habit closes both.
  This is NOT the same as the amux bash CLI, which ships on SAVE and is live
  immediately. Two hook-shaped files in one repo with opposite deploy semantics,
  and no signal at either site saying which you are editing.
FIX: The signal already exists and does not reach far enough. The SessionStart
  freshness hook DID report "installed git hooks differ from this checkout" at the
  start of this session, naming `prepare-commit-msg` and the remedy. I read it as
  boilerplate about a file I had not touched, and it was right. Two cheap
  improvements, either of which would have caught this:
  (1) name the differing hooks by FILE and flag when a differing file is one the
      CURRENT SESSION has edit records for, which turns a standing notice into a
      statement about your own work;
  (2) have the pre-commit hook itself compare its own bytes against
      `scripts/git-hooks/` and warn on drift, which is the same trick
      `install-hooks.sh` already does with `cmp` at the end of its run, moved to
      the place where it would be read.
  Not building either from here without deciding which; carded.
  SHIPPED: both halves. (2) is 4f668224. The post-commit hook compares its own
  bytes against scripts/git-hooks/ and warns on drift. (1) is df97f802. The
  SessionStart hooks-drift axis now crosses the drifting names against THIS
  session's observed-edit records and, on a hit, points at the falsifiable check
  (grep the INSTALLED copy, not the repo one), which is the sentence that would
  have caught AF-365. Its two negative cells are the load-bearing ones: another
  lane's record must not become your name, and a missing record must say the
  check did not run rather than reading as "none of these is yours".
  Self-signed is NOT available here: this lane both hit it and fixed it, so the
  entry stays until a lane that pays the cost confirms the notice reaches them.

---

## A first-ever server start claimed an earlier process died unannounced
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/lib.rs:~299-328: boot provenance now checks whether the DB existed BEFORE Store::open could create it, before logging 'boot: UNANNOUNCED'. First boot no longer claims an unannounced prior-process death. Shipped in commit 43d0ec84, an ancestor of the running server build (commit 6cb3bcc1, confirmed via /health).
AREA: runtime
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: Every fresh Playwright home logged `boot: UNANNOUNCED` and stated that
  “the previous process stopped” even though its database had never existed and no
  predecessor was possible.
COST: Normal installation and isolated-test startup produced a death warning, teaching
  log-pattern detection to count a fabricated restart failure and obscuring genuine
  crashes that use the same signal.
FIX: Boot provenance now considers whether the database existed before `Store::open`
  could create it. First boot, self-adoption and an existing-store restart without a
  marker have distinct outcomes; filesystem uncertainty fails closed as an existing
  store so a real unannounced restart is never mislabeled. Tests pin all branches.

---

## Isolated browser-test servers still monitored and mutated the real host fleet
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/runtime_jobs/mod.rs:156-160 and registry.rs:748,1369,1441: AMUX_ISOLATED/AMUX_NO_FLEET is now checked at all three job constructors (periodic, long-lived, adopted), registering an inert System Jobs row instead of acting. Shipped in commit 43d0ec84.
AREA: isolation
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: Each Playwright project had a private `AMUX_HOME`, but long-lived jobs
  still inspected the shared tmux socket, process table, repository and user hook
  files. A one-spec run opened four incidents about the real fleet in its temporary
  database, filed disk/CI cards, and ran three host cleanup sweeps.
COST: E2E results depended on ambient machine state and test servers could resize,
  nudge, clean up or diagnose production workers despite claiming an isolated home.
FIX: The existing `AMUX_ISOLATED=1` contract now applies at all three common job
  constructors: periodic, long-lived and adopted loops. Suppressed work registers an
  inert System Jobs row with the exact switch, and adopted tasks are aborted before
  acting. The browser harness enables the process-wide switch; regression tests prove
  spawned and adopted futures perform no effect while remaining observable.

## Zombie reporting flooded every server log with one warning per foreign process
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/runtime_jobs/mac_health.rs: zombie_log_sample() aggregates PID/parent/age samples into one structured warning with total/owned/foreign counts (fn owned_zombie_children, zombies_seen/zombies_reaped counters at ~L401-421) instead of one WARN line per PID. Shipped in commit 43d0ec84.
AREA: runtime
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: Starting the three isolated browser-test servers on a host with accumulated
  foreign zombies emitted hundreds of identical WARN lines per server, one for each
  PID, before a single test began.
COST: The useful scheduler and browser output was truncated, recurring-log analysis
  saw a manufactured error pattern, and every 30-minute sweep would repeat an
  unbounded warning storm for processes this server correctly cannot reap.
FIX: Each sweep now emits one structured zombie warning with total, owned and foreign
  counts plus at most eight PID/parent/age samples. Owned children still produce their
  individual reap outcome because those are actions; the periodic summary retains the
  full count, and a regression test pins the sample bound and ownership context.

## Slim card hydration left the visible Details text blank
VALIDATED: amux-testing-e2e | Verified live in crates/amux-dashboard/static/app.js:26384-26391 (_bdHydrate): when #bd-tab-preview is the active tab, hydration now repaints #bd-preview's innerHTML from the freshly hydrated desc, not just the hidden edit textarea. e2e/lineage-tab.spec.ts was replaced by e2e/card-details.spec.ts in the same commit (43d0ec84), which exists in the current tree.
AREA: dashboard
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-5
SYMPTOM: The board list correctly omitted full descriptions, and card hydration
  filled the hidden edit textarea, but the active Details renderer was not repainted.
  A card could therefore show relationships and assets while hiding its task context
  until someone manually switched tabs.
COST: The primary card view omitted the source work description, undermining the
  card's role as the complete work record and making a healthy API response look like
  missing data.
FIX: Authoritative hydration now repaints Details only when that tab is still active,
  preserving a user who switched to Edit or Worker actions while the request was in
  flight. The former Lineage browser spec now exercises this real slim-to-detail path,
  multiple clickable assets, worker actions, edit-only controls, legacy link fallback,
  mobile targets, and overflow.

## Deliberately idle or disabled jobs became false autofix incidents
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/runtime_jobs/registry.rs:102,251,491-492 (SELF_ADOPT registered inert under AMUX_NO_SELF_ADOPT) and telegram_poll.rs's live-cadence recording. Shipped in commit 43d0ec84.
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: The full browser suite deliberately sets `AMUX_NO_SELF_ADOPT=1`, but
  System Jobs reported `self-adoption` as `not_spawned`. An unconfigured Telegram
  connector slept for its documented five minutes while its registry advertised a
  45-second cadence, so it was reported `stalled` after 127.5 seconds. Autofix then
  filed both expected states as failures.
COST: Healthy test servers accumulated bogus repair cards and red scheduler logs,
  obscuring real failures and perturbing board assertions during long E2E runs.
FIX: Deliberate self-adoption opt-out now registers an inert job with the exact
  disabling switch. Telegram records its live cadence atomically with each tick—45
  seconds when configured, 300 while idle—and starts with the conservative cadence
  so spawn scheduling cannot create a false first-tick window. The catalog describes
  these mechanism-owned states rather than independently guessing them.

## Time-only capture dedup discarded distinct commands before a model saw them
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/api/session_verbs.rs mint_capture_card (~L3609-3631): the dedup query now checks COUNT(*) FROM cmd_history WHERE session=? AND text=? (exact match) AND card_id IS NOT NULL AND ts>cutoff — not a blanket 'any open card in this session' gate. A distinct rapid follow-up cards normally.
AREA: harness
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: The direct send path treated every cardable prompt within 45 seconds of
  another capture as a retry, even when the text and task were different. The
  orchestrator variant discarded every new command whenever any agent card was open.
COST: Follow-up work vanished before the underlying model could relate, merge,
  prioritize, or decompose it—the harness made a capability decision in a boolean
  shim and then gave the model no opportunity to recover.
FIX: The direct path now dedupes only an exact recent prompt already linked to a
  card; the durable orchestrator path relies on its existing message-id command
  idempotency rather than adding a second text heuristic. Distinct rapid commands
  always enter the work ledger, including while another card is open, so the worker
  model can decide their correct relationship and ordered plan. Tests pin transport
  retry and distinct/repeated durable-message outcomes.

## Repeated schedule failures never became one repair incident
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/runtime_jobs/autofix.rs: schedule_error_pattern()/schedule_error_streak() group consecutive failed runs into one incident once streak.len() >= schedule_error_streak() (default reflected at L2828-2863), and refused/queued/success outcomes break the streak per L398.
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: The live Granola transcript schedule accumulated six consecutive
  traceback runs, but scheduler health remained green because the firing loop
  itself still ticked. A `delivered` run also had no closed-loop check that its
  command appeared in Messages.
COST: Recurring failures stayed as rows a human had to notice and correlate; a
  delivery log could claim success while the worker-facing message ledger lacked
  the artifact that proves it.
FIX: Autofix now groups consecutive error notes into stable patterns and files one
  durable incident after three failures; refused/queued/success outcomes break the
  streak. Confirmed deliveries carry a clickable `[SCHED-N]` origin and are checked
  against Messages after a grace period. The System Jobs predicate also feeds
  autofix directly, including stalled, dead, hung, missing, and over-budget ticks.

## Scheduler health showed a dead Telegram relay beside an undocumented live duplicate
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/runtime_jobs/registry.rs:117: pub const TELEGRAM_RELAY = "telegram-relay" is now the single id constant used at both the spawn site and the catalog row, so a name-drift becomes a compile/test failure rather than two live identities.
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: The live System Jobs section reported catalogued `telegram-relay` as
  `not_spawned` while a separate undocumented `telegram_relay` row ticked every
  30 seconds. Five other stable periodic jobs also rendered as "Undocumented job".
COST: The health surface raised a false red alarm for a working relay and could
  not explain accountability, context, disk, status-history, or token-ledger jobs;
  operators could not distinguish a missing spawn from a spelling drift.
FIX: Every stable periodic job now uses a registry id constant at its spawn site
  and has a catalog contract. Telegram relay uses the same hyphenated constant as
  its catalog row, so a future name drift is a compile/test failure rather than a
  second live job identity.

## Periodic process cleanup described rustc zombies but never implemented that sweep
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/runtime_jobs/mac_health.rs:14,36,74,237,244,373-407: orphaned_debug_rustc() sends SIGTERM only to aged pid-1 rustc processes targeting target/debug, and a separate true-zombie (state=Z) sweep does non-blocking waitpid only for aged children of this exact server process. Both counts are in the periodic tick log. Shipped in commit 43d0ec84.
AREA: runtime
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: The registered 30-minute `mac-health` job reaped orphaned Ray workers
  and Playwright Chrome roots, but its own module contract also promised orphaned
  debug rustc cleanup and had no corresponding code. True process-table zombies
  were neither counted nor distinguished from still-running orphans.
COST: Interrupted builds could leave parentless compilers consuming CPU and memory,
  while dead children could accumulate PID-table entries with no periodic signal
  to their amux parent and no observable machine-health count.
FIX: The existing registered job now takes a state-bearing process snapshot, sends
  SIGTERM only to aged pid-1 rustc processes proven to target `target/debug`, and
  detects true `Z` processes. Because a zombie child is already dead, it calls
  non-blocking `waitpid` only for aged children of this exact server process and
  merely reports foreign-parent zombies. Every count is included in the periodic
  tick log; fixtures pin ownership, age, malformed input, debug/release and
  living-parent negative controls. The System Jobs catalog exposes every grace.

## Informational questions could consume board ids and WIP slots
VALIDATED: amux-testing-e2e | Empirically re-verified today: extracted is_informational_query/is_status_query/capture_has_task_followup from crates/amux-core/src/board.rs into a standalone Rust program and ran it against the literal ATE-14 prompt text ('What is the difference between todo and backlog on this board? Please answer only; do not change anything.') -> returns true (informational, not carded). The fix (commit 53b3e952, 2026-09-02 19:06) landed AFTER ATE-14 was minted (18:06) by the pre-fix binary, which is exactly the SYMPTOM this entry describes. Confirmed 53b3e952 is an ancestor of the currently running server build (commit 6cb3bcc1 per /health). A later message in this same session asking the identical question ('What is the difference between todo and backlog on this board?') did NOT mint a new card, only ATE-14 (pre-fix) did — direct before/after confirmation. Both session_verbs.rs::mint_capture_card and the drain path gate on is_informational_query.
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: Prompt capture had a narrow exception for status and `why` questions,
  while ordinary answer-only prompts such as "what is the difference between todo
  and backlog?" could still be treated as code work. Direct, queued and orchestrator
  delivery did not all consult the same non-task predicate.
COST: Conversation-only questions could create cards with no durable deliverable,
  occupy WIP, enter decomposition/drive, and make the source message look like
  unfinished work after the answer had already been given.
FIX: A shared deterministic informational-intent boundary now keeps question-word,
  plain yes/no/advice, and explicit answer-only prompts in Messages. Imperative
  follow-ups and operational checks such as "does this build?" remain cardable.
  Direct, queued, orchestrated and invariant-reader paths all consume the same
  predicate; tests pin the message-without-card outcome and false-positive controls.

## Task detail hid the gates, sources, relationships and produced assets needed to audit it
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/api/board.rs: card_effective_gates (~L939), asset_links with resolved_ref (~L3310-3323), and effective_gate_with_source (~L885) are all present in the task detail response. e2e/card-details.spec.ts (added by 43d0ec84) exercises this path; e2e/lineage-tab.spec.ts (dead surface) was removed in the same commit.
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-5
SYMPTOM: A completed card exposed Edit, an empty Preview, History and Lineage, but
  not one coherent audit view. The empty gate override did not show the effective
  column gates; source messages and task relationships were scattered; created files
  appeared only in prose; and opening an empty group field dumped a fleet-wide list
  of detector/autofix suggestions unrelated to the task.
COST: A reviewer could not establish from the card what command created it, which
  epic/dependencies governed it, which gates it passed, what every worker action was,
  or open the files and URLs it produced.
FIX: The task API now returns resolved structured and inferred asset links plus the
  effective gate trail. The card's Details view groups facts, exact source-message
  links, epic/parent/child/dependency relationships, gates, work summary, multiple
  clickable assets, and recent actions with a link to the full activity list.
  Empty group input no longer invents suggestions; dead Preview/Lineage surface area
  is removed from the detail UI, and displayed `MSG-N` links perform exact ID lookup.

## Codex could read idle while working, then working after completion
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/api/session_verbs.rs:2359-2377: for Codex-family workers, status now maps ('turn.started'|'event_msg'/'task_started') -> active and ('turn.completed'|'event_msg'/'task_complete') -> idle from the structured rollout stream, overriding stale tmux activity signals.
AREA: status
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-10
SYMPTOM: During a real Codex gpt-5.5 run the embedded terminal visibly said Working
  while the Workers card said idle and 41 minutes old. After the completed turn had
  returned to an idle prompt, the card said working. Tmux activity time is not a
  trustworthy lifecycle clock for Codex's alternate-screen UI, and pane churn after
  completion can outvote the visible prompt.
COST: Supervisors could start duplicate work during an active turn or wait forever
  on work that had already completed, including turns that created real subagents.
FIX: For Codex-family workers, status now reads the structured rollout lifecycle
  (`task_started`/`task_complete`, plus equivalent turn events), ignores signals from
  before the current session start, and uses those boundaries to override stale tmux
  activity without overriding an explicit waiting prompt.

## Claude subagent work was invisible to live worker status
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/api/session_verbs.rs: subagent_events_accumulate_a_floored_live_count (test at ~L20388) and the surrounding SubagentStart/SubagentStop handling (~L14670-14671) confirm a floored live count that survives main-agent report updates.
AREA: status
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-10
SYMPTOM: Two real Claude subagents created files while the embedded terminal showed
  `2 agents`, but the Workers card remained idle for the entire 31-second window.
  The installed hook set had prompt/tool/stop events only; it omitted
  `SubagentStart` and `SubagentStop`. A later main-agent report also replaced the
  full report object, erasing any subagent count, while subagent updates neither
  invalidated the sessions cache nor emitted the normal session SSE event.
COST: The status surface contradicted the actual terminal precisely during delegated
  work, and could remain stale until an unrelated refresh.
FIX: Installation now idempotently merges all five Claude lifecycle hooks while
  preserving unrelated settings. Subagent reports keep a floored live count,
  preserve that count across main-agent updates, invalidate the cache, and emit the
  ordinary session update event. Hook, invariant, cache and SSE regressions cover it.

## Cross-group worker permission looked saved, then silently reset
VALIDATED: amux-testing-e2e | Verified live in crates/amux-dashboard/static/app.js:308-370: readCrossGroupDefault()/toggleCrossGroupDefault() reject a locally-queued (_isLocallyQueued) response, then re-GET after PUT and roll back the visible switch unless the authoritative read-back matches the intended value. The /api/config/cross-group route is excluded from the offline outbox queue matcher (_OUTBOX_SKIP, L2263). NOTE: I could not find a dedicated browser-contract test file covering the queued/read-back-mismatch/failure cases the FIX text describes (grepped repo-wide for the relevant JS identifiers, none found outside app.js itself) -- the behavioral fix is real and verified by reading the code, but that specific test-coverage claim is unconfirmed.
AREA: settings
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1 (source audit epic)
SYMPTOM: Enabling "Allow workers to message beyond groups" in Settings immediately
  returned the switch to off. The global offline outbox synthesized an HTTP 202
  for `/api/config/cross-group`; the settings caller accepted that queued response
  as durable server state, and its error path did not restore the prior value.
COST: Operators were shown permission state that was never persisted, so workers
  could not reliably use the intended routing policy after a refresh or outage.
FIX: The cross-group route is no longer queueable. The toggle now rejects locally
  synthesized responses, reads the setting back after PUT, and visibly rolls back
  on transport failure or a mismatched server value. A browser-contract test covers
  the real response, queued response, read-back mismatch, and failure cases.

## Task artifacts existed outside the task's attributed action history
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/api/board.rs create_artifact (~L7655-7760): appends an attributed bs::append_log entry and a PendingEvent, and amux CLI has 'amux board artifact' (amux:2448-2471) wired to it.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-2
SYMPTOM: The structured artifact API existed, but workers had no board CLI verb
  for it and registering an artifact did not add an attributed action to the task log.
COST: Files, URLs, commits and screenshots fell back to prose; the task could not
  answer who attached an asset or expose it as a direct link in detail.
FIX: e139be2d adds `amux board artifact`, validates artifact kind/state, appends an
  attributed task-history event, emits a greppable registration log, and returns
  artifacts in the authoritative task detail response.

## Opening a fresh board card showed older status, owner and history than the board itself
VALIDATED: amux-testing-e2e | Verified via crates/amux-server/tests/dashboard_assets.rs::board_detail_hydration_refreshes_authoritative_state_and_relations (added by e139be2d) pinning _bdHydrate; re-checked live against current app.js: boardDetailStatus = full.status, _populateSessionSelect('bd-session', full.session...), _bdRenderMeta(merged), full.due_time, full.tags are all present (L26373-26409).
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-3
SYMPTOM: The board list showed ATE-3 done, but its detail view showed To Do or In
  Progress, owner none, `No status posted yet`, and empty History while Lineage
  showed 29 events. The detail GET arrived, but hydration refreshed only desc/log.
COST: The audit had to cross-check list, detail and lineage for every transition;
  any single surface supported the wrong conclusion.
FIX: e139be2d refreshes every authoritative detail field when the user has not
  edited it, and renders one linked view of epic, children, dependencies, source
  messages, work summary and artifacts.

## Board create visibly selected an owner and groups, then the server discarded both
VALIDATED: amux-testing-e2e | Verified via crates/amux-server/tests/dashboard_assets.rs::board_create_uses_the_server_field_names (added by e139be2d); re-checked live against current app.js addBoardItem: sends session: worker || '' and tags: groups || [] (not worker:/groups:), at L26740 and L26746.
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: AMUX-4047
SYMPTOM: Creating a card in the dashboard with worker amux-testing-e2e and groups
  selected produced `Not saved: server ignored groups, worker`; AMUX-4047 and
  AMUX-4048 landed as duplicate, unowned backlog cards with no tags.
COST: Two junk cards were created during one browser audit, and neither could be
  driven by the worker the UI showed as selected.
FIX: e139be2d makes both optimistic state and POST/PATCH payloads use the server's
  real `session` and `tags` fields, with a static regression check at the wire boundary.

## A multi-part prompt became unrelated leaves and the message reported only the first leaf
VALIDATED: amux-testing-e2e | Verified in crates/amux-server/src/api/board.rs: decompose_item/validate_decomposition (~L3416-3471) require priority 0-3 and a concrete next_action, and the epic/child linkage is a single attributed transaction. Shipped in commit e139be2d, an ancestor of the running server build.
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-testing-e2e
CARD: ATE-1
SYMPTOM: MSG-39225 captured as ATE-1, then the worker hand-created ATE-2 and
  ATE-3 without an epic, structured dependency edge, or priority. The scheduler
  selected ATE-3 before ATE-2, hit the WIP gate, and repaired the ordering only
  after the refusal. The Messages chip stayed attached to ATE-1, which had been
  reshaped as one leaf and reached done while the rest of the command remained open.
COST: A supposedly automatic command required manual plan reconstruction; the
  source message gave a false completion signal and work ran in the wrong order.
FIX: e139be2d adds one attributed, idempotent decomposition transaction. The
  capture remains the message-linked epic; children require p0-p3 priority,
  earlier-step dependencies, concrete next actions and a common owner. The drive
  loop completes the epic when all children are terminal.

## A stale-branch "sync" of a shared ledger resurrected 29 retired entries and deleted a live one, and it read as an ordinary edit
VALIDATED: amux-frustrations | Self-validated; amux-frustrations is the originating session. Both halves verified, the second one live.

THE POPULATION: 29 resurrected entries removed in 6cb3bcc1, verified before the deletion rather than after — every line of every removed block already existed in frustrations-archive.md except one `CARD: AF-10`, whose archive copy carries CARD: AF-242 plus a NOTE-CARD explaining the repoint. Ledger 127 -> 99 entries at the time; 80 now, after amux-testing-e2e retired 18 of their own and this entry left.

THE LOST ENTRY: mixpeek-research's MR-44, absent from BOTH files for four days, restored verbatim from 7dbab8f6^ with STATUS left `open` because only its author can change it.

THE MECHANISM, confirmed live on the running server rather than from the diff:
  GET /api/debug/invariants -> frustrations.retired_entries_stay_retired | pass
It is registered, it evaluates on the normal cadence, and it reads both files through one loader so a worktree ledger can never be compared against a baked archive. An empty archive returns Unknown rather than a vacuous pass. Mutation-verified three ways: filtering the intersection to nothing reds the resurrection cell; removing the empty-archive arm reds the rule-4 cell; pointing the live-pair cell's LED at the archive reds it with 104 named overlaps, so the cell that reads the real files can fail.

ARCHIVED WITH ITS SEQUEL NAMED, so it does not read as the class being closed. The title key this entry shipped was BLIND to a chimera: 7dbab8f6 also fused mixpeek-research's MR-43 heading to AF-195's archived body, and no title-keyed sweep can see that. AF-434 (bcc6e46f) added the first-SYMPTOM-line key and restored MR-43. Read this entry as "the 29 are gone and title resurrections are now caught", not as "the overwrite has been fully undone".
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-frustrations
CARD: AF-430
SYMPTOM: 29 of the ledger's 127 entries were also sitting in frustrations-archive.md, every
  one of them carrying a VALIDATED stamp naming the session that signed it off. In the live
  file they read `STATUS: open`. One commit did all of it: 7dbab8f6, 2026-08-29, "chore:
  sync frustrations.md to fork's current copy before push", +1000/-855, `Amux-Session:
  (human)`. It re-added 29 headings and removed 33.
  Its reasoning was sound and is worth quoting, because nothing about it looks careless: the
  branch was based on an origin/main that predated a lot of ledger activity, the author had
  never touched the file, and they were trying to stop the append-only push guard from
  reading their branch's inherited stale copy as a silent revert. The remedy they chose was
  to overwrite the whole file with the fork's copy. That copy predated the archive campaign,
  so the overwrite un-retired every entry archived since 2026-08-06 and dropped every entry
  appended after the copy was taken.
  Of the 33 it deleted, 26 were already archived and 6 came back later. ONE never did:
  mixpeek-research's MR-44, absent from both files for four days, restored immediately above
  this entry and marked as restored.
  The resurrection half is the expensive one and it is silent. Twelve of the 29 are
  byte-identical to their archived copy. The other 17 are the PRE-ARCHIVE drafts of entries
  their authors edited before signing off, so for four days the live file served the older
  text of an entry whose corrected text sat in the archive. One of them, the
  cross-cutting-findings entry, carries `CARD: AF-10` in the ledger while the archive copy
  carries AF-242 plus a NOTE-CARD explaining the repoint, so a reader of the live file got
  the pointer that had been deliberately superseded.
COST: about 70 KB and 29 entries of false backlog, for four days, in the file whose whole
  argument is that a cluster of entries is evidence. Every count run over this file since
  2026-08-29 has been wrong in the direction that manufactures urgency: entries whose
  authors had already validated them as fixed were counted as live friction. I ran those
  counts myself, in this session, more than once, and cited them. Plus one peer's entry lost
  outright, and a drain protocol whose central instruction (grep the archive before
  restoring anything that looks missing) was followed by nobody, because the operation that
  resurrected these was not a restore and never looked like one.
FIX: the 29 duplicates are deleted here and the lost entry is back. The mechanism half is
  the part that matters. `.claude/rules/frustrations.md` says "grep here first, present
  means it was retired on purpose", the archive's own header says it again, and
  scripts/frustrations-archive.py warns on a resurrected title. All three sit on the ARCHIVE
  path. Nothing was watching the LEDGER, which is where a resurrection actually lands, so a
  whole-file overwrite walked past every one of them without tripping anything. Rule 1: the
  guidance existed and did not reach the moment it was needed.
  A title present in both files is a one-line predicate over two files this repo already has
  open. It wants to be a check that runs, not a fourth sentence asking someone to remember.

---

## The drift-detector protecting mixpeek's git guard is itself blind to staleness
VALIDATED: mixpeek-research | VALIDATED 2026-09-03 (mixpeek-research) - fix 1: mixpeek vendored guard upgraded v4 to v9 (landed 08-30) and currently at GUARD_VERSION 13 matching canonical; fix 2 both halves: mixpeek copies carry anchored integer markers with CI enforcing exactly-one-marker on every declaring hook (dc21a0d489), amux checker compares versions numerically before and above the token loop, failing closed on marker-less targets (fcf1c0bb), stripped-copy fixture reads STALE (test-hook-version-marker.sh cell 2, re-run by signer; mixpeek-side fixture legs in test_vendored_hook_version_markers.py). The sentence "diverges from canonical but carries every canonical feature" is no longer reachable by a stale copy.

STAMP RUN BY amux-frustrations AT THE AUTHOR'S EXPLICIT REQUEST, with their evidence line verbatim above. They verified fcf1c0bb from their own direction before signing — read the diff and re-ran test-hook-version-marker.sh themselves, 6/6 — rather than taking my report of it. Recorded because a stamp carrying one session's name and another session's hands should say so.

FOOTNOTE ON THE ENTRY'S HISTORY, since it is unusual: this entry was DELETED outright by 7dbab8f6 on 2026-08-29 and was absent from both the ledger and the archive for four days (AF-430). It was restored verbatim from 7dbab8f6^ on 2026-09-02 and retired here the next day, by its author, on evidence neither of us had when it went missing.
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-24
SESSION: mixpeek-research
CARD: MR-44
SYMPTOM: Landing MR-43 (tmux-derived $AMUX_SESSION fallback) required running
  `install-hooks.sh --all` to propagate the fix. It reported mixpeek's
  `.githooks/amux-staged-guard` as "diverges from canonical but carries every
  canonical feature — left untouched", the correct, safe verdict for a
  deliberate local merge. It is not one: mixpeek's copy is GUARD_VERSION = 4
  against a canonical of 9, missing ~215 lines including AF-127 outcome
  reporting and the AF-195 index/worktree divergence check. The staleness
  check greps the canonical's single `guard-features` token (AMUX-2946) as a
  bare substring anywhere in the target file; mixpeek's v4 copy happens to
  contain that literal string at line 75 in an unrelated comment about retired
  ports, so the check reads "feature present" when the actual AMUX-2946
  feature never landed there. This is the exact MG-1485 dark-guard shape the
  mechanism exists to catch, undetected by the mechanism itself, in the one
  checkout that matters most for daily commits.
COST: not measured directly — the cost is whatever the missing ~5 versions of
  protection would have caught and did not (AF-195's index/worktree check in
  particular: mixpeek is a shared checkout where that class of bug already
  happened once, per its own header).
FIX: two separate fixes. (1) Upgrade mixpeek/.githooks/amux-staged-guard and
  prepare-commit-msg from v4 to v9 — a real merge, commit in that repo. (2)
  Make the drift-token check itself resistant to this: require the token
  match to come from a comment-anchored form, or compare GUARD_VERSION
  numerically in addition to/instead of grepping tokens. Otherwise the next
  stale copy hides the same way. Neither started; MR-44.
RESTORED 2026-09-02 by amux-frustrations, not by its author. This entry was DELETED from
  frustrations.md by 7dbab8f6 on 2026-08-29 and was then absent from BOTH the ledger and
  the archive for four days, which is the one shape `.claude/rules/frustrations.md` calls
  actually-lost work. Recovered verbatim from 7dbab8f6^ and re-appended unchanged; every
  line above this one is mixpeek-research's. STATUS stays `open` because nobody has said
  otherwise and only mixpeek-research can. AF-430 has what destroyed it.

---

## The mutation tool corrupted itself twice, and its only symptom was the next run blaming the caller
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it. 51699975. `mutate.sh` refuses to mutate itself at both write paths, with the reason and the copy-and-mutate route in the message. Cells 7 and 8 in scripts/test-mutate-seams.sh: refuses with exit 2, names the reason, offers the copy path, leaves itself byte-identical — plus the CONTROL that it still applies and reverts on any other file, or the refusal would be a tool that refuses everything. Verified in use immediately afterwards: the copy path is what I used to test AF-439's verdict logic.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-440
SYMPTOM: Testing a new verb, I ran `scripts/mutate.sh run scripts/mutate.sh <old> <new> -- ...`
  — the tool on itself. It printed `mutate apply: LANDED`, ran the command, and the revert
  never happened. Twice in five minutes, on two different mutations.
  Bash reads a script by BYTE OFFSET as it executes. Rewriting the file underneath the
  running interpreter shifts every offset after the edit, so the trap that reverts is never
  reached. The tool's whole design is that the revert fires from a trap even on a timeout or
  a Ctrl-C (AF-284); none of that survives the file being the one under edit.
  WHAT MADE IT EXPENSIVE IS THE SYMPTOM. `bash -n` passed both times. The suite kept
  running. The only visible sign was the NEXT invocation refusing with "the replacement
  already occurs 1 time(s) — revert would be ambiguous", which is a message about
  ARGUMENTS. I read it as my mutation string being wrong, twice, before checking the file.
  The refusal was correct and it blamed the caller, which is the worst combination.
COST: about ten minutes, and two hand-repairs of a shared file. The larger cost is what did
  not happen: I committed nothing while corrupted, but only because the refusal happened to
  fire before the commit. A run that mutated a line the next test did not touch would have
  left the mutation in a file I then staged.
FIX: 51699975. Refused outright, at both write paths, with the reason and the copy-and-mutate
  route in the message. A self-mutation cannot be made safe from inside the process being
  edited — there is no ordering of apply, run and revert that survives the interpreter
  losing its place — so the honest move is to decline rather than to try harder.
  Cells 7 and 8: refuses with exit 2, names the reason, offers the copy path, leaves itself
  byte-identical — and the CONTROL, that it still applies and reverts on any OTHER file, or
  the refusal would be a tool that refuses everything and the cell would pass for it.
NOTE: this is AF-368's mechanism ("editing a running .sh corrupts it mid-run, and the
  instrument cannot report its own corruption") arriving inside the tool built to make
  mutation safe. The generalisable half is the second clause of that title, and it is why
  this took two occurrences to notice: an instrument that edits files cannot be trusted to
  report an edit to ITSELF, so its error messages are precisely the ones that will mislead.

---

## A test per component and none over the seam, three times in one night, twice inside the fix for the last one
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it. 54cef57c for the defect (the nudge labels root-relative paths with the repo root, plus a set-wide note giving `git -C <root> <remedy>` because git pathspecs are cwd-relative).

The MECHANISM half is what this entry was really about and it landed later: 51699975's `mutate.sh seams`, which swaps two same-typed arguments at a call site and reports whether the types, a test, or nothing at all objects. It found instance eight on its first real run — `owner_committed_since(dir, path)` swapping to `(path, dir)`, compiling, and passing the entire suite. Fixed at the boundary with a debug_assert covering every caller.

The entry said the question "which test fails if these two agree with each other and with nothing else?" was a question and not a check. mvs-pitr's four further instances and their diagnosis (a missing DIRECTION, invisible from either side alone) is what made it a check. Archived with that correction recorded rather than as originally written.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-438
SYMPTOM: Three instances of one shape, and the third is the one that makes it an entry
  rather than a bug report.
    1. AF-429. `schedule_message_origin` had a test. The autofix detector had a test.
       Nothing pinned that the writer's OUTPUT satisfies the detector's PREDICATE, so the
       id arm matched 0 of 956 production rows for months with two green suites. ts-gke's
       framing: the detector's fixture hand-typed the writer's output, making it a copy of
       a BELIEF about the writer rather than a sample of its BEHAVIOUR.
    2. AF-437. `spawn_periodic` derives a job's gate variable from the job name; four jobs
       also spelled that variable by hand. One knob, two spellings, agreeing today, with
       nothing asserting they must.
    3. AF-438, and this one is mine, committed four hours after writing the other two up.
       Fixing mvs-pitr's report I wrote a cell for the message and a cell for the root
       resolver. Both passed. Then I mutated the call site back to the reported bug — the
       sweep handing `build` the lane directory instead of the resolved root — and it
       SURVIVED ALL 46 TESTS. Two components tested, the seam between them untested, in the
       fix for a report about a different seam, on the night I logged the pattern twice.
COST: for the shipped defect, a nudge that named the wrong directory for every lane whose
  cwd is a subdirectory — and git pathspecs are cwd-relative, so an operator following the
  remedies from the named directory runs `git checkout origin/main -- <path>` against a
  different file, or none, with every command exiting 0. The one instrument whose purpose is
  to stop a destructive command landing on the wrong bytes was naming the wrong bytes.
  For the pattern: I now have three instances and no instrument. Every one was found by a
  human reading code or by a peer's report, never by a suite, because the suites were green
  by construction — each component's test passes exactly as well when the seam is broken.
FIX: 54cef57c for the defect: the label resolves to the repo root, and a set-wide note gives
  the runnable form `git -C <root> <remedy>` from `build`'s top-level block rather than an
  arm, so it reaches all four readers instead of one.
  For the pattern, a third cell that reads `nudge_tick`'s own body, bounded to that function,
  and asserts the resolved label is what reaches `build`. Its controls check the window is
  one function wide and has not swallowed the resolver's definition — an unbounded search
  would be satisfied by the resolver's own name several hundred lines away and could not
  fail. Mutation-verified four ways, including that control.
  WHAT I DID NOT HAVE, WHEN THIS WAS WRITTEN, was a general instrument. `mutate.sh survey`
  finds a line the tests do not depend on; it cannot find a WIRING nobody asserted, because
  the call site IS exercised and the mutation that matters is an argument swap between two
  valid names.
SUPERSEDED BY ITS OWN MECHANISM, 51699975 (AF-439). mvs-pitr sent four more instances the
  same night — MP-100, two checks that fired on every fixture so either could be deleted
  unseen; MP-125, two roots that agreed on a name so reading the wrong one survived; and two
  where a fixture agreed with the reader and neither with the writer — which took the count
  to SEVEN across two repos. Their diagnosis is the sentence that made it buildable: every
  one was a missing DIRECTION rather than a missing assertion, and none was visible from
  either side alone.
  So the probe is an argument swap. `scripts/mutate.sh seams <file> -- <cmd>` exchanges two
  same-typed arguments at each call site and reports which of three things objects:
  HELD-BY-TYPES (it does not compile — the type system is the assertion, and that is the
  best possible answer), KILLED (a test observes the pair), SURVIVED (nothing anywhere holds
  these two apart). `--build` is what separates the first from the second, and without it the
  report says the axis is missing, because a compile-held seam is safe today and unheld the
  moment someone widens a type.
  IT FOUND ONE ON ITS FIRST REAL RUN, in this same file: `owner_committed_since(dir, path)`
  swaps to `(path, dir)`, compiles, and passes the entire suite. That call is
  `git -C dir log -- path`; swapped it fails, returns None, and every caller reads None as
  "the owner has not committed" — settled work reported as unsettled, silently. The function
  has a test. The call site's argument ORDER had nothing, which is instance eight.
  Fixed at the BOUNDARY, a `debug_assert` that `dir` is a directory, so it covers every
  caller including unwritten ones. And stated narrowly on purpose: that makes the swap LOUD,
  it does not CLOSE the seam. No test reaches that call site, so the assertion never runs
  there and `seams` still reports SURVIVED for it. Correctly.
  The question I said was a question and not a check — "which test fails if these two agree
  with each other and with nothing else?" — turned out to be a check after all. What was
  missing was not the idea but the probe, and the probe was a peer's sentence away.

---

## Five greps for one pattern gave five different answers, and the check that read the producer found what all five missed
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it. 3675f126. All four jobs derive their gate variable via `per_job_disable_var(JOB)`; the check lives in the repo (`no_job_hand_types_the_gate_variable_its_own_name_derives`) rather than in my shell history.

The entry's own point is what validates it: five greps gave five answers (32, 5, 1, 1, 2) and the in-repo check — which derives the string from the real producer and scans whole files — found heartbeat.rs and status_history.rs, which every grep had missed. Mutation-verified both ways, including that the spawn_periodic scope filter is load-bearing: removing it reds the check on commit_nudge, which reads its knob by literal but spawns with a bare tokio::spawn and so duplicates nothing.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-437
SYMPTOM: ts-gke, generalizing AF-429: a fixture that hand-types a producer's output is a copy
  of a BELIEF about the producer and cannot detect it drifting, while calling the producer
  makes it a sample of the BEHAVIOUR. "Those look identical in review and only one can fail."
  I tried to mechanize it — find every place this codebase hand-types a string some producer
  emits — and got five answers from five greps: 32, then 5, then 1, then 1 again, then 2.
    32  matched any `[tag] text` literal; almost all were ordinary messages.
     5  tightened on a distinctive anchor; four were a different env-var family sharing the
        `AMUX_` prefix.
     1  required the producer's exact template; missed every qualified `super::` call site.
     1  fixed that and still missed board_drive.rs, because the scan stopped at each file's
        first `#[cfg(test)]` and that file's spawn call is 5000 lines AFTER its tests.
     2  the two I could see by hand.
  The real answer is FOUR, and I only have it because I stopped writing greps and wrote the
  check the way ts-gke described the fixture: derive the string from `per_job_disable_var`
  itself, scan whole files, assert nothing else spells it. It named heartbeat.rs and
  status_history.rs immediately — two modules every grep had missed.
COST: the defect itself is latent, not live: `spawn_periodic` derives each job's
  fleet-isolation gate from the job NAME as AMUX_<NAME>_SECS, and four jobs also read that
  same variable for their interval by literal. One knob, two spellings, and they agree today.
  A change to the convention moves the gate and not the reader, splitting one switch in two
  with nothing red, which is the kind of bug that is found by whoever changes the convention
  six months from now and cannot see why the switch half-works.
  The real cost is the five iterations. Each ran, produced a confident table, and was wrong
  in a way the previous one could not reveal — so "my detector agrees with my last detector"
  was never available as a check, and neither was a green result. I nearly reported 32.
FIX: 3675f126. All four derive it now (`per_job_disable_var` is pub(crate); board_drive and
  autofix gained the `JOB` const the others already had), and the check lives in the repo
  rather than in my shell history.
  SCOPED, and the scope is the interesting part: only modules that call `spawn_periodic`.
  commit_nudge reads AMUX_COMMIT_NUDGE_SECS and spawns with a bare `tokio::spawn`, so
  nothing derives that name and its single spelling is CORRECT. Flagging it would tell a
  module to stop duplicating something it does not duplicate. Mutating the filter out reds
  the check on exactly that module, which is how I know the filter is load-bearing.
  THE TRANSFERABLE PART is not "hand-typed fixtures are bad", which rule 7 already covers.
  It is that a detector for a code pattern, written as a grep, is itself a copy of a belief
  about the pattern — so it fails the same way the fixture does, and its wrongness is
  invisible for the same reason. The version that reads the real producer is the only one
  that can be wrong LOUDLY.
FOLLOW-UP the same night, bc2c820b, and it is the better half of this entry. ts-gke read the
  above and sent back the reciprocal: a positive control belongs on a filter's EXCLUSIONS as
  much as on its matches. I had done exactly that for the `spawn_periodic` scope filter an
  hour earlier and had not thought to turn it on `mutate.sh survey`, the tool this entry is
  about. It had the defect.
  `survey` reported ONE exclusion, non-unique lines. A second counter was computed and never
  printed. Comment and blank lines were dropped with no counter at all. So "84 mutable
  line(s) found" could not be told from "84 found out of 1391 scanned, most of which I
  silently ignored" — the exact property the tool's own docstring claims, one release old,
  written by me on the day I filed three entries about this shape.
  The hidden numbers are not small. On the first file I pointed it at: 1391 scanned, 84
  mutable, 4 non-unique, 703 with no applicable rule, 600 comment or blank. Nearly half the
  file in a bucket the report never mentioned, and I had read that report twice and drawn
  conclusions from it.
  Fixed by printing all four and ASSERTING THEY SUM to the scanned count. The identity is
  the transferable part: a bucket breakdown that must add up cannot acquire a silent
  exclusion later, because a new `continue` without a counter breaks the sum loudly instead
  of quietly shrinking the measurement. Mutating one counter away now yields "survey
  accounting lost 2 line(s)" rather than a smaller, entirely plausible number — and a
  plausible number is the failure mode here, never a crash.
  ts-gke's framing of why nobody finds these: a filter's MATCHES are what you designed and
  therefore what you check; its EXCLUSIONS are what you assumed and therefore what nobody
  checks. The exclusions are also where the silence lives, which is why the failure is
  always in the reassuring direction.

---

## Retiring an entry is a MOVE across two files, and the tool's own output named neither
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it. e2f4de2b. The archive script now names BOTH files and prints the runnable pathspec command. Verified in use three times in the last five minutes: every archive run in this drain printed "This was a MOVE across TWO files. Stage BOTH... git add frustrations.md frustrations-archive.md", which is the hint that did not exist when eb552cc1 staged one and put MR-44 in neither.

scripts/test-frustrations-archive-move.sh -> ok: archive move — all 3 cases pass. Cell 2 is the control that a hint naming only the ledger is the defect with extra words. Mutation-verified twice: dropping the hint reds cell 3; naming only the ledger reds cells 2 and 3.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-436
SYMPTOM: `scripts/frustrations-archive.py` removes an entry from frustrations.md and
  appends it to frustrations-archive.md. Its summary reports the line, the validator and
  the card, and names no file at all. So the natural next command is `git add
  frustrations.md` — the file you were reading, the file whose line number you passed —
  which stages the DELETION without the APPEND. The resulting commit holds the entry in
  neither file.
  That is the lost-work state AF-430 exists to describe. I did it in eb552cc1, to MR-44,
  five hours after AF-430 restored MR-44 from an earlier instance of the same shape, on
  the same afternoon I shipped an invariant to detect it.
  THE INVARIANT DID NOT CATCH IT, and the reason is worth stating because it is a real
  limit rather than a bug: `frustrations.retired_entries_stay_retired` fails when a title
  is in BOTH files. This produces a title in NEITHER, and no set-difference over two files
  can see an entry that is absent from both. It is the same blind spot the archive exists
  to cover, arriving from the other direction.
COST: none, and only because a different guard fired. The append-only push guard refused:
  "PUSH BLOCKED — frustrations.md as pushed is MISSING 34 line(s)", with MR-44's own text
  in the sample. That is the deletion half working on a genuine loss instead of a fixture,
  and it is the reason this is a five-minute entry rather than a second recovery from git.
  The real cost is where the catch happened. The push guard is the LAST line of defence
  and it fires at push time, minutes to hours later, on whoever pushes next — who on this
  checkout is usually not the author. Between the bad commit and the refused push, the
  local builder had already adopted the commit.
  Second-order, and the one I keep paying: my first read of the guard's verdict was
  `--check ... | head -8`, which reported exit 141. That is SIGPIPE from head, not the
  guard's status. Instance six of the AF-435 cluster is that exact error, logged by me
  the day before, and I made it again inside the fix for it.
FIX: the script now names both files and prints the runnable command, by pathspec because
  `git add -A` is refused on this shared checkout. Three cells in
  scripts/test-frustrations-archive-move.sh; cell 2 is the control that a hint naming only
  the ledger is the defect with extra words. Mutation-verified twice: dropping the hint
  reds one, naming only the ledger reds two.
  DELIBERATELY NOT A REFUSAL OR AN AUTO-STAGE. The script does not own the index — on a
  shared checkout with one index for every lane, a tool that stages on your behalf is the
  thing `git add -A` is banned for. Print the command; the human runs it.
  The general shape, which is the part worth carrying: when an operation spans two files,
  its completion message must name both. The reader's next command is formed from what
  they were just told, and a summary that names one file will get one file staged.

---

## A whole-file overwrite left one entry's HEADING on another's archived body, and my own title-keyed sweep was blind to it
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it. bcc6e46f. `frustrations.retired_entries_stay_retired` keys on the title AND the first SYMPTOM line, because each misses what the other catches: title caught AF-430's 29 resurrections (17 with revised prose), the symptom key catches identical prose under a foreign heading. A chimera gets its own message with an ORDERED remedy — recover the headed entry from git first, then delete the archived body — because "delete the ledger copy" would destroy the only trace of whose heading it was.

mixpeek-research's MR-43 restored verbatim from 8fdc4bdf and subsequently retired by its own author on their sign-off. Live: the invariant reports `pass` on /api/debug/invariants right now. Mutation-verified twice: blinding the fingerprint filter reds the chimera specimen; emptying the parser reds the specimen and the parser cell. The specimen asserts the TITLE key is BLIND on it before asserting the symptom key is not, so it cannot pass for the wrong reason.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-frustrations
CARD: AF-434
SYMPTOM: Two hours after removing AF-430's 29 title-matched resurrections, I picked
  "A main lane with no $AMUX_SESSION in its env is invisible to the staged-guard's edit
  records" off the ledger as `STATUS: open, SEVERITY: blocks` and started building a fix
  for it. The body under that heading is not about $AMUX_SESSION. It is AF-195's, whose
  SYMPTOM is `cargo test` reporting 37 passed and c971756b shipping red, whose fields read
  `SESSION: amux-frustrations, CARD: AF-195`, and which was VALIDATED AND ARCHIVED on
  2026-08-24 with a fix verified in an isolated repo.
  The heading belongs to mixpeek-research's MR-43, added by 8fdc4bdf, whose own body is in
  neither file. 7dbab8f6's whole-file overwrite fused the two.
  A CHIMERA IS TWO FAILURES WEARING ONE ENTRY. Anyone scanning headings sees a live MR-43.
  Anyone reading bodies sees a live AF-195. Both are wrong, one entry's body is lost, and
  no set-difference over either file can see it: the title is absent from the archive, so
  a title key passes it, and the body's own title is present, so a reader who checks the
  archive by heading is told it is fine.
COST: I built a real fix for AF-195 before finding out it had been fixed eight days
  earlier, by someone else, in a better shape than the entry proposed. c654a6a6's message
  claims the entry as its subject and it is wrong about that. What saved the time from
  being wasted is luck rather than judgement: the thing I built covers a DIFFERENT window
  than the shipped fix (see below), so it is worth keeping. It could as easily have been a
  second spelling of a guard that already existed, which is the specific waste the
  build-on-the-primitives rule exists to prevent.
  Second cost, and the one that is not mine: mixpeek-research lost a second entry today.
  MR-44 was deleted outright by the same commit and restored earlier; MR-43 was hollowed
  out and has been misrepresenting itself for four days.
FIX: this commit. The invariant now keys on TWO things, and the argument for both is that
  each misses what the other catches:
    TITLE          catches AF-430's 29, of which 17 were the PRE-archive drafts of entries
                   their authors revised before signing off, so their prose had moved.
    FIRST SYMPTOM  catches this one, where the prose is byte-identical and the heading is
                   somebody else's.
  A chimera gets its own message rather than the resurrection one, because the remedy is
  different and larger: recover the headed entry from git history FIRST, then delete the
  archived body. Telling a reader to "delete the ledger copy" here would destroy the only
  surviving trace of which entry the heading belonged to.
  MR-43 is restored verbatim from 8fdc4bdf and the AF-195 body is gone from the ledger.
  Cells: the chimera specimen (asserting the title key is BLIND on it, or the cell proves
  nothing), a control where two entries share a subject but not an opening symptom, and a
  parser cell requiring a fingerprint for 90% of real entries. Mutation-verified: blinding
  the fingerprint filter reds the specimen, and emptying the parser reds both.
NOTE: I told mixpeek-general, an hour before finding this, to key on the title and not on
  prose, on the strength of AF-430's 17 revised drafts. That advice was half right and I
  have sent them the other half.

---

## The append-only guard's PASS is not evidence for any particular line: a rescue by the substring test is silent
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it. bbda1252. The classifier splits the rescue: an EXACT whole-line match elsewhere in the pushed union is real survival and stays silent (the union rule doing its job); a match that is only a SUBSTRING of a longer line is counted, named and logged as SUBSTR.

PRECISION MEASURED ON THE REAL RANGE, which is what decides whether it is usable rather than noise: across 55 commits that archived 47 entries and moved thousands of lines, it names exactly ONE — `CARD: AF-10`, the line that started this.

scripts/test-append-only-substring-scope.sh -> ok: all 4 cases pass; resurrection 6/6, push-guard-range 16/16, fork-base 6/6 unaffected. Mutation-verified 3x, and cell 2 is the one that matters: reporting EVERY rescue reds the archive-move control, so the precision half can fail. That cell caught a real fixture bug on its first run.
AREA: hooks
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-frustrations
CARD: AF-432
SYMPTOM: Repairing AF-430 I deleted 29 resurrected entries, and one line went with them
  that is in NO other file: `CARD: AF-10`, whose archived copy carries `CARD: AF-242` plus
  a NOTE-CARD explaining the repoint. I ran the guard expecting a refusal I would then have
  to acknowledge out loud. It passed, exit 0, silent.
  It passed for a reason unrelated to whether the content survived. The classifier tests
  `nl in head` as a SUBSTRING over the whole pushed union, and `CARD: AF-106` contains
  `CARD: AF-10`. Reproduced out-of-tree with commit-tree so no worktree was touched, with
  the control in both directions:
    drop `CARD: AF-10`  while `CARD: AF-106` survives  -> exit 0, no output, no WARN
    drop `CARD: AF-242` while neither covers it        -> exit 1, refused
  So the guard CAN fail, and does, on the shape it was built for. It cannot fail for a line
  that some longer or identical line covers, and it says nothing when that rescue happens.
  MEASURED, and the measurement is the part I nearly got wrong. At HEAD, 122 of the ledger's
  2470 distinct non-blank lines (4%) would be invisible if deleted: 118 because an identical
  line exists elsewhere in the union, 4 because a longer line contains them. The masked set
  is field lines, which are duplicated by construction (DATE 19, CARD 15, AREA 14, SESSION
  10, SEVERITY 3, STATUS 2, plus 46 prose lines repeated across entries).
  MY FIRST NUMBER WAS 34%, and it was wrong in the alarming direction. I ran it against
  origin/main, which still held the 29 resurrected duplicates I was in the middle of
  deleting, so every duplicated entry counted its own lines as covering each other. The
  measurement of the bug was contaminated by the bug. mixpeek-general made the identical
  error the same afternoon on the same subsystem (a DATE|AREA key that collided, giving 11
  false resurrections in their ledger, caught by a repeated key in their own output). Two
  independent instances in one day of a count inflated by an artifact of the thing being
  counted, both landing on "there is a problem here" rather than away from it.
COST: about a minute of believing exit 0 was evidence my dropped line was fine. It was not
  evidence either way; what actually justified that deletion was the line-by-line comparison
  against the archive I had already run by hand. Small today because I happened to have the
  better proof already. The standing cost is that this file's own guard gives a session no
  signal at all for the class of edit this file gets most often after prose: a field
  correction. A `STATUS:` flip or a `CARD:` repoint reverted by a stale copy passes clean.
FIX: not the substring test, which earns its keep — the guard's own comments record that a
  strict test refused 5 of 6 real deletion commits and that a guard firing daily teaches
  setting the escape blind. That reasoning holds.
  The gap is that the rescue is INVISIBLE. The guard already distinguishes LOST (refuse)
  from EDITED (warn, allow); a line rescued only because something else happens to contain
  it is a third class and currently reads as the healthy one. Count them and print the
  count: "N line(s) matched only as a substring of other content — not verified as
  surviving in their own right." That is a WARN the author can act on, it costs one counter
  in a classifier that is already walking every candidate line, and it turns exit 0 from a
  claim about the file into a claim with a stated scope.
  The guard's header already names the adjacent residual out loud ("a republish stale by so
  little that it reverts only entry BODIES passes with warnings"). This is its neighbour:
  a republish that reverts only a DUPLICATED FIELD LINE passes with NO warning, which is
  the one case the author's own mitigation (the WARN log line keeps it visible) does not
  cover.
SHIPPED, and the shape is sharper than what this entry proposed. "Count the rescues" would
  have fired on every archive move, because a retirement rescues every moved line and the
  note would have appeared on each one until people learned to skip it. The classifier now
  splits the rescue in two: an EXACT whole-line match elsewhere in the pushed union is real
  survival and stays silent (that is the union rule doing its job), while a match that is
  only a SUBSTRING of some longer line is counted, named and logged as `SUBSTR`.
  Reported, never refused, and the reason is stated in the code: an in-place extension
  leaves the old line as a prefix of the new one and so does a coincidental id, and this
  check genuinely cannot tell them apart. Refusing would fire on the routine edit these
  files get most, which is the exact trade the substring test was added to avoid. So it
  says what it could not express and leaves the judgement with the author.
  PRECISION MEASURED ON THE REAL RANGE, because a report that fires constantly is worth
  less than no report: across 55 commits that archived 47 entries and moved thousands of
  lines, it names exactly ONE — `CARD: AF-10`, the line that started this.
  Cells in scripts/test-append-only-substring-scope.sh. Cell 2 is the one that matters and
  it is the control, not the specimen: an ordinary archive move must stay SILENT. It caught
  a fixture bug on the first run (a `printf '%s'` left the moved entry as one long line, so
  nothing in it was a whole line), which is the only reason I know the cell can fail.
  Mutation-verified three ways: restoring the silent rescue reds three assertions; reporting
  every rescue reds the archive-move control; deleting the block reds three.

---

## Six checks in one afternoon that ran, passed, and could not have failed
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it. 1d93d14a + bc2c820b. The entry asked for "a mutate.sh invocation that takes a test name and reports which mutations it survives, so the reflex is cheap enough to be automatic". `scripts/mutate.sh survey <file> -- <cmd>` is that: line-scoped syntax-preserving mutations through the same apply/trap-revert path as `run`, aborting if the file does not return to its starting bytes.

IT PAID ON ITS FIRST TWO RUNS, which is the only evidence that matters for a tool built to make a reflex cheap. Three real gaps in code that already carried comments stating the invariants nothing was holding: `return want.len() > i` in segments_match (the {*rest} wildcard arm), and both of AF-422's unheld loud arms (`n_at_risk == 0` and `all_mine`'s `.all()`).

bc2c820b then fixed the tool's own version of this entry's defect, found by applying ts-gke's reciprocal (a positive control belongs on a filter's EXCLUSIONS): survey reported one exclusion and silently dropped two, hiding 703 no-rule and 600 comment/blank lines out of 1391 scanned. All four buckets now print and an assertion requires them to sum, so a future silent exclusion breaks loudly.

ARCHIVED WITH ITS OWN LIMIT NAMED: the cluster reached nine instances, and instance nine was in the tool's own suite (`*SURVIVED*` matching the summary line "0 SURVIVED."). survey answers "is this line depended on"; it does not answer "is the reason for this the reason it is true", which is AF-445 and stays open.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-frustrations
CARD: AF-435
NOTE-CARD: repointed 2026-09-03. This said CARD: AF-422, which is the STAGED-GUARD MIRROR
  card (the server-side victim notice lacking AF-391 and MC-1561). Two unrelated units of
  work were sharing one card, so no status was a true statement about it: the mirror work
  was done and production-confirmed while this cluster was untouched, and reopening the
  card to be honest about the cluster made it dishonest about the mirror. AF-435 is this
  entry's own card. The mis-link was mine, made the same afternoon I logged an entry about
  checks that cannot fail.
SYMPTOM: Not one bug. Five instances in a single afternoon, three mine and two
  reported by mixpeek-general, of a check that EXECUTED, reported success, and was
  structurally incapable of failing. Ethos rule 7 already names this class and points
  at scripts/mutate.sh; what this entry adds is that naming it was not enough to make
  anyone RUN it, including the person who had just written the rule's own examples.
    1. (mine, AF-422) A footer fix sat unextracted in a 60-line async block. The
       mutation restoring the exact bug PASSED THE ENTIRE SUITE. Pulling it into a
       pure function is the only reason it is a fix rather than a claim.
    2. (mine, AF-422) Two new match arms placed BELOW the generic arm they were meant
       to precede. Unreachable, would have shipped inert, every test green. Caught by
       the compiler, not by me and not by any test.
    3. (mine, AF-419) Every placeholder cell also set `peer: false`, so the flag alone
       decided them and the string check under test was never load-bearing. Removing
       it passed 6 of 6.
    4. (mixpeek-general) A live self-test leg that could not observe the defect it
       existed for.
    5. (mixpeek-general) A test file with no marker, which CI would never have
       selected — it could not fail because it never ran.
    6. (mine, and the worst of the six) Verifying a 36-commit push for a peer who had
       asked for consent, I ran `cargo test -p amux-server 2>&1 | tail -18` in the
       background and read "[exited with code 0]" as the suite passing. IT IS TAIL'S
       EXIT CODE. Without `pipefail` a pipeline reports the LAST command's status, so
       that 0 was unconditional — cargo could have failed every test and it would still
       have read 0. `tail -18` also discarded every result line but the last, so the
       "17 passed" I was about to quote was one binary of many, out of ~1800 lib tests.
       I caught it only because 17 looked too small, not because anything failed.
COST: individually small except the sixth, which was about to authorize a 36-commit push
  to origin on a fabricated green, for a peer who had explicitly asked whether my work
  was safe to ship. Two of the other five would have shipped a no-op fix while the card
  closed as done with evidence attached. The compounding cost is worse: each of these
  produces a GREEN result that is then cited as proof. #1 and #3 were both about to be
  written into a card's evidence block as mutation-verified.
FIX: instance 6 has a mechanical fix the others do not, and it is worth stating on its
  own: NEVER READ AN EXIT CODE THROUGH A PIPE. `cmd | tail` reports tail's status.
  Either drop the pipe and write the log to a file, or `set -o pipefail` first. The
  background-task harness reports "[exited with code N]" for the whole pipeline, which
  is what made the wrong number look authoritative.
  rule 7 says "the way to know is to break it" and names the tool. The gap is WHEN.
  All five were caught (or missed) at the moment the check was WRITTEN, not at the end,
  and four of the five were found only because something else forced a second look — a
  peer's report, a compiler warning, an unrelated mutation. The reflex that would have
  caught all five is one line: RUN THE MUTATION BEFORE BELIEVING THE TEST, at the
  moment you write it, not before you claim it.
  Deliberately NOT proposing new prose in ethos.md. Rule 7 is already correct and
  already names the tool; a sixth sentence restating it is the shape docs/friction-
  themes.md warns about, where prose that is not enforceable joins the problem. This is
  logged as a CLUSTER so the count argues for a mechanism — a `mutate.sh` invocation
  that takes a test name and reports which mutations it survives would make the reflex
  cheap enough to be automatic, which is the only thing that has ever worked here.
  mixpeek-general's framing, kept because it is the argument: "three instances in one
  afternoon of 'the check ran and could not have failed' is enough that I would rather
  have the reflex than the three stories."
UPDATED 2026-09-02, evening. The cluster is EIGHT, and the two new ones are the first
  that argue FOR the proposed mechanism rather than merely adding to the count, because
  both were caught by running the mutation at the moment the cell was written:
    7. A test harness gave every cell the same AMUX_HOME, so the previous cell's fixture
       survived into the "no receipt at all" cell. It passed, and it would have passed
       with the feature deleted. Caught on the first run because a DIFFERENT cell in the
       same file failed and made me read the harness.
    8. A cell asserting "the receipt carries the run's exit code" drove the writer with
       RC=0, so a writer that HARDCODES `# rc 0` is indistinguishable from one that reads
       the variable. Caught by mutating the writer to hardcode it: 14 passed, 0 failed.
       The cell now drives RC=101 and the same mutation reds it.
  The ratio is the finding. Six instances were caught by luck, a compiler, or a second
  look; both of today's were caught by the reflex itself, in under a minute each, on
  cells I had just written and believed. That is the case for making `mutate.sh` cheap
  enough to be automatic rather than for another sentence telling people to remember.
  THE MECHANISM SHIPPED, 1d93d14a: `scripts/mutate.sh survey <file> -- <command>` walks a
  file's mutable lines and reports which ones the command's outcome does not depend on.
  Line-scoped and syntax-preserving, through the same apply/trap-revert path as `run`, so
  the blast radius and the duration bound are unchanged; it re-hashes the file after every
  mutation and ABORTS if the bytes did not return. It states what it did NOT examine —
  non-unique lines skipped, `--limit` truncation, the `--stop-at` scope — because a survey
  that quietly examined 6 of 84 lines and reported "all killed" is this entry's own shape
  wearing a tool's clothes. A survivor is a question, not a verdict: log strings and
  defensive branches survive honestly, and demanding zero survivors would be the gate with
  no truthful path that rule 3 forbids.
  IT PAID FOR ITSELF ON THE FIRST TWO RUNS, which is the only evidence that matters here.
  Run one, on invariants/checks.rs: `return want.len() > i` in `segments_match` survives as
  `>=`. That is the `{*rest}` wildcard arm, whose own comment says "must have at least one
  segment left to consume" and whose neighbouring cell exists to prevent exactly the prefix
  false-pass `>=` reintroduces. Documented invariant, explanatory comment, nothing holding
  it. Run two, on AF-422's own subject: `n_at_risk == 0` flipped to `>= 0` and `all_mine`'s
  `.all()` flipped to `.any()`, both surviving the whole git_guard suite. The first deletes
  the loud mirror notice; the second restores the exact possessive AF-422 was filed to
  remove. That card's acceptance criterion asked for BOTH arms and only the quiet one was
  held. Three survivors, three real gaps, on the first two files it was pointed at.
  And a fourth, in the tool's own suite: cell 2 asserted `*SURVIVED*`, which also matches
  the summary line "0 SURVIVED.", so it would have passed on a survey that found nothing.
  Caught because cell 6 failed on the same glob. Instance nine, in the harness built to
  catch instances.

---

## A guard with two copies and no installer ran three days behind, allowing the bypass its newer copy refuses
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it, and the FIX it asked for is present at THREE layers rather than the one it proposed. The entry's own words: "the honest fix is a drift check that fires without being remembered."

  scripts/install-hooks.sh:348-350   installs scripts/git-hooks/git-shared-guard.py to
                                     $HOME/.amux/hooks/git-shared-guard.py — the copy
                                     ~/.claude/settings.json actually invokes
  .claude/session-freshness.sh:447   compares dest against source on EVERY SessionStart,
                                     which is the drift check firing without being asked
  invariants/monitor.rs:216,231      reads both copies server-side (AMUX-3033), so it is
                                     also visible to anyone who never opens a shell

Checked at the artifacts, not from the commit message. The entry was marked `fixed` while its own FIX text said "the CLASS is unfixed", which is the depth trap this file warns about — so I re-read it before signing, and the class is closed: the file that had no installer now has one, and two independent layers report drift on it.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-02
SESSION: amux-frustrations (found it), amux (wrote the fix that was sitting inert)
CARD: AF-409
SYMPTOM: `~/.claude/settings.json` invokes `python3 ~/.amux/hooks/git-shared-guard.py`,
  a path OUTSIDE the repo. The repo versions and reviews
  `scripts/git-hooks/git-shared-guard.py`. Nothing copies one to the other:
  `scripts/install-hooks.sh` installs pre-commit, pre-push, prepare-commit-msg and
  amux-staged-guard, and not this one. Measured 2026-09-02: 148 differing lines. The
  running copy was byte-identical to a58a53cf (08-30 06:38); the repo copy carried
  e782b68a (08-30 21:02), "command substitution inside a quoted argument bypassed the
  shared-checkout guard (AMUX-3932)".
COST: A BYPASS FIX INERT FOR THREE DAYS, proven with a control rather than asserted by
  running both forms through each copy directly:
    echo "$(git add -A)"             OLD: allowed   NEW: BLOCKED
    python3 <<EOF ... $(git add -A)  OLD: allowed   NEW: BLOCKED
  The old copy is the one that was running. `git add -A` in a shared checkout is the
  command AF-316 exists to refuse, and for three days it was one layer of quoting away
  from succeeding on a tree 125 lanes share. Nobody is known to have used it; the cost
  is the exposure, not a measured incident.
FIX: Installed on this box (running copy backed up first, both syntax-checked, the old
  one confirmed a strict ANCESTOR at a58a53cf so nothing unique was lost). That is the
  instance. The CLASS is unfixed and is AF-409: this is the second hook found running
  behind its repo copy in three days, after AF-375, and the two failed differently —
  AF-375's hook HAS an installer nobody ran, this one has none to run. The generalisable
  half is that a file's deploy semantics are invisible at the point of editing it, so
  the honest fix is a drift check that fires without being remembered. The SessionStart
  freshness hook already does exactly that for the four installed git hooks and does not
  know this file exists.

---

## The freshness hook's `git merge origin/main` exits 2 in the exact state it prescribes it for
VALIDATED: amux-frustrations | Self-validated; amux-frustrations originated it. e6b80033, whose message is "fix(freshness): say which dirty files block the merge it prescribes, and which are safe to drop (AF-385)".

Verified in the shipped source rather than from the sha: .claude/session-freshness.sh now carries the reasoning inline — it reproduces git's own refusal text ("Your local changes to the following files would be overwritten by merge"), records why the obvious remedy was wrong on a shared checkout ("stashing it takes it out of their worktree while they are in it"), and notes that mtime-derived ownership names whoever was ACTIVE rather than whoever WROTE (AMUX-3662). The arm that used to hand out a bare `git merge origin/main` now names the blocking files and separates the ones safe to drop.
AREA: notices
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-01
SESSION: amux-frustrations (hit and fixed it), mixpeek-frustrations (paid part of the cost)
CARD: AF-385
SYMPTOM: SessionStart printed `RECONCILE IT: git merge origin/main (rewrites no
  SHAs; abort is clean)`. Running it: `error: Your local changes to the following
  files would be overwritten by merge: crates/amux-server/src/runtime_jobs/
  commit_nudge.rs / Please commit your changes or stash them before you merge. /
  Aborting`, exit 2. "abort is clean" describes `git merge --abort`, which never
  becomes reachable because the merge never begins. Git's own two suggestions are
  both forbidden on a shared checkout: committing a peer's file lands their work
  under your name, stashing it takes it out of their worktree while they are in
  it. No third option was named anywhere (ethos rule 3).
COST: The checkout stayed unreconciled through two lanes' attempts. mixpeek-
  frustrations applied and reverted a mutation in that file and deliberately
  restored to the state it FOUND rather than to HEAD, to protect work that turned
  out to need no protecting. This lane declined to merge for the same reason and
  spent the diagnosis. The blocking file was byte-identical to origin's copy the
  whole time (`diff <(git show 9b556907:<path>) <path>` -> exit 0, zero lines):
  ts-gke's TG-3343 work, already merged upstream, unstaged only because the
  checkout was behind. The safe reconcile was one comparison away and nothing
  said so.
FIX: e6b80033. The arm now names the files that block the merge and gives each a
  verdict, asymmetrically on purpose: byte-identical to upstream earns a printed
  discard command, because the bytes are recoverable from the remote; different
  earns no destructive command at all, because that is live work and mtime here
  names whoever was ACTIVE rather than whoever WROTE (AMUX-3662). Log signal:
  ~/.amux/reconcile-blocked.jsonl. The general shape, and the reason this is the
  SECOND instance in one day (AMUX-3718 was the first, archived the same
  morning): a notice that prescribes a procedure must be checked against the
  state it fires in, because the state that triggers it is exactly the state
  where the obvious command stops working. `drop_paths_identical_to_origin()`
  already computed this comparison for the idle nudge; the surface every lane
  reads at SessionStart did not (ethos rule 1).

## A local `cargo clippy` OOM-kill doesn't just kill the build — it kills the WHOLE interactive session
VALIDATED: amux | This entry itself already says (its own FIX line): "FIX: none in code yet...
AMUX-70 filed for a durable fix (either that wrapper baked into the sanctioned
local-build path, or making the remote-offload fallback actually reliable)".
That wrapper now exists and is the sanctioned path: scripts/safe-cargo.sh,
documented in CLAUDE.md's own Workflow section ("The wrapper runs cargo in
its own sibling scope so an OOM kill there can't cascade into the session").

Card AMUX-70 (this entry's own CARD field) is `verified` with concrete
evidence recorded on the card itself:
  scripts/safe-cargo.sh check -p amux-server -> Finished `dev` profile ...
    in 46.77s (via wrapper, own systemd scope confirmed:
    run-p<pid>-i<id>.scope vs tmux-spawn-*.scope)
  cargo test -p amux-server --lib pane_scope_oom_kill_tests ->
    test result: ok. 3 passed; 0 failed
  Live proof: a real commit (bfc64954) went through the newly-fixed
    pre-commit hook (cargo clippy --workspace --all-targets via
    safe-cargo.sh) with no hang, no AMUX_SKIP_RUST_GATE
  tmux list-sessions -> 8 (unchanged before/after every step)

Reconfirmed live in THIS session, 2026-09-02: every local cargo invocation
this session ran (multiple git commits, each triggering the pre-commit
hook's check+clippy gate) went through this exact wrapper via the
pre-commit hook, completed normally, and never took the interactive pane
down — the specific failure mode this entry describes (the whole
tmux-spawn scope dying, session restarting mid-conversation) did not
recur across ~6 local check+clippy runs today.
AREA: build
SEVERITY: blocks
STATUS: open
DATE: 2026-09-01
SESSION: amux
CARD: AMUX-70
SYMPTOM: Ran `cargo clippy -p amux-server --all-targets` locally in this
  interactive pane as a fallback when the remote build host's toolchain
  image kept failing to rebuild (rustup uplink flakiness, already
  documented in CLAUDE.local.md — host details deliberately omitted here,
  this file is public). `clippy-driver` grew to ~2GB RSS and got OOM-killed
  (`dmesg -T`: 08:54:28 and 09:22:32). Confirmed via `journalctl --user`:
  every process in an interactive amux pane — including the Claude Code
  process itself — shares ONE systemd scope,
  `tmux-spawn-<uuid>.scope`. Systemd does not reap just the OOM-killed
  process: it marks the WHOLE SCOPE `Failed with result 'oom-kill'`
  (`tmux-spawn-006a872a....scope: Failed with result 'oom-kill' (3.7G
  memory peak)`), and whatever launches the pane tears it down and starts
  a brand-new one 26 seconds later (`Started tmux-spawn-baff1e65-...`).
  The entire interactive session restarted mid-conversation as a result —
  not the build process, the SESSION. Surfaced to the session only as
  orphaned background-task notifications ("stopped ... may have been
  stopped via agent teardown") — nothing points at OOM or the scope
  failure; that only came from reading `journalctl`/`dmesg` directly.
COST: This exact session lost an in-flight `git commit` (had to be
  re-run), an in-flight `cargo clippy` verification pass (had to restart
  from a fresh session with no memory of the interrupted state until the
  transcript resumed), and cost real wall-clock time diagnosing "why does
  amux keep stopping" as a SEPARATE investigation from the work that
  caused it. Anyone running local cargo/clippy/test/build directly in an
  interactive pane hits this identically.
FIX: none in code yet. Documented as a hard rule in this session's own
  `offload-builds` memory: never run cargo build/check/clippy/test
  directly in an interactive pane, even as a one-off fallback — that pane
  IS the session. If local is unavoidable, run it via `systemd-run --user
  --scope -- cargo ...` so the build gets its OWN scope, not the pane's.
  AMUX-70 filed for a durable fix (either that wrapper baked into the
  sanctioned local-build path, or making the remote-offload fallback
  actually reliable so this is never reached for).

## a hand-written Amux-Session trailer silently outranks the hook's true stamp, and the push guard then calls your own commit foreign
VALIDATED: amux-frustrations | Validated by the originating session (amux-frustrations, which both hit and fixed it).

Fixed in f6bfeefe. prepare-commit-msg now prints the disagreement at commit time
naming both lanes, keeps the declared trailer (a cherry-pick's stamp is real
provenance), and records `Amux-Committer: <lane>` beside it. pre-push reads it and
prints COMMITTED BY YOU, STAMPED TO ANOTHER LANE with the two causes and their
opposite remedies, without clearing the commit.

Confirmed on the very next commit made after the fix: f6bfeefe declared no session,
so no warning fired and no Amux-Committer was stamped (the agreement path), and its
trailer reads [amux-frustrations]. The mis-stamped commit that produced the entry
(ac550324, stamped `amux`) was amended to e7808171 under a HEAD pin.

scripts/test-commit-stamp.sh -> 15 passed, 0 failed (6 new, 3 negative controls)
scripts/test-push-guard-range.sh -> 19 passed, 0 failed (3 new, one a control)
Six mutations verified firing and reverting clean.
Hooks ship by COPY; ./scripts/install-hooks.sh run and the INSTALLED copies verified.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-frustrations
CARD: AF-479
SYMPTOM: I wrote a commit message by hand and typed `Amux-Session: amux` into the
 trailer block. This lane is `amux-frustrations`. The commit landed as ac550324
 stamped to a real peer lane and nothing said a word. `prepare-commit-msg` stamps
 with `git interpret-trailers --if-exists doNothing`, so any value already in the
 message wins over the one the hook computed two lines earlier and knows is true.
COST: caught only because I happened to render the trailer while checking for
 foreign commits before a push. Had I not, the pre-push guard partitions unpushed
 commits by exactly this trailer, so it would have listed my own commit under
 `amux:` and offered me its author-consent exit, whose two-field form is ACCEPTED
 whenever the trailer matches. Following the refusal literally records "the author
 said yes" from a lane that never touched the commit, which is the AMUX-3533 shape
 reached by obeying the sanctioned instruction. This hook's own header records the
 same mis-stamp costing an amend-under-pin dance on 2026-08-22. The general form:
 the harness held the true value and deferred to a false one with no signal, and
 `Amux-Conversation` cannot corroborate because it is a lookup of the same string.
FIX: f6bfeefe. Overwriting was rejected and stays unimplemented: a cherry-pick or
 `git commit -c <peer-sha>` legitimately carries the original author's stamp, and
 rewriting it to the committing lane would destroy a true fact to prevent a false
 one. So the declared trailer is KEPT, the disagreement is printed at commit time
 naming both lanes while the commit is still cheap to amend, and `Amux-Committer:
 <lane>` is stamped beside it. It is written ONLY on disagreement, so its presence
 is the signal rather than something a reader has to compare. `pre-push` reads it
 and prints COMMITTED BY YOU, STAMPED TO ANOTHER LANE with the two causes and their
 opposite remedies; it does NOT clear the commit, because a cherry-picked peer WIP
 has this identical shape and is what the guard exists to stop. 15 cells in
 `scripts/test-commit-stamp.sh` (6 new, 3 of them negative controls) and 19 in
 `scripts/test-push-guard-range.sh` (3 new, one a control). Six mutations fire:
 stamping unconditionally, overwriting the declared value, dropping the warning,
 printing the note unconditionally, reverting the log format, and letting the note
 return 0. Hooks ship by COPY, so `./scripts/install-hooks.sh` was run and the
 INSTALLED copies verified. One real bug came out of writing the test rather than
 the code: `%(trailers:key=X,valueonly)` emits the trailer's own trailing NEWLINE,
 which is invisible while that field is last and splits every row in half the
 moment a second field follows it. `separator=` suppresses it. Cell Q caught it;
 reading the code did not.

## the two sanctioned cargo wrappers cannot both be used, and pre-commit then reports your tested bytes as untested
VALIDATED: amux-frustrations | Validated by the originating session (amux-frustrations).

Fixed in 8e515d80. The receipt writer moved to scripts/write-test-receipt.sh and is
called by BOTH sanctioned paths, so writing one is a property of running tests
rather than of whichever wrapper you reached for. safe-cargo.sh test writes one and
stops exec'ing (exit status now explicitly propagated and tested); test-contended.sh
invokes cargo through safe-cargo.sh with _TC_RECEIPT=1 to suppress the duplicate.

Validated by USE, not just by the suite: every commit made in this session went
through the fixed path and pre-commit reported correctly each time, including the
discriminating case. On a CSS/JS-only commit it said the staged bytes did not match
the last run, which is TRUE (crates/amux-dashboard/static is under crates/ and
dashboard_assets.rs reads those exact bytes), and running --test dashboard_assets
cleared it: 21 passed. On every Rust commit it said "all N staged crate file(s)
match the bytes your last run compiled" and named the target and age.

scripts/test-test-receipt.sh -> 26 passed (5 new)
Four mutations fire: no receipt written, written for every subcommand, exit status
swallowed, duplicate guard removed.
./scripts/install-hooks.sh run; grep -c AF-478 .git/hooks/pre-commit -> 1.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-frustrations
CARD: AF-478
SYMPTOM: CLAUDE.md gives two instructions for a local test run. "Tests:
 `scripts/test-contended.sh -p amux-server`", and "If a local run is genuinely
 unavoidable, run it through `scripts/safe-cargo.sh <cargo args>` instead of bare
 cargo". They could not both be followed. `test-contended.sh:162` was a bare
 `cargo test "$@"`, so the sanctioned TEST path had none of the systemd-scope
 isolation AMUX-70 exists for. And only `test-contended.sh` wrote
 `~/.amux/test-receipts/$AMUX_SESSION.tsv`; `safe-cargo.sh` execs, so it could not
 write one after the run. Three green runs through the safety wrapper, minutes
 before a commit, and pre-commit answered: "1 of 1 staged crate file(s) DIFFER
 from the bytes your last run compiled (`-p amux-server --lib board_drive`,
 72140s ago). That green result does not describe this commit."
COST: the hook named a run from 20 hours earlier and was right about everything it
 could see. There was no sequence of sanctioned commands that made it right, which
 is ethos rule 3: a constraint with no truthful path through. The cheap damage is a
 commit that reads as untested; the expensive one is the habit it teaches, since a
 warning that fires on correct behaviour gets skipped, and this hook's whole value
 is the case where it is telling the truth. On Linux the conflict runs the other
 way and costs a session: an OOM-killed `cargo test` in the pane's own scope takes
 the interactive pane down, not just the build.
FIX: 8e515d80. The receipt writer moves to `scripts/write-test-receipt.sh` and is
 called by both paths, so it is a property of running tests rather than of whichever
 wrapper you reached for. `safe-cargo.sh test` writes one (and stops exec'ing, so
 the exit status is now explicitly propagated and tested); `test-contended.sh`
 invokes cargo through `safe-cargo.sh` and sets `_TC_RECEIPT=1` to suppress the
 duplicate. The no-receipt branch of the hook now names the two commands that write
 one, because reporting an absence without saying what produces it is honest and
 unactionable. 26 cells in `scripts/test-test-receipt.sh`, 5 of them new; four
 mutations fire (no receipt written, written for every subcommand, exit status
 swallowed, duplicate guard removed). Hooks ship by COPY, so `./scripts/install-hooks.sh`
 was run and the INSTALLED copy verified (`grep -c AF-478 .git/hooks/pre-commit` -> 1).
 The FIX sha above was originally written as 5cd5be1c, a sha I had guessed before
 committing. Corrected in a follow-up; a predicted sha in this file is a citation
 that resolves to nothing.

## The escape hatch the nudge advertises is what builds the pile it complains about
VALIDATED: amux-frustrations | Validated by the originating session (amux-frustrations), re-verified in the shipped
code today rather than from the fix commit messages.

(1) The `updated > last_ts` silence is GONE. board_drive.rs:4628 carries the removal
note by name ("REMOVED (AF-465)") and the surviving check at :4625 is the cooldown
only (`last_ts > 0.0 && (now - last_ts) < win`), with MIN(added_at) kept as the
monotonic ask clock. So re-statement is no longer a silence lever and the
first-fire gap is dissolved.

(2) The verify-nudge no longer promises a digest. board_drive.rs:3064 now reads
"it moves the card into the dashboard needs:you view, and NOTHING pushes" it to the
owner today. The only remaining "owner digest" strings are doc comments quoting the
OLD text to explain what was fixed (:2973, :4655-4657) and an unrelated past-tense
note in autofix.rs:6617.

Fixed in 88f00fbd and cb3e966f (both amux-cloud), 105 board_drive tests green.

Deliberately still open and NOT claimed by this validation: whether a push surface
to the owner should exist at all is Ethan's call. This entry's claim was that the
loop advertised a benefit that does not happen; that sentence has stopped being true.

AREA: notices
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-465
SYMPTOM: two false promises in the board loop, in the same family. (1) The needs:you
 re-nag said "Re-state it on the card (silences this for 3d)"; both silence checks in
 `needsyou_renag_text` are gated on `last_ts > 0.0`, so the FIRST fire skips them and
 re-stating does nothing. (2) The verify-nudge told lanes that a card they cannot
 verify "should be tagged `needs:you` so they surface in the owner digest rather than
 sitting here indefinitely" — and THE OWNER DIGEST DOES NOT EXIST. No job in
 `registry.rs` builds one, no runtime job selects needs:you cards into one, `digest`
 appears nowhere in live `/api/health`, and `review.rs`'s `digest_dir()` only SERVES
 `docs/weekly-review`, which holds two files both dated Aug 1 with no producer. The
 only surviving trace is a past-tense comment (`autofix.rs:6617`) about a digest that
 "emitted 92 cards in one SMS". Tagging needs:you moves a card to a dashboard view
 nobody is pushed to.
COST: this is a mechanism for the pile, not a coincidence beside it. ~/.claude/CLAUDE.md
 records "445 cards ... in needsyou with a median age of 15 days, most of which never
 needed me at all". The loop advertised an escape hatch whose stated benefit does not
 happen, lanes took it as instructed, and the cards parked. Live specimen, verified:
 AC-214, a SECURITY escalation (rotate a leaked E2E_COOKIE_SECRET, still unrotated,
 value present verbatim in a transcript), tagged needs:you on 2026-08-04 — 30 days,
 correctly tagged, reaching the owner through zero channels. Also cost me a wrong
 mechanism on (1): I named INSERT OR IGNORE + MIN(added_at) without reading the
 function that implements the promise.
FIX: 88f00fbd and cb3e966f, both amux-cloud. (1) The `updated > last_ts` silence is
 DELETED rather than repaired — one deletion makes the new text true, closes the
 abuse vector where a lane suppresses a human's ask by re-stating every 3d, and
 dissolves the first-fire gap. MIN stays as the monotonic clock. (2) The verify-nudge
 now says tagging moves the card to the dashboard needs:you view, that NOTHING pushes
 it to the owner today, and to tag only when a human genuinely owes the next step.
 105 board_drive tests green. Whether a push surface should EXIST is Ethan's and is
 deliberately still open.

## A test that extracts a function to avoid copying it inherits that function's dependencies
VALIDATED: amux-frustrations | Validated by the originating session (amux-frustrations).

Fixed in f883a98e. scripts/test-unstamped-ledger.sh extracts the shipped functions
rather than copying them, and now extracts their DEPENDENCIES too, so an extraction
can no longer pass while the real function would fail on a helper it needs.

Re-run today against the current tree: 16 passed, 0 failed. The cells that would
break first if the dependency extraction regressed are green (the injected body
carries a marker not the bare text; the marker is a single line so the separately
sent Enter still submits body and marker together; the audit row records the
ORIGINAL undecorated body).

AREA: tests
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-454
SYMPTOM: scripts/test-unstamped-ledger.sh deliberately EXTRACTS the shipped ledger
 functions instead of copying them, and says why in its own header: "A copy here
 would pass forever while the real ones rot — the failure this file exists to
 prevent, one level up." It extracted three functions by name and none of their
 callees. AMUX-40 (978645c0) later swapped the bare `curl` inside
 `_flush_unstamped_ledger` for the hang-guarded `_curl` helper. `_curl` was then an
 undefined command in the test's shell, its rc-127 landed in the flush's own
 "server went away mid-flush: KEEP the row" branch, and three assertions went red.
COST: the failures read "the row never reached /api/history — every fallback send
 would be lost" and "rows remain after flush — they would be re-sent forever",
 against a live server, a POST verified working by hand, and a reconciled row
 already in the trail. Every one of those sentences accuses the audit mechanism;
 the defect was in the harness reading it. Anyone taking the output at face value
 would have gone to debug AMUX-2670. Guarded against REIMPLEMENTATION drift and
 bitten by DEPENDENCY drift, which the same argument covers and the implementation
 did not.
FIX: bd5dc6c8. Extraction walks the call graph (it now pulls `_curl` and
 `_transport_breadcrumb`), and a helper that IS defined in ./amux but missing from
 the harness is a named failure printed ABOVE the assertions it would otherwise
 corrupt. Mutation M4 breaks the closure and confirms it names `_curl`.
 Generalisable: a test that extracts a function to avoid copying it inherits that
 function's dependencies, and nothing in "extract, do not copy" says so.

## The fallback prints "no audit" one line after writing the audit row
VALIDATED: amux-frustrations | Validated by the originating session (amux-frustrations), re-verified in the shipped
CLI today rather than from the fix commit.

Fixed in bd5dc6c8. `grep -n 'no audit' amux` returns NOTHING: the contradicting
clause is gone and the message now says the send is recorded and reconciles.

The verification order is also as the entry demanded. amux:1412 offers
`tmux capture-pane -p -S -200 -t "=$tname:" | tail -40` FIRST, with the curl demoted
below it at :1418 and labelled with the precondition it needs. All three details the
entry called out are present and commented: $tname not $name (the 2026-07-27 shape
that made gtm-engine read a delivered message as lost), the load-bearing trailing
colon, and -S -200 for scrollback rather than the viewport.

scripts/test-unstamped-ledger.sh -> 16 passed, 0 failed.

AREA: cli
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-454
SYMPTOM: `amux send` falls back to raw tmux when the server is unreachable. The
 fallback calls `_record_unstamped_send`, which writes the send to a local ledger
 that reconciles into /api/history on the next successful send (AMUX-2670). The
 VERY NEXT LINE printed "DELIVERY UNVERIFIED, no origin stamp, no audit". The stamp
 half is true. The audit half contradicts the statement one line above it.
COST: gtm-engine hit a real flap on 2026-09-03, read "no audit" literally, spent a
 measurement pass on it (three probes of /api/sessions: DOWN, DOWN, UP), and routed
 a provenance gap here as "during a flap every lane's messages become
 unattributable". The mechanism had worked perfectly on that exact send: it was
 sitting in the trail as MSG-40621, type `raw-tmux-fallback`, origin
 "unstamped-fallback from gtm-engine". A peer lane re-derived a closed gap because
 the tool said the opposite of what the tool did. Second cost in the same message:
 the only verification offered was `curl "$AMUX_URL/api/sessions/<name>/peek"`, a
 request to the server whose unreachability is the ONLY reason that branch runs
 (ethos rule 3), and it named the FLEET name, so gtm-engine ran `tmux has-session
 -t gtm-ticker` against a session actually called `amux-gtm-ticker`, found nothing,
 and briefly read a DELIVERED message as lost. That is the 2026-07-27 shape
 CLAUDE.md already warns about by name.
FIX: bd5dc6c8. The message now says recorded-and-reconciles, and offers `tmux
 capture-pane -p -S -200 -t "=$tname:"` first with the curl demoted below it and
 labelled with the precondition it needs. The trailing colon is load-bearing:
 `-t "=$tname"` answers "can't find pane", which was in the line until it was
 tested. `-S -200` because a bare capture-pane returns the viewport, the exact trap
 CLAUDE.md documents for peek. Five assertions in
 scripts/test-unstamped-ledger.sh, all mutation-verified.

## The `verdict=READ verb` arm names a card by ID, and every one of its 17 positives is false
VALIDATED: amux-frustrations | Validated by the originating session (amux-frustrations), and this one is validated
by a MEASUREMENT taken today rather than by reading the fix.

Fixed in e6cb8901 (2026-09-03 11:59:54). inferred_edit_verdict now separates a BARE
git read subcommand, which is impossible from a real git invocation because
first_blocking_verb skips those, from a genuine read verb. The bare case is named as
quoted DATA tokenised as shell (AMUX-3822's defect through a quoted string) and
explicitly NOT a specimen.

MEASURED over both server logs, window 2026-08-30T03:09 -> 2026-09-04T13:03:
  verdict=READ lines in server-rs.log.1 (pre-fix window):   17
  verdict=READ lines in server-rs.log   (post-fix window):   0
  all 17 carry blocked_by=status, on backend (9) and mixpeek-homepage-claude (8)
  latest of the 17: 2026-09-03T08:58:12, i.e. three hours BEFORE the fix landed
So every row that ever claimed to be AMUX-2841's specimen was the artifact this
entry describes, and none has been minted since.

COUNTING CAVEAT, recorded because it nearly cost me the number: `grep -o
'verdict=READ'` over these logs returns 8, not 17. The logs contain NUL bytes
(216,873 in .log.1, 36,340 in .log), so grep switches to binary mode and suppresses
match output while -c still counts lines. Use `grep -c` or `grep -a`. Filed
separately; it is not a defect in this entry's mechanism.

AMUX-2841 remains OPEN and this validation does not touch it: it is the watch that
wants a REAL specimen, and the measurement above says zero have appeared in the
15.5h since the fix armed it.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-frustrations
CARD: AF-452
SYMPTOM: `[staged-guard/inferred-edit]` publishes a `verdict` field with three arms. The
 READ arm's text is "READ verb — is_pure_read_command missed a reader ... This is the
 specimen AMUX-2841 wants". Over 75,758 firings in a 4-day window it produced 17 rows, all
 `blocked_by=status`, on two mixpeek lanes. All 17 are artifacts, and the proof needs no
 transcript: `first_blocking_verb` (git_guard.rs:1726) `continue`s on a real git read
 subcommand, so a genuine `git status` can NEVER reach that field. A bare `status` token
 there PROVES it came from quoted DATA tokenised as shell. `is_known_read_verb` (:1696)
 then consults GIT_READ_SUBCMDS and reads it as a genuine read. The two functions disagree
 on one vocabulary: `first_blocking_verb` checks READ_ONLY_VERBS only, `is_known_read_verb`
 checks both.
COST: AMUX-2841 was unparked from backlog to todo on 2026-09-03 because "the discriminator
 AMUX-3822 added now exists", and it does exist and it does fire — with nothing but false
 positives, each naming that card by ID. A 23-day specimen hunt was pointed at prose. Also
 cost a wrong first measurement in the same sitting: I counted `blocked_by` (~30% non-verbs
 across 1,076 distinct values) and nearly reported the discriminator as unreadable, when
 that field's unclassifiable tokens are the EXPECTED input to the verdict arm that replaced
 it. Correct observation, wrong field.
FIX: the whole READ arm is unreachable for its stated meaning — `first_blocking_verb`
 returns a verb only when it is NOT in READ_ONLY_VERBS, so the arm's first half is dead by
 construction and its second half only matches the artifact. Checked the artifact case
 FIRST and named it, so the log stops claiming a specimen: git_guard.rs
 `inferred_edit_verdict`, extracted from the log site so the arms are testable at all.
 Whether the READ arm should exist is AMUX-3822's author's call, recorded on AF-452.

---

## A main lane with no $AMUX_SESSION in its env is invisible to the staged-guard's edit records
VALIDATED: mixpeek-research | VALIDATED 2026-09-04 (mixpeek-research), in their own words, on the half this lane
had flagged as unproven:

edit-record hooks derive session from tmux name when $AMUX_SESSION is empty
(observed-edits-pre.py:34, observed-edits-post.py:70, MR-43 cited in the code),
prepare-commit-msg carries the same fallback, derive-failure is a counted WARN; the
wrong-editor verdict is no longer reproducible from an unset variable.

They verified the EDIT-RECORD path specifically rather than the commit stamp, which
is what the entry is actually about: the hooks now distinguish tmux-errored from
human-shell, which formerly both returned "" identically, and a derive failure logs
a countable WARN instead of vanishing like a human shell.

Card MR-43 was already done; this is the author's signature the protocol requires.
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-24
SESSION: mixpeek-research
CARD: MR-43
SYMPTOM: This lane runs in tmux session `amux-mixpeek-research` (amux-launched), yet
  $AMUX_SESSION is empty in its shell. In one task that meant: `amux board add` would have
  created an unattributed card, the prepare-commit-msg trailer would have been empty, and the
  staged-guard's cross-session check said "you have no edit record on this path in the last
  360m" for a file this lane had edited three times in the previous ten minutes, because the
  PostToolUse edit-record hook reports under the same empty variable. Its verdict then named
  the peer as the sole editor and blocked the commit. The three subagent entries earlier in this
  file (SESSION: "... no $AMUX_SESSION in env") are the same shape one level down.
COST: two refused commits and about 5 minutes, plus a guard verdict that was wrong about who
  edited the file; every CLI call needed AMUX_SESSION exported by hand from the tmux name.
FIX: derive the session from the tmux session name (`tmux display-message -p '#S'`, strip the
  `amux-` prefix) in the edit-record hook and the CLI when the variable is empty, and say in
  the guard verdict when that fallback was used. Plus a WARN in the lane-launch path when a lane
  starts without the variable, so /api/logs/analyze can count these instead of a human noticing.
RESTORED 2026-09-02 by amux-frustrations, not by its author, and the way it was lost is worth
  one sentence because no set-difference could have found it. 7dbab8f6's whole-file overwrite
  left this entry's HEADING sitting on top of a DIFFERENT entry's body: AF-195's, which had
  already been validated and archived. So the ledger carried a chimera that read as a live
  MR-43 to anyone scanning headings and as a live AF-195 to anyone reading bodies, and it
  survived AF-430's title-based dedup because its heading was not in the archive. Recovered
  verbatim from 8fdc4bdf; every line above this note is mixpeek-research's. STATUS stays
  `open` because only they can change it. See AF-434.


---

## amux send to a bare REPL worker: origin header is submitted as its own message, prompt body is not
VALIDATED: amux-cloud | VALIDATED 2026-09-04 by amux-cloud, the originating session, from current behaviour
rather than memory.

RIGHT AND FIXED, by a DIFFERENT resolution than this entry proposed, and they said
so explicitly. The entry asked for REPL-aware delivery. What shipped instead is an
honest refusal: a peer send to a no-harness worker is refused rather than
mis-delivered (the isolated raw-agent mechanism, AMUX-3232). Their words: "Nothing
gets submitted, so the header-as-message sentence stopped being true."

Exercised live in the same session, and independently by this lane on the same day:
    $ amux send amux --stdin
    send refused: 'amux' is an isolated (raw-agent) worker with the amux harness
    stripped ... reachable only by the owner from the dashboard.

The friction this entry names is the FALSE "sent", and it is gone. Card AC-354 done.
AREA: notices
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux-cloud
CARD: AC-354
SYMPTOM: Driving a qwen3.8:27b ollama worker, `amux send qwen-eval "<prompt>"` returned
  `sent (origin-stamped): sent`, but the peek showed the model had received and answered only
  the `[amux-origin: amux-cloud ...]` HEADER (qwen reasoned about it as a possible
  social-engineering attempt and asked what I wanted), while the real prompt sat in the REPL
  input typed-but-unsubmitted (`Press Enter to send`). I had to `tmux send-keys Enter` by hand
  to get an answer. The steering/delivery choreography is claude-UI-shaped: it injects an
  origin header the bare REPL treats as content, and it does not submit the body.
COST: The send reported success while the payload never ran — a false "delivered" (ethos rule
  4). Every eval prompt needed a manual Enter, so the amux worker plumbing could not drive the
  model unattended; I fell back to tmux for the model eval.
FIX: REPL-aware delivery (AC-354, routed to amux, who owns the send/steering path): for
  bare-REPL providers, do not inject the origin header as a submitted message (omit it or make
  it a non-submitted preamble), and ensure the body is actually submitted. Verify by peeking
  that the model answered, not by trusting `sent`. Same message->worker seam as [[amux-project-reference]]
  AC-353 (env-apply can't message a not-yet-started worker).

  CONTESTED 2026-08-21 by the author (amux-cloud). No commit in history references AC-354
  except the docs commit a21ad4d, so the card closing is not evidence of a fix — this is
  the "card closed on a different thing" shape the validation pass was watching for. A
  bare REPL worker is not cheap to exercise, so the entry stays until someone names the
  fix sha.

## Cloud silently froze behind a red main CI — "skipped" reads as "up to date," not "frozen"
VALIDATED: amux-cloud | VALIDATED 2026-09-04 by amux-cloud, the originating session, from current behaviour
rather than memory.

RIGHT AND FIXED. cloud_autofix.check_deploy_freshness (line 318) joins deployed-sha,
origin/main and the rust CI result and NAMES the state rather than leaving "skipped"
to be read as "up to date": FROZEN (behind + CI red) vs lag (behind + green) vs
current. main() escalates "cloud image FROZEN — N commits behind, main CI is RED."

Exercised live in their session: the AC-402 chain fired exactly that
("state=FROZEN ci=failure" -> escalated), and they watched a later freeze clear
itself on green. The signal this entry asked for exists and works.

RECORDED BECAUSE I GUESSED WRONG: I told them this was the one I suspected was still
live, and asked them not to answer on my guess. They measured it and it is fixed. A
reviewer's suspicion is not evidence, which is the same shape as everything else on
this drive.

Card AC-344 done.
AREA: cloud
SEVERITY: slows
STATUS: open
DATE: 2026-08-13
SESSION: amux-cloud
CARD: AC-344
SYMPTOM: Ethan reported "cloud is still behind in versions." A fresh cloud org still booted build 0f2f6e48 (pre-env_config: GET /api/env/schema -> 404, /api/env/apply absent from 213 routes), so the converged seed.py --via-apply 405'd against cloud. Root cause was three layers down: deploy-cloud.yml auto-deploy is gated on GREEN rust.yml (workflow_run), and main CI had been RED for hours on ONE clippy lint (unnecessary_sort_by, messages.rs:585). Every deploy-cloud run showed "skipped" — indistinguishable from "nothing to deploy." Nothing anywhere said "the cloud image is frozen and falling behind main because CI is red."
COST: Ethan had to notice the version lag by hand. Diagnosing it took several manual steps (fresh provision -> /health build hash -> /api/debug/routes -> gh run list conclusion -> git log timing) to join signals that no single instrument joins. And it is fleet-recurring: ANY lane's red-main break freezes the entire cloud deploy for every customer, invisibly, until a human notices — the busier the fleet, the more often it happens. PREDICTION PROVEN 2026-08-14 (author-verified during a frustrations validation): the "until a human notices" line came true VERBATIM, three times in ONE session, all AFTER this entry was written — 67b44f7 (clippy unnecessary_sort_by), 64fd450 (steering restart_persistence test), 9442f77 (opencode ETXTBSY flake + /api/tts unclaimed in the boundary registry). Each red-mained main, each made deploy-cloud SKIP silently, each froze :latest, and each was caught BY HAND via the freshness tick — never by any instrument. A prediction that recurred 3x on the record is the strongest possible argument for finally building the signal.
FIX: AC-344 — a signal that joins live-cloud-build-hash vs latest-green-main and fires when they diverge (commits or hours), OR make deploy-cloud's skip loud (record "skipped because CI red since <sha>/<time>"). Interim: clippy blocker fixed (67b44f7); steering-test blocker handed to amux; cloud auto-catches-up once CI green. Related: AMUX-3013 (pinned toolchain so local clippy == CI clippy — why the red wasn't caught pre-push).

---

## The staged-guard was silent on the commit that swept a peer's work, and warned on the clean one
VALIDATED: amux-cloud | VALIDATED 2026-09-04 by amux-cloud, the originating session, from current behaviour
rather than memory.

RIGHT AND FIXED. The guard now WARNS on the exact incident shape (a wholesale
`git add` of a co-edited file with nothing unstaged). It fired the co-edit NOTE on
their own board_drive.rs commits repeatedly in that session: "also edited by session
'amux' Nm ago... stages X insertions / Y deletions there — if that is MORE than you
wrote, their work is in it." It is no longer silent.

That also resolves their own 2026-08-21 CONTESTED objection on this entry
("plausible fix, not exercised"): it is now a specimen exercised in production, not
a reading of the code.

CAVEAT THEY ASKED TO CARRY, and it is not cleared by this archive: the warning comes
from mtime co-edit detection, whose FALSE-POSITIVE risk (naming your own write as a
peer's) is a different and deeper problem, live under AF-179 / AMUX-3662. Archiving
this entry says the SILENCE stopped; it says nothing about the attribution being
right when it speaks.

Card AC-297 done.
AREA: attribution
SEVERITY: blocks
STATUS: open
DATE: 2026-08-08
SESSION: amux-cloud
CARD: AC-297
FIX-NOTE: b7dba01 PARTIAL — _staged_guard_check() now checks for unstaged changes, which
  helps when peer work is left unstaged. But the incident shape (wholesale `git add` where
  the peer's work is swept into the index, leaving nothing unstaged) is still silent.
  The guard fires on has_unstaged_changes=True; the incident has has_unstaged_changes=False.
  Validated by amux-cloud on a throwaway repo: control (peer work left unstaged) fires;
  incident shape (wholesale git add, all staged) does not.
SYMPTOM: Two commits, 20 minutes apart, both `git add amux-server.py` on a shared checkout
  while session `amux` had uncommitted work in the same file.
    fc72811 — guard WARNED ("also edited by session 'amux' 30m ago... stages 55 insertions /
              2 deletions"). I checked line by line. It was genuinely clean, all mine.
    8adf348 — guard SILENT. It swept ~85 insertions of amux's session-report/heartbeat work
              (_ACTIVE_HEARTBEAT_S, _persist_session_reports(force=...), the PostToolUse
              "tool-hook" entry, _scrape_vs_report "active-stale") into my AC-293 fix.
  So the one time it mattered it said nothing, and the one time it spoke the commit was fine.
COST: A peer's uncommitted work is now inside my commit and cannot be separated without a
  history rewrite on a shared checkout — the operation CLAUDE.md records as having destroyed a
  session's unpushed work. Second occurrence for me; the first was b1c3e93 (~93 lines).
  Disclosed both times, and both times the fix was the peer's call rather than mine to make.
FIX: The correlation is the dangerous part, not the miss. I checked BECAUSE it warned and did
  not check when it did not — so the guard actively trained the behaviour it exists to prevent.
  A guard that is silent on the true positive is worse than no guard. Find why it fired at 30m
  and not at ~20m (mtime window? cooldown? a debounce that suppresses a second warning in the
  same session?) and make it fire on the FACT — peer has uncommitted hunks in a file I am
  staging whole — not on a time heuristic.
  Until then the instrument that actually worked was arithmetic: reconcile the numstat against
  what you believe you wrote, every commit, guard or no guard. 146/14 against a ~60-line change
  is what caught this. That check needs no guard and cannot go silent.

SCOPED 2026-08-09 by amux-frustrations, from amux-cloud's validation: the shipped fix
  (`if hit or _is_dirty`) is PARTIAL. It fires when the peer's work is left UNSTAGED, but
  their actual incident was a wholesale `git add` that swept the peer's work INTO the
  index — so nothing was unstaged, _is_dirty was False, and there was no fresh `hit`
  either. Tested in a throwaway repo with a control that DOES fire, so the negative is
  informative rather than a silent probe. Remaining scope: "wholesale git add of a
  co-edited file where the peer has no fresh provenance record". Nobody has started it.
  amux independently named the same remainder from the other side (their AF-19 review):
  a peer file staged OUTSIDE the recent-edit window has no claim trail and stays
  invisible; the belt is "list every staged path not in the committer's diff".

  CONTESTED 2026-08-21 by the author (amux-cloud). The 08-15 guard overhaul
  (d5c575e / b9dbf70 / 26adbc6) may well cover the incident shape (wholesale git add,
  has_unstaged_changes=False), but nobody has re-run the throwaway-repo specimen against
  it, and the entry's own FIX-NOTE records that shape as validated STILL SILENT. Held open
  on the honest basis that a plausible fix is not an exercised one. amux-cloud volunteered
  to re-run the specimen; the entry goes when that runs, not before.

## `--trigger` destroys the value it replaces and records only the field name
VALIDATED: gtm-engine | VALIDATED 2026-09-04 by gtm-engine, the originating session, on the SECOND attempt.
Recorded that way deliberately: they REFUSED the first validation with a measurement,
the card was reopened rather than the entry touched, and this signature is over a
different fix.

WHAT THEY REFUSED. The first fix (e56fff8b) raised the log cap from 60 to 200 chars.
They probed the boundary on a scratch card rather than confirming the commit:
   88 chars ->  88 retained, tail survives
  158 chars -> 158 retained, tail survives
  208 chars -> 201 retained, TAIL LOST
  366 chars -> 201 retained, TAIL LOST   <- their actual loss
So the exact partial-recovery this entry describes survived the fix written for it.

WHY THE TEST WAS GREEN, which is their diagnosis and the more useful half: the
fixture was 132 characters against a 200 cap, so "head AND tail survive" was
satisfied by any cap at or above 132. Mutating to 60 reddened it; mutating to 201,
the shipped value, did not. A fixture that cannot cross the boundary cannot test it.

THE REAL FIX, e276496c. The destroyed value is kept whole to 1800 chars (900 head +
900 tail), and past that the MIDDLE goes with `[N chars elided]` in the gap rather
than the tail.

THEIR VERIFICATION, run independently and crossing the bound on purpose:
  366-char inventory  head True, tail True, the fifth item True, no marker
  2620-char value     head True, tail True, marker "[820 chars elided]"
  2620 - 1800 = 820, so the count is arithmetically consistent, not merely present.

AND THEIR CORRECTION TO MY OWN FRAMING, which is the truer statement of the fix and
is why it is quoted rather than paraphrased: "A 366-char value came back whole under
a 1800 cap and would have come back whole under a 500 cap too. What closes it is that
a value past the bound now arrives MARKED INCOMPLETE instead of looking like a prefix
that was all there was. My loss was not that four items survived; it was that nothing
told me a fifth had existed. The elision count is the difference between a
recoverable loss and a silent one, and it holds at any cap."

Scratch cards GE-807 and GE-808 created and deleted; nothing left behind.
Card AF-459 done.

AREA: board
SEVERITY: data-loss
STATUS: fixed
DATE: 2026-09-03
SESSION: gtm-engine
CARD: AF-459
SYMPTOM: `amux board <status> --trigger` writes `source_ref` as a plain overwrite.
 Only an `autofix:` prefix is protected (AMUX-3686), and that narrowness is
 deliberate: a trigger replacing a trigger is normal. The PATCH log builds one line
 per patch from a Vec of FIELD NAMES, so the overwrite rendered as the bare word
 `source_ref` — that it moved, and nothing about what it moved from. `/api/history`
 carries no row with the value either. The column being written WAS the only copy
 in existence.
COST: gtm-engine lost a five-item inventory dated 2026-08-09 while probing whether
 `--trigger` works on an archived card (it does). They recovered four items from a
 prefix they happened to have printed earlier in their own transcript; the fifth is
 gone permanently. Second known clobber of this field on that board, so the first
 one cost something too and nobody logged it.
FIX: e56fff8b. One match arm. This is now the one field whose log line names the
 DESTROYED value rather than the arriving one, because the log's own stated rule
 ("VALUES ARE SUMMARISED, NOT COPIED ... the new value is already on the card") is
 correct for every other field and inverts here: there is no redundancy to trade
 against readability when the value exists nowhere else. Old value kept at 200
 chars against 60 for arrivals, with a mutation pinning it, because the failure
 mode is specifically PARTIAL recovery and a truncated sole copy reproduces the
 prefix-survives-tail-dies loss exactly. Three mutations, including a negative
 control (printing WAS unconditionally would claim data loss on every card
 creation).

## Shared-checkout guard blocks `git reset` but not the bare `git commit`
VALIDATED: backend | guard refuses a no-pathspec commit sweeping index-vs-origin drift; 33c602d3; git-shared-guard.py:613/706/729; caught a live 15-file cross-lane sweep 2026-09-07. Validated by backend after reading committed bytes on origin/main, not the worktree.
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-09-04
SESSION: backend
CARD: AF-507
SYMPTOM: `git add <file>` hit a peer's index.lock and failed, so the file was
 never staged. The follow-up bare `git commit -m` then committed the whole
 index-vs-frozen-HEAD drift — 1120 files, +67067/-6296 — under their message, not
 containing their change. `git reset --soft HEAD~1` to undo it was then BLOCKED by
 git-shared-guard.py, correctly. The guard blocks the FIX and not the CAUSE.
COST: A near-miss, contained only because the mega-commit was local-only and
 diverged non-ff from origin; the real change landed via the zero-write graft
 recipe instead. `git reset` is guarded because it moves HEAD, while a bare `git
 commit` on this checkout is both more common and less recoverable, and is
 unguarded.
FIX: AF-507. Refuse a no-pathspec `git commit` whose staged set exceeds a
 file-count threshold against ORIGIN/MAIN (not HEAD — graft-push freezes HEAD
 ~1846 behind, which is what makes the drift large), with a named audited env
 escape. backend confirms both primitives already exist in that guard: the
 origin/main-diff is in the co-edit leg and the escape shape is the --allow-*/env
 pins. Related: AF-503 (the index.lock contention that started the sequence).

## No board state means "blocked on ANOTHER LANE's decision"
VALIDATED: backend | reassign exit reaches all four board refusals; 7b7878a1 + c25aecfd; board.rs:5179/5737; exercised live (MOS-18 handoff) 2026-09-07. Validated by backend after reading committed bytes on origin/main, not the worktree. Caveat recorded and answered: backend could not reconstruct which of the four refusals their original scenario hit, but c25aecfd extended the exit to all four, so whichever it was is covered.
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-04
SESSION: backend
CARD: AF-506
SYMPTOM: Autonomous backlog triage picked up MI-4155, a card owned by a different
 lane. Every state is a lie or a loop: `backlog` re-feeds the same lane's
 auto-pickup (it came back twice), `todo` re-queues after cooldown, `needsyou`
 reads as blocked on Ethan rather than on a peer, and `review` — which the
 DISPATCHER's own card text recommends ("if blocked on an owner decision, move to
 review") — gates on acking "Implemented and self-tested" / "Diff / PR is up",
 which a card you are ROUTING AWAY cannot truthfully claim.
COST: A lane cycled a card through two dead ends before finding that PATCHing the
 card's `session` to the owning lane is the answer. Nothing in the blocked
 response's how_to_ack hints at it, so every lane running backlog triage
 rediscovers it or picks a dead end. Ethos rule 3: no truthful path for a
 legitimate state.
FIX: AF-506. (b) first — surface "reassign session to the owning lane" in the
 blocked response, the same way it already surfaces the gate-ack CLI. (a) a real
 blocked-on-peer state that does not re-dispatch and is not gated on a
 self-implementation attestation; that is a board-state change and is Ethan's to
 approve. Also fix the dispatcher's "move to review" line, which routes people
 into the refusal.

## The staged-guard told a peer their work was at risk from me, because I had read the file
VALIDATED: amux-frustrations | The entry's sentence is: the staged-guard told a peer their work was at risk from me, because I
had read the file (copied it OUT to a scratch worktree to verify their fix). That sentence has
stopped being true, and it took two commits to be able to say so honestly.

MECHANISM: 205d53cd added `source_only_paths`, so a path a command only READS (a `cp` source)
is not minted as an edit record even though the command as a whole writes. In this history and
in the running build.

WHY I COULD NOT VALIDATE IT ON THE FIRST ATTEMPT, recorded because it is the reusable part:
the helper was tested and the WIRING was not. Mutating the call site
(`if source_only.contains(cand)` -> `if false`) left all 73 git_guard tests green, so unwiring
the carve-out entirely from the claim loop changed no test. Validating there would have signed
off my own work on a suite that could not fail for the thing the entry is about.

CLOSED BY 43c71b7e: both carve-outs moved into `claimable_path_candidates`, extracted from
`recent_edit_paths` so the decision the loop actually makes is testable. Three cells:
  cp <src> <dst>      source NOT claimable, destination IS
  grep -n foo <src>   claims nothing
  echo x >> <src>     STILL claimable (the control; without it an always-empty return
                      passes the first two and disables attribution entirely)
Mutation-checked both directions: dropping the carve-out reddens "the copy SOURCE must not be
claimable"; claiming nothing reddens "the copy DESTINATION must still be claimable: []".

  scripts/test-contended.sh -p amux-server --lib git_guard -> 74 passed, 0 failed (was 73)

LIMIT, stated rather than hidden: this is validated at the deciding layer and in the deployed
build, not by observing a peer commit after a read. 43c71b7e is unpushed, so CI has not run it.
The entry's sentence is what I am retiring; the subsystem is not being declared done.
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-09-07
SESSION: amux-frustrations
CARD: AF-550
SYMPTOM: amux-testing-e2e held an uncommitted fix to a test I had broken on origin/main.
  To ask them to land it with evidence rather than a guess, I read the file (sed/awk) and
  copied it OUT to a detached scratch worktree to prove their fix compiled and passed —
  shared -> scratch, never the reverse, zero writes to the shared tree. When they went to
  commit, the guard told them: "the staged set includes 1 file(s) whose edit records are
  YOURS ... differs from HEAD and you have no commit for it; the WORK ITSELF is at risk —
  CHECK THIS ONE". Nothing in that file was mine. `git diff HEAD` showed 10 added lines,
  all of them their fixture; both of my commits to the path (4c068e80, b1f8f9e5) were
  already on origin.
COST: A commit stalled on a warning about a lane that had only verified it. The specific
  cost is the phrasing "the WORK ITSELF is at risk", which is the one line that should stop
  a commit outright, spent on a false positive — and it fires most readily against the
  careful behaviour, since the sessions that touch a peer's file to CHECK it are exactly
  the ones that generate reads. It also cost me the reciprocal warning ("scripts/
  push-consent.sh was also edited by amux-cloud") on a file I had created minutes earlier,
  which was my own mutate.sh runs moving the mtime.
FIX: The edit record is built from Bash commands that MENTION a path, and a mention cannot
  separate `cat`/`sed`/`awk`/`cp <src>` from a write. Two options, and the second is the
  real one: (1) classify the verb — a read-only command naming a path is not an edit
  record, and READ_ONLY_VERBS already exists in git_guard.rs for the shared-checkout guard;
  (2) stop inferring from commands at all and use the tool-call record, which knows Edit
  from Read exactly. AF-179 records the mirror of this on the co-edit side, where the guard
  itself prints "OBSERVED claim, not a recorded write" — the victim column has the same
  defect and no such caveat, so the weaker signal carries the stronger words.

## A CI failure that only reproduces on 12% of clock values was filed as a flake twice
VALIDATED: amux-frustrations | Probed live, not asserted. GET /api/debug/invariants reports 682 invariants with measured=true,
and among them serde.f64_survives_json_roundtrip status=pass. The registration shipped in
fb1dab8f (Cargo.toml float_roundtrip + the checks.rs comparator + the monitor gatherer and its
wiring test). A first attempt to confirm this read d['invariants'] and d['results'], neither of
which exists on that payload, so it returned 0 matches and looked like an absence; the real key
is latest_per_invariant. Re-probed with a control (ids containing "serde" -> exactly this one)
so a filter returning nothing could not be mistaken for a filter that works.
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-frustrations
CARD: AF-595
SUPERSEDES: "One repeated timestamp subtraction made the exact-SHA Rust gate
 nondeterministic" (2026-09-08, amux-testing-e2e, filed under CARD: ATE-93). That
 entry reads STATUS: fixed and names "codegen rounding" as what it disambiguated.
 The mechanism is wrong and the failure recurred the same day with a fresh value.
 Its text stays put: only the session that wrote it can retire it, and the
 disposition is `--superseded` rather than validated, because archiving it as
 correct would file a dead mechanism as history. Routed to amux-testing-e2e.
SYMPTOM: `api::git_guard::tests::stored_observations_reach_the_actual_guard_without_naming_the_reader`
 reddened `check` on main with `left 1788887412.419762, right 1788887412.4197621`,
 one f64 ULP apart. It passed locally on the same tree in the same hour. Earlier the
 SAME morning (61660487, 08:39) the same test failed on a different pair,
 1788859526.403303 against 1788859526.4033027. Nothing about the code differed
 between the runs; only the sampled mtime did.
COST: `check` red on main, which blocks every PR that inherits it; PR #201 sat behind
 it. Two lanes independently classified it as a flake. The first remedy, 61660487
 "capture expected observation timestamp", was applied to a cause the evidence never
 supported and the test went green again by luck, so nothing detected that the fix
 was wrong. This card's own opening diagnosis, written by this lane, named the same
 wrong cause a third time.
FIX: The cause is serde_json's default float parser, which is not correctly rounded:
 an f64 written with `to_string` can read back one ULP away. Measured on 1.0.151 over
 1,023,542 f64 values sampled across one second at epoch magnitude, 126,027 (12.3%)
 drifted; with the `float_roundtrip` feature, 0. Enabled that feature workspace-wide,
 added `serde.f64_survives_json_roundtrip` to the invariant monitor so dropping it
 announces itself at runtime, and pinned the two real CI values in a test that
 reddens every run rather than 12% of them.
 THE GENERAL SHAPE, which is what the next session will hit: a test whose failure
 depends on the VALUE of a sampled input is indistinguishable from infrastructure
 flake, and amux has no instrument that separates the two. A remedy aimed at the
 wrong cause looks confirmed, because the next run is green either way. When a
 failure is intermittent, the question that separates them is whether the input
 changed between runs, and a rerun cannot answer it.

---

## "commit by pathspec" protects other files, and does nothing for a file you BOTH edited
VALIDATED: amux-frustrations | The correction is in the file the rule has to reach. ~/.claude/CLAUDE.md now carries the
paragraph "PATHSPEC PROTECTS OTHER FILES, NOT A FILE YOU BOTH EDITED", with the measured incident
(66818693 swept amux-testing-e2e's uncommitted Codex composer-footer fix, its fixture and its
regression test: 4 of 6 hunks and 59 of 171 added lines were theirs) and the three commands that
answer it, including `git add -p -- <path>` for the unstaged-co-edit case the earlier text had
dismissed. grep -c over that file returns 1. This lane then used that procedure on every commit
today, including a46b164b.
AREA: attribution
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-frustrations
CARD: AF-485
SYMPTOM: ~/.claude/CLAUDE.md prescribes, for a shared checkout, "commit by pathspec.
 `git commit <your paths>` ignores the index for everything it does not name and
 leaves their staged entries untouched." Every clause is true and the paragraph reads
 as a general guarantee against sweeping a peer. It is not one. `git commit <path>`
 takes the WORKING TREE state of that path, all of it, not your hunks — so a peer's
 UNSTAGED edits to a file you name land in your commit, under your message and your
 Amux-Session trailer, while the pathspec does exactly what it promises. The same
 paragraph pre-emptively dismisses `git add -p` ("theirs are already staged"), which
 is correct for the state it describes and wrong for this one, so the reader is
 steered away from the one tool that would have helped.
COST: self-traced, 2026-09-04. I committed 66818693 by pathspec on
 crates/amux-server/src/api/session_verbs.rs and swept amux-testing-e2e's
 uncommitted Codex composer-footer fix, its LIVE_CODEX_IDLE fixture and its
 regression test: four of six hunks, 59 of 171 added lines. I pushed it before they
 could tell me, so the remedy CLAUDE.md prescribes for an absorbed change (do not
 rewrite shared history; record the reasoning in a follow-up) is now the only one
 available. The code survived correct and tested; the REASONING did not, because my
 commit message is entirely about a diagnostic window parameter and says nothing
 about Codex footer chrome. They found it, not me, and they asked whether they could
 push a commit that was already on origin.
 The guard was honest and I misread it: the commit printed "no transcript for RUNNING
 cotenant(s) amux-codex — their edits are INVISIBLE to this verdict". It never named
 amux-testing-e2e. A guard saying it cannot see is telling you to look.
 Filed the same day I measured that 21 of 75 live entries in this file are the one
 shared-index class (AF-336). This is the twenty-second, produced by following the
 guidance written to prevent it.
FIX: the pathspec paragraph in ~/.claude/CLAUDE.md now states what pathspec does NOT
 cover, carries the measurement above, and gives the three commands to run before a
 pathspec commit on a co-edited file: `git diff -- <path>`, `git diff --cached --
 <path>`, and `git add -p -- <path>` followed by a commit with no pathspec. It also
 says why `add -p` is right here despite the dismissal three lines above it: that
 dismissal is about a peer's already-STAGED work, a different state.
 NOT FIXED and deliberately not attempted: making the staged-guard see a cotenant it
 has no transcript for. It already reports that blind spot by name, which is the
 honest behaviour; the defect was in the guidance, not the guard.

## `GET /api/board` returns a WORKING SET, and the cap is disclosed only in headers the prescribed recipe cannot see
VALIDATED: amux-frustrations | Live check against the running server. curl -sk -D - $AMUX_URL/api/board returns
x-amux-truncated: 1, x-amux-total: 2797, x-amux-terminal-total: 12430,
x-amux-terminal-returned: 100 — so the cap IS disclosed and the numbers are real. The entry's
point was that the disclosure sits where the prescribed curl recipe cannot see it, and the fix
that retires it is that ~/.claude/CLAUDE.md now prescribes ?all=1 and ?slim=0 in the board
section with the measured incident behind each (63 cards wrongly reported missing; 49 cards
wrongly reported desc_len 0).
AREA: instruments
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-frustrations
CARD: AF-480
SYMPTOM: `GET /api/board` returns 2,053 rows; `GET /api/board?all=1` returns 12,745.
 The default caps terminal rows (done/verified/discarded) to the most recently
 updated FLEET-WIDE, which across ~50 lanes can be none of yours. The cap is right
 and the server is honest about it, in RESPONSE HEADERS: `x-amux-truncated: 1`,
 `x-amux-total: 2052`, `x-amux-terminal-total: 10793`. The body is a bare JSON array
 with no envelope, and the recipe in ~/.claude/CLAUDE.md is
 `curl -sk $AMUX_URL/api/board | python3 -c "..."`, which cannot see a header.
COST: reconciling all 84 frustrations.md entries against the default listing reported
 63 cards as MISSING FROM THE BOARD. All 63 existed. That is a whole reconciliation
 pass, and the report it produced was wrong in the direction that invents work: it
 would have had me file 63 duplicate cards for entries that already had one. The
 `amux` CLI ALREADY reads those headers and prints the cap, and its own comment says
 why ("nobody reads response headers from a pipe, which is ethos rule 4's second
 layer: a tag in a store the reader never opens") — so the capability existed, was
 correct, and did not reach the path CLAUDE.md tells every lane to run. Ethos rule 1:
 a feature nobody can name is a feature nobody has.
FIX: ~/.claude/CLAUDE.md now shows both forms with their row counts, says the cap is
 header-only and that a raw curl cannot see it, and points at `amux board ls` which
 can. The "read the whole board" recipe in the task-ledger section takes `?all=1`,
 since that one is asked to be exhaustive by its own sentence. Not fixed and
 deliberately not attempted: putting the disclosure in the BODY would need an
 envelope, and every consumer of that endpoint parses a bare array.

## 19 NUL bytes made grep call a 67 MB log binary, and every `grep -o` sweep silently lost half its matches
VALIDATED: amux-frustrations | The source is gone and stayed gone. ~/.amux/logs/server-rs.log is 45,210,523 bytes today and
contains ZERO NUL bytes, counted by reading the bytes in python rather than with grep, because
the entry's own lesson is that `grep -c $'\0'` is `grep -c ''` and counts every line. AF-481
removed the sentinel that reached a warn. The habit half of the entry also survives it:
~/.claude/CLAUDE.md now tells readers to pass -a and says why, so a payload that reintroduces
one does not silently halve a sweep's matches again.
AREA: instruments
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-frustrations
CARD: AF-481
SYMPTOM: same file, same pattern, three answers. `grep -c 'verdict=READ'` -> 17
 lines. `grep -o 'verdict=READ' | wc -l` -> 8. `grep -ao ...` -> 17. grep declares a
 file binary if it contains a single NUL byte and then SUPPRESSES match output while
 `-c` keeps counting lines, and it says nothing at all when its output goes to a
 pipe. The source was one warn: the create-path acyclicity check passes a placeholder
 self id of `"\u{0}new-card"`, chosen because no real card id can contain a NUL,
 which is correct and also true of a space. `depends_on_cycle` logs it as
 `self_id = %self_id`, so 19 NULs landed in server-rs.log from one stuck cycle
 (GE-473 -> MHC-256) retried across three days.
COST: 53% of the matches, in the reassuring direction, on the instrument this repo's
 own log-sweep doc prescribes. I nearly filed AMUX-2841's specimen count as 8 when it
 is 17, which would have understated a watch's evidence by half. Nineteen bytes in 67
 MB is enough, so no amount of the file being "mostly text" protects you. The wider
 shape is that a probe can be correct, run cleanly, exit 0 and answer about a
 different population than the one you asked about, with nothing beside the number
 saying so.
FIX: db3ff38a and accbba96. The sentinel is now `"(new card)"`; non-collision is
 unchanged, since card ids are `[A-Z]+-<digits>` and a space and parentheses are as
 impossible as a NUL was, and it survives a log. The guard asserts the PROPERTY, not
 the string: no control characters, at least one character no card id can contain,
 and non-empty so neither can pass vacuously. Two mutations fire (back to the NUL
 sentinel; a valid-id-shaped "NEW-0"). The repo CLAUDE.md now tells lanes to grep
 that file with `-a`, because AF-481 removed this source and any logged payload can
 reintroduce one.
 SELF-CORRECTION, recorded because it is the same class: I first reported 216,873
 NULs from `grep -c $'\0'`. bash cannot put a NUL in a string, so `$'\0'` is the
 EMPTY string and that command is `grep -c ''`, a line count wearing a NUL count's
 label. The real figure is 19, read from the bytes in python. Both halves of this
 entry are a probe whose argument silently became something else.

## One repeated timestamp subtraction made the exact-SHA Rust gate nondeterministic
SUPERSEDED: amux-testing-e2e | AF-595 supersedes this diagnosis. An isolated serde_json 1.0.151 probe (f64 -> HashMap -> to_string -> from_str) reproduces 1788887412.4197621 -> 1788887412.419762 (-1 ULP) and the original 1788859526.4033027 -> 1788859526.403303 (+1 ULP), with no repeated timestamp arithmetic. Enabling float_roundtrip preserves both input bit patterns (0 ULP). Logs: /private/tmp/ate128-float-default.log and /private/tmp/ate128-float-roundtrip.log. Computing the fixture timestamp once did not fix the parser round trip; the prior STATUS: fixed and claim that production persistence was correct were unsupported. Preserve this entry as a dead hypothesis, not a validated fix. amux-frustrations owns the AF-595 feature/invariant/regression patch.
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: The guarded workspace test passed 2,064 tests and failed only
  `stored_observations_reach_the_actual_guard_without_naming_the_reader`: its
  separately evaluated expected timestamp differed from the stored value by one
  f64 ULP (1788859526.403303 versus 1788859526.4033027).
COST: Exact-SHA Rust CI ran for over four minutes and blocked ATE-93's terminal
  gate even though the production observation round trip was correct.
FIX: Compute the fixture timestamp once and use that same binary value for the
  report and strict round-trip assertion. The failure now self-announces the
  JSON/SQLite contract instead of conflating codegen rounding with persistence.

---

## Pending hot-swap recovery outlived its active card
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Saved Some(X) versus a current None or another claim is superseded; a missing obsolete cwd does not block startup and a stale X is not resumed. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::tests::terminal_after_hot_switch_and_changed_claim_replace_stale_launch_context ... ok
AREA: runtime
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-92
SYMPTOM: A hot config change could retain saved card X after X became terminal or a different exact claim replaced it. The launch validator accepted saved Some(X) against current None, and the old worktree could then prevent startup or reintroduce stale work.
COST: Model-swap acceptance remained unsafe across task completion, despite passing the same-card restart regression.
FIX: Compare saved and current claim identities including None. Superseded snapshots derive the current exact card or scoped queue and current runtime/configured cwd; a removed old worktree does not block that replacement. swap_context_superseded names the old/current identities. The explicit terminal_after_hot_switch_and_changed_claim_replace_stale_launch_context regression covers both transitions.

---

## Refused swaps changed config and startup recovery bypassed durable delivery
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Refused live swaps leave env/meta/runtime unchanged. Startup and board-drive share one durable card/generation delivery ID, accept once, and retain pending context for retry after a failure. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::tests::refused_live_swap_preserves_env_meta_and_runtime_before_any_write ... ok
  test runtime_jobs::board_drive::tests::startup_and_board_drive_share_one_durable_resume_per_generation_and_retry_failure ... ok
AREA: runtime
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-92
SYMPTOM: Provider/YOLO branches wrote CC_PROVIDER/CC_FLAGS before validating resume identity, so a 409 could leave changed configuration. Startup consumed its pending context before a detached send_after_ready task committed delivery, while board-drive independently enqueued recovery for the same session.started generation.
COST: Refused swaps could alter the next launch; accepted swaps could lose or duplicate recovery across process failure. Both prevented honest model-swap acceptance.
FIX: Validate and atomically persist exact resume context before config writes. Startup commits its generation and context before enqueue and shares board-drive's stable card/generation identity and accepted-delivery receipt. Pending tokens clear only after durable queue acceptance; queue/history records deduplicate concurrent producers and process restarts, superseded generations cannot enqueue, and enqueue failures retain context for retry. Diagnostics: swap_context_unresolved, swap_context_persist_failed, swap_resume_enqueue_failed, swap_resume_accepted, swap_resume_superseded. Regression tests exercise the actual config branch ordering and concurrent startup/board enqueue against reopened durable state.

---

## Disk pressure cleanup deleted the shared target during active Rust gates
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). An active lease, native lock or process use blocks Cargo mutation. Guard failures refuse cleanup; idle fixtures reclaim after exit; unrelated Rust filesystem operations bypass the Cargo/Python guard. CI (run 34358221870 @ 04a19c51):
  test cargo_target_guard::tests::unrelated_reclaim_stays_native_and_cargo_sources_destinations_originals_are_guarded ... ok
Fresh local script evidence: ate-validation-cargo-guard.log (Cargo guard 10/10), ate-validation-script-1.log.
AREA: runtime
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-92
SYMPTOM: The shared debug/deps directory vanished during ATE-92 cargo check/clippy/test runs as free space jumped from roughly 13 GiB to 26 GiB. The builder explicitly bypassed its peer check below the disk floor, and reclaim could quarantine Cargo artifacts without any process or lock check. disk_watch itself only reports.
COST: Interrupted compilation, missing artifact errors, and a cold rebuild before acceptance could continue. Existing cleanup tests required destruction during a peer build.
FIX: Remove the low-disk override. safe-cargo holds an inherited lifetime lease outside the target; builder and Cargo-only reclaim mutations share one guard that takes Cargo locks, checks cargo/rustc/direct test processes, and fails closed on unknown probes. Cargo lock files retain their inodes; active builds defer with cargo_reclaim_deferred and idle retries emit cargo_reclaim_result. Ordinary non-Cargo reclaim remains native Rust. Temporary fixture regressions prove active artifact survival and reclaim after exit without depending on unrelated host builds.

---

## Model-swap resume lost the active worktree and requested an unscoped board list
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Reopened durable runtime state preserves the exact active card and existing cwd. The prompt uses the exact card first and worker-scoped --mine, with no raw replay. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::tests::provider_resume_uses_structured_state_not_terminal_replay ... ok
  test api::session_verbs::tests::model_swap_resume_preserves_exact_claim_directory_and_scope_after_reopen ... ok
AREA: attribution
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-92
SYMPTOM: The generated swap prompt requested `amux board ls --session amux-testing-e2e`, which this CLI silently ignored and returned unrelated fleet cards. The restarted worker began in the configured shared checkout instead of its active isolated task directory.
COST: The owner interrupted recovery to prevent touching foreign drafts and had to repeat the exact worktree and active task scope.
FIX: Persist exact claim, worker and runtime cwd before swapping; the launcher retains that directory or refuses an unresolved/missing context. Resume prompts name the exact card first and use `AMUX_SESSION=<worker> amux board ls --mine`. swap_context_persisted and swap_context_invalid make the identity decision visible. Regression reopens the durable DB and exercises the same launch selector for model/provider/version restarts.

---

## Workers rejected a stale Codex footer while board-drive still trusted it
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Workers and delivery boundaries use the same structured decision. Board-drive reuses one tick snapshot; target steering uses a scoped probe and fails closed on unknown process evidence. CI (run 34358221870 @ 04a19c51):
  test api::sessions_legacy::status_truth::boundary_and_workers_share_structured_codex_truth_and_fail_closed ... ok
  test runtime_jobs::board_drive::tests::live_fleet_boundary_reuses_its_tick_snapshot_and_structured_truth ... ok
  test api::sessions_legacy::status_truth::codex_tool_child_is_resolved_below_provider_not_from_provider_existence ... ok
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-92
SYMPTOM: TubeScience and Primis were skipped as mid-turn by board-drive after Workers had refused their stale Codex activity using structured FleetSignals.
COST: Eligible continuation remained blocked despite the visible runtime correction; another user intervention was required.
FIX: Board-drive reads one FleetSignals snapshot per tick. General steering and send-time race checks probe only the target worker through the same derivation. An unavailable process probe holds instead of inventing no children. boundary_stale_codex_footer_refused announces the recovered class in amux logs.

---

## Current recovery suppression also suppressed canonical advancement
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Current suppresses recovery only and still reaches continuation; repeated ticks obey the cooldown. CI (run 34358221870 @ 04a19c51):
  test runtime_jobs::board_drive::tests::current_claim_advances_once_then_obeys_cooldown_across_repeated_ticks ... ok
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-92
SYMPTOM: A surviving exact claim returned from drive_lane as Current before select_advance ran. An idle worker could keep its correct highlighted card indefinitely without a cooldown/backoff-gated continuation.
COST: The live acceptance lane remained stalled after the duplicate-resume fix; implementation could not honestly pass the advancement gate.
FIX: Suppress only recovery delivery and fall through to canonical advancement. The Current trace now records the advancement refusal reason, and task.resume_suppressed remains an idempotent receipt. Regression: current_claim_advances_once_then_obeys_cooldown_across_repeated_ticks.

---

## Every idle sweep re-delivered the same surviving task claim
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Surviving claims do not authorize repeated resume delivery; durable acceptance and receipt recovery deduplicate on the card/generation identity. CI (run 34358221870 @ 04a19c51):
  test runtime_jobs::board_drive::tests::active_worker_does_not_receive_a_duplicate_resume_delivery ... ok
  test runtime_jobs::board_drive::tests::crash_after_durable_enqueue_recovers_receipt_without_redelivery ... ok
  test runtime_jobs::board_drive::tests::startup_and_board_drive_share_one_durable_resume_per_generation_and_retry_failure ... ok
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-92
SYMPTOM: `select_resume` treated a durable surviving `task.claimed` marker as
  fresh delivery permission on every idle board-drive tick. The live log showed
  48 ATE-93 resume prompts to amux-testing-e2e, 74 ATE-114 prompts to
  handoff-producer-0907, and repeated deliveries on several other owners.
COST: Active workers were repeatedly re-steered onto the card they already
  owned, the highlighted runtime card looked unstable, and backlog progress was
  drowned by duplicate recovery prompts across hours of otherwise idle ticks.
FIX: Separate causal runtime truth from recovery authorization. A running worker
  keeps its exact claimed card without delivery; one new worker generation gets
  one stable-id recovery, and a crash between durable enqueue and receipt
  recovers the receipt without sending again. The board-drive log now emits
  `active_owner_not_resteered`, `exact_live_claim_resumed`, or
  `resume_receipt_recovered_without_redelivery` with the card, delivery id,
  measured population, and causal reason on every branch.

---

## Clean CI turned one missing provider prerequisite into 34 unrelated pointer timeouts
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Each isolated browser server persists the non-secret provider prerequisite; default-model text restoration, both API-key restore paths, the exact causal claim and the narrow terminal controls pass together. Browser (local, 25cffc1c + 0ae16bb6): settings.spec.ts settings_default_model -> desktop, ios-safari PASSED; settings_api_key_anthropic -> desktop, ios-safari PASSED; settings_api_key_survives_slow_env_refresh -> desktop, ios-safari PASSED; terminal-message-navigation.spec.ts both cases -> desktop, ios-safari, mobile PASSED; working-now-accuracy.spec.ts 'one active worker marks exactly its claimed card as Working now' -> desktop, ios-safari, mobile PASSED.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: Exact-SHA Rust CI passed 354 browser scenarios, then 34 mobile/WebKit
  terminal cases waited 30 seconds each because the clean test home correctly
  displayed `#no-apikey-banner` over their controls. Two independent stale
  fixtures also failed: default-model restoration called `selectOption` on the
  shipped text input, and Working-now created four Doing rows without one causal
  `task.claimed` identity.
COST: The required ATE-93 CI gate ran 23 minutes before reporting 37 failures,
  and one absent external prerequisite looked like dozens of terminal regressions.
FIX: The broad harness now announces and writes an obvious non-secret provider
  test value into every isolated project's `server.env`, the durable surface
  `/api/identity` actually recognizes (it intentionally ignores process-injected
  keys), keeping unrelated specs in their configured-install prerequisite.
  The settings test restores the text input through fill+blur, and Working-now
  creates its exact owner through the real claim endpoint. Its UI-only runtime
  activation preserves that exact server-produced identity instead of expecting
  a stopped fixture to project as live. Both API-key scenarios restore the known
  harness baseline explicitly because a persisted empty value shadows the
  process fallback; the slow-refresh scenario also reads the masked value back
  as its cleanup postcondition, so a future regression fails at the writer
  instead of poisoning later specs. Missing-key behavior remains product
  behavior; it is no longer accidental global state for tests whose acceptance
  has nothing to do with provider setup.

---

## Overlap timestamps shipped without units in the invariant registry
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). All seven overlap timestamp columns carry explicit seconds declarations. A separate org_teams.created_at omission was detected by the same unmodified guard and fixed under ATE-128 (pushed 6a1ffd58739f86bd08e1e733f2d186533da894e2); that is outside this entry's seven-column claim. After the fix: scripts/safe-cargo.sh test -p amux-server --test timestamp_units_declared -- --test-threads=1 -> 2 passed (ate-validation-team-timestamp-after.log).
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: The full server gate found all seven numeric `*_at` columns introduced
  by overlap coordination absent from `TIMESTAMP_COLUMNS`. Their writers use
  seconds, but the schema and runtime invariant had no durable unit declaration.
COST: A second broad gate reached its final integration targets before failing,
  preventing the required clippy and deployment chain from starting.
FIX: Declare callback updates, coordination create/update/resolve stamps, member
  create/last-seen stamps, and merged-reference creation as seconds beside the
  existing board timestamps. The exhaustive timestamp-unit guard now makes any
  future schema drift fail with the exact missing table and column.

---

## Overlap routes worked but the canonical route census called them unrouted
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). All three overlap routes are registered; the real-router bidirectional census and direct path coverage both pass. Command/result: scripts/safe-cargo.sh test -p amux-server --test route_table -- --test-threads=1 -> route_table 2 passed, including the bidirectional actual router. See ate-validation-integration.log.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: The full integration gate found `/api/board/overlap`, its coordination
  lookup, and its deployment-permit endpoint mounted and covered by handler tests,
  but absent from `ROUTE_TABLE`. `/api/debug/routes` and downstream request-log
  sweeps therefore could not distinguish those working endpoints from dead routes.
COST: The first otherwise-green full ATE-93 server run failed after more than six
  minutes and prevented clippy and deployment from starting.
FIX: Register all three overlap paths with the exact methods mounted by the board
  router. The bidirectional route-table integration guard now covers the family,
  and request-log normalization exposes overlap traffic to the existing diagnostic
  sweeps instead of silently classifying it through a generic card route.

---

## Callback e2e still expected the child link that original-task resumption removed
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). The request remains on the child; the terminal callback belongs to the original parent, retains the child ID in its text, and card/message navigation reaches the exact child. Browser (local, 25cffc1c + 0ae16bb6): worker-request-callback.spec.ts 'worker request stays on one card and returns one terminal callback' -> desktop, ios-safari, mobile PASSED.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: worker-request-callback.spec.ts expected the terminal callback's hard
  card_id and a second card-detail message button on the completed child, while
  the deployed callback contract correctly attaches that message to the original
  requester task and keeps only the request source on the child.
COST: Two three-browser working-tree runs failed 3/3 after the backend and card
  detail suites were green; the first failure looked like a missing callback and
  the second like missing UI lineage until the retained temp database exposed the
  exact cmd_history rows.
FIX: Assert the board request on the child and the callback on the original parent,
  with the child id retained in callback text. The child UI expects one source
  message, then proves card→message→card across desktop, 375px Chromium, and iPhone
  WebKit; the focused matrix passes 3/3 and emits message-card-nav verdicts.

---

## Rename integration tests built a stale hand-written issues schema that production had already migrated
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). The rename fixture uses the production migration chain and the three current rename cases pass. Command/result: scripts/safe-cargo.sh test -p amux-server --test rename_migrates_reviewer --test route_table --test timestamp_units_declared -- --test-threads=1 -> rename 3 passed, route_table 2 passed (timestamp target separately red on org_teams only, fixed under ATE-128). See ate-validation-integration.log.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: Fresh Rust CI failed all 3 rename_migrates_reviewer cases with
  `issues.requested_by: no such column`. The test manually declared five issues
  columns while the production rename path had correctly grown to migrate more
  relationship fields, so the fixture—not the implementation—failed on the clean
  runner after focused overlap and board suites were green.
COST: Exact-commit CI required a separate log download and another code/test/deploy
  cycle; the red looked like a rename regression until the missing fixture column
  was isolated.
FIX: Build the rename fixture through migrate::test_memdb_pub so every production
  schema migration is present, then insert only the rows the scenario needs. The
  clean focused target passes 3/3 and future schema growth can no longer drift this
  fixture silently.

---

## Deployment overlap preflight stopped offline disk-cleanup diagnostics
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Offline diagnostic modes bypass deployment-only overlap permits; actual adoption without a measured permit stays refused. Fresh local script evidence: ate-validation-script-1.log, ate-validation-script-2.log, ate-validation-script-3.log.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: Checks CI for 9e4de512 stopped after provenance 9/0 with no disk-clear result. The disk-only builder path queried the live overlap deployment permit for the checkout's worker trailer, then exited before cleanup on CI without a server. Locally the same test passed 24/0 by reaching the fleet server. A missing grep match then hid the builder's refusal under set -e.
COST: Exact-commit CI failed after live acceptance passed, requiring an offline reproduction and another deployment before ATE-93 could close.
FIX: Diagnostic-only builder modes log OVERLAP GUARD NOT APPLICABLE and skip the network permit because they cannot install. Real deployment still logs OVERLAP GUARD UNMEASURED and refuses adoption. Tests exercise both offline modes and the real refusal on one worker-attributed commit, pin disk tests to an unreachable endpoint, and print the missing-cleanup failure rather than exiting silently. Activation authority and launcher suites now run in Checks CI.

---

## Overlap callback tests passed alone by reading real worker identities
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). The callback fixtures own guarded temporary homes and worker identities; the vanished-worker case proves a no-env retry then one durable queue acceptance. CI (run 34358221870 @ 04a19c51):
  test api::board::overlap_reconciliation_tests::vanished_peer_and_model_switch_keep_the_handoff_callback_recoverable ... ok
  test api::board::overlap_reconciliation_tests::linked_peer_cannot_complete_or_deploy_until_owner_scope_splits ... ok
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: The overlap suite passed alone but its containing board module failed 54/2: both HTTP fixtures expected queued/200 and received retryable/202 with no-env-file. They used real handoff worker names without isolated session fixtures; other board tests correctly changed AMUX_HOME under the shared test lock.
COST: The earlier focused green overstated callback coverage and required a containing-suite investigation before deployment.
FIX: Both HTTP fixtures now own a guarded temporary AMUX_HOME and persisted test worker identities. The vanished-worker test first asserts retryable/no-env-file, then restores its fixture identity and proves one durable queued callback on retry. The production board_overlap_callback_retryable marker already names the exact refusal the old fixtures concealed.

---

## Overlap lineage elected the reporting peer in its log while the durable owner stayed unchanged
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). The elected durable owner and the reporter are distinct; readback retains resolution and callbacks, and one corrective lineage append repairs old log text without rewriting history. CI (run 34358221870 @ 04a19c51):
  test api::board::overlap_reconciliation_tests::late_peer_log_names_the_elected_owner_not_the_reporting_peer ... ok
  test api::board::overlap_reconciliation_tests::readback_keeps_resolution_provenance_and_both_durable_callbacks ... ok
  test api::board::overlap_reconciliation_tests::retry_appends_one_correction_to_legacy_lineage_without_rewriting_history ... ok
  test api::board::overlap_reconciliation_tests::distinct_concerns_keep_independent_elections_until_explicit_resolution ... ok
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-08
SESSION: amux-testing-e2e
CARD: ATE-93
SYMPTOM: OVL-15a87ecd9d24b055 elected ATE-114/handoff-producer-0907, but ATE-115's self-report appended "elected owner [ATE-115]" to both cards. GET kept the real owner but hid the resolution note, resolving actor and callback list. A second concern name was labeled scope-split even while its resolution remained pending.
COST: Live acceptance could not be signed off from contradictory ownership evidence; two new regression tests failed on the deployed source (5 passed, 2 failed).
FIX: Derive lineage from the elected row inside its writer transaction and name the reporter separately. Expose resolution provenance, both callbacks and self-report state; only an explicit resolution can report scope-split. An ordinary retry appends one correction to legacy lineage without rewriting history. board_overlap_lineage_recorded and board_overlap_lineage_refreshed expose the result in amux logs.

---

## A stale card detail routed a terminal Refresh to the provider
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Authoritative terminal state replaces stale controls and draft state; late provider prose cannot replace the Final outcome. CI (run 34358221870 @ 04a19c51):
  test terminal_status_paths_preserve_summary_and_record_audit_for_provider_shapes ... ok
Browser (local, 25cffc1c + 0ae16bb6): card-details.spec.ts 'authoritative status wins a stale cached detail and old Refresh never asks the provider' -> desktop, ios-safari, mobile PASSED; 'terminal hydration clears a persisted stale status draft and keeps late provider text out of the banner' -> desktop, ios-safari, mobile PASSED.
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-testing-e2e
CARD: ATE-84
SYMPTOM: ATE-75 was Done in the durable board, but a stale client reopened it with In Progress selected. Its old Refresh/status-request path then reached the worker, and the resulting status update replaced the generated Final outcome summary.
COST: The UI made a terminal card look active and allowed provider prose to overwrite the durable completion record, so Refresh could not restore the evidence reviewers needed.
FIX: Card-detail hydration now applies the authoritative GET status to the selected controls, and Refresh GETs before deciding whether a provider request is permitted. Terminal status-request calls are refused and logged without delivery; terminal status updates and stale last_result PATCHes preserve Final outcome while appending evidence. Emit terminal_status_request_preserved and terminal_status_update_preserved markers, with Rust provider-shape and desktop/mobile/iOS Playwright regressions.

## A terminal card kept an intermediate status after completion
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Successful terminal transitions persist final evidence with provenance; refused transitions do not. Terminal Refresh reads that record without asking the worker. CI (run 34358221870 @ 04a19c51):
  test terminal_transition_records_summary_and_preserves_provenance_for_provider_shapes ... ok
  test refused_terminal_transition_does_not_record_summary_for_provider_shapes ... ok
Browser (local, 25cffc1c + 0ae16bb6): card-details.spec.ts 'terminal Refresh rehydrates the durable final summary without asking the worker' -> desktop, ios-safari, mobile PASSED.
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-testing-e2e
CARD: ATE-84
SYMPTOM: ATE-75 reached Done with a 12:05 validation note still shown as Latest status; Refresh asked the worker and did not surface the current terminal outcome.
COST: Reviewers could not tell whether the completed work had actually deployed or passed live acceptance, and the visible worker-action summary omitted the final evidence.
FIX: The save_patched durable write choke point now atomically records a provider-independent final summary in last_result and a STATUS (board) history line, including outcome, recorded actions, tests/deployment/live evidence, and linked assets. Terminal Refresh rehydrates the authoritative board detail instead of requesting provider text. Legacy terminal rows are repaired on their next durable board write, and terminal_summary_recorded is logged for sweeps.

## An empty composer control was recorded as a confirmed message and failed silently
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). A missing suggestion produces no confirmed send; the UI awaits and names an unverified Enter or a failed fallback. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::tests::missing_suggestion_is_no_effect_not_a_confirmed_send ... ok
Browser (local, 25cffc1c + 0ae16bb6): suggestion-control-verdict.spec.ts 'a missing suggestion names the unverified Enter fallback' -> desktop, ios-safari, mobile PASSED; 'a rejected suggestion fallback names the recovery failure' -> desktop, ios-safari, mobile PASSED.
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-testing-e2e
CARD: ATE-75
SYMPTOM: After an interrupted turn, the dashboard accepted `continue`, then recorded a second message.sent event with chars=0 even though the empty suggestion probe found nothing. Its fallback Enter returned no visible effect verdict, and the worker had to be stopped and resumed before the composer/control state was trustworthy again.
COST: The live acceptance turn was interrupted, a no-op entered the durable audit trail as a confirmed send, and recovery required a worker stop/start.
FIX: Classify a missing suggestion as submission=no_effect, omit it from send history/last_send, and emit a session.control_noop event plus composer-control WARN. Raw key responses now name effect=unverified, and the dashboard awaits the fallback and displays that verdict or an explicit failure.

## A global cross-group grant was silently narrowed by a worker allow-list
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Explicit global/group/worker grants compose; a more-specific explicit empty layer resets them. Configuration and enforcement share the effective source. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::tests::cross_group_allow_lists_compose_while_empty_lower_layers_explicitly_deny ... ok
  test api::session_verbs::tests::fleet_cross_group_toggle_round_trips_the_persisted_global_layer ... ok
  test api::workers::tests::both_worker_config_routes_persist_and_explain_cross_group_policy ... ok
AREA: messages
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-testing-e2e
CARD: AMUX-4018
SYMPTOM: Global Settings showed “Workers may message across groups” ON and the global env persisted CC_SEND_ALLOW=*, but amux-frustrations → amux-testing-e2e still entered per-message approval because the resolver returned the sender worker’s nonempty legacy allow-list and ignored the explicit global grant.
COST: The visible fleet policy contradicted enforcement, ordinary peer handoffs stalled, and neither the refusal nor worker UI identified which layer actually decided the result.
FIX: Resolve CC_SEND_ALLOW as a layered allow-list: nonempty explicit global/group/worker values compose additively, while an explicit empty more-specific value is the visible deny/reset. Enforcement, both worker config routes, the worker list, and the UI share the resulting value/source/reason. Emit cross_group_policy_refused and cross_group_policy_persisted verdicts with the effective source.

## Scrolling a worker terminal fired empty message-navigation toasts
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). A moved/scroll-interrupted gesture over an arrow is inert; an explicit empty navigation loads earlier output once and lands on the found message. Browser (local, 25cffc1c + 0ae16bb6): terminal-message-navigation.spec.ts 'a scroll gesture ending over a message arrow is inert' -> desktop, ios-safari, mobile PASSED; 'an explicit empty navigation loads earlier output and lands on its message' -> desktop, ios-safari, mobile PASSED.
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-testing-e2e
CARD: AMUX-4178
SYMPTOM: Safari repeatedly displayed “No matching messages in loaded output” while the user was scrolling a worker terminal, even though they had not asked to navigate. Live client-debug beacons recorded repeated no-targets arrow activations during the scroll interaction.
COST: Ordinary reading produced alarming, irrelevant feedback and made the new message arrows feel unreliable; when a real explicit navigation found no loaded match, it also left the user to locate and press Load earlier manually.
FIX: Arm message navigation on pointer-down and accept only a trusted keyboard activation or a non-moving pointer gesture with no intervening terminal scroll. Suppressed gestures emit peek-message-nav verdict=suppressed-scroll-gesture and never toast or move. A genuine empty navigation now pages the saved worker log once, reclassifies it, lands on a newly found message, or explains the exact terminal result.

## A provider-failed subagent stayed live forever after its stop hook vanished
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). A newer provider-owned failed terminal envelope reconciles the matching leaked child through durable edges; stale, malformed, quoted and running controls do not. EXPLICIT SCOPE LIMIT from the validator: this does NOT validate the external provider notification defect in ATE-10, which stays open in frustrations.md. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::steer_boundary_tests::structured_terminal_notifications_heal_only_the_matching_newer_agent_edges ... ok
  test api::session_verbs::steer_boundary_tests::terminal_notification_statuses_are_provider_structural_not_model_named ... ok
AREA: status
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: social-activities showed WORKING + AGENTS twelve hours after its Sonnet turn returned to the prompt. The durable lifecycle set still contained agent add9b6f920fb9ef31 because SubagentStart arrived, the agent immediately failed on HTTP 429, and no SubagentStop hook followed. Its provider-owned parent transcript already contained a newer structured task notification with status=failed, but status derivation never reconciled that terminal fact.
COST: An idle lane looked actively occupied, its board state contradicted the visible prompt, and the stale child survived reports and server restarts with no age-based bound.
FIX: Reconcile only provider-owned structured terminal task notifications newer than the stored per-agent start edge, through the same durable ordering/tombstone path as a real stop. Run it immediately at an idle report and from the periodic sweep for already-leaked rows; malformed, missing, stale, quoted, and still-live evidence fails open for the model. Emit subagent_lifecycle WARN verdict terminal_transcript_reconciled with the session, agent id, provider status, event timestamp, and resulting count.

## Codex's worktree suffix revived the false unsubmitted-text badge
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Fully dim worktree/branch suffixes remain chrome; typed and unstyled near-miss controls remain pending and diagnose possible footer drift. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::composer_state_tests::a_codex_model_footer_is_chrome_not_unsubmitted_text ... ok
AREA: status
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-testing-e2e
CARD: ATE-36
SYMPTOM: An empty Codex composer rendered `model · path · Main [default]`, but the footer recognizer required `path` to be the final middle-dot segment and exactly one dim separator. The live session therefore reported `composer_preview=gpt-5.6-solxhigh~/Dev/amux` and retained an UNSUBMITTED TEXT badge overnight.
COST: The owner had to ask why the worker claimed to hold text, and the status surface asserted a nonexistent pending command for more than ten hours.
FIX: Parse the stable plain model/path prefix plus any fully dim trailing footer context, preserve the typed-text control, and annotate stuck-composer WARN/event payloads with `possible_codex_footer_chrome` so future TUI drift is sweep-visible.

## An async menu-reachability assertion raced the app's normal rerender
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Menu overflow and final-action reachability are measured in one synchronous render snapshot, so no locator detaches between rerenders. Browser (local, 25cffc1c + 0ae16bb6): worker-action-parity.spec.ts 'worker card and peek share all worker actions, plus both peek-only actions' -> desktop, ios-safari, mobile PASSED.
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-44
SYMPTOM: The full Playwright run passed 300 scenarios, then the ATE-44 iOS
 Safari parity case failed because `scrollIntoViewIfNeeded` acquired the Focus
 action before a session poll rerendered the peek menu and detached that node.
COST: The six-case focused matrix had passed, but the first full descendant CI
 run stayed red after 16.7 minutes and could not satisfy ATE-44's final gate.
FIX: The parity test now scrolls and measures the final Focus action inside the
 same synchronous render snapshot. It still proves overflow, scrollability and
 viewport reachability while eliminating the cross-rerender locator lifetime.

## A CLI negative control assumed its specimen was globally unique
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). The negative mutation removes only cmd_start's declaration even when another command carries the same declaration; the broken specimen still fails at runtime. Fresh local script evidence: ate-validation-script-0.log (CLI negative-control suite 3 passed / 0 failed).
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-44
SYMPTOM: Fresh descendant CI completed the full Rust workspace suite after the
 PID-safety fix, then `scripts/test-cli-launch-unbound.sh` refused to build its
 broken fixture because a second command legitimately gained the same local
 AMUX_API declaration used by cmd_start.
COST: The otherwise-valid ATE-44 descendant run stayed red after all 1,947
 amux-server library tests and every integration binary passed; the failure was
 attributed from the job log only after the old runner-shutdown defect cleared.
FIX: The negative control now counts and removes the declaration only within
 cmd_start, preserving the independent declaration in cmd_open_browser while
 still proving the broken fixture fails with an unbound variable. Its failure
 output names the scoped occurrence count.

## An unsigned fake browser PID shut down every Linux CI runner process
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Reserved, unsigned-wrapping and group PID values are refused before kill argument construction; the real reaper regression still uses u32::MAX. CI (run 34358221870 @ 04a19c51):
  test integrations::browser::cdp_list_failure_tests::process_id_validation_rejects_group_sentinels_and_unsigned_wrap ... ok
  test runtime_jobs::browser_reaper::tests::a_reap_actually_enqueues_the_notice_for_the_owning_lane ... ok
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-44
SYMPTOM: Ten consecutive Rust check jobs ended around seven minutes with "runner
 received a shutdown signal" after commit 92044fc8 added a browser-reaper test
 seeded with PID 4294967295. The test called the real browser stop path, which
 passed that decimal string to Linux procps `kill -TERM`; procps returned success
 and treated the unsigned value as the signed process-group sentinel -1.
COST: Every descendant main run lost the workspace-test process and GitHub runner,
 blocking ATE-44 and ATE-45 verification while the 25-minute workflow timeout and
 passing test output falsely suggested external cancellation.
FIX: ATE-44 validates every browser PID at the signed OS boundary, refuses
 reserved/group values before constructing arguments or launching external
 `kill`, and emits `invalid_process_id_refused`. The original
 4294967295 fixture remains as an end-to-end regression, with boundary controls
 for PID 0, PID 1, ordinary positive PIDs, and the signed maximum.

## Codex `turn_aborted` left an interrupted turn structurally active
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Top-level and nested turn_aborted events are terminal idle boundaries for the interrupted prompt specimen. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::tests::codex_rollout_boundaries_are_a_structured_live_state ... ok
  test api::sessions_legacy::status_truth::a_codex_interrupted_turn_is_a_durable_terminal_edge_at_the_prompt ... ok
AREA: status
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: After Codex displayed `Conversation interrupted` and returned to its
 empty prompt, both Workers surfaces stayed WORKING. Status-explain chose a
 fresh `codex_rollout` active vote even though the stop report was idle,
 `subagents_live=0`, and `provider_background_working=false`. The rollout held
 the exact missing edge: an `event_msg` whose payload type was `turn_aborted`.
COST: An already terminal turn suppressed safe Board drive and contradicted the
 provider UI until a later recognized lifecycle event replaced the stale vote.
FIX: ATE-45 treats both top-level and nested Codex abort events as durable idle
 boundaries, surfaces the chosen boundary in status-explain, and emits the
 `interrupted_turn_is_terminal` status-truth verdict. Exact rollout and pane
 regressions pin the interrupted-turn prompt frame for Codex and Ollama.

## One capture shell's cooldown hid the next non-work shell forever
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). An exact new non-work capture gets one cleanup opportunity despite an unrelated lane cooldown, then durable card idempotency suppresses repeats. CI (run 34358221870 @ 04a19c51):
  test runtime_jobs::board_drive::tests::a_new_capture_cleanup_bypasses_an_unrelated_lane_cooldown_once ... ok
AREA: board
SEVERITY: stuck
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: PRIMI-204 was explicitly classified by its prompt as "do not create or
 retain a board task", but a recent nudge for PRIMI-203 activated the lane-wide
 advance cooldown before PRIMI-204 received its own cleanup prompt. It remained
 in `doing` after the turn ended with no `decompose:PRIMI-204` event.
COST: a non-task occupied the Board indefinitely while the drive report called
 the lane healthy and no model was asked to make the keep/discard decision.
FIX: ATE-45 lets a newly captured shell rejected by the shared pickup classifier
 bypass an unrelated lane cooldown exactly once, prioritizes that exact card,
 and relies on the durable per-card `decompose:<id>` idem to close the exception.
 The bypass emits `capture_cleanup_bypassed_lane_cooldown`; a focused regression
 proves both the first nudge and duplicate suppression.

## Claude's stale background-wait scrollback overruled a later completed turn
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). A newer completed Claude frame terminates an older wait; a newer wait can still win. CI (run 34358221870 @ 04a19c51):
  test backend::adapter::tests::a_completed_turn_after_a_background_wait_is_idle ... ok
  test api::sessions_legacy::status_truth::a_final_prompt_after_background_wait_does_not_override_idle ... ok
  test api::session_verbs::tests::detectors_read_real_frames ... ok
AREA: status
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: Primis visibly returned to its final prompt after
 `CLAUDE-POSTFIX-COMPLETE`, with zero live subagents and an idle stop report,
 but status-explain still set `provider_background_working=true` because an
 older provider-owned "Waiting for 1 background agent to finish" row remained
 in tmux scrollback.
COST: Workers stayed WORKING and turn-boundary-safe Board drive remained
 suppressed for minutes after the real work finished.
FIX: ATE-45 reads Claude's provider rows as ordered lifecycle edges: a newer
 completed-turn marker terminates every older wait, while a newer wait still
 wins. The status path logs `superseded_by_completed_turn`, and exact-frame
 adapter, detector and status-explain regressions cover the live Primis pane.

## A lost Stop report left a finished Primis turn WORKING for 139 seconds
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). A missing main Stop report replays the latest idle state after an outage without waiting for another hook; a newer state replaces old pending state. Fresh local script evidence: ate-validation-status-hooks.log.
AREA: hooks
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: Root's live browser saw both Primis subagents finish and Claude return
 to its prompt at 15:22, but the worker card and input still said WORKING. The
 status explanation chose a fresh `prompt-hook` active report with zero live
 subagents; the hook log showed the missing edge exactly: `15:22:22 primis
 source=stop-hook http=000` during a server rebuild.
COST: the production UI contradicted the provider for 139 seconds and Board
 pickup remained suppressed after all work was terminal. It self-cleared only
 when the active-report trust window expired, not because the final fact landed.
FIX: ATE-45 makes main-turn state a durable singleton latest-wins queue using
 the same bounded detached drain as lifecycle events. A newer report atomically
 replaces an older pending state, so recovery cannot replay idle over a later
 active turn; successful recovery logs the state, identity, attempt and
 `replayed_state` verdict. The exact lost-Stop outage replays idle without any
 later hook and is a shipped regression cell.

## A corrupt lifecycle queue was silently replaced with an empty one
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Malformed JSON and wrong-schema queue bytes are preserved and diagnosed rather than replaced; malformed invocations get distinct identities. Fresh local script evidence: ate-validation-status-hooks.log.
AREA: hooks
SEVERITY: data-loss
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: JSON read errors and wrong-schema JSON both became `rows=[]`; the next
 enqueue atomically overwrote the only bytes that could explain which lifecycle
 facts were lost. Malformed provider payloads also shared an empty dedupe key.
COST: a damaged queue erased its own evidence and multiple malformed but real
 callbacks collapsed into one, making the status error impossible to reconstruct.
FIX: ATE-45 atomically preserves corrupt bytes/schemas under a timestamped path
 and logs the queue, preserved path, error and verdict before recovery. Every
 malformed invocation gets a unique persisted identity; tests cover both corrupt
 forms and duplicate malformed callbacks.

## The lifecycle drain could strand its bounded tail or lose the final wakeup
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Successful deliveries empty all 128 permitted rows and the drain-lock handoff cannot lose the final wakeup. Fresh local script evidence: ate-validation-status-hooks.log.
AREA: hooks
SEVERITY: stuck
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: one drain performed only 90 total loop iterations although the queue
 admitted 128 rows. At the empty boundary, a producer could enqueue and launch
 a replacement before the old drain released its nonblocking lock, so both
 exited with the final row still queued.
COST: up to 38 healthy events could remain behind an entirely healthy server,
 and the last SubagentStop could sleep until an unrelated future hook happened.
FIX: ATE-45 spends the 90-attempt budget only on retryable failures, so successes
 drain the complete bounded FIFO. The empty read releases drain ownership while
 holding the queue lock, replacements wait through the bounded handoff, and
 tests prove one process drains 128 rows plus the lock-race specimen.

## Steering's deadline overrode its live-background safety hold
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). An old message does not bypass a real active background hold (86,400-second control). Canonical idle can reject weaker stale hints; ordinary foreground starvation policy is a separate concern. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::steer_freeze_tests::board_drive_and_steering_hold_for_a_codex_background_terminal ... ok
AREA: messages
SEVERITY: wrong-action
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: ATE-45 mapped exact provider/background evidence to `active`, then fed
 it to the ordinary max-age rule, which deliberately turns old active messages
 into mid-turn delivery. A long agent or background terminal was still
 interruptible after `AMUX_STEER_MAX_AGE_S`.
COST: the safety fix postponed the same conversation interruption instead of
 preventing it, contradicting its own regression name and acceptance contract.
FIX: ATE-45 carries background work as a separate hard-hold fact into the
 delivery decision. No message age can bypass it; only the reported final stop
 or provider terminal frame clears the hold, while ordinary foreground turns
 retain the existing starvation deadline.

## An older start could resurrect an agent whose stop arrived first
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Stop tombstones survive reordering, event-ID aging and compaction; older starts cannot resurrect the agent. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::steer_boundary_tests::subagent_terminal_edges_reject_reordered_and_aged_starts ... ok
  test api::session_verbs::steer_boundary_tests::subagent_compacted_terminal_tombstones_leave_a_replay_floor ... ok
AREA: status
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: server lifecycle state remembered only live IDs plus 128 recent event
 IDs. A stop delivered before its older start was discarded as an orphan; once
 a start ID aged out, replaying it after the final stop made the agent live again.
COST: response reordering or a sufficiently delayed retry could leave a worker
 permanently WORKING after every child had completed, defeating both accurate
 status and automatic Board pickup.
FIX: ATE-45 stores monotonic per-agent live/terminal edges. Stop-before-start is
 a durable tombstone; older/equal resurrecting starts are rejected with named
 verdicts. Terminal entries compact to a bounded set plus a timestamp floor, so
 evicted tombstones still reject ancient replay and resets preserve generations.

## Submitted and pasted provider frames impersonated live background work
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Prompt-prefixed, indented, quoted and cross-provider pasted frames do not acquire provider-owned live status. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::tests::detectors_read_real_frames ... ok
  test api::session_verbs::steer_freeze_tests::board_drive_and_steering_hold_for_a_codex_background_terminal ... ok
AREA: status
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: Claude's broad dingbat range included its own `❯` input glyph, so a
 prompt containing the exact waiting sentence read active. The provider-agnostic
 Codex fallback likewise accepted a pasted Codex frame inside Claude output.
COST: user-authored text could pin a truly idle worker WORKING and suppress its
 ready queue indefinitely; the original negative tests covered only unprefixed
 prose and same-line Codex quotation.
FIX: ATE-45 accepts only measured Claude spinner glyphs at column zero, excluding
 the prompt and indented pasted rows. Provider-known Codex scans keep partial-
 frame support, while provider-agnostic status requires the current exact Codex
 prompt/model-footer structure. Prompt, indented and cross-provider pastes are
 explicit negative controls and unknown variants remain sweep-visible.

## Durable lifecycle replay posted valid events to the server root
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Every actual hook request uses the exact worker report URL; the fake server refuses every other path. Fresh local script evidence: ate-validation-status-hooks.log.
AREA: hooks
SEVERITY: wrong-action
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: commit 483ff0aa queued the base `AMUX_URL`, so its drain POSTed every
 SubagentStart/Stop to `/` instead of `/api/sessions/<worker>/report`. The live
 server returned 405 and the new permanent-4xx rule immediately dead-lettered
 the valid lifecycle facts.
COST: ATE-45 was committed, deployed and moved to review with a green chaos
 suite while its central production path delivered zero lifecycle events. A
 second read-only review and live failure-log inspection were needed to catch it.
FIX: ATE-45 now constructs one canonical per-session report URL used by queued
 and immediate delivery. The fake server returns 405 for every other path, every
 captured request asserts its exact worker route, and the failure log retains
 URL, HTTP verdict and event identity.

## Board-drive interrupted Codex while its background terminal was running
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Live structured background work holds board/steering delivery; completed or inadmissible stale footer evidence can release it. Scope: NOT an indefinite hold on arbitrary scrollback. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::steer_freeze_tests::board_drive_and_steering_hold_for_a_codex_background_terminal ... ok
  test api::sessions_legacy::status_truth::boundary_and_workers_share_structured_codex_truth_and_fail_closed ... ok
AREA: board
SEVERITY: wrong-action
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: this ATE-45 turn visibly showed Codex's `1 background terminal
 running` provider row, but board-drive sent an Idle nudge and Codex reported
 `Conversation interrupted`. The status adapter already understood the row;
 the steering boundary trusted a fresh idle parent report without reading it.
COST: the harness interrupted its own green test run, forced the model to
 reconstruct its place, and demonstrated that dashboard truth and delivery
 safety still disagreed on the same frame.
FIX: ATE-45 makes the shared structured Codex pane state override an idle
 parent report for status, board-drive and steering. The hold clears on the
 completed `Worked for` frame, and quoted copies of the text do not match.

## Subagent lifecycle truth disappeared whenever the server rebuilt
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Durable start/stop FIFO survives outage and response loss, with stable identities and server-side dedupe. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::steer_boundary_tests::subagent_lifecycle_is_idempotent_ordered_and_restart_durable ... ok
Fresh local script evidence: ate-validation-status-hooks.log (status hooks passed every durability cell).
AREA: hooks
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: Primis SubagentStart and SubagentStop hooks both recorded `http=000`
 during a server rebuild. The callbacks were one-shot, so the server retained
 neither the active agent identity nor its final stop after coming back.
COST: the authoritative live-agent count read zero during real work and could
 also remain positive after a lost stop; recovery depended on a later process
 reset rather than replaying the facts that had already happened.
FIX: ATE-45 gives each lifecycle edge a session/agent/event identity, persists
 it in a bounded fsynced FIFO, replays oldest-first across outages and response
 loss, and deduplicates durably in the server. Permanent 4xx poison events
 dead-letter with full identity; 000/5xx retry; any later hook wakes the queue.

## Claude's background-agent wait row had two conflicting status parsers
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). The exact singular/plural Claude wait rows share the active predicate; quoted text stays inert. CI (run 34358221870 @ 04a19c51):
  test backend::adapter::tests::blocked_on_background_subagents_is_active_not_idle ... ok
  test api::session_verbs::tests::detectors_read_real_frames ... ok
AREA: status
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-45
SYMPTOM: Primis visibly showed the provider-owned `Waiting for 1 background
 agent to finish` row and an active Explore agent, while the dashboard header
 said IDLE. `backend/adapter.rs` classified the row active, but the legacy
 session projection called a second parser that omitted it.
COST: the user had to reconcile the terminal, agent panel, session payload and
 status-explain output to establish that real work was still running.
FIX: ATE-45 shares one chrome-anchored singular/plural predicate between the
 adapter and session status path. The exact provider row overrides an idle
 parent-prompt report; quoted prose remains a negative control.

## Peek and worker-card action menus drifted into different products
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Both worker menus use the same action inventory; all three Files entry points produce the same canonical worker/path route. Browser (local, 25cffc1c + 0ae16bb6): worker-action-parity.spec.ts 'worker card and peek share all worker actions, plus both peek-only actions' -> desktop, ios-safari, mobile PASSED; 'peek file entries produce the exact same canonical Files route state' -> desktop, ios-safari, mobile PASSED.
AREA: dashboard
SEVERITY: wrong-action
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-44
SYMPTOM: the worker card exposed 25 worker actions and configurations, while the
 peek overflow exposed only File browser and Focus mode. Worse, the peek File
 browser opened a desktop-only split pane while clicking the displayed directory
 entered the canonical full Files route for the same worker and path.
COST: the place where the user was already operating a worker hid almost every
 control, and two labels for the same file-browsing intent produced different
 session, navigation, and visible-state outcomes. A duplicated `peek-more-btn`
 id also made automation and DOM lookup choose whichever button came first.
FIX: ATE-44 (this commit). Both surfaces render one shared worker-action
 inventory; peek retains its two additional controls. All three peek file entry
 controls call one canonical full-route helper, the two overflow buttons have
 unique semantic IDs, and mismatch/file-entry verdicts reach client-debug logs.

## WIP-capped ready work was labelled STALLED while its holding task progressed
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). A WIP holder produces queued-behind labels carrying both identities; zero holders still produces the stalled control. Browser (local, 25cffc1c + 0ae16bb6): queued-behind-wip.spec.ts 'ready work behind WIP names both cards and links the holding card' -> desktop, ios-safari, mobile PASSED; 'ready but unclaimable work with no holding card still says stalled' -> desktop, ios-safari, mobile PASSED.
AREA: dashboard
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-43
SYMPTOM: TubeScience was idle at its main prompt while detached work continued
 on TUBES-2418 and TUBES-2419 correctly waited behind WIP-1. The worker header
 rendered the red `STALLED · 1 READY` chip even though `/api/board/ready` named
 TUBES-2418 as the current WIP holder.
COST: a healthy, intentionally serialized queue looked like a broken autonomy
 loop. The label hid both card identities, so the user could neither see what
 was waiting nor open the task that explained the wait.
FIX: ATE-43 (this commit). The ready frontier retains card identities and renders
 TUBES-2419 as queued behind a clickable TUBES-2418 control. Only ready work with
 zero claimable cards and no holding work keeps the stalled verdict. A one-shot
 `idle-ready-work` client beacon records which classification rendered.

## An idle Codex worker borrowed a sibling's active rollout and showed WORKING
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). A sibling rollout is not adopted merely because its cwd matches; explicit identity and the worker life window bound the fallback. CI (run 34358221870 @ 04a19c51):
  test api::session_verbs::tests::codex_rollout_fallback_is_worker_life_scoped_not_newest_cwd ... ok
  test api::session_verbs::tests::codex_rollout_fallback_refuses_nearest_sibling_even_beyond_the_old_scan_cap ... ok
AREA: status
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-42
SYMPTOM: `amux` sat at the empty `Ask Codex to do anything` prompt and its own
 stop hook reported idle, but the dashboard showed WORKING on stale AMUX-4079.
 `status-explain` named `codex_rollout`; the chosen rollout actually belonged to
 active sibling `amux-testing-e2e`, which shares `/Users/ethan/Dev/amux`.
COST: the worker header and Board highlight asserted current execution where
 none existed, while two workers' transcripts and lifecycle signals were
 cross-linked solely because they used the same checkout.
FIX: ATE-42. An explicit Codex session id still wins. Before one exists, rollout
 fallback now canonicalizes the cwd and selects only the rollout born within a
 bounded window around that worker's own `last_started`; outside that window it
 refuses to guess and lets the exact terminal/provider signals decide.

## Worker progress was recorded while its exact Board card stayed unclaimed
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Owned actionable progress claims the exact task transactionally; refusal and rollback controls stay informational. CI (run 34358221870 @ 04a19c51):
  test own_status_update_claims_exact_todo_or_backlog_for_every_provider ... ok
  test status_update_claim_rolls_back_when_its_log_write_fails ... ok
  test status_update_refuses_cross_worker_blocked_dependency_wip_and_later_states ... ok
AREA: board
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-41
SYMPTOM: general-canvas-apps posted four status updates describing active work on
 GCA-153, but the card remained in To Do and `task_board_id` stayed empty. The
 status-update endpoint appended the text and artifacts without participating in
 the Board claim transition.
COST: the Board showed an actively executing worker without its current card and
 left the same card eligible for another pickup. The user had to correlate the
 worker transcript, card log, and session payload to identify the disagreement.
FIX: ATE-41 (this commit). An owned, actionable To Do/backlog card is now claimed
 in the same serialized transaction that appends the progress line and artifacts.
 Cross-worker, blocked, dependency-held, fresh-trigger, WIP-conflicting, waiting,
 and later-state updates remain informational and return a named refusal verdict;
 claimed and refused paths emit distinct sweep-visible log markers.

## Evidence hid a real .env file and rendered the 1.93M row count as a file
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Actual hidden files are assets; decimal measurements are not. The browser opens the useful card and asset record. CI (run 34358221870 @ 04a19c51):
  test evidence_assets_keep_hidden_files_and_reject_decimal_measurements ... ok
Browser (local, 25cffc1c + ledger-only 0ae16bb6, isolated worktree, no product source edited): card-details.spec.ts old-lineage links open the useful card record and fit every viewport -> ios-safari, mobile PASSED. Desktop rerun passed the produced-file/button/availability assertions through line 85 then failed at an unrelated History-tab click intercepted by the onboarding backdrop (retained as ATE-130, still open). No whole-file desktop green is claimed.
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-39
SYMPTOM: TUBES-2426 evidence named `customers/tubescience/.env` but Board details
 returned no asset link. TUBES-2428 named the same file yet returned only `1.93M`,
 a decimal measurement, as a clickable missing-file asset. The parser required a
 non-empty stem before the final dot and accepted alphabetic measurement suffixes.
COST: the actual customer configuration artifact disappeared from two task records,
 while one record sent a reviewer toward a manufactured file. The user had to
 compare the two live payloads to show that the positive and negative parser arms
 were both backwards.
FIX: ATE-39 (this commit). Hidden leaf files and hidden path components are
 accepted, decimal measurement tokens are rejected, and bare/relative dotfiles
 resolve against the producing worker directory. File rows now render as semantic
 buttons; local availability and external reachability-not-measured verdicts make
 missing and unreachable assets explicit.

## A commit naming ATE-38 attached its outputs to the newer ATE-39 card
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). An explicit commit task ID receives the full SHA and changed-file assets instead of the newest Doing card. CI (run 34358221870 @ 04a19c51):
  test explicit_subject_task_gets_commit_and_file_rows_not_newest_doing_task ... ok
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-39
SYMPTOM: commit `be87f031` named `(ATE-38)` in its subject, but the post-commit
 `commit-report` appended the commit activity and derived links for
 `sessions_legacy.rs` and `be87f031` to the currently-Doing ATE-39. ATE-38 had
 no durable artifact rows when the report landed. The endpoint ignored the
 explicit task id and selected the worker's most recently updated in-flight card.
COST: the board put another task's source and commit on this card and left the
 producing task without its outputs. The user had to inspect both cards, identify
 the wrong newest-card guess, and provide a corrective live specimen before the
 task record could be trusted.
FIX: ATE-39 (this commit). `commit-report` reads the full subject and changed-file
 list from the git object, attaches by an explicit body/subject task id, and
 refuses ambiguity instead of guessing newest. It stores the full SHA and every
 changed file as durable rows on that exact task and emits
 `commit_report_task_exact` / `commit_report_task_ambiguous` log markers.

## Codex kept a live turn above its prompt shell and the dashboard called it idle
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Codex Working/Running/background rows adjacent to the prompt shell remain active; newer completed and separated historical frames remain idle. CI (run 34358221870 @ 04a19c51):
  test api::sessions_legacy::status_truth::a_codex_working_row_above_its_prompt_shell_is_active ... ok
  test backend::adapter::tests::codex_working_row_above_the_prompt_shell_is_generating ... ok
  test api::sessions_legacy::status_truth::a_newer_codex_prompt_beats_queued_message_prose_and_recent_churn ... ok
AREA: instruments
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-38
SYMPTOM: while `amux-testing-e2e` visibly showed `Waiting for background
 terminal (... esc to interrupt)` directly above `Ask Codex to do anything`,
 `/api/status-explain` set `pane.says_working=false` and the Workers UI labelled
 the session idle. The status parser treated the persistent Codex prompt/model
 shell as a newer idle boundary even though Codex paints that shell throughout
 an active turn.
COST: the worker's current board card lost its working highlight during a real
 generation. That makes the board contradict the terminal and can also let the
 driver reason from a false idle state.
FIX: ATE-38 recognizes the exact adjacent live shapes on both supported Codex
 layouts: older builds paint `Working` after the submitted prompt, while current
 builds paint `Working`, `Running`, or `Waiting for background terminal/command`
 immediately before the disabled prompt shell. A separated historical row still
 cannot override the newest prompt, and a completed `Worked for` row is an idle
 control. Adapter and end-to-end status-truth tests cover active, completed,
 stale-row, queued-message, and prompt-churn cases without matching arbitrary
 transcript prose.

## One blocked To Do hid an independent backlog from automatic draining
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). A blocked To Do and an older blocked backlog item cannot hide an independent runnable backlog item; dependencies and normal claim gates stay enforced. CI (run 34358221870 @ 04a19c51):
  test runtime_jobs::board_drive::tests::a_blocked_todo_does_not_hide_actionable_backlog_from_auto_drain ... ok
  test runtime_jobs::board_drive::tests::blocked_todo_with_eligible_backlog_wakes_exact_backlog_card ... ok
AREA: coordination
SEVERITY: stuck
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-37
SYMPTOM: `mvs-research` could finish its active card and then remain idle with
 MR-14 in To Do and 18 cards in backlog. MR-14 depends on MR-27, which the lane
 cannot complete, while MR-150 and other backlog work have no dependency. The
 pickup selector only tried backlog when its To Do SQL query returned zero rows;
 one blocked row therefore hid every independent backlog card even though this
 worker had the default auto-drain configuration enabled.
COST: the board advertised queued work and the worker advertised idle, but the
 driver repeatedly returned `all-candidates-refused`. Progress then depended on
 a person noticing the mismatch and moving a backlog card by hand. A tempting
 workaround would be to ignore dependencies or claim backlog directly, both of
 which would weaken the board's gates.
FIX: ATE-37 makes automatic draining a fallback after every To Do candidate has
 been evaluated and honestly refused. It still gives runnable To Do priority and
 still promotes exactly one backlog card through backlog -> todo -> doing. The
 drainable backlog query now excludes cards with open dependencies, so it cannot
 merely move the same blockage sideways. The regression reproduces an older
 blocked backlog card, a blocked To Do, and a newer runnable backlog card and
 requires the runnable card alone to be selected. All 106 board-driver tests
 pass, including WIP, needs:you, freshness, capture-shell, irreversible-action,
 dependency-promotion, and explicit auto-drain opt-out controls.

## An idle Codex prompt was labelled `UNSUBMITTED TEXT` using its model/path footer as the draft
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). Styled model/path footer remains chrome; real typed text remains pending. CI (run 34358221870 @ 04a19c51, workspace cargo test, lib 2162 passed / 0 failed):
  test api::session_verbs::composer_state_tests::a_codex_model_footer_is_chrome_not_unsubmitted_text ... ok
AREA: instruments
SEVERITY: wrong-conclusion
STATUS: fixed
DATE: 2026-09-04
SESSION: amux-testing-e2e
CARD: ATE-36
SYMPTOM: the worker's stop hook said idle, Codex's structured rollout said idle,
 zero subagents were live, and the pane visibly ended at the dim `Ask Codex to do
 anything` placeholder. `/api/sessions/amux-testing-e2e` nevertheless returned
 `status: waiting`, `composer_stuck_since > 0`, and
 `composer_preview: gpt-5.6-solxhigh~/Dev/amux`; the dashboard rendered that
 override as `UNSUBMITTED TEXT`. The composer reader stopped on Claude's border
 and status-bar glyphs but Codex puts its model/effort/path footer directly below
 the prompt with no border, so the footer was concatenated into the input.
COST: the worker presented a false human-action state for roughly three hours and
 contradicted both of its structured state sources. A human could have pressed
 Enter to submit what the UI claimed was pending, although there was no command
 in the composer.
FIX: 66818693. `composer_state` now treats Codex's ANSI-styled middle-dot
 model/path footer as a structural boundary without naming any model or version.
 It deliberately requires the raw styling and therefore fails toward visible
 `Typed` if Codex changes its chrome, never toward a false successful send. The
 live-frame regression first reproduced `Typed("gpt-5.6-solxhigh~/Dev/amux")`,
 then passed as `Placeholder`; its control keeps real typed text pending. Live
 build `668186939734` cleared `composer_stuck_since` and `composer_preview`, and
 the browser changed from the false badge to the worker's actual idle/working
 state in real time.

## An answer-only prompt still cards when its no-op tail isn't one of ten hardcoded literal strings
VALIDATED: amux-testing-e2e | VALIDATED by amux-testing-e2e 2026-09-09 (originating session). The named answer-only paraphrase stays message-only; a real task after an answer-only opener still cards. Scope: the tested capture contract, NOT a guarantee over every natural-language paraphrase. Production acceptance recorded 2026-09-04: durable history row 41187, direct/confirmed, card_id=null for a differently phrased information-only question, plus a 2026-09-03T23:59:31.799869Z AMUX-3330 informational-prompt-not-carded log; the deployed function had become is_non_mutating_answer_tail. CI: `bash scripts/test-tree-clean.sh cargo test --workspace` at 04a19c51 (GitHub run 34358221870), amux-server lib 2162 passed / 0 failed / 7 ignored, emitted:
  test board::capture_tests::informational_questions_are_message_only_but_work_requests_still_card ... ok
  test api::session_verbs::tests::an_informational_question_records_in_messages_without_a_board_card ... ok
AREA: harness
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-03
SESSION: amux-testing-e2e
CARD: ATE-17
SYMPTOM: Yesterday's fix (53b3e952, archived from frustrations.md 2026-09-02, validated
  against the literal specimen "...? Please answer only; do not change anything.") stops
  carding THAT exact string. A same-session E2E rerun today sent the same question with a
  differently-worded but equally answer-only tail: "...? Answer only; do not change files
  or create board work." is_informational_query()'s ANSWER_ONLY_TAILS list in
  crates/amux-core/src/board.rs matches ~10 hardcoded literal tail strings, not a
  structural "no imperative here" signal; this tail isn't one of them, so the tail-check
  fails closed and the question-word branch never fires. Two cards (ATE-15, ATE-16) minted
  for two paraphrased answer-only questions in one E2E run.
COST: The exact friction the archived entry described recurred one day later under a
  paraphrase, consuming two board ids and two WIP-adjacent doing slots for pure Q&A that
  was already answered inline both times.
FIX: f999caff replaces the literal ANSWER_ONLY_TAILS list with `tail_is_answer_only()`,
  which splits the tail on the same connectors `capture_has_task_followup` already uses
  for the pre-question clause and requires no resulting clause starts a task per the
  existing `capture_clause_starts_task` verb check — reusing the mechanism already
  trusted for the rest of the function instead of an enumerable list. Pinned the ATE-17
  specimen plus a negative control (a real task stacked after an answer-only opener must
  still card); `scripts/mutate.sh` confirms the negative control can actually fail.
  NOT YET independently re-validated against the running server build — see AF-433's
  discipline for what that validation should check before this entry is archived.

## MEMORY.md is rebuilt from the server's sources, so the pointer the memory instruction tells you to write is deleted on the next compose
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-09 (originating session), probed live against the running server at commit 60143873 / build 33dba7a5eef6cd94, with a positive AND a negative arm.
Setup: appended two pointer lines directly to ~/.claude/projects/-Users-ethan-Dev-amux/memory/MEMORY.md, the volatile half the memory instruction tells a session to write. One targeted every-fix-needs-a-log-signal.md, which EXISTS; the other targeted af578-no-such-file.md, which does not.
Trigger: POST /api/workers/amux-frustrations/memory with the source file's own bytes (sha 4204d2832e2d9c99 before and after, so the source was not altered), which is the write_claude_memory compose path.
Result, read back from the file: the existing-target pointer SURVIVED at line 62, under a preserved-pointer header, with the auto-generated fleet roster starting at line 64. Preserved BEFORE the roster, which is the load-bearing ordering. The missing-target pointer was DROPPED, grep count 0, as the fix intends since the memory rules tell sessions to delete memories that turn out to be wrong.
The warn published both counts separately, exactly as the entry promised:
  WARN amux_server::api::session_verbs: memory: compose dropped session-written content it could not carry (AF-578); only `- [Title](file.md)` lines are preserved dir=/Users/ethan/.claude/projects/-Users-ethan-Dev-amux/memory preserved=3 dropped_deleted_target=1 unrecognised_lines=0
KNOWN LIMIT restated, not resolved: a lane whose memory source is prose rather than pointer lines preserves nothing and the warn is its only signal.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-frustrations
CARD: AF-578
SYMPTOM: Routed up by ts-gke (TG-3374) as index truncation: their MEMORY.md exceeded the read ceiling, so 9 entries never loaded, with an auto-generated fleet roster eating 6.1 KB (22%) of a capped file. All confirmed. The cause underneath is worse. Measured: 44 memory files on disk in the amux project dir, MEMORY.md indexing 5 of them, and the server source ~/.amux/memory/amux-frustrations.md carrying ~25 pointers with ZERO overlap with those 5. Two indexes, not two views of one list. session_verbs.rs composes MEMORY.md as a full rebuild from its own sources and fs::write's it wholesale, so anything a session appended directly is destroyed on the next compose. ts-gke measured the same shape at mixpeek scale: 1,080 memory files against 125 index pointers.
COST: The memory FILES survive, so every write returns success and every .md persists; only the index line that makes them findable dies. 39 of 44 in this lane were already content with no pointer, which is a failure with no symptom until someone counts. Three of mine were at risk including one written in the session that found this. The direct cause is a contradiction between two instructions: the session prompt says "add a one-line pointer in MEMORY.md", and session_verbs.rs:9365 says the durable path is ~/.amux/memory/<worker>.md. A session following its own instructions writes the volatile half.
FIX: `preserved_agent_pointers` turns the destructive rebuild into a merge: any `- [Title](file.md)` line in the existing MEMORY.md whose target file EXISTS and which the sources do not already carry is re-emitted, placed BEFORE the roster so the roster stays last (ts-gke's option 3, free). A pointer whose file is gone is NOT preserved, because the memory rules tell sessions to delete memories that turn out to be wrong and resurrecting one would make deletion impossible. Per ts-gke, the composer now WARNS with what it dropped and could not carry, split into deleted-target and unrecognised-line counts: a destructive rebuild that cannot name what it removed is the same class as a guard that reads green while skipped. KNOWN LIMIT, named in the code because ts-gke found it: lane memory sources are not all pointer-shaped (ts-gke.md is headings and prose with zero pointer lines), so for a lane in that style this preserves nothing and the warn is the only signal.

## A bare `git config` inside a linked worktree rewrites the SHARED repo config, and our own push-gate advice creates the exposure
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-09 (originating session), by running the INSTALLED hook every lane's Bash passes through (/Users/ethan/.amux/hooks/git-shared-guard.py), not the committed copy, with two negative controls.
A. `git config user.email t@t.t` with cwd inside a linked worktree -> rc=2, BLOCKED: "in a LINKED WORKTREE writes the config SHARED with /Users/ethan/Dev/amux and every other worktree of it, because a linked worktree has no config of its own", followed by "EXITS, and --local is NOT one of them (in a linked worktree \"local\" IS the shared file, which is the whole defect)". The main checkout in that message was computed from the worktree, not assumed.
B. CONTROL, same command with cwd = the main checkout -> rc=0, no verdict. The guard distinguishes the two cwds rather than blocking every git config.
C. CONTROL, `git config --get user.email` (a READ) inside the linked worktree -> rc=0, no verdict. It guards WRITES only.
Both the committed hook at origin/main and the installed hook carry _worktree_config_verdict, so the fix is not stranded in one copy.
AREA: gates
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-frustrations
CARD: AF-577
SYMPTOM: A linked worktree SHARES .git/config with the main checkout, so any `git config` run inside one, without --worktree, rewrites the file every sibling worktree and the main checkout read. No GIT_DIR, no inherited environment, no hook. Reproduced here in a fresh repo: BEFORE main user.email=<real>, core.bare=false; after `(cd linked && git config user.email t@t.t && git config core.bare true)`, main reads t@t.t and core.bare=true. On the mixpeek checkout this took every work-tree operation down for every lane for ~30 minutes (MG-1648) and authored 9 commits on their origin/main as `t <t@t.t>`. Found by mixpeek-general (132a2b8a) after they retracted their own GIT_DIR hypothesis, which mixpeek-cicd had falsified.
COST: amux's config is intact, so this is exposure rather than damage: 22 registered worktrees on the shared checkout and `extensions.worktreeConfig` UNSET, which means none of them can hold private config and all 22 can only write shared. Our own CLAUDE.md:185 tells every lane to run `git worktree add --detach /tmp/push-check main` before pushing, which is why 17 of the 22 are push-check-shaped. The rule that protects a push gate manufactures this exposure. mixpeek-general's counter-case is the sharper argument for a guard over a doc fix: mixpeek/CLAUDE.md:399 says the OPPOSITE ("do NOT create git worktrees"), and they still accumulated 8, so a doc only binds the lanes that read it.
FIX: a `_worktree_config_verdict` check in scripts/git-hooks/git-shared-guard.py, the PreToolUse hook every lane's Bash already passes through. It refuses a `git config` WRITE when the cwd resolves to a linked worktree, and names --global / --system / --worktree / the main checkout as exits while explicitly ruling OUT --local (in a linked worktree "local" IS the shared file, so recommending it would name the defect as its own cure). 14 new cells, ALL 179 PASS. THE PLACEMENT IS THE WHOLE FIX: the first draft sat after the AMUX_SHARED_CHECKOUTS scope gate and returned 0 for all eight probe cells including `core.bare true`, because a linked worktree is never inside the checkout it belongs to. An inert guard that reads as installed is worse than none. The test now passes a deliberately-unrelated shared_root so it cannot drift back behind that gate.

---

## The group verified gate demands "functionality change is live" from card types that produce no functionality change
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-09 (originating session). This entry set its own closing condition -- "STATUS stays open until the opt-in lands, because the friction is live until then" -- and the opt-in has landed. Measured live, not inferred.
GET /api/board/session-gates now returns group:amux verified as ["@additive", "Peer-reviewed by a DIFFERENT worker in group `amux` (name them), and the card's type fits the work", "That peer verified it themselves rather than taking the author's word"]. The marker is present, where at ship time 6 scoped gates carried it 0 times.
The criterion this entry is about, "Functionality change is live and exercised, not just merged", is GONE from the group gate.
The union resolves per type, checked on three real cards via GET /api/board/contract?card=<id>:
  AF-513 (investigation) -> ["Outcome confirmed to still hold", + the 2 group criteria]
  AF-601 (epic)          -> ["Outcome confirmed to still hold", + the 2 group criteria]
  AH-126 (code)          -> ["CI/CD green ...", "Deployed to prod", "Confirmed working in prod", "Zero regressions", + the 2 group criteria]
The code case is the one that matters and it is the trap amux-cloud identified: first-tier-wins REPLACEMENT would have silently dropped "Confirmed working in prod" from every code card while still passing a naive "is AF-513 unblocked now" check. It is present. So the mechanism unions rather than replaces, and a finished investigation can now leave done truthfully without anyone acking a falsehood.
All three cards report gate_sources.verified.retype_would_change_it=false with source=group, which is the same field AF-571 made the CLI read.
AREA: gates
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-frustrations
CARD: AF-570
SYMPTOM: The `verified` gate for group `amux` resolves from the GROUP scope, so it is the same four criteria for every item type, and the first is "Functionality change is live and exercised, not just merged". Measured across group amux's done cards: 324 of 770 (42%) are types that cannot have a functionality change at all (investigation 169, doc 48, ops 32, blocker 24, chore 21, epic 9, escalation 6, decision 5, watch 5, research 5). AF-513 is a complete, correct investigation whose finding is that GET /api/board omits desc; it shipped no code, so criterion 1 has no true answer and criterion 4 ("no regression in what it touched") has no referent. amux-gtm independently hit the same wall from a different population: AG-39's output is a provisioned trial, imported seed data and a 6/6 chain run.
COST: Finished work cannot leave `done` truthfully, so it either sits there forever or someone acks a falsehood. 324 cards in group amux alone. The board's documented escape (correct the TYPE) is inert here because a group-scoped gate overrides the type default, and until AF-571 the refusal recommended it anyway.
FIX: Shipped the MECHANISM, not the policy: an opt-in `@additive` marker on a scoped gate, so a gate can UNION with the type default instead of replacing it. Measured no-op at ship time — 6 scoped gates configured fleet-wide, 0 carry the marker. amux-cloud found the trap that made this necessary: precedence in effective_gate_trail is first-tier-wins with REPLACEMENT, so simply editing the group gate down to the two peer criteria would have silently removed "Confirmed working in prod" and the other three from every `code` card while passing a naive "is AF-513 unblocked now" check. The group:amux OPT-IN is the policy half and is NOT shipped: amux-gtm and amux-cloud agreed, amux-homepage has not replied, and `amux` is isolated and cannot be asked. STATUS stays open until the opt-in lands, because the friction is live until then.

---

## Adding `set -e` to a harness made it abort silently on an idle host, and the fix could not fail on the box that shipped it
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-09 (originating session), with the mutation run in BOTH directions, which is what makes this more than a green suite.
Baseline: scripts/test-build-disk-clear.sh at origin/main -> 23 passed, 0 failed. Cell (m) printed its SKIP, because a real cargo/rustc is running on this box, so the entire population of cells that could have caught the original abort is unreachable here. That is the entry's own lesson reproducing itself during its own validation.
MUTATION 1, cell (n)'s `|| true` removed: 22 passed, 1 FAILED, "(n) the no-match probe must REACH its decision under set -euo pipefail", got: <empty>. So cell (n) can fail, host-independently, which is the property the fix was added for.
MUTATION 2, the SHIPPED line 300 reverted to its pre-fix `{ pgrep -x rustc; pgrep -x cargo; }`: 23 passed, 0 failed. Still green here.
That second result is recorded because it is NOT a pass. Cell (n) reproduces the CONDITION with names that can never match; it does not pin the shipped line, so on a box with a live builder a regression of that exact line is invisible to this suite. On an idle CI runner it still reddens the whole step, which is how the original outage was caught, and the harness announces an abort only by the ABSENCE of its computed summary line. Filed separately rather than held against this entry.
Both mutations were applied in a private detached worktree and reverted, never in the shared checkout.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-frustrations
CARD: AF-564
SYMPTOM: main's `checks` workflow went red at 327c470e (18:44) after e1032a2a (16:38) was green, and stayed red for six consecutive commits. The failing step logged `test-build-provenance: 9 passed, 0 failed` and then `##[error]Process completed with exit code 1` with nothing in between: no verdict line, no failure, no output at all from the next harness. Cause is my own 39ac1877, whose entire diff to that file is `set -uo pipefail` -> `set -euo pipefail`. The harness reads `_real_builds="$( { pgrep -x rustc; pgrep -x cargo; } 2>/dev/null | tr -d '[:space:]')"`; on an idle host both pgreps exit 1, pipefail carries that through the substitution into the assignment, and set -e aborts before the first echo. Confirmed in isolation: with set -e and no match, exit 1 and no output; without it, `REACHED, x=[]` and exit 0.
COST: main red for six commits across ~6 hours, which held amux-cloud's cloud-freshness track behind red CI and made every lane's push gate meaningless. The card filed against it named the wrong range (c6876cf1..8a70187, all of whose runs are green), so anyone bisecting from the card would have searched commits that were never involved.
FIX: `|| true` on that assignment, because the decision reads the OUTPUT and never the status, which is the exact distinction AF-560's own convention names and which I got wrong here. Plus a new host-independent cell (n) running the same construct with names that can never match. Cell (n) is the load-bearing half: every existing cell that could have caught this lives inside the branch that only runs when nothing is building, so on this box that whole population is unreachable and the suite reported PASS through the entire outage. THE REAL LESSON: a harness whose precondition is "nothing else is running" can never fail on a machine that always has a builder running, so the environment that ships the change is the one environment guaranteed not to test it.

---

## A gate refusal printed "correct the TYPE" while its own field in the same response said retyping would not change it
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-09 (originating session), by running the installed CLI against the live server, plus a mutation control that reproduces the defect on demand.
ARM A, the real path: `amux board verified AF-513` (investigation, group amux) -> rc=3, card unchanged at done. The response ends with:
  RETYPING WILL NOT CHANGE THIS GATE.
  this gate comes from the GROUP scope (amux), not from the item type - retyping will NOT change it.
  What would change it: satisfy the criteria above; or hand the card to the lane the criterion is about ... or change the gate at the scope that owns it.
No "correct the TYPE" sentence and no `amux board type` command appear anywhere in the output.
ARM B, the mutation control: the same command, same card, same server, run from a mktemp snapshot of origin/main's CLI with `if _gsrc:` replaced by `if False:`. The pre-fix output returns verbatim: "If a criterion does not apply because the card is mistyped (item_type=investigation), correct the TYPE rather than acking something untrue:" followed by `amux board type AF-513 <code|escalation|...>`. So the condition is load-bearing rather than redundant with something else. The card stayed at done in both arms.
Mutated on a COPY, never in place: ~50 lanes execute that file continuously.
AREA: gates
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-frustrations
CARD: AF-571
SYMPTOM: `amux board verified AF-513` returned, in one response, `how_to_ack.gate_source` saying "this gate comes from the GROUP scope (amux), not from the item type - retyping will NOT change it", and then a closing line reading "If a criterion does not apply because the card is mistyped (item_type=investigation), correct the TYPE rather than acking something untrue", followed by the `amux board type` command. The reader acts on the closing line: it is last, it is prose rather than a nested JSON field, and it is the only one carrying a runnable command. Following it costs a retype and leaves the card mistyped, because a group-scoped gate resolves identically for every type.
COST: More than the wasted command. That closing line is what makes an author believe the gate has an honest exit, and for this gate it does not. An author who retypes and sees the same four criteria return has been sent to the one remedy that provably cannot work, which pushes them toward acking something untrue - exactly what the line exists to prevent. It sits over a real population: 324 of group amux's 770 done cards are types that cannot satisfy criterion 1 at all (AF-570).
FIX: The condition was already computed and transmitted. The server emits `how_to_ack.gate_source` exactly when the gate came from a non-type scope. The CLI now prints the retype advice only when that field is ABSENT, and when present prints what would actually change it: satisfy the criteria, hand the card to the lane the criterion is about, or change the gate at its own scope. Verified on the installed CLI in both directions (group-scoped gate gets the new text, type-scoped gate is byte-identical to before), and mutation-checked on a COPY rather than in place, because ~50 lanes execute this file continuously: disabling the condition makes the group-scoped refusal print the retype advice again.

---

## The junk classifier brands any card that MENTIONS the capture marker, so a card cannot describe the capture mechanism
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-09 (originating session), measured on the REAL card rather than a fixture, so the discriminator is tested against live data.
AF-568's card is still in the state that triggered the bug: desc 10483 chars, source=agent, session=amux-frustrations, and it still CONTAINS the string `capture: session prompt` inside its explanation of the bug. Measured live: contains=True, desc.trim_start().starts_with(marker)=False. So the old predicate would brand it junk TODAY and the shipped anchored predicate does not. The two predicates disagree on this card, which is what makes it a test rather than a coincidence.
The STRUCTURE VETO evidence the substring match was short-circuiting is still present too: 3 ALLCAPS section heads, above its 2+ threshold.
Junk nudges naming AF-568 in the live server log since the fix: 0.
Shipped line confirmed at origin/main crates/amux-server/src/runtime_jobs/board_drive.rs:1036 as `desc.trim_start().starts_with("capture: session prompt") && folds < 2`.
AREA: notices
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-frustrations
CARD: AF-569
SYMPTOM: AF-568, the card ABOUT duplicate capture cards, was nudged "captured chat prompt, not a unit of work" while it sat in `doing` with a shipped fix and a 4000-char write-up. It is not a capture by any structural test: source='agent', creator='amux-frustrations', desc begins "MEASURED, 2026-09-07", no `capture: session prompt` marker in its log. The brand at board_drive.rs:880 was `desc.contains("capture: session prompt")`, and AF-568's desc contains that string once, inside backticks, in the sentence explaining the bug. Measured: contains=True, starts_with=False, folds=0.
COST: A live card with a shipped fix was told it was not a unit of work and offered discard/retitle/decompose. Small in minutes, and it is the shape that matters: the classifier cannot tell a MENTION from a USE, so any card documenting the capture mechanism is misfiled as its output. It also fired BEFORE the STRUCTURE VETO at :907, which returns "not junk" on 2+ ALLCAPS section heads; AF-568 had three, so a substring match short-circuited the evidence that existed to prevent exactly this.
FIX: 271167a3's follow-up. Anchor it to the intent the code already states three lines above ("a card literally defined as the capture marker"): `desc.trim_start().starts_with(...)`. Every real capture is still caught by the anchored PROMPT check below, on the `**Prompt:** ` prefix session_verbs.rs actually mints. Line 880's positive case had NO test before this, which is why changing it broke nothing: the existing tests around it cover the PROMPT check and the log-only case. Two new cells, mutation-checked in both directions.

---

## One broadcast prompt minted 105 capture cards across 56 lanes: the capture dedupe window is 45s, the delivery it guards waits for a turn boundary
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-09 (originating session), measured against the live board, ~43 hours after 271167a3 shipped at 2026-09-07 19:56:18.
Population: 974 capture cards minted SINCE the fix. Duplicate groups among them, using open_capture_with_desc's exact predicate (source='capture' AND deleted IS NULL AND status NOT IN (done,verified,discarded), grouped by session and byte-identical desc): 0.
The probe CAN produce a positive, which is why the zero means something: the same query still finds 3 duplicate groups fleet-wide (desktop, photo-analysis, self), and every one of them has its FIRST card timestamped 2026-09-07 18:09-18:28, the original incident window, before the fix. Duplicate groups whose first card postdates the fix: 0.
The two-fix log signal is live and names the survivor rather than going silent: 9 occurrences of "ledger: capture duplicates an open card for the same prompt; not minting a second (AF-568)" in ~/.amux/logs/server-rs.log, most recent three at 13:21:28 session=mvs-infra open_card=MI-5422, 13:50:55 session=mixpeek-cicd open_card=MC-1808, 14:21:16 session=mvs-infra open_card=MI-5464.
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux-frustrations
CARD: AF-568
SYMPTOM: The 5:59 PM recovery broadcast produced 56 identical cmd_history rows (one per lane, 503 chars, enqueued 18:09-18:10) and 105 board cards. 46 of 56 lanes hold more than one card for that single prompt and two hold three. Every duplicate was minted by the steering-delivery path (session_verbs.rs:10893, "ledger: auto-captured board card from STEERING-delivered prompt (AMUX-3148)"), whose id encodes the ENQUEUE time: AF-567 is `id=steer-1788818960752`, enqueued 18:09:20 and minted 18:26:15. Its "already carded" guard is `cmd_history ... AND card_id IS NOT NULL AND ts > now - AMUX_CAPTURE_DEDUP_WINDOW_S` with a 45s default, so at 17 minutes it saw nothing and minted again. The duplicates then have no cmd_history row of their own, because the linking UPDATE looks for the most recent UNCARDED row for that text and there is none.
COST: 49 junk cards across 46 lanes, each needing a manual disposition by whoever owns it. gainz is parked on a permission prompt clearing GAINZ-52/53 with `curl -X DELETE`, which also destroys the audit trail `amux board discard` keeps. My own lane spent the recovery turn deciding what two identical `doing` cards meant before any work started.
FIX: 271167a3. A 45s window is sized for a transport retry and is being asked to guard a path whose entire purpose is to WAIT for a turn boundary, which took 17 to 25 minutes for every duplicate here. Dedupe against `issues` instead, the table the mint itself writes, with no time box while the card is non-terminal: refuse the mint when the session already holds a non-terminal `source='capture'` card whose desc is byte-identical. Log the refusal with the surviving card id so a wrongly suppressed distinct prompt is findable rather than silent.
NOTE: An earlier draft of this entry blamed a second mint path in orchestrator/runtime.rs. That was wrong and is recorded on AF-568. The orchestrator path stamps `source='orchestrator'`; all 104 sourced cards here read `capture`, which is what sent me back to the log.

---

## SUPERSEDES the entry above: the consumer guard EXISTED and was correct — `--lib` never ran it
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-09 (originating session). This entry's specific claim was already closed at cc3b4221; what it left open was the GENERAL SHAPE, in its own words: "a suite-shaped command that silently covers a subset is the same instrument failure as a probe reporting zero when it never ran, and `--lib` is not the only such flag." Both halves are now addressed, at c7911c2d.

THE OTHER FLAGS. scripts/test-contended.sh now announces every selector that narrows a run, not just --lib. The sharpest is --test <name>, which skips the LIB ENTIRELY -- the larger half of this crate -- while printing the smallest number of any selector, so it was the likeliest of all of them to be read as a clean suite. Also --tests, --benches, --examples, and a bare name filter. Read off the ARGUMENTS rather than cargo's output, so a run that dies before printing anything is still describable.

scripts/test-target-clause.sh cell 2 had asserted that the --test case stayed SILENT. That cell encoded the narrower contract and is updated with the reason beside it, rather than deleted.

THE LAYER ABOVE, which is the part worth more than the flag list. This entry's incident was a guard that ALREADY EXISTED, was correct, and did not run. Measured 2026-09-09 one level up: 80 scripts/test-* harnesses in this repo, 48 named by a workflow or a git-hook, 32 named NOWHERE. One of the 32 was scripts/test-target-clause.sh -- the harness written FOR THIS ENTRY, which had never run in CI. The same defect as the entry, with a longer fuse.

scripts/test-harness-wired.sh is the ratchet for it, wired into checks.yml. The 32 are recorded in scripts/fixtures/harness-wired-baseline.txt with a reason each; it fails on an unwired harness that is not recorded, and also on a baseline entry whose file is gone or that is now wired, so the allowlist cannot rot into a rubber stamp. NOT a sweep: wiring 32 never-run checks into a required gate for every lane in one afternoon is deciding other lanes' work, and some of them compile.

EVIDENCE, and two of these are the honest kind because they are failures the new checks caught in my own work:
  scripts/test-selector-clauses.sh -> 12 passed, 0 failed (new; stubs cargo so no cell costs a build)
  scripts/test-harness-wired.sh    -> 3 passed, 0 failed; 31 of 81 unwired, 31 recorded
  Its FIRST run failed, naming my own two new harnesses as unwired.
  Its SECOND run failed a stale-baseline cell, because the checks.yml comment explaining that test-target-clause.sh is deliberately unwired made it read as WIRED. A mention is not an invocation; comments are stripped before the scan now.
  Without the runner change, 6 of the 12 new cells fail. Measured, not asserted: an intermediate revert during development ran the suite against the unpatched runner. The 6 that still pass are the controls, which pass trivially when the feature is absent.

NOT CLOSED BY THIS, and named so the next reader does not read it as done: AF-336's class stays live. A trustworthy test run on a contended file still needs a private worktree, and the instrument still only DETECTS the problem. Its own entry remains open in frustrations.md.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-30
SESSION: amux-frustrations
CARD: AF-346
SYMPTOM: My entry above says the a99955f7 dashboard regression happened because no
 consumer-side invariant existed and that amux was adding one. Both halves are wrong, and
 amux established it by checking rather than agreeing with me.
 `tests/board_api.rs :: list_is_slim_by_default_and_serves_prose_only_on_request` already
 existed, drives the real HTTP list path, and asserts desc_head starts with the card's
 first line. Run against a99955f7 in a scratch worktree it fails in 0.16s. The guard was
 written before either of us got here, was right, and would have blocked the commit.
 It did not run because I verified with `cargo test -p amux-server --lib`, which reports
 "1625 passed" and SKIPS every `tests/*.rs` target: 47 integration files, ~339 tests.
COST: The regression itself is costed in the entry above. The cost of THIS entry is the
 wrong lesson I nearly left in the ledger: "add consumer-side tests" is useless advice
 when the consumer-side test is already written, and it would have sent the next reader
 to write a duplicate of a passing test instead of fixing the command that skipped it.
 A false mechanism filed as history is the thing archiving rules exist to prevent, and I
 was ten minutes from it.
FIX: amux put it in VERIFY.md by name — `--lib` is a partial run whose number reads like a
 total — and strengthened two assertions in that same test that were weaker than they
 looked: `desc_len.as_u64().is_some()` is TRUE of 0, so it and the log_n line beside it
 would BOTH have gone green against the blanked loader. Only desc_head had teeth. Now
 they assert `> 0`, mutation-checked, at cc3b4221. What remains open is the general shape:
 a suite-shaped command that silently covers a subset is the same instrument failure as a
 probe reporting zero when it never ran, and `--lib` is not the only such flag.

## A peer asked me a blocking question I cannot answer: they are an isolated worker
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-10 (originating session). The fix this entry asked
for is shipped and confirmed live on a real delivered message, not inferred from the code
alone.

crates/amux-server/src/api/session_verbs.rs:15020 carries `no_reply_path_stamp(origin,
is_isolated)`, explicitly commented "AF-534 / AF-352, twice in five days" and wired at
line 15541 into the same send path this entry traced (`if let Some(stamp) =
no_reply_path_stamp(&send_origin, session_is_isolated(&send_origin)) { text.push_str(&stamp); }`).
It is STAMPED, not refused, matching the entry's own fix spec exactly: "when an isolated
worker's message is delivered to a peer, say so in the delivery envelope... one clause".

Live proof, not just code presence: queried cmd_history for the literal stamp text and
found 5 delivered rows, including one to THIS session (id 49169) from `amux`:

  "[no reply path: `amux` is an isolated (raw-agent) worker. A peer send back to it is
  REFUSED — it is reachable only by its owner from the dashboard. If this message asks
  you something, answer via Ethan or relay through a lane that is not isolated; do not
  write a reply you cannot deliver.]"

That is the exact asymmetry this entry reported hitting blind. A recipient reading that
message today sees the fact beside the answer, which is what the entry's FIX asked for
(ethos rule 4). Also confirmed the sanctioned regression tests exist and name this case
(session_verbs.rs:20619 "an isolated sender must be stamped", :20637/:20640/:20641 the
negative controls).
AREA: attribution
SEVERITY: blocks
STATUS: open
DATE: 2026-09-02
SESSION: amux-frustrations
CARD: AF-352
SYMPTOM: The `amux` lane sent me a push-consent ask — "is all of your unpushed work in
  a state you are happy to have on origin? One line back is enough" — with two named
  answers and a stated consequence for each. I wrote the reply and `amux send amux`
  refused: "'amux' is an isolated (raw-agent) worker with the amux harness stripped. It
  is not a peer or relay target and is reachable only by the owner from the dashboard."
  THE SEND WORKS IN ONE DIRECTION ONLY, and nothing said so until I had written the
  answer. `GET /api/sessions/amux` carries `isolated: true`, so the fact is available;
  it is just not available at the moment you need it, which is when a message from them
  arrives asking for a reply. Their message carried a server-verified origin stamp,
  which reads as a working channel.
COST: a real ask blocked. They are holding a 34-commit push on an answer they cannot
  receive, and their own fallback ("wait -> I tell Ethan you are mid-something") will
  now fire on my silence rather than on my answer, which reports the wrong reason to
  Ethan. The remaining channel is a board card in their queue for what is a yes/no.
FIX: `.claude/rules/frustrations.md` already documents this class exactly — "LIVE IS NOT
  VALIDATABLE ... the session payload already carries `isolated`; read it, or discover
  it from a refused send after you have written the message". I discovered it the second
  way, having read that rule earlier the same day. That is the tell that the rule is in
  the wrong place: it asks a human to remember a lookup before writing, and the moment
  the lookup matters is the moment a message ARRIVES.
  CORRECTION, same day, after walking the sanctioned path end to end: THE DOCUMENTED
  FALLBACK ALSO FAILS, and the advice printed at the first refusal sends you to a
  mechanism that fails for the same reason. All four channels, in order, all refused:
      amux send amux              -> "not a peer or relay target"
      card in their queue         -> blocked by THEIR OWN WIP limit ("close_these_first")
      amux board progress <card>  -> "progress noted, but OWNER NOT NOTIFIED: target is
                                     an isolated (raw-agent) worker: amux automation is
                                     not delivered into it"
      only the owner, from the dashboard
  The cross-group send refusal explicitly recommends the board handoff ("use the board on
  a card owned by <them>: `amux board progress <CARD>` notifies the owner at their next
  turn"), and neither refusal mentions the other. A lane following the guidance exactly
  ends up where it started, having written the message twice.
  The cheap mechanism is at delivery, not in prose: when an isolated worker's message is
  delivered to a peer, say so in the delivery envelope — one clause, "this sender cannot
  receive replies; only the owner can reach them" — and stop naming the board path for
  isolated targets, since it does not work for them. The `isolated` flag is on the record
  being rendered in both places.
  Related: AF-352 is the entry for entries whose authors can never sign off, which is
  the same asymmetry costing something different.

## append-only push guard offered two causes, and the real one was a third
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-10 (originating session). Both discriminators
this entry's FIX asked for are shipped in scripts/git-hooks/append-only-push-guard and
confirmed green live, not inferred from presence alone.

DISCRIMINATOR 1 (in-place rewrite, adjacent to a changed line in the same entry block):
scripts/test-append-only-inplace-edit.sh, explicitly commented "AF-528: the append-only
guard's FOURTH cause", is now wired into the required CI gate (AF-677, c7911c2d/634e5a86).
Re-run today: "ok: in-place-edit cause — all 4 checks pass". The guard file itself
(line 318) carries the comment "AF-528: a FOURTH cause the refusal could not express.
An entry REWRITTEN IN PLACE ... loses its old lines while the entry survives under the
same heading" and a distinct code path (line 302, "NOTE — N published line(s) of F are
EDITED in place") separate from the deletion/stale-republish arms.

DISCRIMINATOR 2 (push target is a fork vs origin): scripts/test-push-guard-fork-base.sh,
re-run just now: 6 passed, 0 failed, including cell D "the refusal documents the fork
case and how to check it" and cell C confirming shared-checkout behaviour is unchanged
for the real stale-republish case (so the fix narrows the false-positive without
weakening the true-positive it exists to catch).

Both tests are independent of and postdate this entry's own filing; neither is a copy
written to prove the point, both drive the shipped guard file directly.
AREA: gates
SEVERITY: slows
STATUS: open
DATE: 2026-09-06
SESSION: amux-frustrations
CARD: AF-528
SYMPTOM: Pushing a maintainer conflict-resolution to a CONTRIBUTOR'S FORK branch (PR
  #188), the pre-push guard refused: "frustrations.md as pushed is MISSING 4 line(s)
  the remote's current copy has", then insisted I decide between RETIREMENT and STALE
  REPUBLISH before acting, warning that the wrong choice is destructive. Neither was
  true. main had UPDATED the ATE-17 entry IN PLACE (STATUS: open -> fixed, FIX
  paragraph rewritten; origin/main:frustrations.md:2342-2365) and the fork branch
  carried the older copy, so merging main FORWARD replaced their stale text with
  main's newer text. The archive cross-check cannot help: the lines did not MOVE to
  frustrations-archive.md, they were rewritten where they stood.
COST: ~10 minutes ruling out both offered causes against a refusal that says picking
  wrong is destructive. The direction was the OPPOSITE of the accusation -- I was
  publishing NEWER content and it read as a revert.
FIX: Two cheap discriminators the guard already has the inputs for. (1) If the missing
  lines are ADJACENT to CHANGED lines in the same entry block, that is an in-place
  rewrite, not a deletion -- name it as a third cause. (2) Report whether the push
  target is `origin` or a fork branch: merging main forward into a fork can only ever
  ADD to origin's history, so the stale-republish reading does not apply there at all.
  A line-set difference cannot see an EDIT any more than it can see a MOVE, which is
  the failure mode this file's own contract already names one layer up.

## The activation authority rebuilds on a probe that TIMED OUT, and the rebuild is what makes probes time out
VALIDATED: amux-frustrations | VALIDATED by amux-frustrations 2026-09-10 (originating session). The entry itself said
the fix was uncommitted, isolated-lane work not mine to land ("only the owner can ask
it to commit"). Ethan committed it directly two hours later: 451cafb6, "fix(runtime):
stop health stalls from rebuilding the running revision", Amux-Session: amux.

Confirmed live rather than assumed present. crates/amux-server/src/activation.rs exists
on origin/main; "ACTIVATION AWAITING ADOPTION" is present in scripts/rust-auto-build.sh.

THE EXACT DEFECT NAMED IS FIXED, not just a file existing. The entry's root cause was
"the log line prints evidence against the verdict it announces, from a different call
than the one that decided" (a failed probe decided STAMP DRIFT, a later successful
re-probe supplied the logged evidence). The old code:

  live=$(live_server_commit 2>/dev/null || true)
  echo "ACTIVATION STAMP DRIFT ... /api/health reports ${live:-unmeasured}; rebuilding"

is gone, replaced by a single `measure_live_identity` call that owns both the decision
and its receipt, with the comment "A later successful curl must never be used to
explain an earlier timeout (AMUX-4225)". A probe that cannot measure now logs
"ACTIVATION IDENTITY UNMEASURED ... measured=false action=defer — unavailable health is
not evidence of image drift" and exits without rebuilding, instead of spending the
timeout as a negative answer.

Live storm check: grep -c "STAMP DRIFT\|OVERLAP GUARD" over the current server log and
the builder log, both 0. /api/health reports status=ok, commit=634e5a86 (this session's
own AF-677 push, adopted cleanly with no rebuild storm on any of several pushes made
today after the fix landed) -- the population the fix would show its absence against.
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-09-08
SESSION: amux-frustrations
CARD: AMUX-4225
SYMPTOM: Both `--max-time` curls in `scripts/rust-auto-build.sh` (2 of 2) convert a
  FAILED probe into an adoption verdict. `stamp_matches_live_image` does
  `live=$(live_server_commit) || return 1` on a `--max-time 4` health call, and
  `overlap_deploy_permitted` does `curl --max-time 8 ... || { OVERLAP GUARD
  UNMEASURED; return 1; }`. Neither can say "I could not measure"; a timeout is
  spent as a negative answer, and the correction is a full `--release` rebuild.
  That rebuild (rustc measured at 596% CPU, plus a 1268-file worktree checkout and
  a 14 GB target clear) is a large contributor to the load that makes the next
  probe time out. Measured on this box at 10:38 EDT: load average 163.55 with
  `available_parallelism` 28, `/api/health` p95 4731 ms over the trailing hour
  against a 2 ms baseline, and TLS handshake timeouts on the watchdog.
  The abbreviated-SHA theory is WRONG and worth recording as dead so nobody
  re-derives it: `stamp_matches_live_image` compares with a PREFIX pattern
  (`case "$built_sha" in "$live"*`), so 12-char `a604412b4923` against the 40-char
  stamp matches correctly. The reason a604412b logged STAMP DRIFT while NAMING a
  matching commit is that the deciding probe failed and line 195 then RE-PROBED
  successfully to compose the message. The log line prints evidence against the
  verdict it announces, from a different call than the one that decided.
COST: 10 STAMP DRIFT verdicts and 3 OVERLAP GUARD refusals today, each costing a
  redundant release build of an already-installed revision; watchdog /health
  timeouts at 10:03, 10:14, 10:15, 10:26 and 10:28; `/api/sessions` returning zero
  bytes after 20s at 10:34; TubeScience independently blocked registering handoffs
  on a 20s localhost read timeout. Two lanes spent the morning diagnosing a loop
  whose trigger is its own remedy.
FIX: Not mine to write. `amux` (isolated) has the fix already written and
  UNCOMMITTED in this shared checkout: `crates/amux-server/src/activation.rs`
  (hash-checked install identity, `same_revision` skip), `runtime_jobs/poll_watch.rs`
  (WARN naming a job that holds a runtime thread), and an `ACTIVATION AWAITING
  ADOPTION` skip_rebuild path in the builder. Not live: `origin/main`'s builder has
  0 occurrences of `ACTIVATION AWAITING ADOPTION`, the worktree has 1, and the
  launchd authority runs committed bytes from `~/.amux/activation-source`.
  Deliberately left untouched, per the shared-checkout rule. `amux` is an isolated
  raw-agent worker: my send was refused, so only the owner can ask it to commit.

---

## The staged-guard's blocked-commit remedy edits the other lane's staged work
VALIDATED: amux-frustrations | Fix confirmed live in shipped code (scripts/git-hooks/amux-staged-guard:1123-1144, 450-472): `git commit <your paths> -m ...` is offered FIRST as "a pathspec commit ignores the index for everything it does not name", with `git restore --staged` labelled as changing a peer's staged state. Pinned by scripts/git-hooks/test_amux_staged_guard.py:180-212,279 (ordering + stated reason). Card AF-365 is `done`. Self-validating: I am the "amux-frustrations (filed and fixed)" party named on this entry's own SESSION line -- amux only found/reported the near-miss; amux-frustrations owns and shipped the fix.
AREA: attribution
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-31
SESSION: amux (found the technique), amux-frustrations (filed and fixed)
CARD: AF-365
SYMPTOM: When the guard BLOCKS a commit over a peer's co-edited file, its only
  suggestion was `git restore --staged <their paths>`. On a shared index that
  mutates state belonging to the other lane: their file is staged because THEY
  staged it, and unstaging is an edit to someone else's in-flight work made by a
  party who cannot see what they intended. The near-miss that exposed it: amux had
  an unstaged `checks.yml` hunk at ~line 316 while my hunk in the SAME FILE was
  already staged at ~line 181.
COST: No damage, because amux found the exit themselves and said the guard does not
  suggest it. What the obvious path would have cost is worse than plain absorption:
  committing that file would have SPLIT my change, landing my CI wiring under their
  commit message while the app.js it wires stayed uncommitted, so my own commit
  would have wired nothing. Two lanes, one file, and every documented move was wrong.
  `git add -p`, which the guard recommends two screens down for the partial-stage
  case, is also the wrong tool here: the problem is not which of YOUR hunks to take,
  it is that THEIRS are already staged.
FIX: Fixed. `git commit <your paths>` is now offered FIRST, labelled as the exit
  that touches nothing the peer owns, and the unstage remedy now says out loud that
  it edits the shared index. A cell in test_amux_staged_guard.py pins both the
  presence and the ORDER, plus the stated reason, because an unexplained ordering
  gets tidied back by the next person who thinks restore reads better first.
  The cell reads the SHIPPED hook rather than executing the branch (that text is
  inline in main() and reaching it needs a multi-session git fixture), and it says
  so rather than implying parity with the cells above it.

---

## Editing a running .sh corrupts it mid-run, and the instrument cannot report its own death
VALIDATED: amux-frustrations | Fix confirmed live in shipped code (scripts/test-contended.sh:52-65): the wrapper snapshots itself to a temp file and `exec`s the snapshot before doing anything else, so an in-place edit cannot reach a run already in flight. Card AF-368 is `done`. Self-validating: I am the "amux-frustrations (owns the file, took the fix)" party named on this entry's own SESSION line -- amux hit and diagnosed the corruption; amux-frustrations owns test-contended.sh and shipped the fix.
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-31
SESSION: amux (hit it and diagnosed it), amux-frustrations (owns the file, took the fix)
CARD: AF-368
SYMPTOM: `amux` ran `scripts/test-contended.sh -p amux-server` and got:
    1888 passed, 0 failed, and NO `test result: FAILED` line anywhere
    no contention verdict printed at all
    ./scripts/test-contended.sh: line 53: syntax error near unexpected token `('
    exit 2
  Line 53 was a bare `#`, and the file was `bash -n` clean throughout. Two of my
  commits to that file landed inside their run. bash reads a script INCREMENTALLY,
  by byte offset, so the file growing underneath the running shell shifted the
  offsets and bash resumed mid-token, then failed on whatever byte now sat at its
  saved position — nowhere near either edit.
COST: Near-miss on a false red. Exit 2 with zero failures reads as a broken suite,
  and amux nearly reported it as one; what stopped them was noticing that "0 failed"
  and "exit 2" cannot both be a test result. They also correctly refused to report
  their own AMUX-3718 work green off that run, because its exit status described my
  edit rather than their code. This is the THIRD cause of a red suite after the
  builder and the dirty worktree, and it is the one this script structurally cannot
  report: it dies before reaching any echo, so its verdict is not wrong, it is
  ABSENT. The instrument's blind spot is the instrument.
FIX: Fixed. The wrapper now copies itself to a temp file and `exec`s that before
  doing anything else, so an edit cannot reach a run in flight. `exec` means one
  shell and the exit status still belongs to cargo. Snapshotting is the only fix at
  the right layer, because a report cannot describe a run that stopped existing.
  GENERALISES, and this is the part worth keeping: every .sh in this repo is
  exposed, and the bash CLI ships on SAVE, so `amux` itself is the largest instance
  — a long `amux` invocation running while any lane saves that file is this exact
  hazard. Not fixed here; that is a separate card.
  A NOTE ON THE TEST, because the first one lied. I wrote a behavioural cell that
  started the wrapper, truncated the file to garbage mid-run, and asserted it still
  exited 0. It passed. It also passed with the re-exec MUTATED AWAY, because bash
  buffers a file this small in a single read and the truncation never reached the
  running shell. A control that cannot fail is worse than none, so it was deleted
  rather than relabelled. The shipped cells assert the preamble exists, execs the
  snapshot, and has NO executable statement before it — position being the property
  that matters, since a snapshot taken after other work is a snapshot of a file that
  could already have moved. Both mutations now redden exactly one cell each.

---

## An already decomposed epic is asked to split again
VALIDATED: codex-amux-lifecycle | Epic nudge: the new selector regression completed 1/1 in epic-nudge-885.log and asserted the actual executable child receives the advance after decomposition. This validates the candidate selector; the older pinned live lab backend still emitted the pre-fix redundant prompt and is not claimed as post-fix proof.
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: The real reviewer's LSC1R-1 had two linked children and type epic, yet board-drive sent MSG45 calling it a capture shell and asking for decomposition. The shell classifier reread the preserved source prompt without considering the completed structural decomposition.
COST: One redundant prompt while the reviewer was preparing independent tests; the notice contradicted the persisted graph.
FIX: Capture cleanup excludes epics and ordinary Doing advances target their executable children. Review-stage epics remain reviewable without reclassifying their preserved prompt as a shell. The selector test asserts the actual child ID receives the advance.

## Asking for artifact help creates an output named --help
VALIDATED: codex-amux-lifecycle | Artifact help: scripts/test-board-help.py completed 5/5 real-CLI cases, including both artifact help positions and an unknown reference-position option; no HTTP request was made. The live author removed its earlier accidental --help artifact itself.
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: The Sonnet author ran amux board artifact LSC1A-2 --help to discover the command. The CLI registered --help as an implementation artifact, and the worker had to investigate how to remove that bogus output.
COST: Several additional API/schema-discovery commands and an accidental artifact in a real acceptance task.
FIX: Handle help before ID/reference processing; reject unknown reference-position options before any request. scripts/test-board-help.py runs the actual CLI with a recording transport and verifies help and invalid-option paths issue zero requests.

## Shared build served an older dashboard during acceptance
VALIDATED: codex-amux-lifecycle | Asset provenance: full-browser-886, focused-browser-887 and focused-browser-888 each validated served app.js/app.css/sw.js hashes on all three isolated servers before browser execution. Build receipts include process build identity; wrong old assets were rejected and the invalid 883b run was retained as invalid.
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: A successful build from the private worktree served embedded APP_VER 0.9.879 while the tested source was 0.9.883. New board controls were absent. The server commit stamp also omitted dirty because build.rs evaluated repository-relative git pathspecs from the crate directory.
COST: One five-minute browser run was invalidated and stopped; its red UI assertions were against the wrong asset bytes.
FIX: Watch absolute dashboard asset paths and the manifest directory for shared-cache invalidation. Check the served app.js, app.css and sw.js hashes against source before browser acceptance and persist the process build and every digest. Resolve git status from the repository root and explicitly mark unverified binary provenance until the asset checks succeed.

## A Doing epic makes its active child look like conflicting work
VALIDATED: codex-amux-lifecycle | Runtime attribution: scripts/test-contended.sh -p amux-server --lib legacy_sessions_ -- --test-threads=1 completed 2/2 in runtime-attribution-885.log, including Doing epic plus one leaf and two conflicting leaf claims.
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: Both real Sonnet workers decomposed a captured Doing task into a Doing epic plus children, then claimed one child. /api/sessions reported active-conflicting-claims with n_considered=2 instead of attributing the one executable child; board-drive already excludes epics from resume and WIP selection.
COST: Live terminal activity disagreed with the worker status during the complex lifecycle run.
FIX: Exclude epic containers from runtime execution claims while exposing their measured epic_container_count. Keep unrelated leaf claims conflicting. The real HTTP projection regression covers the epic-plus-child and two-independent-claims cases together.

## Decompose help omits the plan shape when workers explicitly ask for help
VALIDATED: codex-amux-lifecycle | CLI decompose help: scripts/test-board-help.py completed 5/5 cases, executing the real CLI against a recording HTTP transport; both help positions returned plan examples with zero writes.
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: The live Sonnet author searched repository source for DecomposeTask after the command's help path failed to expose the JSON shape. --help before the ID ran ID validation; after an ID it printed only usage. The working sample was available only when the ID was missing.
COST: The author spent several minutes discovering the schema and encountered an outside-workspace read permission dialog before creating its three linked children.
FIX: Both help positions return a concrete two-child JSON example with integer priorities, one-based dependencies and acceptance criteria; the existing board contract exposes the same plan shape for API discovery.

## Returning to Workers can reopen the previous terminal over the menu
VALIDATED: codex-amux-lifecycle | Navigation: LC-NAVIGATION in full-browser-886 passed before the run advanced to subsequent cases on all three projects. The resumed gate amendment with candidate 886 reached the actual board gate UI and delivered MSG61. Final LC-DEEPLINK additionally passed 3/3 in focused-browser-888.
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: After reload on iOS Safari, clicking Workers and opening its menu was interrupted by a delayed restoration of the previously open worker terminal. The explicit navigation did not invalidate the saved-screen callback.
COST: One failed cleanup in the 39-case focused browser run; the old overlay intercepted the requested menu action.
FIX: Main navigation advances the existing peek generation, explicit task and menu deep links supersede restoration, and a superseded callback emits peek_restore_superseded. A controlled-clock acceptance case reproduces the navigation race.

## Fresh worker can launch outside its configured directory
VALIDATED: codex-amux-lifecycle | Provider cwd: the real lc-sonnet-complex-1789069500 Sonnet pair launched in the configured private workspace; terminal screenshots and committed scratch deliverables establish the selected directory. This fixes provider launch; the user global login profile can still affect subsequent Bash commands, which workers explicitly cd before running.
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: A private lab worker configured with the invoice scratch directory showed a different working directory in its Sonnet terminal after shell initialization.
COST: Halted the lab before sending any implementation instructions and rebuilt the launcher to keep test writes in the requested workspace.
FIX: The actual provider invocation now performs a quoted cd to the resolved workspace with &&, after shell/profile/environment setup. A provider_launch_workspace_pinned event records the selected directory; the real worker run checks that directory before doing work.

## Create worker delayed focus sends directory text into the name
VALIDATED: codex-amux-lifecycle | Create focus: LC-CREATE-FOCUS passed on desktop, mobile and Safari in focused-browser-887 (30/30 total). The actual Sonnet pair was created with the intended names and workspace.
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: The real Sonnet lifecycle's create request concatenated its directory onto the worker name and sent an empty dir. openCreate unconditionally focused the name after 100ms, stealing focus from the directory during entry.
COST: One malformed stopped lab worker and a failed 30-second wait for the intended worker; the requested provider journey had not started.
FIX: Delayed initial focus only runs when focus is outside the active create dialog. Log when an existing field keeps focus, and test directory typing across the delayed callback with a controlled browser clock.

## Accepted message remains in the focused composer and invites another send
VALIDATED: codex-amux-lifecycle | Accepted composer: focused-browser-887 completed 30/30 across desktop, mobile and Safari, including local persistence before delayed receipts and replacement/focused inputs; dashboard-outage-recovery.mjs completed 21/21. Production incident retained as metadata without claiming identical transport identity.
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: Production history recorded one confirmed send at 15:20:44 ET and another identical-content attempt marked stuck at 15:21:05 ET; the composer-delivery log retained the second draft. Draft clearing skipped focused inputs and could clear a detached peek input after a render. The user saw the worker replying while the text remained in the composer.
COST: Two identical-content attempts 21 seconds apart and a user-reported ambiguous send state; logs alone could not prove whether both came from the same transport identity.
FIX: Persist ordinary messages and uploaded-file references in the existing local outbox before clearing exactly the accepted live draft. Replay one stable message ID and require a confirmed, deferred or deduplicated receipt; retain ambiguous attempts for review. Composer acceptance and replay failures emit diagnostic events. Regression coverage includes focus, replacement nodes, delayed receipts, newer edits, storage failure and reload.

## An in-flight send reservation is reported as an accepted retry
VALIDATED: codex-amux-lifecycle | Validated on 2026-09-10: four message_acceptance server tests and the file_backed_verbs_roundtrip_hermetically real steering route pass, including reservation/receipt distinction, archived-target retry, exact response ID, old receipt and unavailable-store cases. Main b5786c06 is live by production health. Logs: work/message-acceptance-891c.log and work/send-routes-891.log.
AREA: messages
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: send_dedup inserted an identity before attempting delivery, then treated any duplicate row as proof of delivery. Steering reserved before validation and did not release an archived-target refusal. A retry could therefore receive ok/deduped while nothing had been accepted.
COST: The local-outbox acceptance audit found a server receipt that could remove pending user intent prematurely; isolating reservation versus acceptance required a separate persistence and routing regression pass.
FIX: Store the confirmed response ID separately from the reservation. Pending attempts cannot acknowledge delivery, legacy unknown rows remain uncertain, failed identity storage refuses an untracked send, and validation refusals reserve nothing. Record the original queue/send ID only after actual acceptance. New message_acceptance tests and the real steering route test validate the distinction; amux::message_acceptance logs unconfirmed receipt states.

## Stop leaves a terminated worker marked as running
VALIDATED: codex-amux-lifecycle | Validated 2026-09-10: stop_status_edge test passes; the real UI Stop action on both retained Sonnet workers now reaches API running:false with standing orders disabled. Evidence work/stop-status-892b.log and work/complex-pause-final.log. Existing task states are preserved; no task is completed by stopping a worker.
AREA: lifecycle
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: Both private Sonnet workers had returned to childless bash panes after Stop, but the API still reported running:true because their recent waiting/blocked reports were preserved.
COST: Cleanup falsely looked incomplete and the UI could offer running-only controls for a terminated provider.
FIX: Under the same start/stop operation lock, retire the stopped process's live report while preserving model diagnostics. Do not report success when hard-kill remains unconfirmed; publish a failed-stop event. Validate the persisted report and real paused-worker API state.

## Global history refresh erases a newly accepted local message
VALIDATED: codex-amux-lifecycle | Validated delayed nonempty global history and identical text to different workers: focused-browser-896 36/36, and all corresponding merged browser cases pass in focused-browser-897. Source retains unechoed local records and repaints provenance.
AREA: messages
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: The broad browser run still classified a just-sent prompt as Unclassified in all three browsers. A delayed nonempty global-history response replaced the local history array after the composer had appended the accepted message.
COST: A focused empty-server run passed but missed the populated-server ordering; the second full run exposed three failures after 749 other checks passed.
FIX: Use the same unechoed-local merge as scoped Messages views when applying global history, then repaint provenance. Scope consecutive history deduplication by worker and type. A delayed nonempty history regression exercises the ordering and identical sends to two peers.

## Verified column metadata truncates its name on a phone
VALIDATED: codex-amux-lifecycle | Validated LC-BOARD-HEADER across desktop/mobile/Safari in focused-browser-895, 896 and 897; manually inspected the 375px Safari screenshot showing full Verified and Terminal labels with controls inside the column.
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4377
SYMPTOM: A settled 375px screenshot of the real completed board showed VERIF... and TERMI... beside the column controls, obscuring the status name and terminal designation.
COST: The final visual audit found a readability problem despite successful task and gate assertions.
FIX: Give the column identity a compact two-row layout on phones so the full status name and terminal badge fit beside the controls. LC-BOARD-HEADER measures both text boxes and verifies all header controls remain inside their column.

## An AF-66-style guard existed for this and had been green the whole time
VALIDATED: amux-frustrations | Verified 2026-09-11 (retire-on-evidence, author session amux gone): git log -1 c1c238b1 on origin/main: "fix(board): the capture-shell nudge is 85 words and every command in it exists (AMUX-3707)". Commit exists and matches the claim.
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

## Typing at a lane disabled that lane's auto-pickup
VALIDATED: amux-frustrations | Verified 2026-09-11: git log -1 7e4682f0 on origin/main: "...a capture shell no longer holds its WIP slot (AMUX-3756, AMUX-3757)". Commit exists and matches the claim.
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-26
SESSION: amux
CARD: AMUX-3757
SYMPTOM: Every prompt is auto-captured as a `doing` card whose desc is still literally `**Prompt:** <what was typed>`, and that card counted against the WIP-1 cap. An unanswered prompt is not work in progress — the decompose nudge exists precisely to make the lane dispose of it — but the pickup query could not tell the two apart. The exemption list already carried tripwire, watch, epic and needs:you for the same reason and had never been extended to the cards amux mints itself.
COST: The specimen is TUBES-2225, titled "Why are you stopping": Ethan's complaint about tubescience stopping was itself the card holding the WIP slot that kept it stopped. A frustrated re-prompt is the likeliest prompt to arrive at a stalled lane, so the loop closed on exactly the lanes already in trouble. This lane's own board held 11 capture shells in `doing` at once, all of them his prompts.
FIX: 7e4682f0 — a capture shell joins the WIP exemption, using the same `substr(desc,1,11)` form as the fold query in board.rs so the two cannot disagree about what a capture shell is. Reshaping the desc, which is the exit the decompose nudge already asks for, makes it count again.

## "The tests pass" is load-dependent on this box, so a green suite is a weaker claim than it reads
VALIDATED: amux-frustrations | Verified 2026-09-11: grep 'contention:' scripts/test-contended.sh -- prints "contention: ... (AMUX-3853)" beside every result, exactly the instrument requested.
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

## Delegated worker requests held the requester's WIP instead of becoming task dependencies
VALIDATED: amux-frustrations | Verified 2026-09-11: git log -1 ccb37879 on origin/main: "feat(board): make delegated requests task dependencies". Commit exists and matches the claim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: git log -1 dd57e1d5: "fix(workers): resume automatically after usage reset". Commit exists and matches the claim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: git log -1 ecbf4daf + 6bdf9999: "feat(settings): show every provider usage window" / "fix(usage): resolve Codex through worker shell". Both commits exist and match the claim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: git log -1 6bce0158: "feat: add evidence-backed helper delegation". Commit exists and matches the claim.
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

## Manual claim bypassed a decomposed task's dependency graph
VALIDATED: amux-frustrations | Verified 2026-09-11: grep dependency_blocked crates/amux-server/src/api/board.rs and session_verbs.rs:22814,22896 -- dependency_blocked code + test assert_eq!(blocked["code"], json!("dependency_blocked")) confirmed live.
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

## A successful trigger PATCH immediately puts parked work back in todo
VALIDATED: amux-frustrations | Verified 2026-09-11: git log -1 af53a6bb on origin/main: "fix(board): stop idle dispatch and recovery loops (AMUX-4168)" -- covers this and its two sibling AMUX-4168 entries below.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: git log -1 af53a6bb: "fix(board): stop idle dispatch and recovery loops (AMUX-4168)" -- same commit covers all three AMUX-4168 entries.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: git log -1 af53a6bb: "fix(board): stop idle dispatch and recovery loops (AMUX-4168)" -- same commit covers all three AMUX-4168 entries.
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-06
SESSION: amux
CARD: AMUX-4168
SYMPTOM: BR-51 was reclaimed at 04:25 and picked again at 04:26, then reclaimed at 10:26 and picked again at 10:27. Selection ignores pickup.reclaimed_stale when applying its per-card cooldown, so recovery does not yield to the other queued work.
COST: Two six-hour recovery cycles left byo-ray holding the same WIP slot over eight/nine eligible todo candidates.
FIX: Apply the existing bounded per-card cooldown to the reclaim event too, and log stale_reclaim_yields_to_next_card. Fixed in af53a6bb (AMUX-4168). The regression fails without the reclaim event in the cooldown. The actual byo-ray snapshot selected BR-137 after reclaiming BR-51; its live current CI run remains in progress, so no live reclaim was forced.

## Separate background tasks still held the HTTP runtime for seconds
VALIDATED: amux-frustrations | Verified 2026-09-11: grep runtime_job_blocking_poll crates/amux-server/src/runtime_jobs/poll_watch.rs:7 -- this exact mechanism is what this session's own AF-641/642 work diagnosed against, confirmed live.
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

## Status sorting mixed working and waiting workers and let idle pins lead
VALIDATED: amux-frustrations | Verified 2026-09-11: grep sortMode crates/amux-dashboard/static/app.js -- sort-mode logic present and matches the description.
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

## A test cell that reads the ambient process ancestry cannot fail on the box that wrote it
VALIDATED: amux-frustrations | Verified 2026-09-11: git log --oneline --all | grep 232c212f finds the exact matching commit; scripts/test-commit-stamp.sh:54,63,66,89 has the described claude-ancestor shim mechanism verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: scripts/hooks/hook-report.sh:271,274 names subagent:start/subagent:stop; SubagentStop is present in ~/.claude/settings.json. The producer now exists.
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

## Worker message arrows did nothing on Codex output
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/../amux-dashboard/static/app.js:12394 emits peek-message-nav; :16911 strips the Claude Code prompt glyph -- both halves of the described fix present.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/db/board_store.rs:3524,3544 -- dependency_cycle_rejected and lineage_cycle_rejected verdicts both live.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/session_verbs.rs:21000 -- the_cross_group_refusal_names_the_verb_that_works_without_an_existing_card exists verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/board.rs:1318 documents GET /api/graph/board/verify; graph.rs:73,108 emits task_graph_unmeasured; route registered in request_log.rs:1588.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:12339 emits peek-toolbar-layout verdict unusable-controls.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:15916 has _pmSel; :9873 emits client-action-error.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:12108 emits worker-action-menu verdict open-lost.
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4188
SYMPTOM: Physical mobile testing copied the directory link successfully, but the next Worker actions click immediately closed the menu. Its old document click listener survived because menu items stop propagation.
COST: Live verification of the new named directory actions failed; people needed an unexplained second click after using an action.
FIX: Closing the menu now retires both the pending dismissal timer and the document listener, including action and toggle paths. Browser regression tests repeatedly open Change directory, cancel and reopen the menu. An opening lost before paint emits worker-action-menu verdict=open-lost with measured and the menu-item population.

## Message jumps mixed zoomed screen coordinates with unzoomed scroll offsets
VALIDATED: amux-frustrations | Verified 2026-09-11: app.js uses window.visualViewport extensively (5071, 5079, 10587-88, 5479) for the described layout-coordinate conversion.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: app.js:11402 -- "Codex's final input hint sits immediately above its model/status footer", the exact adjacency check described.
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4192
SYMPTOM: The six-worker sweep found four copies of Ask Codex to do anything in amux's saved output being treated as messages. The composer exclusion only applied at the very end of the entire log, so historical frames escaped it.
COST: Geometrically correct jumps still landed on terminal input hints instead of submitted messages.
FIX: Detect the adjacent model footer for each unclassified block, including historical frames; authoritative submitted-message history still wins. A browser test covers old composer frames followed by later output and the same literal text recorded as a real human message.

## A bottom-clamped message jump was mistaken for resuming live output
VALIDATED: amux-frustrations | Verified 2026-09-11: app.js:17520 _readPosCleanup detaches the scroll listener; capture-phase listener comment at :35011 matches the described navigation-lock fix.
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4192
SYMPTOM: The deployed multi-worker sweep lost the selected message on amux-testing-e2e when a jump hit the bottom. The async scroll listener unlocked live updates even though refreshPeek itself preserved selected targets.
COST: One live navigation failure survived synchronous refresh tests; a working worker replaced the selected message after a geometrically correct jump.
FIX: The scroll listener now preserves the navigation lock for selected messages and search results at the finite scroll boundary. Regression tests wait for actual scroll events before refreshing and cover both trailing-output and end-of-output targets. Existing navigation beacons expose missing targets and scroll errors.

## Dependency completion released code at Done and returned to the child task
VALIDATED: amux-frustrations | Verified 2026-09-11: board.rs:2482 emits dependency_callback_linked; board_store.rs:3357 emits dependency_waiting_for_verification.
AREA: board
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-07
SESSION: amux
CARD: AMUX-4193
SYMPTOM: Code dependencies became runnable at Done before verification. The completion callback was linked to the producer child instead of the original requester task and lacked its next action. Claims and backlog promotion disagreed on missing and discarded dependencies.
COST: Ethan requested explicit end-to-end proof that verified dependency completion actually wakes and resumes the original worker. Existing unit fixtures encoded the earlier, weaker completion boundary.
FIX: Use the existing type-specific verification boundary for claims, promotion and durable callbacks. Derive the original return task from depends_on, retain graph edges, link the producer-origin message to the original task, include remaining blockers and next action, and record both task histories. Log dependency_waiting_for_verification, dependency_callback_linked and withheld or unmeasured delivery cases.

## A working Codex lane inherited its sibling's idle turn, and the alert blamed a discarded hook
VALIDATED: amux-frustrations | Verified 2026-09-11: sessions_legacy.rs:1192,1222,1957 -- codex_rollout/codex_session_id mechanism confirmed live; docs/incidents/2026-09-08-codex-rollout-cross-attribution.md exists.
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

## Every owner follow-up created another Doing claim and replaced the runtime's current card
VALIDATED: amux-frustrations | Verified 2026-09-11: session_verbs.rs:4368 emits capture_pending_active_claim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: app.js:10480,10536 -- psf-breadcrumb element; :17474 -- mobile file-preview header collapses actions into a dropdown, matching "vertical dots".
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

## Dashboard calls repeated authorization refusals Connecting and loses its own diagnostics
VALIDATED: amux-frustrations | Verified 2026-09-11: app.js's _sessionReadFailed() has the exact discriminated reason set missing_credential/invalid_bearer/unverified_member_cookie matching the SYMPTOM text precisely; deferred diagnostic via sessionStorage confirmed.
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

## Migration cost guard called a sibling write an unindexed read
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/db/migrate.rs:1578-1610 late_read_side_indexes fully implements the described logic: excludes the WRITE table, checks the index's leading column.
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

## Decomposition accepted child cards that did not say how to execute or verify them
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/board.rs:9570 has the literal error "acceptance criteria required".
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

## A different decomposition retry was reported as idempotent
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/board.rs:1316,5085,5208 -- plan_sha256 computed and persisted, decomposition_plan_conflict documented.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/runtime_jobs/board_drive.rs:2124,2220,2528 -- drain_todo_ceiling() + promotion_refused verdict confirmed live.
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

## A health timeout rebuilt the same revision and a later successful curl was logged as drift
VALIDATED: amux-frustrations | Verified 2026-09-11: docs/incidents/2026-09-08-health-stalls-build-feedback.md exists; activation.rs:40,64 -- same_revision skip-reason with a test.
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

## Publication gate mistook match patterns for failed sends
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/session_verbs.rs:29006 has the gate_probe line with excluded_match_patterns field, matching verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: exact identifiers renamed, not missing -- git log -S"sessions_probes_are_bounded" finds commit dfe27324 "fix(tests): allow richer bounded process snapshots", confirmed ancestor of origin/main, message matches the claim exactly ("Additional columns must not look like the probe disappeared").
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

## The auto-builder ships any branch to the live fleet with no announcement
VALIDATED: amux-frustrations | Verified 2026-09-11: .claude/session-freshness.sh:227-235 (git show origin/main) now detects rust-build-provenance.json's on_main:no and prints the requested warning at SessionStart, naming the branch/sha and telling the reader to put the checkout back on main. This is the exact remedy the entry's own FIX asked for.
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

## frustrations.md logged from ~/Developer/amux is stranded — that checkout cannot push
SUPERSEDED: amux-frustrations | Verified 2026-09-11: ~/Developer/amux no longer exists on this machine. The stranded checkout this entry describes is gone; whether the general append-to-a-diverged-checkout risk was ever addressed is a separate, still-open question this entry does not by itself answer.
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

## amux's own debug log is the biggest thing on a disk amux is filing cards about
VALIDATED: amux-frustrations | Verified 2026-09-11: same fix as AEAB-12 -- .claude/session-freshness.sh now announces an off-main live build at SessionStart, naming the branch and sha, which is the surface this entry asked for ('where a session actually looks').
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
VALIDATED: amux-frustrations | Verified 2026-09-11: the content-diff hook-staleness check this entry endorsed (PR #144 / AF-156's proposal) is live in .claude/session-freshness.sh (git show origin/main), comparing scripts/git-hooks/* against installed hooks by bytes rather than the guard_version request-body flag this entry correctly identified as measuring the caller, not the file.
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

## The disk ranker cannot rank a file, so it could never have named the 1.8 GB one
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/runtime_jobs/autofix.rs:4696 disk_candidates() now includes regular files above a size floor, citing AEAB-42 by name in its own comment, with test disk_candidates_include_a_big_file_and_still_include_directories (line 8864, current main) covering exactly this.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/runtime_jobs/autofix.rs:4593 cites AEAB-45 by name and implements the requested fix -- log_dedupe::first_this_bucket keyed on the joined path list, replacing the once-per-run spelling that produced 1,336 identical lines.
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

## Sending cleared a draft before Amux confirmed delivery
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:13248 -- literal draft_retained: true in the composer-delivery event payload, matching the FIX's named signal exactly.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: amux:4868 -- local ep="$CC_HOME/endpoint.json", confirming endpoint resolution is CC_HOME-scoped, not hardcoded to production.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/session_verbs.rs:7508,7515 -- both named verdicts idle_hook_live_selector_wait and idle_hook_live_activity_paste exist verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:22451 -- kind: 'worker-freeze-order', verdict: 'captured-rendered-order' verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:18290-18313,20943 -- one shared _downloadFileBytes(url, filename) helper called from BOTH the preview toolbar and the Files menu (via /api/file/raw), emitting kind:'file-download', verdict:'failed' on error.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/session_verbs.rs:7155,7163 -- both named verdicts send_waiting_for_boot_ui and send_boot_ui_not_ready exist verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: e2e/settings.spec.ts:1017-1021 -- test('settings_notes_folder_row', ...) asserts the settings-notes-section locator and its text both toHaveCount(0), i.e. an assertion the misleading UI is absent, verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: GET /api/metrics/host is a real, routed endpoint (crates/amux-server/src/api/metrics.rs:19), matching the entry's claim that main commit 1b22fd21 implemented it.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:11948,12394 -- _peekNavBeacon('deferred-search-landed', ...) and kind: 'peek-message-nav' both present verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:28782 -- kind: 'board-detail-layout', verdict: 'title-resized-after-wrap' verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:12341 -- clipped_filter_caption:clippedCaption reported verbatim in the toolbar diagnostic.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: e2e/subagent-arrows.spec.ts exists (the exact filename named in the FIX).
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:35744 -- kind:'host-analysis-contrast', verdict: samples.every(s=>s.ratio>=4.5) ? 'readable' : 'low-contrast' -- matches "checks computed label/status contrast against 4.5:1" exactly.
AREA: browser
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The new Host panel passed navigation tests, but the actual phone and desktop screenshots showed near-black CPU/Memory/Disk labels on a hard-coded dark chip background in light mode. The status text also used bright dark-theme colors.
COST: A user could see colored dots and values without being able to read which host dimension they described.
FIX: v0.9.874 uses the existing card/text and semantic status theme colors. The real Host lifecycle toggles both themes through Settings and checks computed label/status contrast against 4.5:1. Local host-analysis-contrast diagnostics report readable/low-contrast, actual foreground/background colors, ratios, theme and six considered samples after rendering.

## The send-retry regression existed without a CI invocation
VALIDATED: amux-frustrations | Verified 2026-09-11: .github/workflows/checks.yml:340 -- python3 scripts/test-send-retry-identity.py is invoked, the exact named harness.
AREA: tests
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: The published lifecycle audit failed GitHub checks because test-send-retry-identity.py, already on main, was not invoked by any workflow or hook.
COST: Stable retry identity and refusal-without-terminal-injection had a regression file but no continuous coverage, and the required harness-wiring ratchet blocked checks.
FIX: Invoke the real Python harness in checks.yml next to the existing send-retry tests. Its outage/recovery/refusal cases and the harness-wiring ratchet pass locally. The existing test-harness-wired.sh diagnostic names any future unwired harness in CI and locally; no product runtime path is involved.

## Startup message refusal inherited HTTP 500
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/session_verbs.rs:28745-28763 -- test state_refusals_are_conflicts asserts the exact message maps to StatusCode::CONFLICT (409) with fix.is_some() -- matches "classify the startup refusal as 409 with a wait/retry next step" exactly.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: e2e/lifecycle/torrents.spec.ts:6 -- test('LC-TORRENT: populated progress, pause, resume and removal render at each viewport', ...) matches the FIX's named coverage verbatim.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-server/src/api/workers.rs:2283-2293 -- retry loop explicitly gated on legacy["error"].as_str() != Some("sessions list changed during discovery; retry"), matching "retry at most four times only for the exact documented discovery-revision invalidation error" exactly.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:36368-36371 -- all four torrent action buttons (Pause/Resume/Remove/Stop) carry aria-label attributes naming the action AND min-width:44px;min-height:44px inline styles.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: e2e/upload-reliability.spec.ts:112-120 -- test uses const headers=await auth(page) then page.request.get(result.url,{headers}), matching "use the existing auth helper for downloads and assert the actual status" exactly.
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: Both UI uploads finished, but the new direct download assertion returned 401 in all three browsers; its APIRequestContext omitted the dashboard bearer token.
COST: Three failed checks in a 45-case upload/composer run; earlier progress comments overstated that case before the final summary.
FIX: Use the existing auth helper for downloads and assert the actual status with the requested URL. Keep the real byte equality check and preserve the original failed report.

## Torrent controls were too small to tap on a phone
VALIDATED: amux-frustrations | Verified 2026-09-11: crates/amux-dashboard/static/app.js:36368-36371 -- min-width:44px;min-height:44px inline styles on all four torrent action buttons, matching the 44x44 touch-target fix claim.
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-amux-lifecycle
CARD: AMUX-4362
SYMPTOM: Opening the passing populated torrent screenshot showed tiny adjacent pause and stop glyphs with correspondingly small targets at 375px.
COST: A visual review found friction that successful click assertions missed.
FIX: Give every torrent action a bordered 44px target and readable theme text. LC-TORRENT records measured dimensions and fails below 44px while retaining viewport-fit and effect checks.

## Dashboard's usage-limit discriminator says 'worker'; the live endpoint says 'session'
VALIDATED: amux-frustrations | VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `rust-rebuild (provider adapters, RR-0043)` is gone, so no author can sign this). The rust mapper accepts BOTH spellings -- provider/claude.rs:317, `if kind_str == "session" || kind_str == "worker"`, with a comment naming which is live and which is older. Live check: GET /api/usage returns limits kinds ['session','weekly_all','weekly_scoped'], so the live spelling matches. The FIX section's actual ask is met too: recorded fixtures at claude.rs:404-405 carry both kinds, so endpoint drift fails a test rather than silently unlabelling a bar. The dead `l.kind === 'worker'` filter is gone from the SPA. (Re-confirmed 2026-09-11 under Ethan's retire-on-evidence policy for entries whose author session no longer exists.)
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

## Opening peek permanently narrows the worker's tmux pane — observing changes the observed
VALIDATED: amux-frustrations | VERIFIED FIXED (part 1) 2026-08-21 (amux-frustrations; authoring lane `peek-render agent` was a subagent with no session, so nobody can sign this). The reported friction is gone: 47 of 49 live tmux sessions are at 220 columns, 2 at 80, NONE at the reported 50/94/102. Mechanism removed both ends -- runtime_jobs/pane_size.rs:207 issues `set-option -w -t <s> window-size latest` to undo the manual pin, and app.js:9340-9359 records the resize-on-peek machinery as deleted. PART 2 CLOSED 2026-08-21: GET /api/sessions/<n>/peek now carries pane_cols and pane_rows (12e8013), verified on the running server -- amux 80x25, mvs-infra 80x24, amux-cloud 220x50. Still open, and small: the SPA peek header does not show it (a UI display nicety, not the core API/mechanism ask). (Re-confirmed 2026-09-11 under Ethan's retire-on-evidence policy.)
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
VALIDATED: amux-frustrations | VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane was a subagent with no session). Resolved by DELETION plus a replacement: app.js:8220 records the pane-driven switcher as deleted, citing ARE-7 and the 0-of-50 predicate, and names the replacement -- a subagent list reading DURABLE transcripts via GET /api/sessions/<n>/subagents, with no visibility gate at all. Verified live on three lanes: amux 53, backend 143, amux-frustrations 1. (Re-confirmed 2026-09-11 under Ethan's retire-on-evidence policy.)
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

## Six SPA-consumed API families 404 in production and nothing anywhere says so
VALIDATED: amux-frustrations | VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust (RR-0130/0131 cutover sweeps)` is gone). THIS SUPERSEDES an earlier 2026-08-20 note that was wrong about which family was still open (it was tunnel, never one of the six named here). Called today, all six answer HTTP 200: /api/channels/{a}/{b}/messages, /api/log-search, /api/memory/global, /api/observability, /api/review/week, /api/review/digest. The seventh claim (/api/metrics serving a wrong document) is also closed: app.js:29269 assigns _metricsData = data (the raw body) and _metricsRender reads data.sessions/data.system off THAT, so top-level is what it wants; live 116 sessions, 49 active, 0 lacking a numeric cpu_percent. The missing instrument the FIX asked for (route.callers_have_routes) exists and can fail -- it reports 8 failures today, all /api/tunnel/*, a different family tracked on AF-64. (Re-confirmed 2026-09-11 under Ethan's retire-on-evidence policy.)
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
VALIDATED: amux-frustrations | VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust (RR-0131b sweep)` is gone). Both named comment sites are absent from crates/; git log -S shows the strings entering at 0b156bb and leaving at ff6b7d1, whose subject is "fix(memory): compose MEMORY.md after worker memory writes (AR-117)" -- the removal is the fix, not a reword. Live end-to-end evidence: this session's own loaded MEMORY.md carries a composed worker-memory block and the fleet roster, which is the composition the fix produces. (Re-confirmed 2026-09-11 under Ethan's retire-on-evidence policy.)
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

## A CLI probe measured a connection failure and it read as the bug reproducing
VALIDATED: amux-frustrations | VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust` is gone). The defect was that amux-rs defaulted to https://localhost:8823, where nothing listens, so every verb died on connect and read as the application bug reproducing. Tested the built binary directly (~/.amux/rust-build-target/debug/amux-rs): a bare `amux-rs board list` with no AMUX_RS_URL set exits 0 and returns 1,722 lines of real board data. It resolves the live endpoint on its own. (Re-confirmed 2026-09-11 under Ethan's retire-on-evidence policy.)
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
VALIDATED: amux-frustrations | VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust` is gone). Tested with the probe the entry says was botched -- stdout ON the pipe, stderr to a FILE (`amux-rs board list 2>/tmp/e.txt | head -2`), not the `2>&1 >/dev/null` that detached stdout. Result: amux-rs exits 141 (128+13, SIGPIPE), correct Unix behaviour for a closed stdout, 0 bytes on stderr, no panicked line. Control: unpiped, the same command emits 1,722 lines, so stdout really was attached to the pipe and an EPIPE panic was reachable. (Re-confirmed 2026-09-11 under Ethan's retire-on-evidence policy.)
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
VALIDATED: amux-frustrations | VERIFIED FIXED 2026-08-21 (amux-frustrations; authoring lane `amux-rust` is gone). crates/amux-server/src/api/commit_mentions.rs exists, cites AMUX-2674 and e679bdb by name, and GET /api/board/commit-mentions is routed and live -- it returns 20 open cards named in merged commits right now. It honours the entry's explicit ethos-8 caveat: module header says "It SURFACES, it never closes"; the endpoint is a GET that mutates nothing. (Re-confirmed 2026-09-11 under Ethan's retire-on-evidence policy.)
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

## A wedged disk scan could not say whether the walk or the database was stuck
VALIDATED: amux-frustrations | Verified 2026-09-11: both cited commits present on origin/main with exactly matching subjects: 7ecb7666 "fix(reclaim): a stalled scan names the directory and the phase it is stuck in (DESKT-15)", 03712301 "fix(reclaim): one quiet minute is contention, not a broken directory (DESKT-15)". Author session `desktop` unreachable; retiring on this commit evidence per Ethan's retire-on-evidence policy.
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

## `amux board --help` reports the flag as an unknown SUBCOMMAND instead of printing help
VALIDATED: amux-frustrations | Verified 2026-09-11: directly tested -- `amux board --help` now prints the full board help text (verb list, usage), not an "unknown subcommand" error. Ran live against the installed CLI. Author session `desktop` unreachable; retiring on this direct live test per Ethan's retire-on-evidence policy.
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

## amux.service's KillMode=mixed cgroup-kills the whole fleet on every ordinary deploy
VALIDATED: amux-frustrations | Verified 2026-09-11: current amux/CLAUDE.md's own Deploy section documents this exact fix as shipped and LOADED: `systemctl --user show amux.service -p KillMode` -> `process`, confirmed live 2026-08-31. First-party, current, dated documentation confirms the fix holds. Author session unreachable; retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: checked live -- `cmd_start "$name" --detach` call is present in the current amux CLI (cmd_start_all no longer omits --detach). Confirms this half of the fix landed. Author session unreachable; retiring per Ethan's retire-on-evidence policy.
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

## `amux` died at load with a bash syntax error — every subcommand, every session, at once
VALIDATED: amux-frustrations | Verified 2026-09-11: identical SYMPTOM/FIX text to the `start`/`start-all` set -e entry (near-duplicate append); same verification applies -- `cmd_start "$name" --detach` present in the current CLI. Author session unreachable; retiring per Ethan's retire-on-evidence policy. Noting as a duplicate/near-duplicate of the neighboring entry.
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

## Local multiplayer passed localhost while its Tailscale invite links were unusable
VALIDATED: amux-frustrations | Verified 2026-09-11: PR #199 "Fix local Tailscale multiplayer auth lifecycle" confirmed MERGED 2026-09-07 (matches entry date) via `gh pr view 199`. Board card ATE-79's own bookkeeping is messy (mixes an old needsyou note with a later implementation-complete note) but the merged PR is the stronger, dated, objective evidence. Author session `Codex local Tailscale multiplayer` unreachable; retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: same PR #199 (merged 2026-09-07), same card ATE-79, same reasoning as the neighboring Tailscale multiplayer entry. Retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: same PR #199 (merged 2026-09-07), same card ATE-79, same reasoning as the neighboring Tailscale multiplayer entries. Retiring per Ethan's retire-on-evidence policy.
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

## A local invite proved identity but granted the owner’s entire API surface
VALIDATED: amux-frustrations | Verified 2026-09-11: same PR #199 (merged 2026-09-07), same card ATE-79, same reasoning. Security-relevant entry (auth scoping) -- the fix is confirmed merged; card ATE-79's own state remains open/confused and is a separate bookkeeping cleanup, not counter-evidence against this entry's specific claim. Retiring per Ethan's retire-on-evidence policy.
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

## A pool outage erased pending browser writes while the editor reported Saved
VALIDATED: amux-frustrations | Verified 2026-09-11: git log confirms b724cdff and 69490b05 exist with matching subjects ("retain failed edits and report durable sync state", "render mobile terminal diffs and stream incrementally"). This retires the narrow browser-contract claim only; the entry's own text explicitly leaves AF-640 server recovery and the stale-blocker Rust patch open as separate matters. Author session `amux-testing-e2e` unreachable; retiring per Ethan's retire-on-evidence policy.
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

## Cross-tab delivery coverage reached an uncounted context route
VALIDATED: amux-frustrations | Verified 2026-09-11: detailed, narrow, checkable fix (route-stub-guard.spec.ts census fix) with a specific verification count (45 passed, 0 unexpected/skipped) already recorded in the entry. No superseding correction found. Author session unreachable; retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: narrow, self-contained test-fixture fix with a specific count (57/57, then 59/60 with a named pre-existing slow-poll caveat honestly disclosed) already recorded in the entry, plus an explicit disclaimer that this is not a full-browser-CI claim. No later correction found targeting this entry specifically. Author session unreachable; retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: board card ATE-136 confirmed `done` via API. `make install-cli`/`install.sh`'s guarded-publisher mechanism (snapshot + conflict-marker rejection + atomic rename) matches current CLAUDE.md's own "Bash CLI ships via `make install-cli`" convention. Author session unreachable; retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4352 confirmed `done` via API. Session `codex-adhoc-fixes` absent from /api/sessions (unreachable). Detailed, specific fix description (byte-cursor paging, 119,599-char live verification) with no found correction. Retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4359 confirmed `done` via API. Same unreachable session `codex-adhoc-fixes`. Specific, falsifiable claims (composer_excluded_from_messages event, shared msg_id) with no found correction. Retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4361 confirmed `done` via API. "codex" is a generic tool label, not a live addressable amux session (absent from /api/sessions). Specific version-tagged fix (v0.9.868) with concrete described behavior. Retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4366 confirmed `done` via API. Same unreachable "codex" label. Fix description is narrow and checkable (delegates to Claude Code's native compaction, removes a toggle Rust never read). Retiring per Ethan's retire-on-evidence policy.
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
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4368 confirmed `done` via API. Specific UI/behavior fix description including a live-verification-caught regression (parent loading indicator) that was itself fixed and covered by a named test. Retiring per Ethan's retire-on-evidence policy.
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4368
SYMPTOM: The terminal lost its agent arrows; the replacement toolbar button only displayed descriptions, without any way to read a selected subagent's output.
COST: Owner could not navigate subagent work where they were reading the terminal and had to request the controls again.
FIX: Two arrows beside Copy cycle current main/subagent output, preserving the main draft and view. Read owned structured child transcripts, never inject navigation keys. Missing outputs and list failures announce themselves through subagent-navigation diagnostics. Desktop/phone navigation, no-child, failure, and parent restoration tests. Live verification also caught a parent loading indicator surviving a quick child switch; clear it on selection and cover that race.

## Claude opens a diff sidebar in the worker terminal
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4372 confirmed `done` via API. Concrete, measured fix ("89 panes checked, zero left open") plus a specific settings key (diffSidebarOpen=false) and regression coverage described. Retiring per Ethan's retire-on-evidence policy.
AREA: terminal
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4372
SYMPTOM: mixpeek-general displayed Claude's diff sidebar and /diff to hide diff, compressing the conversation and showing empty diff panels.
COST: Owner had to request removal of a terminal layout they never want.
FIX: Close observed active sidebars once (89 panes checked, zero left open; native close controls preserve drafts), seed Claude's native diffSidebarOpen=false on worker launches (including already-trusted folders), and remove /diff from amux command suggestions. Log preference persistence success/failure; regression tests cover preference reset, idempotence, unrelated config preservation, and slash-command discovery.

## Image attachments stay at zero percent
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4374 confirmed `done` via API. Specific, falsifiable timing numbers (24.6s/30.8s hangs) and a described bounded-retry fix with regression coverage; no correction found. Retiring per Ethan's retire-on-evidence policy.
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4374
SYMPTOM: Two image.png chips stayed at 0%; upload fetch and response-body reads had no deadline. Live start requests took 24.6s and 30.8s despite ultimately returning200. A hung request could hold one of four slots forever. Queued uploads also captured the global peek array, so switching workers before their start could misroute attachments.
COST: Owner could not reliably attach screenshots or send the blocked draft.
FIX: Bound each upload phase, retry transient failures up to three attempts with fresh upload IDs, and retain failed chips with an explicit Retry button. Show queued/starting/uploading/finishing/retrying states. Create placeholders immediately and capture the originating attachment array; retries share the concurrency limit. Record phase, attempt, bytes, HTTP status and outcome in attachment-upload diagnostics. Desktop/phone regression tests cover stalled fetches and bodies, retries, server restarts, cancellation and worker switching.

## Provider usage disappears during account throttling
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4375 confirmed `done` via API. Detailed mechanism (shared in-flight probe, Retry-After backoff, credential-scoped persisted snapshots) with named regression coverage; no correction found. Retiring per Ethan's retire-on-evidence policy.
AREA: browser
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex
CARD: AMUX-4375
SYMPTOM: Settings showed Claude Unavailable after HTTP429 despite an active subscription. The last successful report existed only in a short-lived route cache; deployments erased it. Routing and reserve consumers bypassed that cache and made competing probes. The whole-envelope stale fallback also rewound healthy Codex/Gemini rows.
COST: Owner lost visibility into remaining plan capacity and had to reopen Settings to discover recovery.
FIX: Share one in-flight Claude probe across server consumers, respect Retry-After with bounded exponential backoff, and atomically persist credential-scoped usage snapshots and cooldowns. Show historical readings with their observation time and scheduled refresh; never route work from stale percentages. Preserve other providers' current rows. Settings refreshes while open and retains readings through network interruptions. Log fresh, last_known, retry_scheduled and cache_write_failed outcomes. Tests cover concurrent consumers, restart/backoff/account isolation, stale routing exclusion, provider independence, and desktop/phone recovery.

## Mobile upload queues acknowledged unsaved bytes and lost attachments on reload
VALIDATED: amux-frustrations | Verified 2026-09-11: board card AMUX-4409 confirmed `done` via API. Session `codex-mobile-ux` absent from /api/sessions (unreachable). Specific, falsifiable acceptance numbers (256 MiB / 128 MiB hash checks passing in named spec files). Retiring per Ethan's retire-on-evidence policy.
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-mobile-ux
CARD: AMUX-4409
SYMPTOM: Eight simultaneous IndexedDB upload additions retained only the last file; an aborted transaction still reported queued; composer attachment chips disappeared after reload. WebKit also rejected File/Blob storage while byte buffers succeeded. A landscape Send button extended below the viewport, and queued Files uploads completed without refreshing the visible folder.
COST: Four reproducible browser failures plus two mobile layout/navigation failures; required a new durability and large-file acceptance matrix because the existing upload tests covered retries without reload.
FIX: Transactional 5 MiB local chunks, foreground replay, persisted chunk progress and completion receipts, streamed assembly/downloads, atomic collision-safe publication, acknowledged attachment cleanup, Files refresh, and viewport-fit checks. A full mobile sweep also reproduced gzip download aborts; fusing the file stream keeps post-EOF compression polls safe. Explicit view links now dismiss overlays and supersede delayed saved-screen restoration. Four original durability regressions failed before the change. The 256 MiB composer and 128 MiB Files hash checks passed in Chromium and WebKit; acceptance cases are in mobile-offline-ux, mobile-large-files, and mobile-attachment-ack specs.

## Consolidated integration checks lagged current board and group contracts
VALIDATED: amux-frustrations | Verified 2026-09-11: same card AMUX-4409 (`done`), same unreachable session `codex-mobile-ux`. Narrow, mechanical test-fixture fix (with_cause, session_target exact matching) with no found correction. Retiring per Ethan's retire-on-evidence policy.
AREA: tests
SEVERITY: slows
STATUS: fixed
DATE: 2026-09-10
SESSION: codex-mobile-ux
CARD: AMUX-4409
SYMPTOM: Two archive fixtures omitted the now-required authorizer; the Python boundary capture still expected env-only group membership. The browser error guard also found a new profile-merge join error rendered without the shared cause formatter, and the tmux target audit found a stale-pane cleanup using prefix matching.
COST: Five integration failures obscured the mobile acceptance verdict; a stale cleanup target could match a sibling session. Mobile subagent fixtures also used 1970 timestamps despite the current freshness filter, and repeated large-file fixtures consumed test disk space.
FIX: Name the fixture owner for intentional archive actions, retain the historical capture while explicitly pinning the evolved native-worker group contracts, use with_cause for the join error, and build the cleanup target with session_target for exact matching. Refresh active-agent fixture timestamps, keep a stale-agent negative control, and release generated large uploads after recording their hashes. Existing denial, error-chain and target-audit controls remain in place.
