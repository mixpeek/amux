"""Regenerate the human acceptance guide from the canonical case catalog."""
import json
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]

def render():
    cases = json.loads((ROOT / 'e2e/lifecycle/cases.json').read_text())
    head = '''# Amux consolidated lifecycle acceptance

One suite, one run directory, one evidence ledger. This consolidates the golden
scenarios, RR-0133 all-subsystem acceptance, the seven claims in
`docs/e2e-acceptance.md`, worker configurations, browser regressions and UX discovery.
Same-group and cross-group peer discovery, task awareness, request/reply boundaries,
changes-requested review, independent re-review and dependent integration are
first-class scenarios. Existing specs remain independently runnable; no coverage is deleted or duplicated
by importing test files into a giant order-dependent test.

Current release evidence and every known unresolved lifecycle gap are tracked in
[Lifecycle status and closure checklist](lifecycle-open-work.md), with the latest
[steering and sync validation](lifecycle-validation-2026-09-12.md). That status is
INCOMPLETE; the case catalog is not a completed test run.

## Run

```bash
npm ci
npx playwright install chromium webkit
python3 scripts/lifecycle/run.py plan
python3 scripts/lifecycle/run.py browser
python3 scripts/lifecycle/run.py full
```

`plan` lists every browser case and inventories supporting tests without starting a
server. `browser` builds this checkout, pins a private executable, boots separate
temporary homes for desktop Chromium, 375px Chromium and iPhone WebKit, and runs all
browser specs plus the connected journey. `full` also runs workspace syntax and
Rust unit/integration tests, existing opt-in real-provider/backend scenarios, and
the new autonomous worker journey. The shell/node regression files are inventoried;
only those invoked by existing Rust contracts or CI equivalents are automated here.

For the autonomous journey configure a dedicated amux test installation with a
working provider, an EMPTY scratch repository accessible to that installation,
the normal harness and board driver enabled, and these environment variables:

```bash
export AMUX_LIFECYCLE_LAB_URL=https://localhost:YOUR_TEST_PORT
export AMUX_LIFECYCLE_LAB_WORKSPACE=/absolute/path/to/empty/scratch-repo
export AMUX_LIFECYCLE_LAB_ACK=dedicated-test-instance
# Optional: AMUX_LIFECYCLE_PROVIDER=claude|codex|gemini|ollama
# Optional: AMUX_LIFECYCLE_STORAGE_STATE=/path/to/test-browser-auth.json
python3 scripts/lifecycle/run.py live
```

Isolated workers are first-class cases: `LC-08` runs in all three browser
projects; `LC-ISOLATED-NATIVE` runs the real queued-file/raw-provider scenario.
Queue persistence alone is not evidence that the provider consumed the message.
The remaining board/restart/offline continuation is guided and must be recorded
separately; see [isolated lifecycle validation](lifecycle-isolated-validation-2026-09-12.md).

```bash
python3 scripts/lifecycle/run.py browser --grep LC-ISOLATED
AMUX_LIFECYCLE_PROVIDER=claude python3 scripts/lifecycle/run.py live --grep LC-ISOLATED-NATIVE
AMUX_LIFECYCLE_PROVIDER=gemini python3 scripts/lifecycle/run.py live --grep LC-ISOLATED-NATIVE
```

For background browser expiry, run the real scratch-Chrome case explicitly:

```bash
AMUX_LIFECYCLE_BROWSER_TTL_S=20 python3 scripts/lifecycle/run.py browser \
  --project desktop --grep LC-BROWSER-BACKGROUND
python3 scripts/lifecycle/run.py browser \
  --grep 'LC-SYNC-PROGRESS|LC-COMPOSER-FILES|LC-COMPOSER-LAYOUT'
AMUX_LIFECYCLE_PROVIDER=claude python3 scripts/lifecycle/run.py live --grep LC-STEERING-AUTO
AMUX_LIFECYCLE_PROVIDER=gemini python3 scripts/lifecycle/run.py live --grep LC-STEERING-AUTO
bash scripts/test-contended.sh -p amux-server --lib real_tmux_submission_replay_keeps_generating_input_unconfirmed -- --ignored --nocapture
```

The TTL setting is scoped to temporary test servers; it disables their idle and
activity expiry so the hard lifetime is independently exercised. Only the browser
reaper is enabled through existing per-job controls; every other catalogued loop
stays disabled, and the test verifies that isolation before launching Chrome. Run
the TTL selection separately from ordinary browser tests; those retain global
fleet isolation. Omitting the TTL setting
skips the real-Chrome case, which is not a pass. Native steering requires normal
worker admission, uploaded bytes, confirmed automatic delivery and an original
terminal task with evidence. Its preflight refuses a denied host before creation.

The live phase also runs two three-worker coordination journeys: all peers in one
group, then implementation in one group and review/integration in another. The
reviewer must reject a seeded defect, the author must revise, the reviewer must
approve, and the consumer must finish its dependent task. Durable message origin
and ordering, actual peer task IDs, final board state and independent artifact
checks are required; an operator writing a review file cannot substitute for the
peer-message evidence. The deterministic policy test separately checks discovery,
peer task reads, explicit cross-group denial and same-group isolation.

The lab is separate because the ordinary browser harness disables host-wide fleet
jobs and seeds a fake provider key. Those prerequisites cannot prove autonomous
work. Do not use a production URL. The live test creates one uniquely named worker
through the UI with three real deliverables. After submission the observer only
reads state: it does not PATCH tasks forward or supply their evidence. It records
the timeline, final card details, actual file bytes, and a rendered HTML artifact.
A timeout is a failure, with the unfinished tasks retained for diagnosis.

A focused development run is `browser --project desktop --grep LC-BOARD`.
`--binary /path/to/server` saves build time but records unverified source provenance;
it does not prove that binary corresponds to this checkout. Every invocation gets
a fresh output directory. Never combine stale evidence from previous runs.

## Coverage and verdicts

The runner emits `index.html`, `summary.json`, per-stage logs, the Playwright HTML
report, traces, videos, screenshots, discovered control inventories, and a copy of
`cases.json` initialized to NOT_RUN. PASS, FAIL, INCOMPLETE and NOT_RUN are distinct.
A skipped or unavailable real provider is not a pass. Exit 1 means failure, 2 means
incomplete, 0 means only that the selected automated scope passed. A `full` run
stays INCOMPLETE until a reviewer finishes the guided/visual ledger; there is no
automatic claim of total UI coverage. The ledger is a review artifact, not a way
to override automated failures.

For each case below record viewport/provider, exact action, entity IDs, screenshot,
read-only API or file proof, result and any failure reason. Follow each discovered
control into its dialog/popover and add a ledger row for new controls. A control
merely being present, or a mocked response rendering, is not a successful effect.
Do not equate crawler fixture coverage with shipped-dashboard coverage.

For **every button, menu item, tab, link, input, select, switch, draggable row and
keyboard shortcut** in each surface: open/use it, check the visible outcome, verify
persisted state where applicable, reload, cancel without mutation, try invalid and
empty input, and repeat at phone width. Also check keyboard focus, Tab order,
Enter/Escape, screen-reader name, disabled/loading state, clipping, touch reach,
long text and back navigation. Do not force-click or replace DOM state to make a
journey pass. For external sends use only a configured test sink and explicit
recipient authorization; otherwise leave the send case INCOMPLETE.

Inspect the images: desktop, 375px, real WebKit, light/dark, empty/populated,
loading/error, long text and on-screen keyboard. Record actual defects, including
which control is obscured. Retain the screenshot before changing anything.

## Ordered acceptance cases

Run in the order below within a scratch installation. The matrix is a canonical
operator script for coverage that is not yet fully automated. Automated tests are
supporting evidence; their existence does not pre-mark any of these rows PASS.

'''
    # Preserve the maintained provider/run notes outside generated case rows.
    existing = (ROOT / 'docs/consolidated-lifecycle.md').read_text()
    notes_marker = '\n## Running the same lifecycle with Gemini'
    notes = notes_marker + existing.split(notes_marker, 1)[1] if notes_marker in existing else ''
    parts = [head]
    for c in cases:
        parts.append(f"### {c['id']} — {c['surface']}\n\n{c['actions']}\n\nPass requires: {c['expected']}\n\nSupporting coverage: {', '.join('`'+s+'`' for s in c['sources'])}.\n\n")
    parts.append('''## End-state and cleanup record

For every created worker, card, dependency, schedule, browser profile, group,
file, memory and external fixture, record its ID and final state. Completed work
must have a real artifact and command/result evidence. Review/blocked/needsyou is
not completion; report the exact blocker and next action. Confirm leases/active
indicators settle, no duplicate dispatch occurs, and no abandoned recurring job
continues running. Stop/archive only the run-owned workers and schedules after
capturing evidence. Keep the run directory and outputs until reviewed.

When a task stalls, distinguish a pending interval/mid-turn from a gate,
dependency, WIP cap, missing provider, failed delivery or exhausted budget. Capture
`/api/board/ready?session=...`, `/api/debug/board-drive`, health build, and the card
history. Do not repair the state from the observer to turn a failure green.

Source inventory is generated on every run with SHA-256 hashes. New browser specs
are automatically included. Review uncommitted tests in other checkouts separately;
a stable run of this checkout cannot certify concurrent drafts. The inherited
Playwright startup banner describes configured browser targets; the report’s
Selection field and executed test counts are the authoritative scope of this run.
''')
    (ROOT / 'docs/consolidated-lifecycle.md').write_text(''.join(parts) + notes)

if __name__ == '__main__': render()
