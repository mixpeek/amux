# Token efficiency in Amux

Optimize verified outcomes per model turn. A short prompt can still cause a costly turn when the worker rereads a large conversation. Browser polling, terminal rendering, and local upload retries do not themselves use model tokens; redundant delivered prompts and unnecessary model tool rounds do.

Measure the existing `/api/observability?days=1` rollup and its freshness before tuning. The ledger sums fresh input, cached input, cache writes, and output. Its dollar figure is an estimate using configured list prices, not a subscription bill. Blank task attribution means unassigned accounting, not proof of wasted work. Avoid making savings claims from token totals alone.

Recommended order:

1. Attribute turns to the triggering user message, board task, scheduler, or coordination event. Track turns, cached context, estimated cost, latency, and review/reopen outcomes per completed/Verified task. Compare similar tasks on the same model.
2. Suppress repeated unchanged nudges and coalesce related board updates at the next worker boundary. Preserve the first actionable change, explicit user messages, gate changes, and peer-review findings. Existing board-drive cooldowns and unheeded backoff should be extended with evidence, not replaced with arbitrary silence.
3. Include enough task context in pickup messages: objective, exact criteria/revision, dependencies, next action, and relevant artifact links. Cutting a message to an ID can force several extra tool/model rounds. The existing configurable pickup excerpt is deliberately larger for that reason.
4. Keep routine deterministic work in code: queue reconciliation, file checksums, polling, status checks, and exact duplicate IDs do not need model calls. Use semantic models for ambiguous intent; reuse a comparison only while the relevant task revisions remain unchanged.
5. Keep worker context focused. At a natural task/epic boundary, preserve decisions, unresolved work, peer commitments, and evidence in a handoff before starting fresh or compacting. Do not blindly clear context during work or repeatedly compact on a timer.
6. Benchmark Sonnet for routine implementation and review, and escalate difficult failures deliberately. Model changes affect cost per token, while fewer unnecessary turns and smaller relevant context affect token usage. Preserve the user's selected model for active work.
7. Bound tool output without hiding failure details: summaries plus exact artifact paths, targeted log slices, and batch independent checks. Keep stable instructions stable so prompt caches remain useful.

Claude Code's own guidance supports focused context, deliberate model choice, and reducing unnecessary output. Its cache documentation distinguishes cache reads from newly written context; cache hits are useful but do not make repeated turns free. Sources: [cost management](https://code.claude.com/docs/en/costs), [prompt caching](https://code.claude.com/docs/en/prompt-caching).

Acceptance for an optimization: replay the same meaningful task set, preserve completion and independent review quality, and report actual token/turn totals alongside cost and latency. Include negative controls: changed criteria must reach the worker, a blocked task must become eligible when its dependency changes, and distinct peer findings must not be swallowed as duplicates. A lower token number caused by unfinished work is a failure.
