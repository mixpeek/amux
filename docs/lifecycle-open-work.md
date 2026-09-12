# Lifecycle status and closure checklist

Updated September 12, 2026. See the [steering and sync validation](lifecycle-validation-2026-09-12.md) for the latest candidate, runtime fixes and explicit native-run limits. Tracking work: **AMUX-4417**.
Overall status: **INCOMPLETE**. This register covers all known unresolved items
from the mobile messaging, board, Sonnet and Gemini lifecycle work. It is not a
claim that every undiscovered product defect is enumerated.

The September 11 deployment receipt was `241b92acb02c719cb72b9ef8d70109c1f75fa3e4`, dashboard
0.9.911, build `a6d4111bd0eaff6c`. That read-only health probe returned
`status=ok`, `store=ok`, `pressure=warn`, `swap_used_mb=22176.1875`,
`admission=deny`. Healthy serving and permission to launch another worker are
separate facts. Admission denial is an observed host condition; the cause of
all retained memory/swap has not been established.

Use the [acceptance guide](consolidated-lifecycle.md) for the full procedure and
[case catalog](../e2e/lifecycle/cases.json) for individual acceptance conditions.
The [latest portable evidence summary](evidence/lifecycle-2026-09-12.json) preserves run
identities, hashes, selections, failed stages and the deployment receipt. The
[detailed incident report](lifecycle-validation-2026-09-11.md) preserves the
investigation and earlier fixes. Raw traces remain in the local evidence archive;
`archive_key` identifies their result directory. The committed summary contains
no authentication headers or full user conversations.

## Fixed and checked

| Area | Shipped behavior and actual proof |
| --- | --- |
| Local acceptance and draft retention | Durable intent takes priority over disposable caches; exact message IDs survive retries; a newer draft is not cleared. Real WebKit quota reproduction and subsequent browser regressions are recorded in the incident report. |
| Sent versus pending | Read-only acceptance receipts reconcile a pending browser request with native acceptance without sending again. Receipt tests use controlled transport; they do not prove model execution. |
| Rapid input | Distinct pointer/touch gestures are accepted within 350 ms; newly queued work avoids outage backoff; a bounded lightweight poll burst displays terminal updates. Earlier measured dispatch/display timings are fixture timings, not native-model latency. |
| Mobile composer | The September 12 candidate preserves the newer compact writing row with aligned 44px More/Send/Queue controls, a reachable attachment menu and bounded long-draft height. The final 18-case run includes narrow phone and keyboard-height viewport checks, with screenshots opened. |
| Failed offline operations | Failed and retryable counts remain distinct while offline; stale dismissal cannot remove an edit resumed by another tab. A real 409, offline period and reconnect verify the surviving edit. |
| Verified and changed gates | Older evidence remains visible; changing the gate requires a fresh check and stale acknowledgements are refused. Browser cases pass on desktop Chromium, phone Chromium and iPhone WebKit. |
| Linked task detail | Epic, child, source message, file, URL and commit navigation plus evidence and actual uploaded bytes passed the selected browser checks. This does not prove autonomous decomposition. |
| Large upload transport | 268,435,579 bytes survived interruption and reload using 52 persisted chunks. Source and downloaded SHA-256 match. iPhone WebKit emulation passed; no native model read this 256 MB file. |
| False watchdog restart | Earlier fixes retain recent database-probe progress rather than restarting for repeated short probe timeouts. Their regression evidence is in the incident report; underlying host pressure and long-duration reliability remain open below. |

Latest focused source-built run: **18 browser cases passed**, zero skipped/flaky,
and **34 outbox contracts passed**. The separate large-upload run passed one
case using a pinned binary whose hash matches the preceding source build. Do not
sum earlier partial runs into an invented full-suite result. In particular,
`board-continue-20260911` had 21 passing browser cases but a failed outbox assertion;
its overall result is FAIL. `board-outbox-main-911` exposed the composer regression
and is also retained as FAIL; the repaired run is the later pass.

## Remaining work

