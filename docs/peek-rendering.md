# How amux renders the peek

This lists every change amux makes to a worker's terminal output between tmux
and the peek on screen. The lists live in code and run as written:

- **Server:** `PEEK_SERVER_STEPS` in `crates/amux-server/src/api/session_verbs.rs`.
  Every peek response carries `render: {path, steps}`, naming the path it took and
  the steps that ran, in order. `GET /api/peek/steps` serves the list.
- **Dashboard:** `PEEK_STEPS`, `PEEK_PIPELINES` and `PEEK_FRAME_STEPS` in
  `crates/amux-dashboard/static/app.js`. Every render goes through `_peekRun`,
  which records each step's characters in and out and its time.

To see what happened to the frame on screen, open a worker's peek, then
**⋮ → Rendering steps**. It shows the server path and steps, the dashboard
passes with their effect, and the frame decisions, beside the registered steps
that did not run.

`tests/peek-render-registry.test.mjs` fails if a step or path exists in code
and is missing here, or if code names a step that is not registered.

## Server paths (which source the text came from)

| path | provider | history comes from |
|---|---|---|
| `tmux-scrollback` | Claude on the normal screen (default) | tmux's own scrollback above the screen. The thin pass-through: no transcript, no trimming |
| `claude-transcript` | Claude on the alternate screen, or after resume | the Claude conversation record |
| `codex-rollout` | Codex, Ollama | the Codex rollout file; tmux gives only the current screen |
| `gemini-frame` | Gemini | none; the cleaned current frame only |
| `session-log` | other providers, short screens | the saved session log, with the live screen appended |
| `tmux-capture` | other providers, long screens, or nothing saved | the capture itself |
| `tmux-live` | any (`live=1` polls) | none; the live screen only |
| `worker-adapter` | worker types with their own adapter | the adapter's own peek; no server steps run |

## Server steps

| step | what it does |
|---|---|
| `tmux-capture` | Read the pane with `tmux capture-pane -p -e` (colours kept), the last `lines` rows |
| `tmux-scrollback` | Read tmux's scrollback above the screen as history and the screen as live |
| `strip-scroll-pill` | Remove Claude Code's "Jump to bottom" overlay, which is not output |
| `clean-gemini-frame` | Gemini only: drop its repainted chrome from the captured frame |
| `claude-transcript` | Rebuild history from the Claude conversation record, with tables laid out for the reader's width |
| `codex-rollout` | Rebuild history from the Codex/Ollama rollout |
| `strip-launch-noise` | Drop the CLI's launch banner lines from the live screen |
| `trim-live-overlap` | Remove the start of the live screen that repeats the end of history |
| `collapse-blank-runs` | Squeeze runs of blank lines to one |
| `save-session-log` | Save a long normal-screen capture as the session log |
| `session-log` | Use the saved session log as history, appending the live screen if it is not already its tail |
| `transcript-for-torn-log` | The saved log was garbled by cursor repaints, so the transcript is rendered instead |

## Dashboard text passes

| step | what it does |
|---|---|
| `ansi-to-html` | Escape the text as HTML; terminal colours become styled spans; URLs and terminal hyperlinks become links |
| `link-paths` | File and folder paths become links that open in the viewer, resolved against the worker folder |
| `tag-prompts` | Wrap each prompt as a navigable message labelled with its sender. Prompt marker per provider: Claude `❯`, Codex `›`, Gemini `>`. The Codex composer is recognised here |
| `tag-prompts-thin` | Thin mode: label each prompt, leaving the text exactly as the terminal drew it |
| `collapse-tool-calls` | Claude Code: each tool call (`⏺ Bash(…)`) becomes a collapsed accordion; replies stay open |
| `fit-rules` | Horizontal rules (the input box border) are resized to the width instead of scrolling |
| `scroll-box-blocks` | Tables and framed boxes get their own horizontal scroller so columns stay aligned |
| `hang-indent` | A wrapped list or indented line continues under its own text |

## Dashboard pipelines, per provider

Each CLI draws its terminal differently, so each provider has its own pipeline.
A provider without an entry uses `default`.

| provider | `full` | input box (`composer`) |
|---|---|---|
| `claude` | ansi-to-html, link-paths, tag-prompts, collapse-tool-calls, fit-rules, scroll-box-blocks, hang-indent | `claude`: `❯` between two rules, then the `⏵`/shift+tab footer |
| `codex` | ansi-to-html, link-paths, tag-prompts, fit-rules, scroll-box-blocks, hang-indent (no `⏺` tool calls) | `none`: recognised in tag-prompts |
| `gemini` | same as codex | `none` |
| `default` | same as claude | `claude` |

All providers share `thin` (ansi-to-html, link-paths, fit-rules,
tag-prompts-thin), `thin-composer` (ansi-to-html, link-paths, fit-rules) and
`footer` (ansi-to-html).

## Dashboard frame decisions

| step | what it does |
|---|---|
| `wrap-at-pane-width` | Wrap at the worker's terminal width (`pane_cols`), not the browser window |
| `trim-live-overlap` | Drop the start of the live screen that repeats history already shown (a second check after the server's) |
| `split-composer` | Separate the input box at the bottom of the screen from the messages above |
| `show-draft` | Show text typed in the input box but not yet sent |
| `hide-steering-echo` | Hide input-box text that is a queued Steering message or the "Press up to edit queued messages" hint |
| `skip-unchanged` | Nothing changed since the last frame, so nothing was re-rendered |
| `hold-while-reading` | New output is kept off screen while you are scrolled up, searching or pressing, and painted when you return |
| `chat-owns-body` | The Chat tab is showing, so the frame updated in the background and was not painted |
| `painted` | The frame was painted |
