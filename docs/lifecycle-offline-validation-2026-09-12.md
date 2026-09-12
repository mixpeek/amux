# Offline reconnect lifecycle validation — September 12, 2026

The offline lifecycle now includes a cold page reload with real browser networking
turned off. The existing outage tests alone did not prove that journey: their
90 cases passed before a new UI-driven run found that cached board edits could
not enter the outbox.

## Changes verified

- Complete task details are stored in the existing IndexedDB issue mirror. Offline
  hydration requires the matching ID, an integer revision, and every editable
  field. Slim list entries cannot authorize a save. HTTP refusals do not fall back
  to stale copies; transport failures may. The original revision still gates replay.
- Browser offline mode preserves pending work without repeatedly painting a
  failed sync panel over the editor. Server reachability failures still retry when
  the browser has networking, and the actual online event triggers reconnect.
- Directory uploads use the same reconnect checklist as board edits and messages,
  while retaining their chunked IndexedDB byte store. Completed rows remain visible
  across retries; removed entries cannot receive an unearned acknowledgement. A checkmark requires the
  file finish receipt and successful pending-record removal. Upload-only failures
  participate in the retry timer, and durable finish receipts survive reload.
- A real scratch-server proxy uses loopback HTTP, which browsers treat as a secure
  context, to exercise service-worker caching. The scratch server's self-signed
  certificate otherwise prevents registration even with Playwright's TLS-ignore
  option. Production TLS settings are unchanged; the test stubs no API response.
  The proxy destroys connections during the outage, including across reload.
  WebKit's Playwright offline emulator also breaks local File reads and cached
  navigations (reproduced without Amux), so those two steps retain the physical
  socket outage while temporarily allowing the emulator's local I/O. Chromium's
  online hint can reset on navigation; an actual failed health request proves
  the cold page remained disconnected. No navigator value or sync function is mocked.

Background history imports bypass the user outbox and defer while messages remain
pending. This keeps optimistic local history from being imported as a past delivery
before its queued message is acknowledged. The existing interaction-receipt test
now excludes the unrelated default tab-layout preference write from its replay count.

## Acceptance journey

`LC-OFFLINE-ROUNDTRIP` warms the shell and task details, disables networking, edits
three cards through their Save controls, queues two owner messages, and adds two
files (32 MiB plus 17 bytes, and a Unicode-named evidence file). It reloads while
offline and compares durable IDs, message bodies and upload metadata. Reconnect
uses the browser's online event without Retry or a direct replay-function call.
A read-only DOM observer records every checklist transition; all seven operations
must run and receive an acknowledgement. Server reads verify card titles and queue
rows, and SHA-256 compares the uploaded bytes. Another online reload must not
resurrect messages or pending writes.

`LC-SYNC-PROGRESS` separately pauses individual acknowledgements and introduces a
conflict: a running item has no checkmark, the conflict stays failed and retained,
and a later explicit retry is independently checked. Other retained cases cover
real peer revision conflicts, cross-tab delivery, lost/ambiguous receipts, stalled
response bodies, storage failure, chunk retries, and newer drafts during sends.

## Historical comparison

The August 30 implementation (`6db5796f`) showed the reconnect checklist. Recent
quiet-send changes hid it; `bc5e2385` restored it on September 12. Restoring the
old replay algorithm wholesale would lose newer durability guarantees: the old
implementation emptied its queue before delivery. This change retains the
individual acknowledgement and stable-message-ID approach and tests the missing
cold offline UI path.

## Scope

A server-accepted steering message is not proof that a native model consumed it.
This scenario deliberately uses a stopped fixture worker and checks that distinction.
Sonnet/Gemini execution, isolated automatic pickup, real-device background/VPN soak,
and the broader lifecycle remain tracked in [the open-work register](lifecycle-open-work.md).
The evidence below measures this offline scope, not those outstanding native journeys.
The candidate incorporates upstream `eebbb58f` interaction receipts; their browser
and state-kernel tests are part of the compatibility checks.

## Recorded results

Candidate dashboard **0.9.923**, incorporating upstream `eebbb58f`. All three
served dashboard assets matched the checkout hashes. The source-built private
server SHA-256 was `c3ede66bac01c4f920247c3cd9ee4ed3ccf3ad364465492f14c8376f06766d62`.

