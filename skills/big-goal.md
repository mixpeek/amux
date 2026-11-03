---
description: Set up a large, multi-day or multi-worker goal so it runs without the owner: a plan file, one checker per owner, a checker-backed /goal on each owning worker, and an hourly coordinator schedule that reports one line. Use for any big goal; a small one needs only a /goal plus AMUX_GOAL_CHECK.
argument-hint: <the goal in plain words> [plan=<path.md>] [workers=a,b,c] [group=<g>]
allowed-tools: Bash, Read, Write, Edit
amux-managed: true
---

# Big goal setup

The goal: **$ARGUMENTS**

A big goal fails in predictable ways (measured on mixpeek-dev, 2026-10-31 to 11-03):
- The owner became the status loop: 291 messages to one worker in 7 days, most asking for status.
- A coordinator held a /goal on numbers other workers owned. It looped on "only you can clear the goal".
- Two workers ran one checker and read different numbers (19 and 23) at the same moment.
- Rows went stale as fast as they were fixed: 46 gained and 52 lost in 24 hours, so the goal could never finish.

This setup prevents each of those. Do every step, then report what you set up.

## 1. Read before you build

- Look for an existing plan, checker, schedule or worker that already covers this goal. Search the repo, `amux` schedules (`curl -sk "$(amux url)/api/schedules"`) and workers (`/api/sessions`). Adopt what exists rather than duplicating it.
- Read the repo's CLAUDE.md for what needs the owner (new endpoints, money, outside sends, production data).

## 2. Write the plan file (in the repo, committed)

`<repo>/<goals or docs dir>/<slug>.md`, containing:
- **End state:** what is true when this is done, in a few sentences.
- **Rows:** numbered acceptance rows. Each row has a check you can run and exactly one owning worker.
- **Owners:** the list of workers and the rows each owns. One number per owner. If two workers would share a row, split it or give it to one.
- **Boundary:** what the workers may do without asking, and what stops them. The default stops are outside sends, deleting customer or production data, spending money, and whatever the repo's CLAUDE.md gates.
- **Freshness:** for any row that can go stale (a verification older than N hours, a dependency that moves), how the checker re-verifies it. A row that only needs re-checking is never counted as an open gap.

## 3. One checker per owner

A script in the repo, e.g. `scripts/goals/<slug>-<owner>.sh`, that:
- exits 0 when every row that owner owns is met, 1 when not, anything else when it cannot tell (and prints why);
- prints one JSON line of progress: `{"done": 12, "total": 20, "summary": "8 real gaps: rows 3, 7, ..."}`;
- re-verifies stale rows itself, and counts only real gaps as not done;
- needs no model call, and finishes well inside 120 seconds.

Run each checker once, and paste its output in your report. A checker you never ran is a claim.

## 4. Each owning worker gets its own goal

For each owner, send it (`amux send <worker> --stdin`) an order to do two things itself:
1. Set its checker as its amux goal check, on its own worker layer:
   ```bash
   curl -sk -X PUT -H 'Content-Type: application/json' -H "X-Amux-Session: $AMUX_SESSION" \
     -d '{"level":"worker","name":"'"$AMUX_SESSION"'","capability":"env","value":{"AMUX_GOAL_CHECK":"scripts/goals/<slug>-<owner>.sh"}}' \
     "$(amux url)/api/scope"
   ```
   amux then runs it every 10 minutes with no model. Its exit code decides "met" and outranks Claude Code's own evaluator. The goal keeper keeps the worker going while it says not met. The owner gets one alert if the number has not moved in 6 hours.
2. Set a one-sentence `/goal`:
   ```
   /goal Done when scripts/goals/<slug>-<owner>.sh exits 0; plan in <plan path>. Stop only for <boundary>.
   ```

A worker can set only its own worker layer, so each owner does step 1 itself. If an owner is isolated, amux refuses your send: give the owner the exact text to paste and say so in your report.

Keep the board off for every owner and the coordinator. Off has been the default for a worker nobody configured since 2026-11-02, and Off means no card is filed for the worker, nothing about the board is typed into it, and no dispatch or nudge competes with its goal. Check `board_used` is false on `GET /api/sessions` for each one; if it is true, ask the owner to switch the Board tab off. The record of the work is the checker output, the coordinator's hourly log line, Messages history and `amux requests`, none of which needs a board.

If the owner wants the board on for these workers, the board only records: it files and keeps cards and types nothing into the worker (amux's default; only `AMUX_BOARD_STEERS=1` lets it steer, so never set that for a goal worker). Then make the cards the plan's rows, one per row, owned by the row's worker, and close a card only when that owner's checker reports the row met. A task the board captures from a message gets the triage tag (`amux board triage <ID>`), so it waits until accepted and never jumps ahead of the goal.

New workers, if the goal needs them, go in one group with the coordinator, so they can message each other. Never create an isolated worker for this.

## 5. The coordinator gets a schedule, not a goal

The coordinator (you, or the worker the owner names) owns no row. It gets two schedules (`POST /api/schedules`, with `X-Amux-Session`):
- **Measure, hourly, `kind: "shell"`:** run every checker, append one line to a log (time, each owner's number, its 6-hour change, an ETA at that rate), and exit non-zero only when a number has not moved for the stall window or a checker cannot tell. This costs no model.
- **Act, hourly, `kind: "tmux"` to the coordinator:** read the newest log line. Post it to whoever owns the goal as one line, with `amux post p:<their name or email> --stdin` (it lands in your own thread and @mentions them). Order the owner of the biggest real gap to work on it. At the next fire, confirm the worker took it up (its task or its number changed). If a fire has nothing to act on, it says so in one line and stops.

Do not give the coordinator a /goal on the owners' numbers. It cannot move them itself.

## 6. Report

One message to whoever asked, listing:
- the plan path and its commit;
- each owner, its checker's first output, and whether its goal check and /goal are set (or the paste text, if you could not send);
- the two schedule ids;
- anything that needs the owner, as one question and what a yes unblocks.

## Rules

- Any boundary in the plan or the repo's CLAUDE.md overrides this skill.
- Never write to another worker's scope or set its /goal for it. Send it the order.
- No second copy of an amux primitive. If amux lacks something this needs, log it in amux's `frustrations.md` rather than building a private watcher.
- A small goal (one worker, hours) skips steps 2, 3 and 5: a one-sentence /goal plus `AMUX_GOAL_CHECK` on that worker is enough.