| ID / status | Evidence and impact | Required closure |
| --- | --- | --- |
| LW-01 — BLOCKED: live worker admission | September 12 health denies new workers with memory pressure and about 30 GB of swap. Both new native-steering provider runs failed this preflight before worker creation. `semantic-messages-live` failed preflight before creating a worker or sending a message. | Identify current memory consumers and release only authorized resources, or use an authorized independent lab. Recheck admission, then run both provider journeys. Do not disable admission to turn this red prerequisite green. |
| LW-02 — OPEN: misleading helper error | The retained helper probe exited 1 with `You've hit your session limit` on stdout. `complete_cli` returns nonempty stdout even after failure, so intake reports invalid classifier JSON. This is a confirmed code-path defect. | Classifier calls must honor unsuccessful exit status, preserve a bounded useful diagnostic, and log exit/timeout separately from invalid successful output. Cover stdout-only quota, stderr-only failure, empty failure, failed JSON-shaped output, timeout and successful JSON. LC-HELPER-FAILURE is now in the guided catalog; it has not been implemented as an automated regression. |
| LW-03 — INCOMPLETE: message-driven semantic deduplication | Live model-backed append/update assertions remain unexecuted in the new six-message scenario. When comparison fails, `board_intake::plan` preserves the request as a separate record with `measured=false`; this can accumulate near-duplicates. A Gemini worker still uses the server's configured helper, which may be Claude. | First fix LW-02 and establish helper availability independently of worker provider. Pass LC-SEMANTIC-MESSAGES: six actual composer messages, exactly three task IDs, four full source links on the survivor, measured append/update decisions, increasing revision and preserved/refined requirements. Separately test failure/recovery policy; do not silently merge on guessed title similarity or claim the current fallback deduplicates. |
| LW-04 — FAILED: older Gemini peer terminal history | The cross-group run exchanged CROSS_REQUEST, CROSS_ACK and CROSS_DONE and completed core work, but the older peer message was not found under the terminal Workers filter. Readable history falls back to terminal paint logs rather than Gemini's structured conversation journal. | Add or repair provider history ingestion with durable origin/ID mapping. After enough output to leave the live frame, load older history, find the exact peer message under Workers, and navigate its task at desktop and phone widths. Repeat human-origin and live-frame positive controls. LC-27 and LC-SONNET-CROSSGROUP now explicitly require this. |
| LW-05 — INCOMPLETE: backlog starvation and repeated coordination work | Reviewer LG1R-8/9 finished with correct artifacts; author LG1A-10/11 stayed Backlog/Todo while acknowledgement captures generated further work. The observer was stopped after 6.5 minutes. This is a partial result, not proof of a particular driver bug. | Trace which source messages create each extra card and which dispatch decision defers the originals. Repair capture/reply/dispatch at the responsible boundary. Repeat LC-SONNET-QUEUE without chat nudges or observer status writes: both workers finish their original Backlog and Todo IDs with dependencies and evidence. After handoff, observe three complete driver cycles with no receipt-only tasks or turns. |
| LW-06 — INCOMPLETE: full real-provider coordination and complex verification | Browser policy checks establish visibility and allow/deny behavior; they cannot establish real independent peer review. The prior Gemini pair is partial, and no complete fresh Sonnet/Gemini matrix is certified. | Run same-group, cross-group and ungrouped native-peer cases, explicit denial/reply/isolation controls, seeded failure → reviewer rejection → author repair → independent approval → dependent completion. Then LC-COMPLEX-VERIFIED must produce linked epics/children/messages/files/commits, amend the gate, rerun actual criteria, and reach evidenced Verified on every real deliverable. Preserve failures rather than manually completing cards. |
| LW-07 — PARTIAL: upload/provider matrix | A real Gemini worker read an earlier small UI upload, reported row_count=2 and total_count=6, and completed LG1A-7. Initial folder trust required operator help. Large-file transport passed separately. | Repeat the upload scenario with a fresh Sonnet worker and a fresh Gemini worker; record trust setup honestly. Test representative large text/binary files, native access to the exact uploaded path, interruption/reload/retry/cancel, linked artifact preview/download, and independently checked output. Treat provider context/file-size limits as visible outcomes, not infinite-file support. |
| LW-08 — NOT CERTIFIED: every UI action and physical mobile behavior | Selected desktop/phone/WebKit screenshots and interactions passed. Browser viewport resizing does not reproduce every physical iPhone keyboard, Safari/PWA lifecycle, background suspension or VPN condition. The complete guided ledger is unfinished. | Walk every discovered control using LC-01–LC-63 and supplemental cases: visible effect, persisted result/reload, cancel, invalid/empty input, keyboard focus and touch. Complete light/dark, long content, real keyboard, browser/PWA, background/reopen and back-navigation checks. External sends require a configured test sink. Leave unavailable controls/capabilities explicitly INCOMPLETE. |
| LW-09 — UNMEASURED: token efficiency | Fewer terminal HTTP bytes and faster polling are not token savings. Repeated coordination work is observed; no matched native-worker plus helper token baseline proves a reduction. Intake considers at most 80 recent scoped candidates with 2,500 description characters each; incoming content and helper output also affect cost. | Measure identical verified outcomes before/after: model/settings, worker and helper calls, input/output/cache tokens when exposed, elapsed time and task/message growth. Remove receipt-only loops first; evaluate candidate/context changes without losing target selection or evidence. LC-TOKEN-EFFICIENCY records missing counters as UNMEASURED. Report tokens per verified outcome and quality together. |
| LW-10 — PARTIAL: sustained disconnect recovery | Specific false restarts and browser state bugs were reproduced and fixed. A post-deploy health probe returned 503 at its 250ms writer deadline, then succeeded at 702ms without a PID change. Worker-list requests took 5.9s and 3.2s; host pressure persists. No final long-duration phone/VPN/background soak proves all disconnects resolved. | Run LC-40–LC-43/LC-51 with controlled outage, response loss after acceptance, server adoption and background/reopen. Record build identities, health/probe ages, connection events and exact accepted message IDs. Require one delivery per ID, no lost intent/new draft, correct failed/uncertain state, and no watchdog restart merely because a progressing probe exceeds its short deadline. |

