# ACP coverage spike (RR-0028f)

> **STATUS (2026-10-30): the "uniform ACP ingester" recommendation below was
> REJECTED by the owner, not deferred.** OpenCode — the runtime this fleet
> actually runs, and the owner's main interest — moved to its HTTP API (see
> `docs/opencode-provider-plan.md`, M3/M4), so ACP now describes the *fallback*
> transport rather than the way forward. Building one ingester around ACP would
> optimise for the least-used path and still leave the HTTP client to maintain.
> If a second provider ever matters, the question is "does it have an HTTP
> surface like OpenCode's", not "does it speak ACP". The protocol analysis below
> stays as measured history.

Question: should ACP (Agent Client Protocol, agentclientprotocol.com — Zed's
JSON-RPC agent<->client standard) be amux's abstraction for uniform provider
output rendering? Follows the "do it" on uniform provider output (AMUX-3201) and
mirrors the opencode structured-events spike (RR-0028e, docs/opencode-spike-results.md).

## What ACP gives us that WorkerEvent does not

amux's `WorkerEvent` (amux-core/protocol.rs) was built for the D1 status-detection
exit — is a worker active/idle/waiting, which tool, how many tokens. Its payloads
are lifecycle-only: `ToolEvent { tool, detail }` where detail is a SHORT summary
(not the full input/output), and `ProgressReport { summary, tokens }`. That is
enough to drive the board/status; it is not enough to render the conversation.

ACP's content model IS the renderable conversation: text blocks, tool calls with
full input AND output, reasoning/thought blocks, permission requests, diffs, plans.
A uniform renderer needs that model. Building it on top of WorkerEvent would be
reinventing ACP.

## Provider coverage (probed on this machine, 2026-08-16)

| provider | ACP path | evidence |
|---|---|---|
| gemini | NATIVE | `gemini --acp` ("Starts the agent in ACP mode"); starts clean on empty stdin (exit 0) |
| claude | via adapter | `@zed-industries/claude-code-acp@0.16.2` on npm (Zed-maintained; what Zed itself uses). No native `--acp` |
| codex | NONE | no `--acp`, no `proto` subcommand. Has `exec --json`, `mcp-server` (stdio MCP), `app-server` (experimental) |
| ollama | NONE | rides codex (`codex --oss --local-provider ollama`), so inherits codex's gap |

So ACP is a clean, rich-content path for 2 of 4 (gemini native, claude via a
maintained third-party adapter). codex — and ollama, which rides it — have no ACP
today. They stay on the structured `exec --json` stream the opencode spike
(RR-0028e) already parses.

This tempers the "one protocol replaces all 3 bespoke parsers" pitch: codex keeps a
bespoke ingester. But it is still a net reduction — 2 standard rich paths + 1
bespoke, versus 3 bespoke today — and codex is moving fast (app-server is already
experimental), so its ACP gap is likely to close.

## Recommended shape (ethos-aligned)

The renderer targets ONE internal rich-content model (the "render model"). Ingesters
feed it:

- gemini: ACP -> render model (native, no adapter to maintain)
- claude: ACP via `@zed-industries/claude-code-acp` -> render model (adapter maintained upstream)
- codex/ollama: existing `exec --json` -> render model (one bespoke ingester, already largely built in opencode/)

One render model, one renderer. ACP is the primary ingester where it exists; the
bespoke surface is a single codex ingester that SHRINKS as codex adopts ACP, rather
than three parsers that grow. That satisfies the compounding test: gemini and claude
rendering improve as the ACP standard and the Zed adapter improve, with no amux
change; only the codex path is amux's to maintain, and it is on the way out.

Keep WorkerEvent for state/status (the D1 concern) — ACP handles rendering, they are
complementary. ACP session updates can also feed state, but that is a later
consolidation, not a prerequisite.

## Open items before building

1. Validate `@zed-industries/claude-code-acp@0.16.2` end to end (auth passthrough,
   conversation continuity) — it is third-party; confirm it works with our
   `claude` login, not just that it installs.
2. Confirm gemini `--acp` carries the content richness we need (tool input+output,
   reasoning) in a live turn, not just that the process starts.
3. Map ACP's content types onto the internal render model, and map codex `exec --json`
   onto the SAME model, so the renderer sees one shape.