```bash
AMUX_SESSION=codex-server-sync AMUX_LIFECYCLE_PORT=19923 CARGO_BUILD_JOBS=2 \
python3 scripts/lifecycle/run.py browser \
  --grep 'offline-roundtrip.spec|outage-recovery.spec|outbox-connectivity.spec|mobile-offline-ux.spec|upload-reliability.spec|sync-progress.spec|blocked-outbox.spec|uploads.spec|interaction-receipts.spec' \
  --output <artifact-root>/reviewed
# 138 passed (3.5m), 0 failed, 0 skipped, 0 flaky
node --test tests/dashboard-outage-recovery.mjs
# 46 passed, 0 failed
node --test e2e/state-kernel.test.mjs
# 27 passed, 0 failed
AMUX_SESSION=codex-server-sync CARGO_BUILD_JOBS=2 \
bash scripts/test-contended.sh -p amux-server --test dashboard_assets --test interactions_api
# dashboard_assets: 38 passed; interactions_api: 11 passed; 0 failed
AMUX_SESSION=codex-server-sync CARGO_BUILD_JOBS=2 bash scripts/safe-cargo.sh check --workspace
# PASS
AMUX_SESSION=codex-server-sync CARGO_BUILD_JOBS=2 bash scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings
# PASS
```

Five exact negative controls removed the offline cache, offline replay guard,
file acknowledgement requirement, retained checkmarks, and history outbox bypass
one at a time. Each mutation landed, caused a matching failure, was reverted, and
was followed by a passing baseline. Logs are retained with the run artifacts.

Visual review of all four committed screenshots confirmed readable checkmarks
on desktop, mobile and iPhone WebKit. That review caught a real stale offline
toast obscuring the completed checklist despite a green earlier run. The fix
clears only obsolete sync notices; a regression assertion requires the seven
checks to remain visible with no covering toast. The cold-outage screenshot
records failed transport attempts after reload, before reconnect: these are
retained pending operations, with no false success marks.

- [Desktop: seven acknowledgements](evidence/offline-sync-2026-09-12-desktop-all-seven-acknowledgements.png)
- [Mobile: cold offline reload](evidence/offline-sync-2026-09-12-mobile-cold-offline-reload.png)
- [Mobile: seven acknowledgements](evidence/offline-sync-2026-09-12-mobile-all-seven-acknowledgements.png)
- [iPhone WebKit: seven acknowledgements](evidence/offline-sync-2026-09-12-ios-safari-all-seven-acknowledgements.png)
- [Machine-readable transitions, file hashes and source provenance](evidence/lifecycle-offline-sync-2026-09-12.json)

The upstream upload subrequest interaction receipts still report `phase=unknown`
for some successful start/finish requests. This is separately recorded in the
open-work register; actual file acknowledgement and bytes were verified here.

The supplemental attachment run initially returned 29 passed and 7 failed.
Six failures came from a stale test expecting the worker-card Attach button,
which the owner explicitly removed in `01b69d59`; card paste/drop remain supported.
The test now supplies a real File through the paste event and retains the native
file picker on details. The seventh was a test navigation error: WebKit restored
the existing details panel after reload, obscuring the worker-card menu. The
journey now closes the restored panel through its Close control before reopening.
Neither change restores a removed product control or bypasses upload/send code.

The corrected supplemental run passed **36/36**, with no failures, skips or flakes,
using the exact same binary SHA-256 as the source-built 138-case run:

```bash
AMUX_SESSION=codex-server-sync python3 scripts/lifecycle/run.py browser \
  --binary <artifact-root>/reviewed/amux-server \
  --grep 'composer-file-surfaces.spec|composer-layout.spec|mobile-large-files.spec|mobile-attachment-ack.spec' \
  --output <artifact-root>/attachments-reviewed
# 36 passed (1.8m)
```

That is **174 browser cases** across the two final runs. The supplemental run
verifies card paste and details file selection, Send/Queue offline replay, newer
drafts, interrupted 256 MiB + 123 byte attachments (52 bounded storage chunks),
128 MiB + 17 byte Files-page uploads, exact uploaded/downloaded SHA-256, and
portrait/landscape/reduced-keyboard controls. Controlled send receipts in the
composer-parity cases measure client behavior; the new seven-operation journey
uses real server acknowledgements.

Run-owned duplicate binaries were removed after testing (12 copies, 2.45 GB logical
size; actual APFS reclaimed blocks were not measured). Pre-fix and final tested
binaries, logs, traces and screenshot evidence remain available. Cargo gates used
the shared target and two-job budget; no additional per-worktree Cargo target was
created.
