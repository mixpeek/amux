# OpenCode `serve` spike + build results (M3)

Question: should amux talk to `opencode serve`'s v2 HTTP API instead of driving one
`opencode acp` child per worker — and does it deliver the four things ACP cannot?
Follows `docs/opencode-provider-plan.md` (M0/M1) and `docs/acp-spike-results.md`
(the ACP transport, M2).

**Answer: yes, and it is built.** `crates/amux-server/src/opencode/serve.rs` (M3.1–M3.2),
wired into `structured.rs` behind `AMUX_OPENCODE_TRANSPORT` (M3.3–M3.6).

Versions matter here: the spike ran on **2.1.4**, the ACP work on **2.0.32**, and the
final live smoke on **2.2.3** — three releases in four days. Every number below names
the version it was measured on.

---

## Measured, 2026-10-26, opencode 2.1.4, this box

### The four things ACP could not do

| amux need | serve endpoint | verdict for amux |
|---|---|---|
| **Mid-turn steering** | `POST /api/session/{id}/prompt` with `delivery:"steer"` | **WORKS** — proven below |
| **Real interrupt** | `POST /api/session/{id}/interrupt?resume=` → `{interrupted:bool}` | **WORKS** — `{"interrupted":true}`, turn closed `idle outcome=interrupted`, **0 of 25** requested files written |
| **Permissions as resources** | `GET/POST /api/session/{id}/permission`, `…/permission/{id}/reply` (`once｜always｜reject`) | shape confirmed in the live `openapi.json`; built in M3.4 |
| **Typed errors** | `SessionBusyErrorEncoded`, `provider.quota`, `provider.free-limit` | replaced text-sniffing for the busy case; see the error table below |

### Steering: the headline claim holds

Session in a scratch cwd, model pinned. A 700-word essay was running (`write` tool
streaming) when this landed:

```jsonc
POST /api/session/{id}/prompt
{"text":"STOP the essay. Reply with exactly: STEERED-OK","delivery":"steer"}   → 200
```

Final message list, newest first:

```
[0] idle      outcome=succeeded
[1] assistant finish=stop   text="STEERED-OK"      ← the steered turn
[2] user      "STOP the essay…"  (delivery steer)
[3] assistant finish=tool-calls parts=[reasoning, tool:write]   ← essay turn
[4] user      "Write a 700 word essay…"
```

The essay turn `[3]` has **no `idle` marker after it** — it never reached a boundary of
its own. The steer landed *inside* the running turn. This is exactly the case ACP
refuses with `-32603 "Session already has an active ACP prompt"`.

### The finding that mattered more than steering

**`POST …/prompt` is asynchronous.** It returns the admitted user message immediately
(`{data:{id,type:"user",…}}`); the agent loop runs in the background. Measured: the call
returned in well under 2 s while the turn was still streaming.

ACP's `session/prompt` blocks for the entire turn, which is why `acp.rs` needed a
pending-request map, an id demux, a reader/writer task pair and a spawn-retry. On HTTP
that whole class of machinery is unnecessary — a send is a fire-and-forget admission and
completion is *observed*, not awaited. `serve.rs` came out at **897 lines against
`acp.rs`'s 1741** at the same milestone depth.

### Memory: one serve for the fleet (D1)

Dedicated `opencode serve --hostname 127.0.0.1 --port 49901`, spawned, measured, killed:

| state | RSS |
|---|---|
| idle floor | **~135–188 MB** |
| during an active turn | **spikes to 1.8–3.6 GB** (model weights resident) |
| ~3 s after turn end | **settles back to ~188 MB** |
| marginal extra idle session | **~0 MB** — a session is a row; cost is per-*turn* |

Sessions are free, turns are not. N per-worker serves would multiply the idle floor *and*
allow N concurrent 1.8–3.6 GB spikes; one shared serve contains the spike to one process.
**This is the AMUX-74 OOM in one number** (a 6.6 GiB ACP pane): under 2-processes-per-worker,
N busy workers stack N spikes on a 15.5 GB box.

A dedicated serve generates **its own password** printed to its startup log
(`server password <43 chars>`) — it does *not* read `service.json`, so amux captures the
credential from the spawn output. Auth is HTTP Basic `opencode:<password>`; Bearer,
empty-user Basic and `?password=` all return 401 (measured).

### The durable log does NOT replay (measured limit)

`GET /api/experimental/session/{id}/log?after=<cursor>&follow=true` claims it "reads
events after an exclusive aggregate sequence and continues with live events". Measured:
it returned **only** `{"type":"log.synced","seq":30}` and replayed none of the 30 prior
events, on an 8 s window.