| LW-11 — BROKEN: autofix alert refresh/expiry | Live logs repeatedly report `no such column: updated_at`; these queries use `updated_at/created_at`, while the issues schema uses `updated/created`. The dormant expiry update also discards by age with an unconditional ID-only write, so correcting the column names alone would enable unsafe cleanup. | Exercise LC-AUTOFIX-CLEANUP on a migrated scratch DB. Correct schema access, require resolved/obsolete evidence before terminal cleanup, and recheck status/claim/revision atomically so work claimed after selection survives. Keep ongoing faults, human work and claimed work intact. |

## Concrete repair entry points

- **LW-02:** [`complete_cli`](../crates/amux-server/src/api/mdai.rs) and
  [`classify` / `plan`](../crates/amux-server/src/api/board_intake.rs). The first
  currently promotes failed stdout to an answer; the second correctly exposes
  unsuccessful comparison as unmeasured but loses the operational reason in the
  card's generic fallback line. Preserve compatibility intentionally for other
  helper consumers; do not change every model path without testing them.
- **LW-03:** the same intake code, scoped candidate selection and revision-checked
  `apply`, plus [real sent-message acceptance](../e2e/lifecycle/live-semantic-messages.spec.ts).
  Existing graph preservation and concurrent revision checks must survive a fix.
- **LW-04:** provider/history handling in
  [sessions_legacy.rs](../crates/amux-server/src/api/sessions_legacy.rs), exercised by
  [the cross-group observer](../e2e/lifecycle/sonnet-crossgroup.ts). Test against
  retained native output; a fabricated terminal response cannot close this gap.
- **LW-05/06:** [queue observer](../e2e/lifecycle/sonnet-queue.ts),
  [three-worker coordination](../e2e/lifecycle/live-coordination.spec.ts), and
  [complex verification](../e2e/lifecycle/live-complex-verified.spec.ts). Use
  captured source-message IDs and driver decisions to distinguish intake loops,
  dispatch starvation and model behavior before changing scheduling.

## Rerun order and commands

