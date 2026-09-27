# Chat-mode streaming (AMUX-5263)

Chat workers run headless provider turns (`claude -p --output-format stream-json`,
`codex exec --json`). This doc records how their output reaches the dashboard
while it is generated, which libraries render it, and what a chat worker gets
from the harness compared with a terminal (coding) worker.

## Library decision

**Keep what the dashboard already ships, and change how it is driven.**

- **Markdown: marked + DOMPurify + remend** (vendored in `static/vendor/md.js`,
  pinned by `scripts/build-md.mjs`). Every markdown surface in the dashboard
  already uses marked, and remend is the healing step from Vercel's Streamdown,
  published on its own: it closes an unfinished fence, link or emphasis for one
  frame so raw markup never flashes.
- **Incremental rendering: block memoization in `app.js`**, the technique
  Streamdown uses in React. `marked.lexer` splits the streamed text; every block
  followed by a blank line is final, rendered once and frozen as DOM. Only the
  unsettled tail is re-parsed, once per animation frame at most. When the turn
  ends, the message is rendered from its full text, so the finished bubble is
  exactly what a full re-render produces (the e2e asserts this).
- **Animation: Motion (motion.dev, already loaded) for per-turn entrances** (the
  live bubble, each tool card), **CSS keyframes for the per-block fade, caret,
  spinner and thinking pulse**. Per-block work runs many times a turn, so it
  stays on the compositor. Everything honours `prefers-reduced-motion`.

Rejected:

- **streaming-markdown (smd)**: a good vanilla incremental parser, but it is a
  second markdown dialect. The finished message would either render differently
  from the stream or have to swap renderers at the end, which is the flicker
  this work removes. It also has no GFM tables.
- **Streamdown, Vercel AI Elements, assistant-ui**: React only. Used as reference
  designs (block memoization, healing, tool cards, collapsed reasoning).

## Streaming path

`provider stdout` → `chat_stream::Parser` → typed events → `Journal::push` (stamps
`seq`, `epoch`, folds into the live `Assembly`, keeps a bounded ring) →
`broadcast` → SSE `GET /api/sessions/{name}/chat/stream`.

Every event carries `type`, `turn_id`, `seq`, `epoch`. The SSE `id` is
`epoch:seq`.

| type | fields | from |
|---|---|---|
| `user` | text, origin, ts, waiting | turn start |
| `meta` | model | claude init |
| `phase` | phase (`requesting`) | claude status, codex turn.started |
| `thinking` | text | thinking_delta, codex reasoning |
| `delta` | text | text_delta, codex agent_message |
| `tool_start` | id, name | tool_use block start, codex item.started |
| `tool_args` | id, text, truncated | input_json_delta (preview) |
| `tool_input` | id, name, input, truncated | final tool input |
| `tool_result` | id, text, is_error, truncated | tool_result, codex item.completed |
| `usage` | tokens, cost_usd, duration_api_ms | result, codex turn.completed |
| `limit` | status, resets_at, window, utilization | rate_limit_event |
| `error` | text | provider error, spawn failure, idle timeout |
| `interrupted` | | owner pressed Stop |
| `retry` | | stale conversation reset, live view restarts |
| `done` | message (the persisted row) | turn end |
| `queued`, `stopped` | | queue and worker lifecycle |

**Resume.** `GET /api/sessions/{name}/chat` returns the live `Assembly` and the
`cursor` it corresponds to, taken under one lock. The client opens the stream
with `?after=<cursor>`; the browser's own reconnect sends `Last-Event-ID`, which
the server prefers. The ring replays exactly the missed events. A cursor from
another process (`epoch`), past the ring (`evicted`), or a lagging subscriber
yields `{"type":"gap"}` and the client refetches history. It never guesses.

**Bounds.** Ring: 4096 events and 4 MB per worker (`stream_ring` in the history
response reports what is retained and evicted). Tool results are kept to 8 KB,
tool args to 16 KB, thinking to 64 KB; each carries the dropped byte count and
the UI shows it.

**One assembler.** `Assembly::apply` (Rust) builds the live snapshot and the
persisted message; `_chatApply` (app.js) mirrors it. The persisted message holds
`text`, `thinking`, `tool_calls`, `usage`, `limit`, `interrupted`.

## UI

- Settled blocks are frozen DOM; the tail repaints at most once per frame, and
  backs off (up to 250 ms) when a paint costs more than two frames, so a phone
  stays scrollable while text flows.
- Auto-scroll follows only while the reader is at the bottom; scrolling up pins
  the view and shows a "Latest" pill.
- Stop sends `POST /api/sessions/{name}/chat/interrupt`. The partial reply is
  kept and marked stopped. Escape or C-c through the keys verb do the same.
- Tool calls are `<details>` cards patched in place (an open card stays open),
  with a spinner, then a green or red dot, the command or path as the summary,
  and input and result inside. Thinking is a collapsed `<details>`.

## Parity with coding workers

| capability | chat before | chat now |
|---|---|---|
| Status working/idle/error | partial: no report during a long silent turn, so it read idle after 120s | yes: 45s heartbeat while a turn runs |
| Tool lifecycle (PreToolUse/PostToolUse) | no | yes, reported through native_status with the tool name |
| Stop / interrupt a turn | no (only stop the whole worker, which drops the queue) | yes: route, Stop button, keys verb Escape/C-c |
| Board attribution | yes (headless_turn_prelude) | yes |
| Steering, queued sends | yes | yes |
| Owner-ask (turn_end.rs) | conditional: Stop carried no session_id | wired: Stop carries the conversation id (not exercised end to end here) |
| Promise nudge | conditional, same cause | wired, same fix (not exercised end to end here) |
| Rate-limit auto-resume | no: transcript unreadable for a default-dir chat | wired for claude (unit-tested parts only): `session_jsonl_path` resolves the chat dir, so the sweep's transcript check and auto-resume apply; the limit also shows live in the bubble |
| Deploy-wake | yes | yes |
| History (cmd_history, session_events) | yes | yes |
| Transcript history for default-dir chat | no | yes (same `session_jsonl_path` fix) |

Not closed:

- **Codex rate limits.** The sweep's transcript check is claude only, so a
  codex chat worker shows the limit in the bubble but is not auto-resumed.
- **Notification / PermissionRequest / Subagent events.** A headless turn cannot
  answer a permission prompt, so there is nothing to report; subagent activity is
  not surfaced.
- **Duplicate legacy hook reports.** `claude -p` inherits the user's Claude Code
  hooks, and `hook-report.sh` fires because `AMUX_SESSION` is exported. They
  report the same states the adapter does; not measured as harmful.

## Fixed along the way

Boot recovery ran 3s after start and treated any turn in flight as cut off by
the restart, including one started in those 3s by this process. The false error
row took the `chat:{turn}:assistant` dedupe key the real reply needed. Recovery
now skips a lane that is busy in this process (`verdict=chat_recovery_skipped_live_turn`).

## Verification

- `cargo test -p amux-server --lib chat_` (parser on a real captured
  stream-json run, assembler, ordering, resume, gap, byte and event bounds).
- `node e2e/chat-streaming.mjs <amux-server> <shot-dir>`: boots this checkout's
  server on a throwaway home with `e2e/fixtures/fake-claude-stream.py` as the
  provider, and checks incremental text, a forced mid-stream reconnect with no
  duplicate or missing delta, the finished bubble against a full render, and
  Stop, at 390px and 1280px.
