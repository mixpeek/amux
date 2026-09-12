# Steering, offline recovery and browser lifetime validation

Tracking: AMUX-4417. Overall lifecycle status remains **INCOMPLETE**; this report
separates client/API checks from actual model execution. Candidate dashboard is
0.9.919, integrated on upstream `bc5e2385` with its durable card-attachment cleanup
and immediate worker-details refresh retained.

## Behavior repaired

- A composer clears only after local durable acceptance; network delivery remains
  asynchronous. Uncertain native submission keeps the original ID, message and
  file references for review instead of deleting the intent and marking it synced.
- Pending steering is projected from durable storage, not a two-minute in-memory
  text-match cache. Distinct identical messages remain distinct through reload.
- Worker cards now have a touch-accessible file picker. Both composer surfaces
  use the same upload path; sent attachments are removed durably. The compact
  one-row phone composer remains. Its attachment menu now fits narrow viewports.
- Reconnect shows individual running, successful and failed operations. Only
  acknowledgements receive checkmarks; failed rows remain visible until reviewed.
- Browser API starts default to headless. Screenshot failure cannot restore or
  raise a window. Explicit headed sign-in remains intentional and supported.
- Disabling idle browser expiry no longer disables activity and hard lifetime
  limits. The expiry notice names the actual idle configuration variable.
- A historical boxed Gemini picker above a newer bare prompt no longer reports
  an idle worker as waiting and prevents automatic steering at that boundary.

## Evidence completed before final integration

`steering-sync-fixes-914`: 36 outbox contracts passed; 27 browser cases passed and
three layout cases failed on a real attachment-menu overflow. The file scenarios
use real upload/download bytes, browser storage and UI but controlled worker
status/final transport receipts. They do **not** certify model execution.

`background-layout-914`: all three repaired layout cases passed on desktop,
phone Chromium and iPhone WebKit, including 320px width, landscape and reduced
keyboard height. Its three new browser-lifetime cases failed during test setup;
those failures are retained, not counted as runtime evidence.

`browser-ttl-enabled-914`: one actual Chrome case passed. It created a scratch
profile through the API without opening a login window, launched headlessly,
operated a page button, captured PNG output, observed automatic hard expiry after
20 seconds with both idle/activity expiry disabled, and read the retained profile
sentinel. The recorded foreground application was `com.apple.loginwindow` before
and after; this proves the locked desktop remained unchanged, not an exhaustive
focus trace during an unlocked desktop session. The test used the exact pinned
binary from the source-built layout run; the supplied-binary runner correctly
labels independent source provenance unverified. The source run's asset receipts
and the identical executable hash link the two artifacts.

The initial TTL attempt exposed a harness prerequisite: global fleet isolation
also disables the reaper. The opt-in scenario now enables only the browser reaper
using existing per-job switches, with a private home and tmux socket. All other
catalogued jobs remain disabled. No production expiry settings were changed.

`uncertain-negative-control-914`: temporarily restoring the delete-and-checkmark
behavior made the new uncertain-submission contract fail (0 passed, 1 failed).
The mutation helper restored the candidate source. This verifies that the new
contract detects the bug rather than merely exercising the happy path.

Screenshots of iPhone worker-card Queue, details Send, per-operation checkmarks,
320px attachment-menu placement and actual Chrome page output were opened and
reviewed. The final browser run also checks that sent file chips stay absent
following reload.

## Native worker and broad-suite limits

`native-steering-sonnet-914` and `native-steering-gemini-914` each failed admission
preflight before creating a worker. The dedicated lab used the candidate binary;
health reported admission denied under memory pressure with about 30 GB of swap.
A later read-only `memory_pressure -Q` reported 47% free on this 96 GB host,
while health still reported kernel warning and about 30 GB swap. The current
admission policy denies at an absolute 8 GB swap threshold; its denial is not
proof that the machine has zero free memory or that all retained swap is active.
Whether that policy over-restricts this host remains open; it was not loosened to
make a test pass. No model launch, automatic pickup or terminal task completion
is claimed from these attempts. The test-owned lab server was stopped afterward.

The initial full server command reached 2,295 unit passes, eight failures and
seven ignored cases before cargo stopped at the failing unit target. Six failures
were host-admission/start refusals; one was a sessions-list discovery race; one
was the historical picker regression repaired here. Browser capture-failure and
real-reaper-stop contracts passed. A final `--no-fail-fast` run is required to
retain results from integration targets as well; an initial unit total is not a
full-suite pass.

