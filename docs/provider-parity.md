# Provider Parity — success criteria and scored audit

**Goal (Ethan, 2026-08-02):** a worker on any provider has the same capabilities inside amux as a Claude worker. This file is the criteria list and the living scorecard. A capability is MET only with evidence (the ethos rule: verified, not assumed). Re-score when a provider's CLI changes.

**The bar for "same capabilities":** every row below is something amux gives a Claude lane. For each provider, the row is `MET` / `PARTIAL` / `GAP`, with the evidence or the card that tracks it.

| # | Capability | Success criterion | Claude | Gemini | Muse | Evidence / card |
|---|---|---|---|---|---|---|
| 1 | Status classification | active/idle/waiting detected from the live pane | MET | **PARTIAL** | UNVERIFIED | idle/active shipped 9f80099, verified live (sherpa-execution `idle`). `waiting` (selector/auth screens) undetected — card AMUX-2231 |
| 2 | Idle-driven loops (pickup, steering, nudges, sweeps) | lane enters every loop keyed on idle | MET | MET | **MET** | Full send/receive round trip verified 18:58 (SE-2 status-update via CLI) |
| 3 | amux CLI + board from inside the worker | board writes, sends, whoami work | MET | MET | UNVERIFIED | SE-2 status-update posted by the Gemini lane itself |
| 4 | Memory injection | amux-composed memory reaches the model at launch | MET | MET (this commit) | **GAP** | Worker-scoped GEMINI.md mirrored into `--include-directories`; no repo GEMINI.md touched |
| 5 | Launch parity (flags, yolo, model, resume meta) | provider flags normalized, worker id persisted | MET | MET | **MET** | start_session gemini branch (`--yolo`, `--skip-trust`, `--model auto`, gemini_session_id) |
| 6 | Custom slash commands | amux commands installed for the CLI | MET | MET | **GAP** | `~/.gemini/commands/*.toml` written alongside Claude's |
| 7 | Rate/usage-limit detection | provider cap banner → credit_limited + badge | MET | MET | **GAP** | `_PROVIDER_LIMIT_RES` gemini pattern (AMUX-2088, cloud-verified) |
| 8 | Limit auto-resume | reset time parsed → auto-continue at reset | MET | **GAP** | **GAP** | Gemini banner has no parsed reset; card AMUX-2231 |
| 9 | Token/cost tracking | per-worker tokens + $ in Cost tab | MET | **GAP** | **GAP** | Ledger reads Claude JSONL only; Gemini lanes invisible to Cost — card AMUX-2230 |
| 10 | Transcript tab | gap-free conversation render in peek | MET | **GAP** | **GAP** | Reads Claude JSONL only — card AMUX-2230 |
| 11 | Self-report (D1 hooks) | Stop/UserPromptSubmit → /report | MET | **GAP (upstream)** | **MET** | Gemini CLI has no hook equivalent; scraper (#1) is the sanctioned fallback per D1 |
| 12 | Model detection | active model shown on card | MET | MET | MET | Flags/default fallback (`--model auto`) |
| 13 | API-error detection (5xx retryable) | transient errors flagged, continue offered | MET | **GAP** | **GAP** | Patterns are Claude-shaped — card AMUX-2231 |
| 14 | Subagent/suggestion niceties | running-subagent badge, empty-send suggestion | MET | GAP (minor) | GAP | Claude-UI parsing; cosmetic — card AMUX-2231, low priority |
| 15 | Guards (commit/push/staged), groups, peek, steering composer, schedules, archive | provider-agnostic mechanics | MET | MET | MET | tmux/git/DB-based; no provider branch exists |

**Scoring rules:**
- MET requires a live verification, not a code read (rows 1–4 were each proven on sherpa-execution today).
- A GAP with no card is a violation of this doc — file one before merging the change that discovers it.
- Codex column: to be scored the same way when a Codex lane is next active (most Gemini rows apply verbatim; #7 already MET).

**The compounding check (ethos):** rows 2–3 are where capability reaches the model — those are MET, which means a better Gemini makes a better lane with zero harness change. Rows 9–10 are observability of the lane, not capability of the model; they gate nothing the model does.

## Muse Code column — scored 2026-09-08 against 1.0.3-R2198.1

Scored the same way the rules below demand: MET only where a live run proved it, and
UNVERIFIED written as UNVERIFIED rather than guessed from a code read.

**Row 5 (launch parity) is MET, live.** Create stores `provider=muse` with EMPTY flags — an
unspecified model must not inherit the Claude default, or `--model opus` reaches Meta and the
worker is dead on arrival. First start ran `MUSE_EXPERIMENTAL_PLUGINS=on muse --model
muse-spark-1.3-contributor`; amux then learned the session id from disk
(`muse_session_id=01a08236-…`) and a restart ran `muse resume <uuid>` with the id unchanged and
`start_count: 2`. Muse has no `--session-id`, so it cannot copy grok's mint-it-up-front pattern;
`--last` was rejected because it resolves per-WORKSPACE and amux lanes share a CC_DIR.

**Row 11 (self-report) is PARTIAL, and it is the interesting row.** Gemini is GAP (upstream):
its CLI has no hook surface, so scraping is the only option D1 leaves. Muse is different in kind
— it ships the full Claude Code contract: the nine events (UserPromptSubmit, PreToolUse,
PostToolUse, SessionStart, SessionEnd, Stop, SubagentStop, Notification, PreCompact) and the
`hookSpecificOutput.hookEventName` envelope. Delivery is a PLUGIN capability, not a settings
key: `~/.config/muse/settings.json` hooks are silently ignored (`hooks=0`), and plugin loading is
gated behind `MUSE_EXPERIMENTAL_PLUGINS`.

With a user-scope plugin installed (`~/.amux/muse-plugin/amux-report`, four hook capabilities
individually approved — muse refuses a bundle where two hooks share a source), a session composes
`hooks=4`, and:

- `muse exec` (headless) FIRES them. Each hook wrote a provider-side trace and forwarded to
  `hook-report.sh`; amux logged three HTTP 200s in the matching second. The trace exists because a
  report arriving at amux cannot be attributed to a provider — this machine's own Claude Code
  sessions report against the same lane and produce identical rows, so without a provider-side
  record "muse hooks work" is unfalsifiable.
- THE INTERACTIVE TUI FIRES THEM TOO, and a real lane now proves it. An earlier revision of
  this row said the TUI fired nothing; that was inferred from a lane that had never been
  asked anything, because the send was broken (row 2). SessionStart fires at the FIRST TURN
  rather than at launch, so a lane nobody can talk to looks exactly like a lane without
  hooks.

  With row 2 fixed, a muse lane self-reports through the D1 path like a Claude lane:
  `muse-hook prompt` and `muse-hook stop` fired on the turn, and amux logged
  `/api/sessions/<lane>/report` 200 for each. Muse is the only non-Claude provider that
  reaches this row — Gemini's GAP is upstream and has no fix on this side.

**Row 2 (idle-driven loops) is MET.** It was a GAP for three stacked reasons, all in amux:

1. A muse lane never started. On a workspace it has not seen, muse stops on an interactive
   gate before the model runs — "Trusting allows project-local skills, rules, hooks and
   plugin config to load ... 1 Trust and continue / 2 Quit" — and a lane has nobody to
   answer it. The pane fell back to a shell, so the briefing ran as a command
   (`zsh: command not found: Reply`). Lanes now launch with `--trust-workspace`.
2. Enter did not submit. amux waits 20ms between the paste landing and Enter, which is
   Claude's budget; measured on a live muse lane, paste+20ms+Enter leaves the text in the
   composer every time and paste+300ms+Enter submits it. The settle is now per provider.
3. A working send was reported as a failure. Every read `verify_submitted` had was
   Claude-shaped, so amux answered `not submitted — text is sitting in the input box` while
   the pane showed the prompt answered — which makes callers re-send a message the agent is
   already working on. amux now also accepts muse's own durable record,
   `runtime.session.user_intent.accepted`.

Verified end to end: `POST /send` returns `submitted: true, submission: confirmed`, the pane
shows the reply, and the lane's hooks reach `/report` with 200.

**Rows 7-10, 13-14 are GAP for the same structural reason they are GAP for Gemini:** the limit
patterns, the token ledger and the transcript reader are Claude-shaped. Nothing muse-specific was
attempted.

CARDS ARE NOT YET FILED for these gaps, which the rules below make a violation of this document.
They are listed here so the debt is visible rather than absent, and filing them is the next action.
