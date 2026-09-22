# herdr / tmux send parity

Deliverable for AMUX-2667. This is the checklist its trigger refers to. It
migrates nothing.

Measured 2026-09-22 against `origin/main`.

## The comparison in the card is between two different layers

AMUX-2667 and the merged AMUX-2836 both frame this as "herdr `send_text` is
~15 lines against ~331 lines of tmux `send_text_inner`". Re-measured today
those numbers are **11** and **601**. But the ratio is not the finding. The
layering is.

`backend/mod.rs:188` documents the trait default, and says so outright:

> Default = unsupported, so a backend with no delivery verb reports that
> instead of silently dropping a message; herdr overrides it. tmux keeps its
> own delivery path in session_verbs (the escape/C-u/paste-buffer discipline
> and its read-back verification), which this must not duplicate.

So:

| | `SessionBackend::send_text` | delivery choreography |
|---|---|---|
| **tmux** | not implemented (inherits the "unsupported" default) | `api/session_verbs.rs::send_text_inner`, 601 lines, **above** the seam |
| **herdr** | overrides it, 11 lines | `backend/herdr.rs::do_pane_prompt` (`pane send-keys ctrl+u`, then `agent prompt`) |

`grep send_text crates/amux-server/src/backend/tmux.rs` is empty. The two are
not competing implementations of one interface. Growing herdr's trait impl can
never reach parity with a function tmux itself does not implement as a trait
method.

**herdr does not lack these behaviours. It is routed around them.**
`send_text_inner:8857` is an unconditional early return:

```rust
if backend_of_cfg(&cfg) == "herdr" { return herdr_send(name, text).await; }
```

Everything numbered 6 and above below is therefore tmux-only *by
construction*, not by omission. Order matters: behaviours 1-4 run before
that branch and DO apply to herdr.

## What herdr already has

From `session_verbs.rs:967-993` (`herdr_send`) plus the four pre-branch gates:

| # | Behaviour | Where |
|---|---|---|
| 1 | Per-lane send serialisation (lane lock) | pre-branch, 8816 |
| 2 | Isolation gate (no automation into an isolated lane) | pre-branch, 8832 |
| 3 | Paused-lane refusal for automation | pre-branch, 8846 |
| 4 | iTerm2 backend refusal | pre-branch, 8853 |
|  | Single pane resolution for probe + read + delivery | `herdr_send` 968 |
|  | Backend status check, unreadable server is not a stopped worker | `herdr_send` 977 |
|  | Resume-picker guard on a 15-line capture | `herdr_send` |
|  | Composer clear before write (`pane send-keys ctrl+u`) | `herdr.rs::do_pane_prompt` |

Six of the thirty-eight, and the composer clear is the only piece of the
keystroke discipline it reproduces.

## What it does not have

Marked **absent** (would need building), **not-needed** (herdr's daemon owns
it), or **unknowable** (cannot be decided until the daemon runs).

