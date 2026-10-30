# Plan: first-class OpenCode provider for amux

**Fork:** `pedrovgp/amux` · **Branch:** `feat/opencode-provider` · **Base:** `main` (== upstream `1c7fbc5`)
**Target binary:** OpenCode `v2.0.32` for M0–M2 (measured 2026-10-17; the plan began
against `v2.0.28`), **`v2.2.3`** for M3 (the CLI moved 2.1.4 → 2.2.1 → 2.2.3 during M3
alone). Re-measure on upgrade — the version drift is real, not hypothetical.
**Status:** M0 + M1 shipped (PR #335, upstream `a95b05ab`); **M2 built** (ACP
transport, `crates/amux-server/src/opencode/acp.rs`); **M3 built** (`serve` HTTP
transport — see the M3 section below and `docs/serve-spike-results.md`).

## Why

amux ships six providers (`claude-code`, `gemini`, `codex`, `ollama`, `grok`, `muse`)
and no OpenCode. The `opencode/` module in the tree is amux's *internal*
`AgentProtocol`, not the OpenCode CLI. We add OpenCode as a real runtime so
workers can run it.

## Definition of done

An OpenCode worker can be **registered, started, sent a prompt, peeked,
steered, stopped, and (structured) tracked** by amux — verified by a live
smoke test and by `cargo test -p amux-server opencode`.

---

## Integration seams (the four places a provider touches)

| # | File | Change |
|---|---|---|
| S1 | `crates/amux-server/src/provider/static_providers.rs` | add `OpenCodeAdapter` |
| S2 | `crates/amux-server/src/provider/mod.rs` | register it in `default_registry()`; bump the count test 6→7 |
| S3 | `crates/amux-server/src/opencode/structured.rs` | add `CliProvider::OpenCode` (binary, args, `conversation_ref`, `envs`) + `from_provider_id("opencode")` + `family()` |
| S4 | `crates/amux-server/src/opencode/events.rs` | add `translate_opencode` |

Plus two call-site seams that must stay consistent:

| # | File | Change |
|---|---|---|
| S5 | `amux` (bash CLI, ~line 1114 command build) | `CC_PROVIDER=opencode` branch for the interactive tmux launch |
| S6 | `docs/provider-coverage.csv`, `docs/opencode-spike-results.md` | record the measured coverage |

`CliProvider` is deliberately closed (`structured.rs:79-88`): a provider not
listed is terminal-hosted only. So M0 (terminal) needs only S1/S2/S5; M1
(structured) adds S3/S4.

---

## Measured facts (this machine)

```
$ opencode run --format json --model opencode-go/deepseek-v4.1-flash "Reply with exactly one word: ready"
{"type":"step_start","timestamp":...,"sessionID":"ses_ec340a338ffeqYv0Ij0TkzlBuv","part":{"id":"prt_...","type":"step-start"}}
{"type":"text","timestamp":...,"sessionID":"ses_...","part":{"id":"prt_...","type":"text","text":"ready","time":{"start":...,"end":...}}}
```

- Output is **JSONL**, one object per line, with a top-level `type` and a stable
  `sessionID` (`ses_...`) carried on every line.
- Observed types so far: `step_start`, `text`, `error`. A quota failure emits
  `{"type":"error","error":{"type":"provider.quota","status":429,"message":...}}`
  — mappable to `RateLimited`.
- Session continuity: `--session <id>` **creates if missing**, or `--continue`.
- Model format: `provider/model#variant` (e.g. `opencode-go/deepseek-v4.1-flash`).
- Approval: `--auto` auto-approves; **needed for headless** or a write hangs.
- `opencode serve` (v2 HTTP API) and `opencode acp` (Agent Client Protocol)
  exist — the path to richer events later (M2).

**Still to measure before M1 code:** the full event set for a *tool-using* run
(`tool_use`/`step_finish` shapes), resume after process death, and behavior with
no `--model` set.

