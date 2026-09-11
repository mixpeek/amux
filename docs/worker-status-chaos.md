# Worker status audit and chaos coverage

AMUX-4420 / AMUX-4421, 2026-09-11.

## Vocabulary and authority

Execution state is separate from board/task state and from the browser's network
connection. The core has seven states (`amux-core/src/worker.rs`):

| Core state | Dashboard / legacy API | Meaning |
| --- | --- | --- |
| Stopped | `running:false`, blank legacy status; **Stopped** | No live worker process. Retained terminal output does not make it active. |
| Starting | `starting`; **Starting** | A session is being created or replaced. |
| Active | `active`; **Working** | A live turn or positively observed live tool/subagent is running. |
| Idle | `idle`; **Idle** | The worker is ready. A ready composer is not a question. |
| Waiting | `waiting`; reason-specific badge | A current input/permission dialog or another explicit wait. |
| RateLimited | `rate_limited`; **Rate limited** | Provider capacity/usage restriction; reset time is optional, never invented. |
| Error | `error`; **Error** | Execution failed and needs recovery or configuration. |

The compatibility API also exposes `blocked` (permission), `api_error` (a current
retryable provider 5xx), and `unattributed` (observed activity without a valid board
assignment). The last is a board reconciliation warning, not a new execution
state. “Queued behind”, “stalled”, and board synchronization badges describe work
assignment alongside the execution status. `waiting_reason` distinguishes
`permission_prompt`, `user_input`, and `rate_limit`; unsubmitted composer text has
its own explanation. A generic “esc to cancel” string does not establish input. A wait without a known human-input reason is labelled Waiting, not Needs input.

Sources: `FleetSignals::derive_status_explain` combines bounded reports, turn
boundaries, process/child evidence and current pane chrome. `/status-explain`
names the deciding rule. The preview enrichment pass may refine a reason but
must not downgrade a provider quota/error into a human question. The terminal
adapter emits events; the event reducer must retain the same meaning.

## Bugs corrected

* Claude's auto-resume footer was missed below the composer, while “esc to
  cancel” overwrote the status as human input. Its reset clock was also discarded
  by the credit-cap branch. The sweep and preview now recognize the current
  provider footer, retain its clock, and do not press a key for this warning.
* A ready composer emitted `Waiting(idle_prompt)`, which the reducer stored as
  Waiting. It now becomes Idle and keeps its original idle timestamp on replay.
* Rate-limit and error events could be followed by a ready-composer event from
  the same capture, overwriting the failure. Such captures no longer emit the
  contradictory waiting event.
* The legacy core-state projection emitted `rate-limited` while the dashboard
  expected `rate_limited`; substring matching could also confuse a reason with
  the state tag. Projection now reads the JSON tag.
* A quoted old picker/menu above the current empty composer could summon input
  or qualify for an automatic keypress. Current-prompt scoping excludes it.
* Starting and Error had no explicit dashboard badge/filter group in some
  views. Stopped terminal headers could retain an old Working badge.
* Opening a terminal derived follow behavior from temporary scroll geometry.
  Separate bottom-follow intent now survives live/history races, content reflow,
  and viewport resizing. User scrolling and explicit Locate/message navigation
  relinquish that intent; Jump to bottom restores it and flushes buffered output.

## Reproducible checks

```
AMUX_SESSION=codex scripts/safe-cargo.sh test -p amux-server --lib status_chaos
AMUX_SESSION=codex scripts/safe-cargo.sh test -p amux-server --lib backend::adapter::tests
AMUX_SESSION=codex scripts/safe-cargo.sh test -p amux-server --lib api::sessions_legacy::tests
AMUX_SESSION=codex scripts/safe-cargo.sh test -p amux-server --lib orchestrator::events::tests
AMUX_SESSION=codex scripts/test-contended.sh -p amux-server
```

`api/status_chaos_tests.rs` exercises model/effort variations, ANSI decoration,
CRLFs, blank rows, long old history, conflicting reports, process death, real
questions versus cancellation chrome, quoted menus, automatic reset and a stale
reset clock after its deadline. It includes a provider-chrome capture of
mixpeek-frustrations **after it recovered**, as a negative quota control. The
original quota frame is transcribed from the user's screenshot in `AUTO`.

The event-reducer chaos test applies repeated quota → ready → genuine-input
cycles through SQLite for Claude Sonnet/Haiku, GPT-5/Luna and Gemini Flash/Lite,
plus a provider 529 followed by a ready composer. Existing status tests cover
stale/restarted reports, structured completion, background tools and subagents,
provider-child probes that fail to measure, permission prompts, API limits and
authentication errors.

Browser specs (desktop 1280×800 and phone 375×667):

* `e2e/worker-status-chaos.spec.ts`: labels, filters, recovery, stopped precedence,
  and no input notification for a quota wait across the three provider families.
* `e2e/terminal-open-bottom.spec.ts`: both response orders, late width/height
  changes, manual scroll during a pending history response, and Locate.
* Existing `terminal-scroll-accuracy.spec.ts` and `worker-status-order.spec.ts`
  preserve navigation at multiple zoom levels and worker ordering.

These are controlled fault/state replays through production code. They do not
claim a real provider was rate-limited, disconnected, or crashed during a live
model call, and do not deliberately disrupt the user's workers or exhaust an
account's quota.

## Actual model smoke runs

Separate fresh, bounded headless calls with no task tools requested, 2026-09-11:

| Requested runtime | Observed outcome |
| --- | --- |
| Claude `sonnet`, low effort | Exact `STATUS_SMOKE_OK`; resolved to **claude-sonnet-5**, 6.6 seconds. |
| Codex `gpt-5`, low effort | Explicit HTTP 400: model unsupported with this ChatGPT account. Not a pass. |
| Codex `gpt-5.6-luna`, low effort | Exact assistant response `STATUS_SMOKE_OK`, 17.7 seconds. |
| Gemini `gemini-2.5-flash-lite` | Exact assistant response `STATUS_SMOKE_OK`, 3.3 seconds. |

These prove the successful model routes can complete a turn; the deterministic
replays above prove the adverse classification cases. Model capability and
harness classification are measured separately.

## Validation results

On the integrated v0.9.907 candidate:

* Status-filtered Rust tests: **80 passed, 1 ignored**.
* Provider adapter tests: **44 passed**.
* Quota/reset tests: **17 passed**.
* Desktop/phone browser checks: **56 passed**.
* Disabling the automatic-resume detector caused **3 named chaos failures**;
  disabling opening follow intent caused the terminal-open regression to fail.
  Both mutations were restored.
* The earlier broad server run recorded **2,278 passed, 8 failed, 7 ignored**.
  One failure caught loss of the credit-menu selector evidence and was fixed;
  all 44 adapter tests subsequently passed. The other seven were the live-host
  memory admission guard, five worker-start tests refused with HTTP 503 under
  that guard, and concurrent fleet discovery in the Git cache test. That broad
  run is not a clean-suite claim; its integration targets did not run after
  the library failures.

Screenshots were inspected at both desktop and phone widths. The report keeps
live model smoke results separate from controlled failure replays.
