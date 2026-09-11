# Mobile messaging and Gemini lifecycle validation — 2026-09-11

This is a partial live-provider result, not a full consolidated-suite pass.

## Production messaging and connection failures

Seven Safari 0.9.900 studio-plg failures recorded only `failed/unconfirmed`; the underlying exception was absent. Read-only inspection measured 5,193,082 localStorage bytes, dominated by history and reproducible caches. A real WebKit quota test against the pre-fix JavaScript failed local acceptance. The reproduced mechanism is consistent with this storage condition; the historical exception itself cannot be recovered.

Published fix fe8cd4d4 prioritizes atomic user-intent writes over reproducible caches, preserves drafts and pending operations, and reports specific local storage failure through Sync. Ordinary background delivery keeps Send stable, hides the transient queue pill, and cannot clear a newer draft. Attachment cancellation is journaled before a chip disappears, with recovery after reload. Diagnostics record categories and sizes without message content.

The watchdog explicitly issued production restarts at 13:16:19 and 13:29:32 after three 250 ms database-probe timeouts. launchd recorded SIGTERM. Late successful probes were discarded, allowing a slow but progressing store to be mistaken for a dead one. Published fix 02712586 preserves completion and in-flight ages. Current unmeasured readiness remains 503; only recent progress or bounded initial work defers restart. Stale progress, actual store errors and absent listeners retain recovery. This fixes false restarts, not underlying host memory pressure or every network fault.

## Verification receipts

| Check | Result and scope |
| --- | --- |
| Old-source WebKit quota reproduction | Failed, expected queued and received failed |
| Focused messaging/terminal checks | 96 passed across desktop Chromium, mobile Chromium and iPhone WebKit |
| Integrated main offline/reconnect/composer/tab checks | 87 passed |
| Final quiet composer assertions | 18 passed, including hidden transient pill and preserved newer draft |
| Outbox concurrency/recovery contracts | 26 passed |
| Earlier broad browser candidate | 798 passed, 3 failed / 801; all three missing Tabs labels were later corrected and focused checks passed; the complete 801 were not rerun on final source |
| Large files | Earlier 256 MiB interruption/recovery and byte hashing passed on all three browser targets |
| Health handler | 3 passed; blocked-writer regression failed when late completion publication was deliberately suppressed, then passed after restoration |
| Watchdog | 7 passed, including HTTP 503 and restart/no-restart loop controls |
| Gemini fresh-conversation contract | 2 passed, including actual config handler and subsequent exact resume identity |
| Lifecycle runner | 8 passed |

These counts overlap and must not be added into a unique total. Workspace/all-target Clippy passed for the messaging/watchdog fix and the fresh-conversation/source-context changes. A broader Rust integration run stopped at host memory admission in replay_roundtrip, with swap above the configured limit; it is not a green full integration result. Browser runs using prebuilt APIs explicitly recorded candidate JavaScript/HTML hashes. Raw traces remain private because they can contain authentication headers.

## Real Gemini coverage and retained failures

The dedicated lab uses separate database, port, tmux socket and work files. Actual Gemini CLI workers produced code, peer review, HTML output and board evidence. A single-worker journey completed three deliverables and passed its read-only observer. Earlier report corrections required explicit operator feedback and are labeled assisted.

The same-group pair completed changes-requested, revision and approval with independently executable output. Its terminal navigation assertion still failed for an older approval outside the loaded tail. This failure is retained; it is not converted into a pass by checking message history alone.

A complex epic/subtask run independently verified core deliverables after changed gates, but its strict final observer failed because an extra FYI card used gate acknowledgement instead of independent verification. A circular completion callback instruction was corrected and covered by durable-queue tests; reduced live repetition is not yet certified.

The upload follow-on exposed New conversation resuming the prior Gemini identity because only the Claude key was cleared and non-Claude launch paths ignored the fresh flag. The fix clears provider identities and honors fresh launch for Gemini/Codex. A stopped worker requires Start after reset; the acceptance flow exercises that UI path too. Fresh Gemini IDs use the CLI's UUID contract. The upload result is described below; cross-group and backlog follow-ons remain pending until their real-provider receipts pass.

The next real upload attempt verified the fresh launch, then exposed a native read-approval prompt for the uploaded file outside the checkout. The Gemini launcher now includes the Amux uploads directory alongside logs. First launch required explicit native trust of the dedicated lab uploads directory; this assisted setup is recorded separately from the worker result.

The upload run completed a file read and a board task after explicit native trust setup, but failed its required receipt schema (`rows` instead of `row_count`). Investigation showed that `amux board show` omitted the API's linked full source messages; its captured description ended at `row`. The CLI now exposes source IDs and `--messages`, and structured recovery requires the complete source for captured previews. The real CLI regression fails against the old source and passes after the fix. Reset also holds the existing per-lane send boundary across asynchronous stop/start; the pre-fix real trace showed a message accepted 20 seconds before the replacement launched.

