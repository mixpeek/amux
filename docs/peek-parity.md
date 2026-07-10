# Peek ↔ tmux parity — success criteria

The session peek must be a faithful window onto the tmux pane. Every peek bug
we have shipped traces back to violating one of the criteria below, so treat
this list as the contract: a change to peek, send, or status detection is done
only when it can point at the criterion it preserves, and the automated checks
in `tests/test_peek_parity.py` still pass.

**Source of truth:** the tmux pane (`tmux capture-pane`) for live state; the
pipe-pane log (`~/.amux/logs/<name>.log`) for anything that scrolled off; the
conversation JSONL for message-level history. Peek never invents state that is
not in one of those three.

## Criteria

### P1 — Content parity
The peek terminal shows exactly what `tmux capture-pane -e -S -N` returns for
the requested window: same lines, same order, no duplicates, no truncation of
the first or last line. ANSI → HTML conversion may restyle, never reorder or
drop.

### P2 — Scrollback exists
Claude Code runs on the terminal's **alternate screen**, so tmux keeps zero
history (`history_size: 0`) — the pane top is a hard cutoff, not a bug in
capture. Peek must not present that cutoff as "the beginning": the
*Load earlier output* bar above the live view pulls the pipe-pane log tail
(`GET /log?plain=1&tail_kb=N`) so history is reachable, clearly marked as log
content behind a seam ("end of log · live view below").

### P3 — Freshness
While a peek is open, a pane change is visible within ~2s (poll/SSE tick). The
footer timestamp reflects the last actual fetch, and the served `APP_VER`
matches the running server (a stale service worker shows in the footer — one
reload cycle behind is expected, more is a bug).

### P4 — Status honesty
The status badge is derived from the live pane tail and must never contradict
what a human would infer from iTerm:
- banners echoed from **subagents** or sitting in scrollback never flag the
  parent (limit detection is scoped below the session's last own output);
- a session the **user interrupted** sits at a usable prompt → `idle`, not
  "needs input";
- an actively generating session is never simultaneously "limited".

### P5 — Sends are exactly-once
One logical send = exactly one submitted message in the pane, across every
failure mode we have hit in production:
- **key traps** — two Escapes within ~1s read as a double-press and eat the
  pending input (Claude Code v2.1.205); all Escape pairs are spaced ≥1.3s and
  the trailing Escape is sent only for picker-opening text (`@`, leading `/`);
- **lost responses** — client retries carry a `msg_id`; the server dedups via
  the sqlite `send_dedup` table (persisted because the loss window *is* a
  server restart);
- **boot races** — a send within 20s of `last_started` waits for the
  in-flight boot instead of double-starting Claude (a second start causes a
  session-ID conflict whose auto-restart replays the message);
- **verification** — submission is confirmed against the pane (wrap-aware
  pending-input read, two consecutive clear looks), not assumed from
  `send-keys` succeeding.

### P6 — Keys are capture-justified
Automation never presses a key the current capture does not justify. Known
traps, learned the hard way: `Left` at the prompt opens *Background this
session?* whose **default stops all agents**; `x` in the agents panel kills
the selected agent; with agent rows hidden, `↓` opens the background-shells
manager instead. The agent switcher (`/agent-nav`) re-captures between every
step and refuses rather than guesses.

### P7 — Meta is honest about staleness
Derived UI (plan strip, task headline) must carry its provenance: the plan
strip appends "plan last updated Nh ago" when the newest task file is >6h old,
and a freshly restarted conversation (splash with zero turns) shows no plan at
all rather than a dead conversation's plan.

### P8 — Nothing user-authored is destroyed
Clearing/typing into the input box must never eat queued or user-composed
text: `C-u` only fires on amux's own delivery path, ghost-rescue only submits
text carrying amux's `[H:MM AM]` prefix, and switching agent views round-trips
without touching pending input.

## Automated checks

`tests/test_peek_parity.py` covers the parser-level invariants (P1 windowing,
P4 scoping, P5 wrap-aware pending, P6 panel parsing) as pure-logic tests, plus
optional read-only live checks against a running server (no contradictory
badges fleet-wide, peek == capture for a scratch pane). Run:

```bash
python3 -m pytest tests/test_peek_parity.py -q          # pure logic only
AMUX_LIVE=1 python3 -m pytest tests/test_peek_parity.py -q   # + live checks
```

UI-only criteria (P2 seam rendering, P3 footer, P7 strip suffix) are verified
manually or via the browser API; the doc section for each names what to look
at.