First resolve admission and helper availability in a dedicated test installation.
Keep production workers and their pending queues intact. Configure the lab as in
[the acceptance guide](consolidated-lifecycle.md#run); do not point these live
commands at production. Use a fresh output directory for every invocation and a fresh scratch workspace
for each provider sequence. Stop only run-owned workers between sequences; keep
their records and artifacts for diagnosis.

```bash
# Configuration examples: replace paths/port with the dedicated lab's values.
export AMUX_LIFECYCLE_LAB_URL=https://localhost:YOUR_TEST_PORT
export AMUX_LIFECYCLE_LAB_WORKSPACE=/absolute/path/to/empty/scratch-repo
export AMUX_LIFECYCLE_LAB_ACK=dedicated-test-instance
# Set AMUX_LIFECYCLE_STORAGE_STATE if this lab requires saved browser auth.

# The Claude provider selector explicitly chooses Sonnet.
AMUX_LIFECYCLE_PROVIDER=claude python3 scripts/lifecycle/run.py live \
  --grep 'LC-SEMANTIC-MESSAGES'
AMUX_LIFECYCLE_PROVIDER=gemini python3 scripts/lifecycle/run.py live \
  --grep 'LC-SEMANTIC-MESSAGES'

# Pair-dependent steps must share one invocation/run identity. The LC-SONNET
# names are historical; AMUX_LIFECYCLE_PROVIDER selects Claude or Gemini.
AMUX_LIFECYCLE_PROVIDER=claude python3 scripts/lifecycle/run.py live \
  --grep 'LC-SONNET-(PAIR|UPLOAD|CROSSGROUP|QUEUE)'
AMUX_LIFECYCLE_PROVIDER=gemini python3 scripts/lifecycle/run.py live \
  --grep 'LC-SONNET-(PAIR|UPLOAD|CROSSGROUP|QUEUE)'

AMUX_LIFECYCLE_PROVIDER=claude python3 scripts/lifecycle/run.py live \
  --grep 'LC-COORD-LIVE|LC-COMPLEX-VERIFIED'
AMUX_LIFECYCLE_PROVIDER=gemini python3 scripts/lifecycle/run.py live \
  --grep 'LC-COORD-LIVE|LC-COMPLEX-VERIFIED'

# Recheck the recent browser fixes against a fresh source build.
python3 scripts/lifecycle/run.py browser \
  --grep 'LC-BLOCKED-OUTBOX|LC-GATE-REVISION|LC-LINKED-RECORD|LC-RECEIPT|LC-LATENCY|LC-COMPOSER-LAYOUT'
python3 scripts/lifecycle/run.py browser --project ios-safari \
  --grep 'large mobile file survives interrupted upload'
```

These commands are a rerun recipe, not results from this documentation update.
Honor normal model quotas and resource admission. Existing test timeouts bound
individual cases; also stop a run-owned coordination loop once repeated receipt
work is established, preserving its timeline and marking the run INCOMPLETE.
Do not repeatedly start replacement workers or change billing to get a green run.

After the targeted repairs, run `python3 scripts/lifecycle/run.py full` with each
provider configuration and finish the guided visual ledger. Newly expanded
acceptance text is a requirement, not evidence that an existing spec implements
all of it. Helper-failure handling and token benchmarking still need automated
regression coverage. Three post-completion quiet driver cycles are now required
by the paired queue observer, but their live execution remains admission-blocked.

Close each LW item only with a named run, exact commit/build/provider/model,
original task/message IDs, independently checked artifacts/criteria, inspected
screenshots and the actual pass line. Record residual limits beside the result.
AMUX-4417 remains open until all requested deliverables and acceptance cases have
an honest terminal verdict.


## Historical September 11 discovery

`python3 -m unittest discover -s scripts/lifecycle -p 'test_*.py'` passed all nine
runner contracts. `python3 scripts/lifecycle/run.py plan` discovered 900 browser
tests in 78 files and correctly returned INCOMPLETE (exit 2); all 76 generated
guided rows remained NOT_RUN with empty evidence. Live `--list` discovery found
10 tests in six files for each of the Claude and Gemini configurations. Catalog
IDs are unique, source/document links resolve, and `git diff --check` passed.
These are catalog/discovery checks, not new live-provider or runtime test passes.

## Isolated-worker follow-up

[Isolated lifecycle validation](lifecycle-isolated-validation-2026-09-12.md) adds
owner queue/retry and UI-toggle coverage plus a native queued-file scenario. It
records the peer Queue bypass fix and the still-open disagreement between board
lane selection and isolated wake/steering refusals. Native isolated task drain,
restart and offline continuations remain unverified.