Holding an HTTP send through reset was also rejected by a real mobile timeout/retry. The final restart path accepts ordinary input into durable steering immediately and returns a receipt, while the native drain waits for the replacement process. A new test holds the actual per-lane lock and calls the real send handler: acceptance must complete without native progress, and a same-ID retry must leave exactly one queue row. That test passed. Interactive slash commands remain explicit refusals during restart.


The final queued restart was exercised live: reset at 18:41:20Z, durable acceptance at 18:41:21Z, replacement launch at 18:41:42Z and confirmed steering delivery at 18:42:28Z. Gemini produced the exact uploaded path, `row_count: 2`, `total_count: 6`, then completed LG1A-7 with file evidence. The original delivery observer failed because it inspected unstamped command-history metadata for a queued send. The corrected contract reads the actual steering outcome, excludes dead-letter timestamps, and exposes the restart queue ID. A handler regression verifies confirmed/retried/dead outcomes as well as immediate acceptance and one-row retry deduplication (1 passed).

A subsequent read-only upload observer passed with the real receipt, all author cards terminal, and linked messages/terminal captured at 1280 and 375 pixels. Native folder trust was operator-assisted; the receipt and task completion were performed by Gemini. This is an upload pass, not a full pair/complex/backlog pass. Workspace/all-target Clippy passed after the receipt changes.


Visual review of the real upload caught a new message showing `direct?`: `_msgNorm` and the initial history-cache mapping discarded server delivery fields. Version 0.9.903 retains delivery, timing and submission metadata on all message surfaces. The loader/renderer regression passed on desktop Chromium, mobile Chromium and iPhone WebKit (3 passed), with exact embedded asset hashes verified from a fresh source build. The real mobile message was visually inspected with candidate JavaScript over the unchanged lab API; this separate screenshot is frontend evidence. The source build also reran all 26 outbox contracts successfully.

History pinpoints the recent changes: 9727ee4c (September 10) introduced local message queueing, ea06c0f4 (September 11) expanded immediate local acceptance, and 673b2962 (September 11) reverted to direct sending. The 250 ms database health deadline dates to 451cafb6 (September 8). These dates identify the changed paths; the log and reproduction evidence above establish the specific failures rather than assigning every network fault to those commits.


## Final deployment and bounded Gemini result

Code commits 1b233ed2 and 34607cf3 were pushed to main. Production 0.9.903 reported commit 34607cf3c9b4 / build 39c022ce23044fca; served app.js and sw.js matched the committed source exactly. The installed Bash CLI and watchdog matched their source bytes. The watchdog remained running, and no further watchdog-issued restart was observed before this check. The code adoption retained PID 75977.

The cross-group phase produced correct peer/group/task metadata, exchanged CROSS_REQUEST, CROSS_ACK and CROSS_DONE, and completed its core work. Its final visual assertion failed: an older peer message could not be found with the terminal Workers filter. Gemini readable scrollback currently falls back to terminal paint logs instead of its native structured conversation journal. This remains an unresolved provider-history gap.

The separate backlog/todo case seeded four cards without sending chat prompts or writing completion states. The reviewer independently completed LG1R-8 (Todo, result 21) and its dependent LG1R-9 (Backlog, result 24), with matching JSON files and prerequisite IDs. The author's LG1A-10/11 remained Backlog/Todo while cross-group acknowledgement captures repeatedly generated new work. After 6.5 minutes the run was explicitly interrupted to bound wasted tokens; it is incomplete, not passed. Both dedicated workers were stopped through the API, then the dedicated server was stopped. Records, files, traces and failures were preserved.

A read-only helper probe returned exit 1 and `You've hit your session limit`, with a 6:10pm America/New_York reset. Semantic intake was using that Claude helper even for Gemini workers, and interpreting its stdout as classifier JSON, so the operational log only reported an invalid classifier response. This explains the observed unavailable semantic comparison; the repeated-work behavior and misleading helper diagnostic remain open. No quota was bypassed, no alternative billing credentials were substituted, and no task was manually completed to make this run pass.


## Follow-up: native acceptance before the browser receipt (0.9.904)

The additional user screenshots showed the same homepage request in Claude's native queue and in a local pending card. Read-only inspection found exact MSG-55405 direct/confirmed at 11:55:12, preceding the 11:55:17 screenshot. Local pending previously implied not delivered even though it only meant the complete POST had not been acknowledged. Version 0.9.904 adds GET `/api/sessions/{name}/send?msg_id=...` over the durable acceptance ledger; it does not send, reserve an identity or infer acceptance from message text. The client reconciles that exact receipt while the original bounded POST may finish board work. Until confirmation, attempted and legacy-unknown entries are labeled uncertain and cannot be cancelled as unattempted sends. Native queue acceptance does not claim that the worker has executed or completed the task.