The new LC-STEERING-AUTO case requires an exact steering receipt with confirmed
submission, file-derived output, an original terminal board task and real evidence.
The paired queue observer already requires the original backlog/todo cards and
all remaining owned captures to reach truthful terminal states without operator
status writes. Neither requirement is satisfied by the blocked native runs.
Busy-worker arrival remains an explicit guided check. The paired queue observer
now records three real quiet driver cycles with no new cards or reopened work;
that added observation is discovered but remains unexecuted under admission denial.
Semantic model-backed merging, cross-group native review, retained Gemini peer
history and token-per-outcome measurement remain open in the closure register.

## Final integration

The source-built `messaging-final-915` selection passed 33 browser cases across
all three projects and 36 outbox contracts. `browser-background-final-915` passed
one real Chrome case, including the enabled-reaper/disabled-other-jobs assertion.
The combined TTL/messaging attempt remains recorded with 26 passes and 10 failures:
seven harness failures (ordinary board intake must retain global isolation and
controlled transport must respect offline mode) and three real reaper-status
failures. Subsequent separate runs verify the corrections; failures are retained.

The `--no-fail-fast` server run completed 60 targets: 2,611 passes, 13 failures and
32 ignored cases. Five failures were outdated fixture expectations: four open-card
archive requests omitted the newly required truthful archive outcome, and one
asset assertion demanded the unsafe uncertain-message deletion. The corrected
board API target passed 89/89; dashboard assets passed 34/34 before the final
upstream view-refresh guard was integrated. Seven failures were host admission
refusals and one was a sessions-list structural-change race. The quoted-picker
regression passed after its fix. Full workspace/all-target clippy passed.

The final 0.9.916 run and deployment receipt are recorded in the accompanying
portable evidence summary. No successful fixture result overrides a failed prerequisite or
an unexecuted guided case. The canonical catalog now contains 80 acceptance rows.

## Release verification

`messaging-release-916` passed 33 browser cases and 36 durable-outbox contracts.
Visual review then caught a lingering queue toast (the reconnect toast was already
suppressed); the checklist now dismisses obsolete queue feedback on opening. The
follow-up visibility assertion checks the actual visible class, not just wording.

`large-file-release-916` passed on iPhone WebKit: a 268,435,579-byte file survived
four interrupted requests and reload with 52 persisted chunks. The downloaded
SHA-256 exactly matched the source. This proves upload/storage recovery, not a
native model consuming that large attachment.

`browser-owned-focus-916` passed real Chrome launch, page interaction, screenshot,
and 20-second hard expiry with idle/activity expiry disabled. The actual process
arguments contained `--headless=new`; three foreground samples remained the user's
Safari Web App, never the owned Chrome PID. The profile sentinel survived expiry.
The previous `browser-background-release-916` run failed because the user changed
from Codex to Safari during launch: requiring an unchanged *unrelated* foreground
app was an incorrect assertion. The corrected test permits normal user activity
and checks browser ownership. Sampling does not prove the absence of every
possible transient focus event; headless process mode and no-focus capture
contracts provide the complementary evidence. The real page PNG was opened.

Final all-target workspace clippy passed. The dashboard-assets integration target
passed 35/35 including the upstream immediate-details-refresh guard; the isolated
sessions-list cache module passed 7/7 after its earlier discovery race. These
follow-ups do not erase the broad suite's seven host-admission failures.

`messaging-visible-toast-916` passed all 33 browser cases and 36 outbox contracts
after clearing the visible class. Screenshot review still showed the notification:
Motion animation fill kept it painted despite the class change. The final fix
cancels that animation and the test now requires computed opacity zero. This
is why checking a class alone was insufficient. Browser focus/TTL and large-file
runs cover unchanged server/upload behavior.

`messaging-painted-toast-916` then passed 33/33 browser cases and 36/36 outbox
contracts with computed opacity checked. Its iPhone screenshot was opened: all
three individual receipt rows and the conflict explanation are unobscured.

## Concurrent uncertain-message sweep

Main advanced to `b3a06028` during final verification. Its boot/sync sweep
automatically removed older blocked uncertain messages. An unconfirmed receipt
does not establish successful delivery; deleting the only local intent and file
references prevents later reconciliation. The new reload case preserves the
exact old ID and attachment-bearing body, requires reviewable failure details,
and refuses a successful checkmark. The integration intentionally supersedes
the automatic sweep and its source-string assertion while retaining the prior
upstream attachment cleanup and immediate worker-details refresh.

