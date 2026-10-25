# codex-skill reference

`codex-skill` (source `skills/codex/scripts/codex-skill.py`) is a read-only observer of Codex
threads. This page records what each Codex interface actually exposes on this machine, how
status is decided, and the output shapes. Every claim below was measured on 2026-10-23 against
codex-cli 0.162.0-alpha.17.2 and the Codex desktop app (ChatGPT.app, Codex framework 155.0),
442 local threads.

## Capability matrix

| Need | Desktop app threads | CLI / TUI threads | `codex exec` runs | App Server clients (codex-new.py) | Subagent threads | Codex Cloud tasks |
|---|---|---|---|---|---|---|
| Discover | yes, `thread/list` (originator `Codex Desktop`) | yes, `thread/list` | yes, but only with `sourceKinds` set | yes, `thread/list` | only from state_5.sqlite: `thread/list` drops threads with no user message | yes, `codex cloud list --json` (0 tasks on this account) |
| Title, cwd, model, created/updated | yes | yes | yes | yes | yes, via `thread/read` | partial (task fields) |
| Repository and branch | state_5.sqlite only (`gitInfo` is null in the API) | same | same | same | same | from the task |
| Full history | yes | yes | yes | yes | yes | no JSON history API (`cloud status`/`diff` print text) |
| History from API | only if the thread has a projection in thread_history_1.sqlite (1 of 442 here; live threads get one) | same | same | same | same | n/a |
| History from rollout file | yes, `.jsonl` or `.jsonl.zst` | yes | yes | yes | yes | n/a |
| Tool calls, outputs, errors | yes (commandExecution, mcpToolCall, fileChange, webSearch, ...) | yes | yes | yes | yes | diff only |
| Live status | inferred: the app's app-server is a private stdio pipe | inferred | inferred | inferred | inferred | `codex cloud status` |
| Event subscription | no: events go only to the client that loaded the thread, and loading it is `thread/resume` (a write) | no | no | no | no | no |
| Pending approvals | no: approval requests reach only the owning client and are not persisted | no | no | no | no | no |
| Queued inputs | yes, count from queue_1.sqlite | yes | yes | yes | yes | n/a |

Not used, and why:

- `~/.codex/ipc/ipc.sock`: the desktop app's private IPC, undocumented.
- The app-server daemon (`codex app-server daemon`, `codex agents`): its control socket did not
  exist on this machine, so a daemon-owned thread could not be tested. `capabilities` reports it.
- `codex resume` / `codex fork` / `codex migrate-rollouts --apply`: all change state.
- UI scraping of the desktop app: structured access exists for everything it shows.

## Three API behaviours the tool works around

1. **Status of another process's thread is wrong in the API.** A second app-server reports a live
   Desktop turn as `interrupted` with `completedAt: null` and the thread as `notLoaded`, while the
   rollout was being written every few seconds. The tool classifies an unfinished turn from facts
   instead (see Status).
2. **Empty pages without a projection.** `thread/turns/list` and `thread/items/list` return an empty
   page for a thread no app-server has projected, even when its rollout holds hundreds of items. An
   empty API page on a thread whose rollout exists is read from the rollout, and the output names
   the source in `history_source` (`app_server` or `rollout`). Both use the same checkpoint, so a
   reader can switch between them.
3. **`thread/list` writes and filters by default.** Without `useStateDbOnly: true` it scans rollouts
   and repairs state-DB metadata. Without `sourceKinds` and `modelProviders: []` it hides exec,
   subagent and non-default-provider threads. Even then it omits threads with no user message (85
   of 443 here, mostly subagents), which `list` adds from state_5.sqlite. `n_listed_by` says how many
   came from each.

## Status

`status.state`, with `status.basis` saying how it was decided:

| state | basis | meaning |
|---|---|---|
| `idle` | observed | last turn has a completion record |
| `interrupted` | observed | last turn was aborted (`turn_aborted`) |
| `failed` | observed | last turn ended with an error |
| `new` | observed | thread exists, no turns |
| `running` | inferred | last turn has no end record and the rollout was written in the last 180s |
| `waiting` | inferred | no end record, no writes for 180s+, a live process holds the rollout open: an approval, input, or a long silent command |
| `abandoned` | inferred | no end record, no process holds the rollout, no recent writes: the process exited mid-turn |
| `open_turn_quiet` | inferred | no end record, no recent writes, lsof unavailable so waiting and abandoned cannot be told apart |
| `unknown` | unavailable | no API history and no rollout file |
| `not_computed` | skipped | `list` skipped it (older than `--status-days`, default 7) |

`status.evidence` carries the facts: `history_source`, `rollout_last_write_age_s`,
`rollout_held_open_by` (pid and command from lsof), `history_projection_status`,
`queued_inputs`. `list --active` returns `running`, `waiting` and `open_turn_quiet`.

## Checkpoints