Validation: `node --test tests/dashboard-outage-recovery.mjs` passed all 29 contracts, including exact-ID receipt reconciliation while the POST is held, rejection of unaccepted/wrong-ID receipts, and cancellation protection after an attempt. `python3 scripts/lifecycle/run.py browser --grep LC-RECEIPT` passed desktop Chromium, mobile Chromium and iPhone WebKit (3 passed) against a fresh source build with embedded asset hashes checked. Before/after screenshots were opened and visually reviewed; the pending card clears before the held POST is released, the composer stays empty, and exactly one POST occurs. This controlled transport test uses an isolated stopped worker; it does not pretend to execute a new native Claude task.

The broader composer run initially reported 18 passed / 3 failed. Two storage-failure cases reached their required diagnostics but teardown clicked through an open Connection modal; a third timed out during iPhone boot. The teardown now dismisses the modal through its backdrop and that case has a 60-second budget. Its focused rerun passed all three browser projects (3 passed). Workspace/all-target Clippy passed. The read-only receipt handler test covers reservation versus acceptance, session isolation, unknown IDs, no-store and unchanged send/history counts; the Rust check remains targeted rather than a full integration-suite claim.


## Follow-up: quicker local input and terminal display (0.9.905)

The user confirmed the stale queued banner was resolved and requested lower input latency. This change preserves durable local acceptance, serialized sends, and exact-ID acknowledgements. A message added while a replay is already running now continues on the next tick rather than inheriting the outage retry timer. A mutation disabling that continuation failed the new contract with an 8000 ms timer instead of zero; restoring the fix passed all 31 outbox/polling contracts.

Post-input terminal updates now use a single bounded live-frame poll loop (first tick 40 ms, then 100 ms interval for 1.5 seconds), returning to adaptive cadence afterward. Full history and turn-end refreshes remain available after that burst. This replaces two immediate full-history requests and overlapping poll timers. Read-only measurements from the live orchestrator before the change: full peek 73–109 ms, median 75 ms, about 128 KB; live-only peek 52–56 ms, median 53 ms, about 5 KB (five requests each). These are sampled server request times and payloads, not a claim of native provider end-to-end latency.


The first rapid-send browser run exposed a separate real UI failure on all three targets: the second button press left the new draft untouched. `_btnFire` used a 350 ms time window to suppress synthesized click echoes, but did not distinguish a new physical press. Pointerdown/touchstart now begins a new gesture; duplicate click echoes within that gesture remain suppressed, and keyboard activation is independent. The existing `send-fire` diagnostics continue to report called/resolved outcomes. A unit contract covers distinct rapid pointer and touch gestures versus their duplicate click echoes.


Final source-built browser run: `python3 scripts/lifecycle/run.py browser --grep 'LC-LATENCY|LC-RECEIPT'` — 6 passed, zero skipped/flaky, across desktop Chromium, mobile Chromium and iPhone WebKit. The iPhone trace records two real touchscreen `tap` calls 294 ms apart. In the controlled transport scenario, dispatch after releasing the preceding HTTP response measured 6/7/34 ms and terminal visibility after dispatch measured 50/50/173 ms respectively. Exactly two distinct message IDs were sent and the queue drained. Screenshots `send-latency-905-desktop.png` and `send-latency-905-ios-safari.png` were opened and visually reviewed. These numbers cover the application transport/display fixture, not native model execution or remote VPN latency. All 32 outbox and input-event contracts passed on the final source.


## Message-driven semantic intake coverage

Audit found that `LC-SEMANTIC-INTAKE` exercised only direct `POST /api/board`, despite the canonical case calling for captured worker messages. Added `LC-SEMANTIC-MESSAGES` to the automatically discovered live suite: six real composer messages, real native delivery and model-backed intake, exactly three task IDs, four source messages linked to one surviving task, increasing revisions, retained original/refined requirements, measured create/append/update logs, and clickable source links on desktop and phone task details. No observer board/history writes, explicit task-ID shortcuts, mocked classifier or manual completion are allowed. The direct-board case remains separate.

Attempted `python3 scripts/lifecycle/run.py live --grep LC-SEMANTIC-MESSAGES` against the dedicated lab using production commit ced4b617 / build dfd33c6f8a8e4c81. The required preflight failed before creating a worker or sending any messages: `admission: deny`, memory pressure `warn`, swap used 20758.6875 MB. The test result is one failed prerequisite, not a semantic comparison pass. The health attachment, trace and report are retained under `semantic-messages-live`. The lab server was stopped afterward. The new scenario compiles and is listed by the live Playwright configuration; native/message/model assertions remain unexecuted until worker admission permits the run.