`uncertain-sweep-negative-916` failed the new iPhone reload test against the
integrated upstream sweep: expected one durable operation, received zero. The
candidate removes automatic deletion; only an acknowledgement or explicit user
dismissal removes that intent. This negative run remains part of the evidence.

`messaging-final-917` passed 35 cases and failed one mobile test measurement:
the immediate `boundingBox()` call returned null during a re-render. The test now
waits for the actual 44px target geometry before tapping. All three uncertain
reload cases passed. Dashboard assets passed 35/35. Main's subsequent missing-file
preview explanation (`2287c484`) is retained in the 0.9.918 integration.

## Final candidate result

`messaging-final-918`: **36 passed, 0 failed, 0 skipped, 0 flaky**, plus 36
durable-outbox contracts. This source-built run includes the preserved upstream
missing-file preview explanation, all three old-uncertain-message reload cases,
and the computed-opacity sync check. The catalog has 80 unique cases with valid
source links; all nine Python runner contracts and JS syntax checks pass.

The inspected [iPhone sync checklist](evidence/lifecycle-sync-checkmarks-2026-09-12.png)
and [real background Chrome output](evidence/lifecycle-browser-background-2026-09-12.png)
are small, fixture-only visual receipts. They contain no authentication headers
or user conversations. Native steering and backlog completion remain blocked;
this is a passing client/API selection, not a full lifecycle verdict.

The final main integration retains `bc5e2385`'s visible multi-operation batches
and its reconnect guard, with our animation cancellation and failure retention
inside that visibility branch. A new executable outbox contract checks a quiet
single operation versus a quiet two-operation batch. The dashboard/cache version
is 0.9.919. This final integration follows the 36-case 0.9.918 browser run.

`integration-final-919` passed **9/9 browser cases** across all three projects
and **37/37 outbox contracts** after the final integration. The new quiet-batch
contract initially used an invalid empty board acknowledgement; returning the
actual task ID repaired that fixture. Commit hooks passed security, all-target
clippy and JavaScript syntax. The implementation was pushed to main as
`7a2218d90e920e9c849d2d1560ed5969b9693f69`. GitHub accepted the App's push while
required checks were still expected; CI was then observed running, not yet green.

## Deployment and remaining live findings

At 14:05 UTC, live health reported implementation `7a2218d9`, build
`81128895cb210bfb`, status/store `ok`, and the same PID 8407. Served app.js,
app.css and sw.js matched the 0.9.919 source hashes exactly, with the same build
before and after the probe. Steering and browser cleanup were ticking; a later
read also found board-drive status `ok` with two completed ticks. This verifies
enabled loops, not native task completion. Worker admission remained denied
under kernel warning with about 34 GB swap.

The worker-list GET returned 200 for 135 rows in 5,914ms, and a later read took
3,204ms. At 14:06:43 UTC, health returned a transient 503: its 250ms deadline
expired in phase 1 (the serialized writer/read-pool probe), then the underlying
probe succeeded at 702ms. Subsequent health returned 200 in 18ms; PID and build
remained unchanged. These are remaining latency/readiness findings, not proof
of a server crash or proof that all mobile disconnects are resolved.

Logs also repeatedly show autofix expiry failing on nonexistent issues timestamp
columns. The schema uses `created/updated`; refresh and expiry SQL use
`created_at/updated_at`. Simply replacing the names would activate unconditional
age-based discard and an ID-only update that can race a claim. That requires a
truthful cleanup policy and atomic revalidation, not a blind timestamp patch.
LW-11 and new LC-AUTOFIX-CLEANUP document the exact repair and counterexamples;
the catalog now contains 81 cases. The new case is explicitly NOT_RUN.

`browser-process-exit-919` passed the strengthened real-Chrome test: after TTL
the actual owned PID no longer existed, and the saved profile remained intact.
This test-only addition and the deployment receipt follow the implementation
commit; production runtime bytes are unchanged.

The required GitHub `checks` job subsequently passed. The separate `check` and
`e2e` jobs were still running at the final observation; a complete CI pass is not
claimed. The cloud deployment workflow skipped; the deployment receipt above is
for the local Amux server.
