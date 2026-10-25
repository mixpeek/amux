# Owner messages vs "How I prompt" (2026-10-14)

Compares how the owner has used amux, measured from the messages typed to
workers, with the six habits in the talk "How I prompt" (Brendon Geils, Athena
Intelligence, October 2026). The talk measured 31,399 Claude Code prompts and
found output per prompt rose from 0.14 to 1.23 PRs as prompts got longer, fewer
and more autonomous.

## Data

- **Owner messages:** `cmd_history` rows with `type='user'` in `~/.amux/amux.db`,
  3,480 messages from 2026-07-25 to 2026-10-14. About 15% went to personal
  (non-engineering) sessions; the trends below hold without them, the exact
  ratios shift a little.
- **Output:** commits authored under the owner's name on `origin/main` of
  mixpeek and amux, per month. Workers commit as the owner and push to main, so
  this is the nearest equivalent of the talk's PR count, and it is not directly
  comparable to it. The direction is the point.
- **Pattern rates:** regex matches over the message text, with pasted content
  and @-mention footers stripped.

## The numbers

| | Owner | Talk author |
|---|---|---|
| Messages a month | 835 (Aug), 1,493 (Sep), about 2,250 (Oct pace) | peaked in June, then fell |
| Output per message | about 12.4 commits (Aug), 6.2 (Sep), 6.4 (Oct) | 0.14 to 1.23 PRs per prompt |
| First message to a session, median words | 22 (Aug), 16 (Sep), 12 (Oct) | 13, 69, 137 |
| `/goal` | 1.4% of messages | 92% of PRs came from `/goal` sessions |
| Sessions messaged a day | 5.6 (Aug) to 13.5 (Oct) | about 3 at a time, by rule |
| Pushback ("why", "are you sure", "I thought") | 3.2% | 2.4% |
| Fence language ("don't", "never", "spend", "limit") | 11.7% | not measured |
| Write-it-down (doc, ticket, card, ledger) | 8.5% | not measured |

The talk's curve: prompts got longer and fewer, and output rose. The owner's:
prompts got shorter and more frequent, and output per message halved.

## What already works

- **Argue with it (habit 4).** Pushback runs above the talk's rate, and it pays:
  "why do i keep getting mvs errors" and "is this intentional" each surfaced a
  real defect.
- **Manage, don't type (habit 6).** "whats the status" is one of the most common
  prompts, which is what the talk recommends.
- **Doc-as-brief.** Pasting a researched document with "do these next" or "fix
  these /e2e-test" is habit 1 in its strongest form. The GS-12 `/goal` handoff
  (2026-10-08) matched the talk's slide 8: end state, an evidence file, a done
  condition, and "do not trust any existing claim".

## Where the most is to gain

1. **Brief like a senior hire (habit 1).** Opening messages have shrunk to a
   median of 12 words ("gs12 progress?", "do it", "whatever opportunities there
   are to speed things up do them"). The talk's six parts: the job, the end
   state, your own ideas, what's cheap right now, the output you want, and where
   to stop. "What's cheap" would have saved two round trips on GS-12: "downtime
   is ok if it means pursuing the goal" arrived after a cutover had been rolled
   back, and "today is a production down possible day" arrived on day three.

2. **Ask for decisions, answer in one line (habit 2).** Approvals tend to come in
   bulk ("approve all 19 left with me", "blanket approval for everything"). The
   talk's pattern is "list the decisions you need from me, 2-3 options each, your
   recommendation and why", answered as "D1: A, D2: A + upgrade Milvus". Bulk
   approval let the GS-12 worker take automatic approvals as the owner's, and make
   calls that were later reversed (six plan rewordings, a 20-lane fan-out).

3. **Hand over the whole loop with `/goal` (habit 3).** At 1.4% this is the
   largest gap. Open project work with `/goal`, a done condition and the fence in
   the same message. Keep the condition stable: the GS-12 goal was overwritten
   twice ("continue what you were doing", then an hourly schedule sending
   "/goal continue: ..."). Never put `/goal` in a schedule; to nudge a running
   goal, type "continue".

4. **Say the fence before, not after (habit 3b).** Fences often arrive as
   corrections ("do not reprocess anything", "stick to the gs12 plan"). Put the
   environment, the data scope, a spend cap with a number, and what still needs a
   yes in the first message.

5. **Cap the fan-out (habit 6).** GS-12 ran 20 lanes and an orchestrator before
   consolidation; the single worker that replaced them made about 6 proof rows a
   day against about 1.6. The 11-lane push on 2026-10-11 hit the account usage
   limit twice. The talk's rule is three sessions at once.

6. **One record per project (habit 5).** Notes are spread across the board, chat,
   frustrations files and ledgers. GS-12 sped up once everything lived in one
   file (12-COMPLETION.md) with a checker. Name the tracking file in the first
   message.

## Template for a project opening

```
/goal <one-sentence end state>, done when <the check that proves it>.
Context: <what exists, what failed before>.
My ideas: 1. ... 2. ... Then add 10 better ones.
Cheap right now: <downtime ok / spend up to $X / no soak needed>.
Fence: <environment>, <data scope>, never <delete customer data / email outsiders>.
Before acting, list the decisions you need from me: 2-3 options, your pick, why.
Track everything in <file>. Max 3 lanes.
```
