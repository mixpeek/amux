# Plan: first-class OpenCode provider for amux

**Fork:** `pedrovgp/amux` · **Branch:** `feat/opencode-provider` · **Base:** `main` (== upstream `1c7fbc5`)
**Target binary:** OpenCode `v2.0.28` (measured on this machine, 2026-10-14)
**Status:** plan — no code yet

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

---

## Risks / honest limits

- **One-shot turns (M1).** `opencode run` has no stdin session, so mid-turn
  messages reject at the turn boundary — same shape as claude/gemini/codex
  headless today. ACP/serve (M2) is the fix.
- **Approvals.** Headless writes hang without `--auto` (or a config). Must set it
  for workers; that is a policy choice to state, not hide.
- **Provider-id spelling.** Keep `"opencode"` canonical; no legacy alias needed.
- **Version drift.** OpenCode's `run`/v2 API is young; pin the observed version
  in the coverage doc and re-measure on upgrade.

---

## Verification

- `cargo test -p amux-server opencode` and the provider conformance tests.
- `make check` (cargo check + JS syntax) before any land.
- Live smoke (M0): register/start/send/peek an opencode worker.
- Live smoke (M1): a headless turn emits `turn_started` → `progress` →
  `turn_completed`, and a second turn resumes the same `ses_...`.

## Workflow

`feat/opencode-provider` → commit per milestone → push to `pedrovgp/amux` →
open an upstream PR against `mixpeek/amux` if the maintainers want it.

## Immediate next actions

1. Measure the tool-using event vocabulary and resume behavior (30 min).
2. Implement M0 (S1, S2, S5) + tests.
3. Smoke-test M0, then M1.
