# Provider Parity — success criteria and scored audit

**Goal (Ethan, 2026-08-02):** a worker on any provider has the same capabilities inside amux as a Claude worker. This file is the criteria list and the living scorecard. A capability is MET only with evidence (the ethos rule: verified, not assumed). Re-score when a provider's CLI changes.

**The bar for "same capabilities":** every row below is something amux gives a Claude lane. For each provider, the row is `MET` / `PARTIAL` / `GAP`, with the evidence or the card that tracks it.

| # | Capability | Success criterion | Claude | Gemini | Muse | Evidence / card |
|---|---|---|---|---|---|---|
| 1 | Status classification | active/idle/waiting detected from the live pane | MET | **PARTIAL** | UNVERIFIED | idle/active shipped 9f80099, verified live (sherpa-execution `idle`). `waiting` (selector/auth screens) undetected — card AMUX-2231 |
| 2 | Idle-driven loops (pickup, steering, nudges, sweeps) | lane enters every loop keyed on idle | MET | MET | **GAP** | Full send/receive round trip verified 18:58 (SE-2 status-update via CLI) |
| 3 | amux CLI + board from inside the worker | board writes, sends, whoami work | MET | MET | UNVERIFIED | SE-2 status-update posted by the Gemini lane itself |
| 4 | Memory injection | amux-composed memory reaches the model at launch | MET | MET (this commit) | **GAP** | Worker-scoped GEMINI.md mirrored into `--include-directories`; no repo GEMINI.md touched |
| 5 | Launch parity (flags, yolo, model, resume meta) | provider flags normalized, worker id persisted | MET | MET | **MET** | start_session gemini branch (`--yolo`, `--skip-trust`, `--model auto`, gemini_session_id) |
| 6 | Custom slash commands | amux commands installed for the CLI | MET | MET | **GAP** | `~/.gemini/commands/*.toml` written alongside Claude's |
| 7 | Rate/usage-limit detection | provider cap banner → credit_limited + badge | MET | MET | **GAP** | `_PROVIDER_LIMIT_RES` gemini pattern (AMUX-2088, cloud-verified) |
| 8 | Limit auto-resume | reset time parsed → auto-continue at reset | MET | **GAP** | **GAP** | Gemini banner has no parsed reset; card AMUX-2231 |
| 9 | Token/cost tracking | per-worker tokens + $ in Cost tab | MET | **GAP** | **GAP** | Ledger reads Claude JSONL only; Gemini lanes invisible to Cost — card AMUX-2230 |
| 10 | Transcript tab | gap-free conversation render in peek | MET | **GAP** | **GAP** | Reads Claude JSONL only — card AMUX-2230 |
| 11 | Self-report (D1 hooks) | Stop/UserPromptSubmit → /report | MET | **GAP (upstream)** | **BLOCKED ON ROW 2** | Gemini CLI has no hook equivalent; scraper (#1) is the sanctioned fallback per D1 |
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
- THE INTERACTIVE TUI FIRES THEM TOO. An earlier revision of this row said the TUI fired nothing;
  that was wrong, and the way it was wrong is worth keeping. The TUI composes `hooks=4` exactly as
  `exec` does (`mode="tui"` in its own capability snapshot), but SessionStart fires at the FIRST
  TURN, not at launch. In an amux lane no turn ever happened — `amux send` never submitted (row 2)
  — so nothing fired, and "no hooks in the TUI" was inferred from a lane that had never been asked
  anything. Typing the same prompt straight into the pane produced all three hooks in three
  seconds: session-start, prompt, stop.

  So row 11 is blocked only by row 2, not by anything upstream. Fix the send and muse self-reports.

So the capability is real and the wiring is proven; only TUI delivery is missing. That is why
`MuseAdapter::capabilities().hooks` is true while this row is PARTIAL: the flag describes the
CLI, the row describes the lane.

**Row 2 (idle-driven loops) is GAP, blocks more than itself, and now has a measured cause.**
`amux send` returns `not submitted — text is sitting in the input box (autocomplete popup ate the
Enter?)`. Send/receive is the round trip rows 2, 3 and 11 are built on.

Measured against the TUI directly: typing the text, waiting 150ms and pressing Enter — the launch
path's timing — loses it, and the input line stays empty. The same keys with a ~2s gap submit
cleanly and the turn runs. amux already retries a dropped Enter, so what fails on muse is the
composer read that decides whether a retry is needed: `Submission::Stuck` is being reached against
an input box drawn differently from Claude's. The fix is provider-aware composer detection (and a
longer settle before Enter), not a change to muse.

**Rows 7-10, 13-14 are GAP for the same structural reason they are GAP for Gemini:** the limit
patterns, the token ledger and the transcript reader are Claude-shaped. Nothing muse-specific was
attempted.

CARDS ARE NOT YET FILED for these gaps, which the rules below make a violation of this document.
They are listed here so the debt is visible rather than absent, and filing them is the next action.