A checkpoint is an integer rollout ordinal: the line number of the `item_completed` record of the
last item returned. `read`, `events` and `watch` return one; pass it back as `--since` to get only
items completed after it. Items are written once, when they complete, so an exclusive ordinal
cursor cannot skip or repeat one (checked: `rollout_ordinal == updated_at_ordinal` for all 993
projected items). A command still running is not listed until it finishes.

## Output

Every command prints one JSON object; `watch` prints one per line. Schema:
`skills/codex/schemas/codex-skill.schema.json`. Common fields: `tool`, `version`, `command`,
`generated_at`, `ok`, `measured`, `redactions`. A failure is `ok: false` with `error`
(`thread_not_found`, `thread_ambiguous`, `codex_not_found`, `app_server_error`,
`app_server_timeout`, `zstd_missing`, `method_not_allowed`) and `message`. Exit codes: 0 ok,
1 failure, 2 thread not found or ambiguous (`candidates` lists matches).

Items (`read`, `watch`) are normalised: `kind`, `id`, `turn_id`, `started_at`, `completed_at`,
`role` (`user`, `assistant`, `tool`, `system`), then by kind: `text` (messages), `command`, `cwd`,
`status`, `exit_code`, `duration_ms`, `output` (commands), `server`, `tool`, `arguments`,
`result`, `error` (MCP calls), `summary` (reasoning; usually empty because reasoning is
encrypted), `detail` (every other kind, passed through). Strings longer than `--max-chars`
(default 4000) end in `[... truncated, N chars total]`; `--full` turns that off.

Commands rebuilt from a rollout are `shlex`-joined, so their quoting can differ from the API's
rendering of the same command.

## Safety

- The App Server client refuses any method outside `initialize`, `thread/list`, `thread/read`,
  `thread/turns/list`, `thread/items/list`, `thread/loaded/list` (`method_not_allowed`).
- SQLite stores are opened `mode=ro`. `auth.json` and `config.toml` are never opened.
- Every output string passes a redactor (OpenAI/Anthropic-style keys, GitHub and Slack tokens,
  AWS and Google keys, JWTs, bearer tokens, private keys, `*PASSWORD=`/`*TOKEN=`/`*SECRET=`
  assignments); `redactions` counts the hits. The redactor is a regex, so treat it as a net,
  not a guarantee.
- `codex cloud list` writes `error.log` into its cwd; the tool runs it in a temporary directory.

## Examples for agents

```bash
CS=~/.amux/scripts/codex-skill.py

# What is Codex doing right now?
$CS list --active | jq '.threads[] | {id, name, source, state: .status.state, detail: .status.detail}'

# Everything about one project, newest first
$CS list --project mixpeek --days 14 | jq '.threads[] | {id, name, branch, state: .status.state}'

# Read a thread, then only what is new since
$CS read '#amux codex-skill e2e' > /tmp/t.json
CK=$(jq .checkpoint /tmp/t.json)
$CS read '#amux codex-skill e2e' --since "$CK"

# Commands it ran, and which failed
$CS read <thread> --types commandExecution | jq '.items[] | {command, exit_code}'
$CS events <thread> | jq '.events[] | select(.failed)'

# Find work by keyword across sessions (titles, messages, commands, outputs)
$CS search "MXP-GS12" --days 30 | jq '.matches[] | {thread_id, name, kind, excerpt}'

# Markdown transcripts of the newest threads, shown in the MDAI tab with no model call
$CS export --recent 5 --exclude '^#amux codex-skill' --out ~/.amux/local/codex-threads/latest-5.md
#   ~/.amux/local/Codex-Threads.mdai:  fetch: https://localhost:8824/api/file/raw?path=<that file>

# Follow a thread until it settles (bounded; NDJSON)
$CS watch <thread> --until-idle --duration 900
```

## Tests

- `python3 skills/codex/tests/test_codex_skill.py`: unit tests on a synthetic rollout (parsing,
  checkpoints, redaction, the read-only allowlist) plus live tests on this machine's real
  threads (full coverage of the state DB, Desktop discovery, chunked reads equal full reads from
  both the API and a rollout, no unexplained status). `CODEX_SKILL_LIVE=0` runs only the unit tests.
- `skills/codex/tests/e2e-browser-roundtrip.sh`: starts a named Codex thread, has it do a browser
  task and post the result to this amux worker, and watches it with `codex-skill` until idle.
  It spends model tokens, so it runs by hand. Result on 2026-10-23: thread
  `01a16ae9-d14e-7931-ac76-eadfef1ec4a5` found by `list --active` on the first poll beside a
  concurrent Desktop thread, watched `running` then `idle` (12 items), its curl returned 200 and
  the result arrived in the worker. PASS.
- The first scripted runs failed for a reason outside codex-skill: `codex-new.py`'s detached child
  kept the caller's stdout, so `$(codex-new.py ...)` blocked until the turn ended and every poll
  saw an idle thread. The child now points its stdio at /dev/null, and the start record in
  `~/.amux/logs/codex-send.log` carries `returned_after_s` (1.1s after the fix).
