<!-- rules: root owner=amux-vision -->
# amux vision

The source of truth for what amux is for and the rules every change keeps. Binding for every
person and agent working on amux. Owner: `amux-vision`. Root `CLAUDE.md` says how to work
here; each folder's `CLAUDE.md` says what is true of that folder. Neither restates this file.

- **Before you build anything,** find the invariants your change touches and name them in the
  commit body and on the board card: `Vision: A2, A5`.
- **If your change cannot keep an invariant, stop** and raise it with `amux-vision` (or Ethan).
  Do not route around it.
- **If something here is wrong, change it here,** in its own commit. Rule files cite
  invariants by id and never restate them, so nothing else needs editing (A10).

## North star

**amux lets one person run a company with AI workers.**

The owner sets direction and makes the calls only they can make. Workers do the rest: they
build the product, run go-to-market and support, and handle the owner's admin. amux is where
the owner and the workers meet. It shows the owner what every worker is doing, carries the
owner's intent to them, and holds them to the owner's rules, from a desk or a phone.

**amux builds closed loops.** The aim is a software factory, in the sense of Factory or
Cognition: work runs from intent to a verified, live result, and what happens in production
(failures, friction, the owner's feedback) flows back in as the next piece of work, with no human
in the loop except for the owner's own decisions. A loop is closed when its output is measured
against the goal, and a miss starts the next turn on its own: a red main makes a card, a
frustration becomes a fix, a failed deploy rolls itself back.

**amux gets better as models get better.** It gives the model context, tools and reach, and
gets out of its way. A better model makes amux better. It never makes part of amux obsolete,
and amux is never the ceiling on what the model can do.

## Invariants

### A1. Eight primitives, no ninth

amux is built from workers, board, messages, schedules, files, groups, memories and
environment. A new feature is configuration of these plus the surface that shows it. A request
that seems to need a ninth primitive means the design is wrong.

### A2. Get out of the model's way

A change gives the model context, tools or reach. It never does the model's job for it. Ask
before building: when the next model is better, does this get better with it, or does it have
to be torn out?

- Build what the model cannot get by itself: where the work is, who else is working, what the
  owner decided, credentials and their limits, a durable record, a way to reach the owner.
- Do not build a scaffold over something the model will learn to do: orchestrating its
  reasoning, splitting its tasks for it, retrying its judgment, scraping and second-guessing
  its output.
- When the model's own harness offers a capability, amux uses it rather than building its
  own. Workers managing other workers did worse than letting Claude Code run its own
  subagents.
- A workaround for a model's weakness is deleted when a model no longer needs it. The ethos's
  known deviations (`.claude/rules/ethos.md`) are the list to watch.

### A3. The owner's attention is the scarce resource

amux spends it only on what is the owner's to decide: money, anyone outside the company,
production data, direction, and the `[owner]` paths in `scripts/review-tiers.txt`. Everything
else gets done without asking. A gate the owner set is never bypassed.

### A4. Nothing happens silently

Every action is attributed to who did it. Every claim of done carries evidence (`VERIFY.md`).
Every failure shows up where the owner and a sweep already look: a bug fix brings the fix at
the root cause and a log signal so the next instance announces itself.

### A5. An instrument says whether it measured

A diagnostic that can read zero or empty also reports `measured` and `n_considered`, so a
quiet window and a probe that never ran never look alike. Enforced by
`crates/amux-server/tests/diagnostic_contract.rs`.

### A6. Work is done when it is live on main

Everything lands on `origin/main` through the land gate: no branches, no forks, no hand-built
binaries. A request is finished when its change is live, or when it is closed with a reason.

### A7. One amux, everywhere

The server is the same code on a Mac and in the cloud: no `if IS_CLOUD`. Differences come
from environment and gateway headers. The dashboard, the Mac app and the phone are surfaces of
the same state.

### A8. Reuse the paradigm that exists

A folder is the file view, a prompt is the shared dialog, a list is the existing list. A
missing hook is added to the shared one. A second copy drifts from the first on every later
change.

### A9. Isolation means isolation

An isolated worker gets direct owner-to-CLI transport: no board, no injected prompts, no
routing. Only the exceptions the owner configured reach it (`crates/amux-server/CLAUDE.md`).

### A10. One source per fact

Each fact has one source. Everything else links to it or computes it from it. A copy that
must be kept in sync is a defect.

### A11. A change to shared infrastructure is proven against the failure it can cause

The tmux server, the deploy pipeline, the land gate and the outbox are shared by every
worker. A change to one is tested against how it can fail the whole fleet, and ships with a
way to turn it off. On 2026-10-29 a tmux control client passed every test and still crashed the
tmux server holding every worker.

## Where it is going

- **From operator to owner.** Today the owner still drives individual workers. The end state
  is the owner setting goals and amux running the fleet against them.
- **Every loop closed.** Each surface (the product, go-to-market, support, the harness itself)
  runs as a loop that measures its own result and acts on a miss, so the owner reviews outcomes
  rather than steps.
- **Workers that improve the harness.** Workers find their own friction (`frustrations.md`),
  fix it at the root, and get faster and cheaper over time.
- **The same thing for anyone.** Cloud amux gives another founder this without running a Mac.

## Values

The eight rules in `.claude/rules/ethos.md` are the lens every review applies. The incidents
behind them are in `docs/ethos-incidents.md`.
