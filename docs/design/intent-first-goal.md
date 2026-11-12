# Intent-first goal brief

The /goal condition points here. Read /Users/ethan/Dev/amux/docs/design/intent-first.md first.

## What to build (the design doc's phase order)

1. Router: POST /api/intent/plan (answer / send / create / continue).
2. Outcome threads and the Home view (Chat, Ideas, Goals, Workspace), the default for new people behind the home=intent|fleet pref.
3. Ideas drawn from the owner's messages to every worker plus email and calendar.
4. Inferred Goals with Tracking and Goals tiers, a status line each, worked on through scheduled cheap checks.
5. Self-configuration.

Also: an Outcomes tab first in the worker view, and a line on each worker card naming its outcomes.

Decided: routed and created workers are not isolated by default. Inferred goals start on their own but only read and draft until the owner approves their first outside action.

## How to work

- Reuse outcome_plan.rs, orchestrate.rs, conversations.rs, simple.rs, planning.rs, goal_loop.rs, the scheduler and memories. Add no ninth primitive.
- Work in a detached worktree at origin/main. Land each phase with amux land, run as: env -u AMUX_BOARD_FORCE_ADHERENCE -u AMUX_BOARD_DELEGATION -u CC_AUTO_PICKUP -u CC_AUTO_CONTINUE -u AMUX_COMMAND_LIFECYCLE -u AMUX_DISPATCH_BACKLOG_WHEN_IDLE amux land
- Land this brief and the design doc as the first commit.
- Mutation-check every new rule with scripts/mutate.sh.
- Every router decision, idea and goal records the messages it came from and returns measured and why.

## Use cases (each needs a Playwright spec in e2e/ against the live dashboard, at 375px and desktop width)

1. "Remind me to call the camp director about Rafi's waitlist" becomes a reminder AND an Idea offering to draft a nudge from the earlier camp email thread.
2. "Remind me to fill in Ezra's school application" becomes an Idea that checks Gmail for the confirmation and deadline emails and gives a done or missing verdict.
3. "Go through my inbox and find things for my kids' school" produces the items, and the inferred goal "Never miss a school form" appears under Goals with a status line.
4. "Plan dinners this week" produces a plan, and an inferred meal-planning goal appears.
5. "Rank my parent-teacher conference bookings before registration opens" produces a ranked order from the kids' grades and due work.
6. "Trace the $388 car rental payment from Tova" produces a one-page reconciliation from email and calendar.
7. "Stay ahead of the family calendar" produces a goal whose status line names the next event and what is ready for it.
8. A deadline item such as open enrollment appears under Tracking with the days left.
9. A follow-up in an existing outcome goes back to the same worker, and "Change" moves a misrouted request to another worker.

## Persona evaluation

For every use case, run a fresh-context evaluator subagent playing Dana: a busy non-technical parent who uses ChatGPT and Muse daily, has never heard of amux, a terminal or an AI model, and gives up after two confusing screens. Dana gets only the task in plain words and the amux browser on the dashboard at a phone viewport. No docs, no code.

Dana scores 1 to 5 on each of:

- finished without help
- got a correct, useful result
- never saw jargon (worker, session, model, provider, tmux, terminal, card, board, scope, YOLO)
- the number of steps felt like ChatGPT
- trust: she could see what would happen before anything was sent

Save each run's transcript, screenshots and scores to e2e/persona/<date>/<case>.json. Fix what Dana stumbles on and rerun with a fresh evaluator. Never reuse an evaluator that has seen the fix.

## Boundaries

- Reading Ethan's real email, calendar and messages is allowed.
- Nothing goes to anyone outside the company. Every email, text or message stays a draft, and the specs assert no send happened (GET /api/email/log shows no new send from these runs).
- No spending money. No changes to customer or production data. Do not edit the boundary guard files.
- If a case needs something only Ethan can provide (a sign-in, an approval), file one needsyou card with the question and what unblocks it, and keep working on the other cases.