**Verdict: cursor replay is NOT proven.** The M3.2 backfill was therefore built on
`GET /api/session/{id}/message`, which *did* return the full durable history including
interrupted turns and their `idle outcome=` markers. SSE is a **progress** channel only;
turn outcome comes from `/message` (the plan's D3).

### Event stream contract (verbatim from the running openapi)

> *"Volatile by contract: a slow consumer overflows and fails the stream, and events
> during disconnection are missed."*

So a dropped frame is expected behaviour, not a bug to hide: `ServeEventHub` counts
`frames`, counts lag, logs a `verdict=` and marks the session owed a backfill.

---

## Built (M3.1–M3.7) — what shipped

| piece | where |
|---|---|
| HTTP client, auth, session create/resume, non-blocking prompt, `/message` | `opencode/serve.rs` |
| one SSE reader per serve process, demuxed per session into per-session broadcast channels | `serve.rs` + `ServeEventHub` |
| steering / interrupt / permission polling + reply | `structured.rs`, `serve.rs` |
| `Transport { Run, Acp, Serve }` replacing the `uses_acp` bool | `structured.rs` |
| typed error mapping → `WorkerEvent` | `serve_worker_event` in `serve.rs` |

Per-session channels, not one shared channel: a burst on one worker cannot evict
another's unread frames.

`AMUX_OPENCODE_TRANSPORT=run|acp|serve`; **the default is `acp`**, because flipping a
default is a behaviour change for every existing OpenCode worker and belongs behind a
proven full pass plus peer verification.

---

## Live smokes (the proof, not the claim)

All `#[ignore]`d: they need a running serve and provider quota. D1 shape — one dedicated
serve, password captured from its spawn log, cleaned up afterwards.

| when | opencode | what it proved |
|---|---|---|
| 2026-10-29 | 2.2.1 | steer admitted mid-turn + `interrupt` acked `interrupted=true`; outcomes read from `/message` |
| 2026-10-30 12:1x | 2.2.3 | transport, but the turn was refused: `provider.free-limit` (429) on the free default model |
| **2026-10-30 13:2x** | **2.2.3** | **full pass** — see below |

The 13:2x run, `AMUX_SMOKE_MODEL=opencode-go/deepseek-v4.1-flash` (paid credits, owner
authorized):

```
smoke: model = opencode-go/deepseek-v4.1-flash
smoke: events = 96 total — trace_observed 1, turn_started 2, progress 91,
                 tool_used 1, turn_completed 1
smoke: outcome = succeeded
smoke: hub frames = 131
smoke: steered message present in /message (session ses_e725cf487ffeVc776EsizpMhvS)
LIVE SMOKE VERDICT: full turn succeeded end to end
```

131 SSE frames produced 96 `WorkerEvent`s: the stream is global and carries frames that
map to no worker event at all (usage, step, `provider.updated`/`model.updated` config
noise), so a 1:1 ratio is not expected. `frames` is the reader's proof-of-life; the
event list is what the worker's channel actually received.

Provider-side confirmation for the same session — `model {providerID:"opencode-go",
id:"deepseek-v4.1-flash"}`, 15 005 input / 4 086 output / 1 095 reasoning tokens,
`outcome: succeeded`, and a message list of exactly the D4 shape:

```
idle      outcome=succeeded
assistant finish=stop        parts=[reasoning, text]        ← the steered reply
user      "SMOKE-STEER-1 stop the essay and reply exactly STEERED-OK"
assistant finish=tool-calls  parts=[reasoning, tool]        ← the essay turn, no idle of its own
user      "Write a 3000-word essay about lighthouses into essay.txt…"
```

`cost: 0` on the session row even though the model is paid: the `opencode-go`
subscription bills the account, not the session. Do not read that zero as "the turn
was free".

One honest gap in the smoke itself: it accepts a **typed** provider refusal
(`provider.free-limit` / `provider.quota`) with a distinct verdict printed, because the
transport is what it tests and a spent quota says nothing about the transport. An
untyped failure still panics. The full pass above is why that escape hatch is not
load-bearing.

---

## Measured error shapes

```jsonc
{"type":"provider.quota","status":402,"message":"You have exceeded your monthly quota","retryAfterMs":429981254}
{"type":"provider.free-limit","status":429,"message":"Free usage limit reached. Connect a provider or subscribe to OpenCode Go…"}
{"_tag":"SessionBusyError","sessionID":"ses_…","message":"…"}
```

The first carries a clock (`retryAfterMs` → `RateLimited.reset_at`); the second does not,
and **must not** get an invented one (a timer cannot fix a quota). `provider.free-limit`
was captured by the 12:1x live smoke and is the case that proved the classifier had to be
wider than `provider.quota`.

---

## Honest limits

- **Now the default, when there is a server to join.** `serve` is what an
  un-pinned OpenCode worker uses as soon as amux can discover the shared
  service opencode registers (`~/.local/state/opencode/service.json`); on a box
  with no service it falls back to `acp` with one logged verdict. The escape
  hatch is `AMUX_OPENCODE_TRANSPORT=acp|run`.
- **Durable-log replay is unproven** (above). Reconciliation rides `/message`, which
  returns full history but is a full read, not a cursor.
- **Both former "one-probe gaps" are closed, without spending a token.** A dead
  ref measures `404 SessionNotFoundError` on `/message`, on `/session/{id}` and
  on `/prompt` — exactly what `session_exists` assumed. And the busy HTTP status
  turned out to be the wrong question: the captured openapi declares
  `SessionBusyError` on ONLY the three `revert*` endpoints (409), none of which
  amux calls, and prompt admission is asynchronous — so a "busy prompt"
  rejection cannot occur at all. The fixture's 409 tests classification, not a
  production path.
- **Version drift is the norm here**, not the exception: three releases in four days. The
  SSE capture taken during M3.2 is pinned to 2.1.4 and has not been re-measured on
  ≥2.2.1 (it lives in the local memory bank, which this repo does not track).
- **One server per box, now enforced by amux too.** Measured 2026-10-30 on
  opencode 2.2.3: a bare `opencode` **attaches** to one background service
  (`opencode serve --service`, `~/.local/state/opencode/service.json` carries
  `{url, pid, password}`), and new servers appear only on `--standalone` or
  `opencode acp` — which has no `--server` flag and therefore always spawns a
  private `serve --stdio` child. That made `acp`, the old default, the thing
  breaking one-server-per-box: N acp workers = N private servers, 2 processes
  each. amux now discovers the registry and defaults to `serve`; the supervisor
  is opencode's own (`opencode service start|stop|status`), so there is no amux
  daemon to babysit.
- **The shared server's spike is the whole box's spike.** Re-measured while
  answering that question: `~5.8 GiB` RSS during a turn, back to `356 MiB` within
  a minute (D1 recorded 1.8–3.6 GB; the real number is larger), with 2.4 GB of
  swap already in use. One process contains it; it does not make it free.