*(Measured 2026-10-17, during M2 — see the M2 section below: ACP carries the
tool-using event set (`tool_call`/`tool_call_update`, with `rawInput`, `content`
and a `diff` for edits), continuity is `session/resume <id>`, and an unset model
leaves the session on the agent's own default. The `run`-path shapes above are
kept as the fallback transport's record.)*

---

## Staging

### M0 — terminal-hosted OpenCode worker (minimal, unblocks everything)

Goal: amux manages an `opencode` worker in tmux via its existing terminal
backend (send-keys / capture-pane). Full worker lifecycle, board, steering,
peek — no structured events yet.

1. `OpenCodeAdapter` in `static_providers.rs`:
   - `id` → `ProviderId::new("opencode")`
   - `capabilities` → `ProviderCapabilities` with the honest terminal-only profile
     (no hooks; `PromptMode::Interactive => vec!["opencode"]`,
     `HeadlessStructured => vec!["opencode","run","--format","json"]` so the
     command is never empty for conformance).
   - `usage()` → `ProviderUsage::unknown(id)` (never invent numbers).
   - `models()` → parse `opencode models` (best-effort; empty on failure).
2. Register in `default_registry()`; update `default_registry_registers_all_known`
   (`reg.len()` 6→7) and add `"opencode"` to the id list.
3. Bash CLI: add an `opencode` branch to the interactive command build
   (`cmd="opencode ${CC_FLAGS:-}"`), before the `else` claude fallback. Prefer
   routing interactive start through `_start_via_api` so the provider→binary map
   has one home (`amux` line ~1082 warns about exactly this class of bug).
4. `conformance_static`/`conformance` test for the adapter.

**Exit:** `amux register oc --dir <proj>` with `CC_PROVIDER=opencode`,
`amux start oc`, send + peek work.

### M1 — structured headless (the event path)

1. `CliProvider::OpenCode`:
   - `binary()` → `"opencode"`
   - `from_provider_id("opencode")` → `Some(OpenCode)`; `family()` → `"opencode"`
   - `args(prompt, model, resume)` →
     `["run","--format","json"] ++ (["--auto"] as needed) ++
      (["--model",m]) ++ (["--session",r] | []) ++ [prompt]`
   - `conversation_ref(line)` → `sessionID` (capture only from `step_start`, so a
     dead-session error can't re-arm the ref — mirrors the claude rule)
   - `envs()` → whatever opencode needs to be non-interactive/untrusted-dir-safe
2. `events::translate_opencode`: `step_start`→`TurnStarted`, `text`→`Progress`
   (text from `part.text`), `error`→`Failed` (or `RateLimited` when
   `error.type` is quota/429), EOF→`TurnCompleted`.
3. Unit tests replaying captured JSONL fixtures through the full
   spawn→read→translate path (the existing `fixture_script` harness), plus a
   `--resume` argv assertion.

**Exit:** a headless OpenCode turn produces the same lifecycle event sequence
as the other providers in `structured.rs` tests.

### M2 — richer protocol (optional, later)

Use `opencode acp` or `opencode serve`'s v2 API for tool-level events,
mid-turn message delivery, and real approval handling. Only if M1's limitations
bite (they will for long tasks: a one-shot `run` has no mid-turn stdin).

**STATUS: BUILT (2026-10-17) over `opencode acp`, re-measured on v2.0.32.**
The transport lives in `crates/amux-server/src/opencode/acp.rs` and is selected
inside `StructuredCliProtocol` for OpenCode workers (`AMUX_OPENCODE_ACP=0` opts
back into the M1 `run` path). What it delivers, and what it cannot:

| M2 goal | Result |
|---|---|
| Tool-level events | **Yes.** `tool_call` / `tool_call_update` carry the tool name, `kind`, `locations`, full `rawInput`, the tool's `content`, and a `diff` block for edits. |
| Real approval handling | **Yes.** When the worker's opencode config marks a tool `ask`, the agent sends `session/request_permission` (`once`/`always`/`reject`) and blocks until amux answers. |
| Mid-turn message delivery | **No — measured limit.** A second `session/prompt` while a turn runs is rejected: `-32603 Session already has an active ACP prompt`. Messages keep queueing at the turn boundary (`AtTurnBoundary`). True steering needs `opencode serve` (HTTP v2, `delivery: "steer"`). |
| Bonus: usage | `usage_update` (tokens + USD cost) and the prompt result's `inputTokens`/`outputTokens`/`thoughtTokens`/`cachedReadTokens`. |
| Bonus: continuity | `session/resume <id>` continues a persisted session; a refused id is dropped (same dead-ref rule as M1). |

Other measured protocol facts: the model is a config option
(`session/set_config_option`, `configId: "model"`, value `provider/model`;
there is no `session/set_model`), and `session/cancel` is a **notification**
(sent as a request the agent answers `-32601`).

#### The default model is pinned (2026-10-25)

An OpenCode worker that names no model runs
**`opencode/deepseek-v4.1-flash-free`** — *DeepSeek V4.1 Flash Free* — because
the agent's own default is not a fact amux can rely on. Measured on this box,
same binary, same account, bare `opencode acp` with no amux in the loop:

| model | result |
|---|---|
| the agent's own default | `503 Upstream request failed: Endpoint is unavailable` on every prompt (9 retries observed) |
| `opencode/deepseek-v4.1-flash-free` | `end_turn`, file written, turn over in 12s |

One constant, `opencode::DEFAULT_MODEL` (`crates/amux-server/src/opencode/mod.rs`),
read by every path that can carry it, so they cannot disagree:

- the structured transport (`run` and `acp` both pass it as `--model`, and ACP
  applies it with `session/set_config_option`);
- the worker row (`default_model_for_provider("opencode")`), so the model a
  worker runs is visible and editable rather than implicit.

**The interactive launch cannot take it, and must not try.** Measured
2026-10-25 on opencode 2.1.4: the TUI has no model flag (`opencode --help`
lists only `--standalone/--server/--auto/--continue/--session/--prompt`), and a
`--model` on that command line prints
`ERROR Unrecognized flag: --model in command opencode` and the lane never
starts — a live worker did exactly that the moment the launch began appending
the pin. `OPENCODE_CONFIG_CONTENT` is not a channel either: the same session
measured `{"model":"opencode-go/beam-free"}` being ignored (`run` stayed on its
own default) while `run --model opencode-go/beam-free` switched. So
`opencode_launch_opts` **strips** `--model`/`--model=`/`-m` (from `CC_FLAGS`
and from the resume choreography) and logs a WARN naming what it dropped. The
lane's *structured* turns still run the pinned model; its TUI does not, and
the log says so rather than pretending.

Two overrides, both intentional and both winning over the constant:

1. **Per worker** — `amux register NAME --provider opencode --model X`
   (`_amux_workers.model`, i.e. `--model` in its `CC_FLAGS`). It reaches the
   worker's structured turns; the interactive launch drops it (see above).
2. **Fleet-wide** — `AMUX_OPENCODE_MODEL=X` in the server environment
   (`~/.amux/server.env`), for a box whose accounts can reach a different model.

A blank value is *no* value: whitespace never becomes a model name.

---

### M3 — native HTTP transport (`opencode serve`)

**STATUS: BUILT, and `serve` is the DEFAULT when the box has a shared server.**
Measured record: `docs/serve-spike-results.md`. Code: `opencode/serve.rs` (client,
SSE hub, typed errors) wired in `opencode/structured.rs`.

The default is not a blind flip: an un-pinned OpenCode worker takes `serve` when
amux can discover the single `opencode serve --service` that opencode itself
runs (`~/.local/state/opencode/service.json` → `{url, pid, password}`), and
falls back to `acp` with one logged verdict on a box where nothing is
registered. `AMUX_OPENCODE_TRANSPORT=run|acp` is the escape hatch and always
wins; `AMUX_OPENCODE_ACP=0` still means `run`.

Why: M2 ended on a measured wall — ACP rejects a second concurrent prompt with
`-32603 "Session already has an active ACP prompt"`, so **true mid-turn steering is
impossible on ACP**. `opencode serve`'s v2 HTTP API has `delivery: "steer"`, and the same
API answers the other three M2 limits: a real `{interrupted:bool}` ack, permissions as
listable/repliable resources, and typed error bodies instead of text sniffing.

What changed, and the measured fact behind each decision:

| M3 goal | Result |
|---|---|
| **True mid-turn steering** | **Yes.** `POST …/prompt {delivery:"steer"}` lands *inside* a running turn: the essay turn closes with no `idle` marker of its own and the run ends on the steered reply. The queue's `AtTurnBoundary` path is untouched for `run`/`acp` (D4 — a contract change ships behind the enum, never silently). |
| **Real interrupt** | **Yes.** `POST …/interrupt` → `{"interrupted":true}`; the turn closes `idle outcome=interrupted` and **0 of 25** requested files were written. |
| **Permissions** | **Yes.** `GET …/permission` + `POST …/permission/{id}/reply` (`once｜always｜reject`); amux policy answers, and a `reject` actually stops the tool. |
| **Typed errors** | **Yes.** `SessionBusyError` is a rejection, not a failure (the queue retries at the turn boundary); `provider.quota`/429 → `RateLimited`, with `reset_at` **only** when `retryAfterMs` exists; everything else → `Failed{retryable}` naming the endpoint (4xx no, 5xx/refused yes). |
| **One process for the fleet** | **The server exists; amux does not use it.** Measured 2026-10-30: a bare `opencode` attaches to ONE background service (`opencode serve --service`, registered in `~/.local/state/opencode/service.json` with `{url, pid, password}`); only `--standalone` and `opencode acp` spawn private `serve --stdio` children, and `acp` has no `--server` flag so it never shares. Memory: ~135–188 MB idle, **~5.8 GiB during a turn**, back to 356 MiB a minute later; extra sessions ~0 MB. So one shared serve contains the spike to one process — and `serve` can join the one already running via `AMUX_OPENCODE_SERVICE_URL` (implemented in `ServeClient::from_env()`). `opencode service start\|stop\|status` is the supervisor, so amux needs configuration, not a new daemon. |

The architectural finding worth carrying forward: **`POST …/prompt` is asynchronous** — it
returns the admitted message immediately and the turn runs in the background. ACP's
`session/prompt` blocks for the whole turn, which is why `acp.rs` needed a pending-request
map, id demux, reader/writer tasks and spawn-retry. On HTTP that machinery is
unnecessary, and `serve.rs` came out at **897 lines vs `acp.rs`'s 1741** at the same
milestone depth.

Two measured limits, stated rather than buried:

- **The durable log does not replay.** `…/log?after=` returned only a `log.synced` marker
  and none of the 30 prior events, contradicting its own description. The backfill is
  therefore `GET /session/{id}/message` — proven to return full history including
  interrupted turns. SSE is a **progress** channel only; **turn outcome always comes from
  `/message`** (`/api/event` is "volatile by contract": missed events are expected).
- **A dedicated serve prints its own password** to its startup log (`server password
  <43 chars>`) and does not read `service.json`, so amux captures the credential from the
  spawn output. Auth is HTTP Basic `opencode:<password>` — Bearer, empty-user Basic and
  `?password=` all measure 401.

This is why the flip needed care, and what it now is. The plan's D2 said the flip
happens once a live smoke proves a real worker turn end to end; that smoke passed
on 2026-10-30 (`outcome=succeeded`, 131 SSE frames, a steered message in the
history — see the spike record). The owner then asked whether a freshly started
`opencode` shares the fleet's server, and the measurement answered it: **it does**
— a bare `opencode` is a client of the one background service, and only
`--standalone` or `opencode acp` spawns a private one. That made the default `acp`
a **cost** decision rather than a caution: every acp worker paid for a private
`serve --stdio` child (2 processes each) beside a shared service that already
existed.

So the default is now resolved, not flipped blindly:

```
AMUX_OPENCODE_TRANSPORT set       → that value wins (run | acp | serve)
AMUX_OPENCODE_ACP=0               → run
shared service discoverable       → serve      ← the normal case on this box
otherwise                         → acp, + ONE logged verdict naming why
```

Discovery reads `~/.local/state/opencode/service.json`, which carries `url` and
`password` together, so no hand-set variable is needed; `AMUX_OPENCODE_SERVICE_URL`
still overrides. Two facts made the flip safe, both measured at zero token cost:
session refs are **portable across serve instances** (the shared service answers
`200` for a `ses_` a session my dedicated smoke serve created — same
`opencode.db`), and a dead ref is a clean `404 SessionNotFoundError`, which is
what `session_exists` already assumed.

---

## Risks / honest limits

- **One-shot turns (M1).** `opencode run` has no stdin session, so mid-turn
  messages reject at the turn boundary — same shape as claude/gemini/codex
  headless today. ACP/serve (M2) is the fix.
  **Measured 2026-10-17: ACP does NOT fix this.** OpenCode's ACP adapter rejects
  a second concurrent `session/prompt` with `-32603 "Session already has an
  active ACP prompt"`, so mid-turn delivery still happens at the turn boundary
  on both transports. `opencode serve` (`delivery: "steer"`) is the only path
  that actually steers a running turn — **built in M3** and proven by a live
  smoke; it is not yet the default transport.
- **Approvals.** Headless writes hang without `--auto` (or a config). Must set it
  for workers; that is a policy choice to state, not hide.
- **Provider-id spelling.** Keep `"opencode"` canonical; no legacy alias needed.
- **Version drift.** OpenCode's `run`/v2 API is young — it moved 2.1.4 → 2.2.1 →
  2.2.3 in four days during M3. Pin the observed version in the coverage doc and
  re-measure on upgrade; the M3 SSE capture has NOT been re-measured on ≥2.2.1.
- **M3's durable log is unproven.** `…/log?after=` did not replay history. Turn
  reconciliation rides `GET /session/{id}/message`; a full read, not a cursor.
- **No supervisor yet.** A serve process exists only when someone starts it; M3
  built the transport, not the thing that owns one for the fleet.

---

## Verification

- `cargo test -p amux-server opencode` and the provider conformance tests.
- `make check` (cargo check + JS syntax) before any land.
- Live smoke (M0): register/start/send/peek an opencode worker.
- Live smoke (M1): a headless turn emits `turn_started` → `progress` →
  `turn_completed`, and a second turn resumes the same `ses_...`.
- Live smoke (M2): `cargo test -p amux-server opencode::acp -- --ignored` drives
  a REAL `opencode acp` process — a tool-calling turn must surface a `tool_used`
  with the written path, usage tokens, and two turns on ONE session id.
- Live smoke (M3): the `#[ignore]`d tests in `opencode::structured::tests` drive a
  REAL `opencode serve` (D1 shape: dedicated serve, password from its spawn log,
  cleaned up afterwards) via `AMUX_SMOKE_URL` / `AMUX_SMOKE_PASSWORD` /
  `AMUX_SMOKE_MODEL`. A pass is `LIVE SMOKE VERDICT: full turn succeeded end to
  end` plus a steered message in the session's `/message` history. Recorded runs:
  `docs/serve-spike-results.md`.

## Workflow

`feat/opencode-provider` → commit per milestone → push to `pedrovgp/amux` →
open an upstream PR against `mixpeek/amux` if the maintainers want it.

## Immediate next actions

1. ~~Measure the tool-using event vocabulary and resume behavior (30 min).~~ Done
   (2026-10-17, ACP; see the M2 section).
2. ~~Implement M0 (S1, S2, S5) + tests.~~ Done.
3. ~~Smoke-test M0, then M1.~~ Done (PR #335).
4. ~~M2: ACP transport.~~ Built; M3 then answered the `opencode serve` question.
5. ~~M3: `opencode serve` transport.~~ **Built** (`opencode/serve.rs` + `Transport::Serve`,
   `docs/serve-spike-results.md`). Remaining before the default flip: peer verification,
   the two one-probe gaps (busy HTTP status, dead-ref 404), and whoever launches the one
   shared serve in production.
