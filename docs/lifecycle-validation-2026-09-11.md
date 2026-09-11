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
