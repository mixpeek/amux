---
description: Drive a large multi-day, multi-worker goal to done and keep it moving. Takes a goal spec (.md), sets up a local coordinator worker on top of /big-goal's scaffolding, and every hour senses what is holding the goal (owner waits, stale proofs, a jammed merge queue, capacity, a faulty instrument, a missed forecast, a bar that passes while users don't see it) and switches on only the mechanism that fixes it. Use when the owner hands over a goal and wants it finished without supervising it.
argument-hint: <goal spec path or the goal in plain words> [workers=a,b,c] [coordinator=<name>] [preapprove=<USD>]
allowed-tools: Bash, Read, Write, Edit
amux-managed: true
---

# Goal accelerator

The goal: **$ARGUMENTS**

Measured on one Mixpeek goal pair (GS-12 and CI/CD, mixpeek-dev, 2026-11-02 to 11-23): push to production
went from a 192-minute p90 to 2 to 9 minutes in six days, and dev plus CI cost from 219 to 9 USD a day,
while most dated forecasts were killed early and the gate moved ten times. This skill keeps what moved
the numbers and puts a guard on each thing that slowed them.

## 0. Scaffold with /big-goal first

Run `/big-goal` for the plan file, one checker per owner, each owner's `AMUX_GOAL_CHECK` and `/goal`, and
the hourly measure and act schedules. This skill adds the layer below. If /big-goal's scaffolding already
exists for this goal, adopt it.

Pick the coordinator: a local worker in the goal's repo that does no product work. It reads, rules,
relays and reports. Owning workers can run in the cloud. Put every owner and the coordinator in one
amux group, so no message needs a relay.

## 1. Sense every hour, then act on one thing

At each hourly act fire, before ordering anyone:

1. Read every checker. Record real gaps open, gaps closed in 24 h, stale-only items, and cost per day.
2. Name what held the goal in the last 6 h, checking in this order:
   - **owner wait**: an ask to the owner older than 2 h;
   - **staleness**: items going stale faster than they verify;
   - **queue**: main held by a red batch or a bisect;
   - **capacity**: box load, a pool at zero, a quota, or the model account under its floor (`amux usage`, `amux resources`);
   - **instrument**: a number that moved with no change behind it;
   - **forecast**: a dated step that missed;
   - **scope**: a bar that passes while the user-facing behaviour doesn't.
3. Act on the top class with its section below, and append one log line: class, evidence, action.
4. If every number has been flat for 6 h, tell the owner in one line, with the class and the fix.

## 2. Owner wait: the decision queue

- Ask the owner on day one for a **standing pre-approval with a price limit** (for example 50 USD per item)
  and for "act, then report" inside the plan's boundary. Without it, every small paid run waits on a reply.
- Keep a decision queue file the daily report reads. Each entry: the question in one line, its price,
  the recommended answer, the **default** taken if unanswered by a set time, and what it unblocks. At that
  time, act on the default unless the owner said no.
- Ask only for money past the limit, outside sends, customer or production data deletes, what the repo's
  CLAUDE.md gates, and bar changes.
- Keep one priority number one, in the owner's words, in every schedule prompt.
- Every relayed decision quotes the owner's words and time, and is read back after acting.

## 3. Staleness: batch the re-proofs

- A change that stales a proven item lands in a batch, right before a scheduled re-prove, so each item is
  re-proven once. A live production fault is the exception: it lands now, and its stale items join the
  next batch.
- The checker counts stale-only items apart from real gaps (/big-goal's freshness rule), so staleness
  never reads as lost progress.

## 4. Queue: keep main moving

- Short CI checks get runner slots that long jobs can't take.
- When a merge batch goes red, drop it and let each change re-land on its own. Main never waits on a bisect.
- No holds or freezes. A test that needs quiet gets its own environment, outside CI's path.
- Only release guards sit between main and production. Broad validation runs on a schedule against a named commit.

## 5. Capacity and cost

- Work local first. Use cloud only when the check needs it, with the reason in the dispatch. A new server
  needs the owner's price-tagged yes or the pre-approval.
- Pools default to min 0. A run creates its capacity and removes it when it ends.
- Price every CI and proof run (node-seconds times list price), and alert the owner the moment one passes
  the cost bar, with its cause and fix time.
- Below the model account's floor, the coordinator works alone and owners run without subagents.

## 6. Instrument: read the meter first

- Before acting on a number, read how it is computed. Pair every zero with a positive control, and use
  credentials with enough life left for the whole run.
- A changed measure is a recorded misread fix with its cause. A bar moves only on the owner's word.

## 7. Forecast: hypotheses name the first blocker

Keep one open hypothesis per goal: "if we do X, metric A moves to B by T; the first step to miss would be
S". Judge it at T, or as soon as its miss is certain, and log KEPT, KILLED or FAILED with the step that
missed. Its value is naming the first blocker. Treat its dates as guesses.

## 8. Event-driven waking

Add a `kind: "shell"` watcher every 3 minutes that checks the owners' inbox or notes, the checkers, disk,
load, production health and due milestones. It writes `events-latest.json` and calls
`POST /api/schedules/<event-run>/run` only when something is new. The event run (`kind: "tmux"`) reads that
file, fixes or routes each item, and files it. A quiet hour costs no model.

## 9. Scope: finish where the user is

Every bar is the owner's number, measured on production or the user's path. A test environment alone never
closes a bar, and any report-only check is listed by name with the owner's words that allowed it. When the
checkers pass, read them back on main, tell the owner in one line, and draft the next goal from every gap
still open between the proofs and what users see.

## Report

One message to whoever asked: the coordinator, the schedule ids, the pre-approval limit and decision queue
path, the first sense reading (class, evidence, action), and anything that needs the owner as one question
with what a yes unblocks.