| # | Behaviour | tmux lines | Status for herdr | Incident on record |
|---|---|---|---|---|
| 7 | Boot-UI wait: do not type into the launch shell | 8865-8883 | **absent** | AMUX-3055: a lane booted without the skip-permissions flag was never detected ready; the create-modal start prompt was dropped |
| 8 | Resume-picker guard (main path) | 8884-8886 | present in `herdr_send` | *none recorded* |
| 9 | Background-manager collapse, verify, then deliver | 8887-8932 | **absent** | AMUX-2681: the `amux` lane unreachable **5.04h** across 12 sends from 3 clients; AMUX-2680, a lane quoted the headline in its own report and locked itself out for **6.6h** |
| 10 | Wake detection (shell prompt or dead session) | 8933-8938 | **unknowable** | AMUX-4826: `pgrep` reports the pid, never the state |
| 11 | Defer without restarting when a boot is in flight | 8939-8945 | **absent** | AMUX-3055: used to return silently, leaving no trace of a dropped prompt |
| 12 | No env file -> "not running" | 8946-8948 | **absent** | *none recorded* |
| 13 | Auto-wake: start, then deliver once ready | 8949-8958 | **absent** | parity with py:25463 |
| 14 | Empty send at a selector -> press Enter | 8959-9003 | **absent** | AMUX-3054, generalising AMUX-2952: re-typing the label into a key-reading UI is swallowed whole while the response still says "sent" |
| 15 | Footer bail-out ("to navigate" / "enter to select") | 9004-9015 | **absent** | *none recorded* |
| 16 | `❯` suggestion extraction, numbered-picker Enter fallback | 9016-9074 | **absent** | AMUX-2952: Ethan live: "i keep sending enter when it needs input and its not doing anything" |
| 17 | Structured turn-boundary gate for steering | 9075-9082 | **absent** | a derived idle is usable only with positive evidence |
| 18 | Three-way (generating, waiting) determination | 9083-9090 | **absent** | D1 exit: reported state outranks the scrape |
| 19 | Resume-mode selector auto-answer (digit `1`) | 9096-9120 | **absent** | the 2026-08-19 panic restart parked a third of the fleet on this selector |
| 20 | Rate-limit menu: stamp, then answer "stop and wait" | 9121-9159 | **absent** | AMUX-2820: permanent deadlock on mvs-infra, two messages queued **400s+** behind a menu nothing would answer |
| 21 | `defer_if_busy` park with a NAMED guard | 9161-9174 | **absent** | AMUX-3764: an empty guard laundered an automated send into an owner send |
| 22 | Steering refusal at a live selector, deadline does not override | 9175-9181 | **absent** | the AskUserQuestion kill, 2026-07-15 |
| 23 | Picker-shaped text routed to paste, not deferred | 9182-9204 | **absent** | the "15:06 @image ghost", 2026-07-10; typed mid-turn lost 1/1, pasted accepted 4/4 |
| 24 | Pane-exclusive send lock across type/paste + Enter + verify | 9205-9211 | **not-needed?** | AMUX-2629: the ghost-rescue sweep must not fire an Enter mid-type |
| 25 | `sent_at` evidence window | 9212-9214 | **absent** | an older identical message must not count as this send |
| 26 | Stale-idle-hook live re-read | 9215-9235 | **absent** | a Stop hook authorises draining the queue, not pressing Escape forever |
| 27 | Fresh generating re-check immediately before the Escape | 9236-9261 | **absent** | an unscoped substring match froze the queue **4h**; re-scraping over the hook froze it **2h+** |
| 28 | Paste-vs-type decision | 9262-9279 | **absent** | AMUX-2909: every short human message took the lossy mode precisely when the lane was busiest |
| 29 | Mid-turn delivery-mode telemetry | 9280-9315 | **absent** | AEAB-25: 25 of 25 records all-time were the mode the sentence calls correct; it has never fired on the condition it describes |
| 30 | Composer clear (`C-u`) before every write | 9316-9317 | **present** | the 40ms that follows it has no recorded justification |
| 31 | Paste path: named buffer via temp file | 9318-9338 | **not-needed** | herdr's `agent prompt` takes the text as an argument |
| 32 | Type path: `send-keys -l` | 9339-9341 | **not-needed** | as above |
| 33 | Provider-specific composer settle before Enter | 9342-9358 | **unknowable** | measured: muse needs 300ms, Claude 20ms. Comment says 300, constant ships 350 |
| 34 | Legacy picker-closing Escape, >=1.3s spacing | 9359-9374 | **not-needed** | already unreachable on tmux |
| 35 | The submit keypress | 9375 | **not-needed** | `agent prompt` submits |
| 36 | **The evidence gate (`verify_submitted`)** | 9376-9395 | **absent, and the decisive one** | AMUX-2629; AC-271 (9 workers across 3 customer envs reported sent and never ran); AMUX-3876 (`submit_verdict=confirmed` while the lane held the paste unsubmitted **50 minutes** later); AMUX-3870; AMUX-3880 |
| 37 | Mid-turn bare-Enter retry | 9396-9408 | **absent** | Escape mid-turn is an interrupt |
| 38 | Outcome classification into exact strings | 9407-9409 | **absent** | downstream `submit_verdict_of` / `submission_verdict` key off these exact strings (ATE-75, AMUX-2643, AMUX-3541) |

## The one that decides the sequence

**#36, the evidence gate.** Everything above it proves only that bytes reached
the pty. `verify_submitted` (8155-8305) reads Claude Code's own artifacts back:
five looks at 300ms, two consecutive clear frames before Confirmed, two
consecutive stuck frames before acting, then durable JSONL evidence
(`queue-operation: enqueue`) or the muse transcript, and an honest third state
`Unverified` when no composer could be read at all.

herdr has no equivalent. `do_pane_prompt` returns `Ok(())` when the CLI call
succeeds, which is a claim about the CLI, not about the agent. That is exactly
the shape AC-271 describes: *send reported "sent", the schedule recorded ok,
and the worker never ran, 9 workers across 3 customer envs.*

