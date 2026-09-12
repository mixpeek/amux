# Isolated worker lifecycle — September 12, 2026

Isolation now has an expanded deterministic case (`LC-08`) and an opt-in native
provider case (`LC-ISOLATED-NATIVE`) in the consolidated catalog. The browser
configuration includes the boundary/owner spec on desktop Chromium, 375px
Chromium and iPhone WebKit. The live configuration discovers the native spec for
both selected providers; discovery is not execution.

## Coverage

- Same-group and outside-group peers, including explicit send allowances, must
  receive `403 isolated_target` through both Send and Queue. Refusals must not
  mint approval requests, task cards, history entries or queued messages.
- Owner queued messages survive reloads and isolation changes. Repeating the
  operation ID preserves one durable queue entry. Pending on a stopped worker
  is never treated as delivery.
- The real Configurations switch persists across reload. Warm peer rosters hide
  the newly isolated worker; disabling isolation restores its visibility. An
  ordinary same-group worker remains visible as a positive control.
- Native case: create an isolated Sonnet/Gemini worker through the UI, attach
  JSON, queue an assignment, wait for a confirmed native submission, verify the
  worker-produced sum/file and absent harness environment, and capture terminal
  output. It never presses Enter to rescue pickup or writes the output itself.

## Regression found

The source-built baseline returned **200 from `/steer` for a same-group peer**
while `/send` correctly returned 403. The direct handler owned the isolation
check, but Queue reached the durable store without that check. The shared early
refusal now covers both paths, before deduplication, history or queue mutation,
and cannot be bypassed by disabling group-send enforcement. Owner/self and
resource-authorized human member access retain their existing behavior.

The refusal emits `send.isolated_refused`, plus a warning with
`verdict=isolated_target`. Rust controls prove rejected peers leave zero rows and
that an owner can reuse the rejected operation ID, retry it, and retain exactly
one queue/history row.

## Results and limits

The source-built focused browser run passed **6/6 cases**, with no skips or
retries, across desktop Chromium, 375px Chromium and iPhone WebKit. The final
screenshot refinement passed 6/6 against the identical pinned executable; asset
hashes matched on all three servers. Desktop and both phone configuration
screenshots were opened and inspected, including the visible isolation switch.
The long fixture header labels truncate on phones; this is not a full-terminal
visual certification.

`bash scripts/test-contended.sh -p amux-server --lib isolated -- --nocapture`
passed **13 tests**. Workspace check and all-target Clippy with `-D warnings`
passed. Lifecycle runner contracts passed **9 tests**; the catalog contains
**88 unique cases**, with all source paths present. Sonnet and Gemini native test
discovery each list the new case, but neither is counted as execution.

Private raw artifacts use archive key `isolated-lifecycle-20260912`; traces
contain temporary test authentication and stay outside the repository. A redundant
visual-run rebuild was cancelled while waiting for a build slot; the completed
visual rerun reused the source-built executable to avoid another compilation.

The first prebuilt-binary attempt was correctly refused because all three
embedded dashboard asset hashes differed from this checkout. It establishes no
product result. The rebuilt baseline reproduced the peer-Queue bypass. Its
owner case then timed out because the test reopened an already-open panel; the
test now closes the panel through its UI before reopening it.

Native Sonnet/Gemini execution is **NOT RUN** in this validation: the current
host `/health` reports `admission: deny`. No admission override or unrelated
worker termination is used to turn that condition into a pass. The new native
case is discoverable, but automatic file pickup/output is not certified here.

Full server command: `bash scripts/test-contended.sh -p amux-server --no-fail-fast`.
Result: **2642 passed, 7 failed, 33 ignored across 60 targets** (exit 101).
The failures are the same host-admission check, five native worker lifecycle
checks, and replay worker-start roundtrip recorded in the prior validation.
They report admission denial/503; the full suite is **FAIL**, not certified green.
Rust source bytes stayed fixed during that run; only test/docs work continued.
See [portable evidence](evidence/lifecycle-isolated-2026-09-12.json) for exact
failure names, source hashes and binary provenance.

## Open board-policy conflict

Per-worker isolation is distinct from `AMUX_ISOLATED=1`, the test-server switch
that disables background fleet jobs. A deterministic browser server with those
jobs disabled cannot establish anything about autonomous board drain.

The real board code currently disagrees with itself: `LiveFleet::lanes()`
includes isolated workers and its newer comment says they should be driven,
while `start_for_board_dispatch` refuses their launch and guarded steering
refuses their board-drive messages. The create UI and older isolation docs also
promise no automatic pickup. This change does not resolve that product-policy
conflict or mark isolated backlog/todo as drained. The catalog explicitly keeps
that acceptance condition open: selection, wake, delivery and evidenced terminal
state must agree before it can pass. The guided restart/offline/board continuation
remains unverified as well.
