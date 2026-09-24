# Worker types

A worker's `worker_type` picks two things: how its turns execute and how its
output is shown. Everything else about the worker is shared across types.

```text
Worker -> WorkerType -> Execution adapter            -> Output renderer
coding -> terminal session (tmux/herdr), unchanged    -> terminal / peek
chat   -> headless provider turns (stream-json)       -> chat transcript
```

## Where it lives

| Piece | Location |
|---|---|
| Type id, descriptors, requirements | `crates/amux-core/src/worker_type.rs` (`REGISTRY`) |
| Adapter trait and registry | `crates/amux-server/src/api/worker_exec.rs` (`ExecutionAdapter`, `ADAPTERS`) |
| Chat adapter | `crates/amux-server/src/api/chat_worker.rs` |
| Env-file persistence | `CC_WORKER_TYPE` in `~/.amux/sessions/<name>.env`. Absent means `coding` |
| Store persistence | `_amux_workers.worker_type`, migration `0087_worker_type.sql`, default `coding` |
| Dashboard | `app.js`: `_workerTypes`, `_workerRenderer`, `_chat*`, `_selectWorkerType`, `_workerTypeSet` |

## The seam

The lifecycle code asks the worker's adapter first. It falls through to the
terminal pipeline only when the adapter answers `Dispatch::Terminal`:

- `session_verbs::start_session` (start, auto-start on create, board dispatch start)
- `session_verbs::stop_for_pause` and `stop_session_process` (stop, pause, archive, delete)
- `session_verbs::send_text_inner_bound`. Every producer converges here: owner
  send, peer messages, the steering queue (board dispatch, orchestrator,
  reminders) and schedules. It runs after the isolation, pause and project gates.
- `session_verbs::is_running` and the fleet list's `running` field
- `session_verbs::peek_verb` (same keys as the terminal shape: `output`, `history`, `live`)

The coding adapter answers `Terminal` for every operation, so coding workers
run through exactly the code they ran through before. Nothing outside
`worker_exec.rs` branches on a type name. Requirements are read from the
descriptor instead: the create path's worktree/provider validation, the
autostart spawn guard (`terminal`), the scratch working directory
(`project_dir`), and the dashboard's field visibility.

## Shared capabilities (both types)

| Capability | How a chat worker gets it |
|---|---|
| Identity, config, rename | Same env file and `/config` verb |
| Lifecycle | Same `start`/`stop`/`pause`/`archive`/`delete` verbs, dispatched to the adapter |
| Status and presence | Turn state goes through `native_status` (the channel provider hooks use), so the list, SSE and lease heartbeat need no chat branch |
| Boards, tasks, claims | The worker is an ordinary board owner. Each turn gets `AMUX_SESSION`/`AMUX_URL`, so `amux board ...` works from inside the chat |
| Messages, `@worker` | Same `/send` path and `cmd_history` ledger; peer sends arrive with the usual harness wrapper |
| Groups, scope env, connectors | `headless_turn_prelude` sources the same global -> group -> worker env layers as a tmux launch |
| Memory, gates, approvals, artifacts | Keyed by worker name, as for any worker |
| Schedules | `kind: tmux` schedules deliver through steering to the adapter |
| History and events | Messages are `session_events` rows of type `chat.message`. There is no chat-only table |
| Model/provider | `CC_FLAGS --model` / `CC_MODEL`; chat supports `claude` and `codex` |
| Persistence and resume | Conversation id in worker meta (`cc_conversation_id`, `chat_codex_thread`); each turn resumes it. A lost conversation is replaced once and logged |

## Chat adapter behaviour

- One provider process per turn. The prompt goes on stdin, never argv.
- Turns are serialized per worker. A message sent mid-turn is queued in memory
  and runs next. Automation that finds the worker stopped stays in the durable
  steering queue.
- An owner send to a stopped chat worker starts it. Automation does not.
- Live deltas: `GET /api/sessions/{name}/chat/stream` (SSE: `user`, `delta`,
  `tool`, `done`, `queued`, `stopped`, `lagged`). History:
  `GET /api/sessions/{name}/chat?limit=&before=`.
- A turn with no output for 15 minutes is killed and recorded as failed.
- Server restarts (the builder exec()s the server on every commit) do not lose
  work. The queue is persisted under `AMUX_HOME/chat-state/`, and the running
  turn is marked in meta. Boot recovery resumes queued messages. A turn the
  restart cut off gets an assistant message with an explicit error and is not
  re-run, because its tools may already have acted.
- The conversation id is owned by the adapter (`chat_conversation_id`) and
  mirrored to `cc_conversation_id` for the transcript readers. If the mirror is
  changed from outside, the adapter restores it and logs it.
- `AMUX_CHAT_CLAUDE_BIN` / `AMUX_CHAT_CODEX_BIN` override the provider binary
  (tests use a fake that speaks stream-json).

Log verdicts: `worker_exec_dispatch` (debug, per handled operation),
`chat_turn_completed`, `chat_turn_failed`, `chat_conversation_reset`,
`chat_conversation_mirror_repaired`, `chat_turn_interrupted`,
`chat_queue_recovered`, `chat_recovery_pass`, `chat_worker_stopped`,
`worker_type_changed`.

## API

- `GET /api/worker-types` lists the registry, which the create/edit UI renders from.
- `POST /api/sessions` and `POST /api/workers` take `worker_type`. Omitting it
  means `coding`. Impossible combinations get a 400 that names the reason, for
  example a chat worker with `worktree: true` or a provider the adapter cannot drive.
- `PATCH /api/sessions/{name}/config {"worker_type": ...}` stops the worker
  with its current adapter, writes the type, and restarts it with the new one
  if it was running. `PATCH /api/workers/{id}` classifies the change as
  `SessionRestart`.
- List rows carry `worker_type` and `renderer`.

## Adding a type

1. Add a `WorkerTypeDescriptor` to `REGISTRY` with its requirements and renderer id.
2. Implement `ExecutionAdapter` and add it to `ADAPTERS`.
3. If it needs a new renderer, add a case to `_workerRenderer`'s consumers in
   `app.js`. If it reuses `terminal` or `chat`, no dashboard change is needed.

None of the board, message, schedule, group or lifecycle code changes.

## Verification

`crates/amux-server/tests/worker_types_e2e.rs` takes one coding and one chat
worker through create, list, board, start, send, streamed reply, resume,
canonical events, peek, type edit, stop and the store API.
`crates/amux-server/tests/chat_worker_recovery.rs` pins restart recovery.

`e2e/worker-types-click.cjs` drives the real dashboard with clicks and taps
only (desktop and iPhone 13): create a chat worker from the + menu, watch a
reply stream in, queue two messages, use the shared tabs, send from the card,
stop and start from the menu, switch a coding worker to chat and back, reload,
and repeat the core flow on the phone. Run it against an isolated server; its
header has the exact command. Unset `TMUX` when you start that server, or it
will reach the real fleet's panes.
