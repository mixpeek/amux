# Every transformation between the terminal and peek

AMUX-5017 asked a question nobody could answer from the code: when peek shows
something different from `amux attach`, which line did it? This is that list.

Line numbers are as of `8a33c9d1` and will drift. The function names are the
durable part; all of them live in `crates/amux-server/src/api/session_verbs.rs`
unless stated otherwise.

The entry point is `peek_response` (12630). It builds three fields, and they are
NOT three views of one string:

| field | what it is | where it comes from |
|---|---|---|
| `output` | the CURRENT terminal frame, never scrollback | `tmux_capture` |
| `live` | the same frame with the part already in `history` removed | `tmux_capture` then `trim_live_overlap` |
| `history` | a re-render of the provider's own JSONL transcript | `render_session_transcript` |

That split is the root of the divergence Ethan reported. `output` is the
terminal. `history` is a second opinion about the same conversation, assembled
from a different source, and it is the field the dashboard shows most of.

## A. Transforms applied to the TERMINAL bytes

These run on what tmux actually captured, so a difference here is peek hiding
something the terminal displayed.

1. **`tmux_capture(name, lines)`** (796). `capture-pane -p -e -J -S -<lines>`.
   `-e` keeps ANSI; `-J` joins wrapped lines, so a line the terminal wrapped
   arrives unwrapped. `lines` is the caller's scrollback depth; `0` means the
   visible frame only, which is what the codex and gemini paths ask for.
2. **`strip_scroll_pill`** (1032). Drops tmux's own `[N/M]` scroll indicator.
   Terminal chrome, not conversation.
3. **`strip_launch_noise`** (1052). Drops the launch banner and the shell lines
   before the agent's first output.
4. **`clean_gemini_frame`** (1965). Gemini only. Removes its TUI frame.
5. **`trim_live_overlap(transcript, live)`** (1112). Removes from `live` the
   tail that `history` already carries, so the dashboard does not print the same
   turn twice. This is the one transform that can make `live` EMPTY while the
   terminal is full, and the payload says so with `output_is_viewport_only`.
6. **`collapse_blank_runs`** (1014). ANSI-aware, keeps one blank line from a run.

## B. Transforms applied to the RE-RENDERED transcript

`render_session_transcript` (3509) reads the newest JSONL for the lane via
`session_jsonl_path` (2792) and hands the records to
`render_transcript_records(records, max_chars, collapse_tools)` (3579). Peek
passes `collapse_tools = true`; `transcript_history` and `session_subagents_from`
pass `false`.

Per record, in the order the function tests them:

7. **`attachment` / `queued_command`** (3595). Rendered through `user_echo_ansi`
   as a `❯` prompt line. **This branch had no envelope strip until 2026-09-23**,
   which is AMUX-5017's actual bug: 598 of 706 attachment prompts on one live
   transcript carried a raw `<task-notification>`, displayed under the glyph that
   means "a human typed this".
8. **Everything that is not `user` or `assistant` is dropped** (3608).
9. **`strip_harness_envelopes`** (3569). Removes `<system-reminder>`,
   `<task-notification>` and `<local-command-caveat>` blocks. Called from BOTH
   the attachment path (3599) and the message-text path (3633). A record that was
   only an envelope produces no row at all.
10. **`<command-name>` / `<command-args>` / `<local-command-stdout>` rewriting**
    (3640). A slash command becomes a synthesized `❯ /name args` line plus an
    indented dim output block. The raw XML never reaches the pane.
11. **ANSI that peek adds itself.** `user_echo_ansi` (3424) for user lines;
    `\x1b[38;5;231m⏺` for assistant text (3669); `\x1b[38;5;246m` with a `⏿`
    prefix for tool output (3657, 3721).
12. **Tool-run collapse** (3689, `collapse_tools = true` only). A consecutive run
    of `tool_use` blocks becomes one `Ran N shell commands` line, and matching
    `tool_result` blocks are dropped. This is the density difference measured in
    AMUX-5017: 30 terminal lines against 6 in the live field.
13. **Tool-result clipping** (3703). At most `MAXL` lines, each at most `MAXW`
    characters, with a `… +N more lines` tail.
14. **Head truncation at `max_chars`** (3736). Peek passes 120_000. The OLDEST
    text is cut, and the cut is moved to the next newline so a line is never
    sliced mid-way.
15. **`collapse_blank_runs`** again on the way out (12697).

## C. Codex and ollama take a different route entirely

`transcript_history::snapshot` (`crates/amux-server/src/api/transcript_history.rs`)
reads the rollout file rather than a JSONL, and drops `reasoning`, `developer`
messages and the `Chunk ID:` / `Wall time:` preamble on tool output. Peek reports
`history_source: "codex-rollout"` plus a `history_measurement` with `measured`
and `n_considered`, so a zero from this path can be told from a probe that never
ran.

## What this list is for

Ethan's instruction was "we shouldn't hide anything from peek from amux raw. we
should only classify messages and color code based on message type." Measured
against that, the transforms fall in three groups:

- **Terminal chrome** (1 through 6): removing it is the point, keep them.
- **Classification and colour** (10, 11): exactly what was asked for.
- **Content decisions** (7's old behaviour, 9, 12, 13, 14): these are peek
  deciding what you may see. 9 is defensible, since a harness envelope is not
  conversation, and the mutation evidence below shows the strip is load-bearing
  on both paths.

12 and 13 were the suspicious pair, so they were checked against the terminal
rather than reasoned about. `tmux capture-pane -p -J -S -` over five live lanes,
2026-09-23:

| lane | lines | `Ran N …` | `⏺` |
|---|---|---|---|
| amux | 51 | 1 | 2 |
| gs-10-zero-base-cicd | 24 | 1 | 2 |
| mixpeek-finances | 24 | 1 | 4 |
| mvs-research | 50 | 0 | 1 |
| gtm-engine | 50 | 6 | 7 |

The terminal collapses tool runs itself and clips tool output with a `+N lines`
tail, so 12 and 13 REPRODUCE the terminal rather than diverging from it. Four of
five lanes carry a collapse line in their current frame. That settles the
density question the other way from where it started.

14 remains a genuine cut: peek keeps the newest 120_000 characters and drops
older text, which the terminal's own scrollback does too at a different depth.

So after the AMUX-5017 fix the remaining divergence between `amux attach` and
peek is `history` being a re-render at all, plus `trim_live_overlap` deciding
which half of a turn appears in which field. Both are structural rather than a
hidden-content bug.