Note the design preference already present in tmux's version: the durable half
(JSONL / transcript) is **backend-neutral filesystem evidence** and the code
prefers it over the pane. A herdr equivalent could reuse that half as-is. That
is the cheapest real parity work available and it does not need the daemon.

## Behaviours with no recorded reason

Parity cannot be decided for these, because the intent was never written down.
Listing them is part of the deliverable.

1. iTerm2 refusal (8853)
2. Resume-picker guard: the U+2315 requirement and four alternative phrases (1318-1325)
3. `!env_path(name).exists()` -> "not running" (8946)
4. The "to navigate" / "enter to select" footer bail (9004), referenced retrospectively at 9031 but never justified
5. The type path itself (9339, helper 832)
6. Timing constants: 40ms after `C-u`, 50ms after the leading Escape, 60ms after the closing one, 400ms after the background-manager Escape, 250ms boot poll, 30s boot deadline, 20s boot-in-flight window, 30s/60s `send_after_ready`, 1200ms settle, 300ms verify cadence and the `0..5` bound. By contrast 1300ms, 350ms and the 250ms log threshold **are** justified.

## Two internal inconsistencies

- 9354 ships `350` while 9344-9349 measures `300` as the working value.
- `verify_submitted:8209` caps `no_ui_looks <= 12` inside a `for _ in 0..5`,
  documented at 8191 as dead by construction and left wrong deliberately so the
  reader sees the intended number.

## Blocker, re-measured 2026-09-22

```
herdr on PATH     yes (/usr/local/bin/herdr);  opencode NOT on PATH
daemon            not running, no server at ~/.config/herdr/herdr.sock
socket            does not exist
lanes on herdr    0
tmux-hosted lanes 164          (card title says 115; 2026-08-24 triage said 119)
```

Six weeks, ratio unmoved, fleet up 49. The migration is not stalled. It was correctly
never started.

Three gaps are documented in `herdr.rs:24-34`, marked inline 9 times, and the
card tracks only the first:

- **GAP-EXIT-CODE**: herdr 0.8.0 reaps the pane and its single-pane workspace
  the moment the process exits (verified 2026-08-09: `exec sleep 3` -> gone
  from `workspace list` 1s later, `pane_get` -> `pane_not_found`) and exposes
  no exit status. `Completed`/`Crashed` are unobservable; a finished session is
  indistinguishable from one that never existed. **Restated, not closed.**
- **GAP-DIRECT-EXEC**: no verb runs a command *as* the pane process; `pane run`
  TYPES the line into the pane's shell. A stdin-reading shell profile can eat it.
- **GAP-ATTACH**: no per-workspace attach verb.

D6's CI claim verified: `.github/workflows/rust-nightly-deep.yml` says in its
own header that herdr is not installable on a hosted runner. `herdr.rs`'s tests
assert envelope *parsing* against real captured bytes; nothing spawns herdr or
touches the socket.

## The "~60 python-managed sessions" number cannot mean what it looks like

`managed_by` is an unconditional literal at `sessions_legacy.rs:3996` -
`"managed_by": "python",`, the only occurrence in the codebase, read by
nothing. It reports `python` for **164 of 164** sessions today and would report
exactly the same if every lane were herdr-backed. A count taken from this field
measured the field, not the fleet. Whatever the ~60 was, it did not come from
an instrument that can distinguish the two states. Filed separately.

## What this says about sequencing

The card's own sequence stands, with one correction: parity is not a matter of
growing an 11-line function. Choose the layer first.

1. **Start the daemon.** `herdr api snapshot` must return a real snapshot.
   Nothing below is testable until it does, and nothing above needed it.
2. **Decide the layer.** Either move the choreography below the seam so both
   backends inherit it, which is precisely ethos D6's stated exit ("AgentRuntime
   seam replaces per-site branches"), or accept delivery as backend-specific and
   build herdr's own #36. `AgentRuntime` does not exist; the only reference in
   the tree is a doc comment at `amux-core/src/lib.rs:28` naming it as that exit.
3. **Port #36 first, whichever layer wins.** Its durable half is already
   backend-neutral.
4. **Then one throwaway lane**, end to end: spawn, capture, send, status
   detection, steering delivery.
5. **Then 3-5 low-traffic lanes.** Never a customer lane mid-work.
