# Automations / Robots

**Status:** design proposal (2026-07-03)
**Owner:** Ethan
**One-liner:** A first-class tab where users define automations as Markdown SOPs and amux executes them by driving a browser (and, locally, the desktop) — intent re-resolved every run, not brittle coordinate replay.

---

## 1. Vision

amux already orchestrates a fleet of *coding* agents. Automations extend that to *GUI* work: repetitive SaaS/web/desktop tasks a human does with a mouse and keyboard — pulling a report, reconciling two dashboards, filing a form, checking a status every morning.

A **robot** is a saved, MD-defined automation. It has a trigger (manual / schedule / webhook), a target (a browser profile or the local desktop), and a body describing the SOP. Running it produces a **run** with a recording, a pass/fail, and a place it stopped.

The differentiator is **composition**, not the automation itself (browser-agents are a crowded space). In amux these are first-class citizens next to coding agents: schedulable, phone-monitorable, and able to hand off to/from a Claude Code session (robot scrapes a dashboard → coding agent acts on the result). Nobody else has that in one pane.

## 2. Core principle — intent, re-resolved every run

The single most important design decision. "Mouse + clicks + keyboard" must NOT mean recorded coordinates replayed blindly (the classic RPA trap — breaks on the first resolution change, layout shift, A/B test, or slow load). Instead the MD describes *what* to accomplish; an agent inspects the current screen (DOM / accessibility tree / screenshot) and decides the next action each run. The mouse/keyboard is the **actuator of last resort**, never the plan.

### Execution ladder

Every step resolves down this ladder, preferring the highest-reliability rung that works:

| Rung | Mechanism | When | Cost/reliability |
|---|---|---|---|
| 1. API / CDP | direct HTTP call, Chrome DevTools Protocol, DOM selector | target exposes it | fast, cheap, deterministic |
| 2. Browser vision agent | `/api/browser/agent` + saved profile | dynamic web UI, no stable selector | medium |
| 3. Desktop computer-use | Claude vision loop → real mouse/keyboard (cliclick/pyautogui) | native app, no API — **local only** | slow, most brittle |

Cache the resolved plan while the UI is stable; **re-plan on failure**. This gets determinism's speed/cost with vision's resilience.

## 3. The robot (MD schema)

A robot is one Markdown file — same authoring model as a Claude Code `SKILL.md`, pointed at a GUI. Stored at `~/.amux/robots/<slug>.md` (flat file → git-friendly, human-editable, diffable), run history in SQLite.

```markdown
---
name: Pull daily Stripe payouts
slug: stripe-daily-payouts
trigger: schedule            # manual | schedule | webhook
schedule_expr: daily at 8am  # when trigger=schedule (reuses scheduler grammar)
target: browser:stripe       # browser:<profile> | desktop
allow_irreversible: false    # gate destructive/outward-facing steps (see §8)
handoff: null                # optional: session to notify with the result
---

## Goal
Grab yesterday's payout total from the Stripe dashboard and post it to the board.

## Steps
1. Go to https://dashboard.stripe.com/payouts
2. Filter to yesterday's date range.
3. Read the total payout amount.
4. Post it to the amux board as an issue titled "Stripe payout <date>: <amount>".

## Success when
An issue exists on the board with yesterday's payout amount.

## Notes
If a 2FA prompt appears, stop and surface it — do not attempt to bypass.
```

Frontmatter is machine-read; the body is the SOP the runner-agent follows. `## Success when` gives the runner a checkable stop condition (avoids "did it actually work?" ambiguity).

## 4. Architecture

```
Automations tab (UI)
      │  CRUD robot MDs
      ▼
/api/robots/*  ──►  ~/.amux/robots/<slug>.md   (source of truth, flat MD)
      │
      ▼
Runner  ──► walks execution ladder (§2) ──► browser API / CDP / desktop actuator
      │                                         │
      │  emits run events (SSE)                 └─ screenshots + record (MP4)
      ▼
robot_runs (SQLite in amux.db)  ──► run log, pass/fail, stopped-at, artifacts
      │
      └─ optional handoff: POST /api/sessions/<handoff>/send with the result
```

- **Source of truth:** MD files (like notes/memories). **Queryable state:** SQLite (like issues).
- **New SQLite table `robot_runs`:** `id, slug, started, ended, status (running/passed/failed/stopped), stopped_at_step, artifacts_json, error`.
- **Runner** is a Python worker thread (same pattern as existing background workers) that reads the MD, drives the ladder, streams events over the existing SSE channel, and writes the run row.
- **Triggers:** `trigger=schedule` registers with the existing `/api/schedules` (the schedule's `command` invokes the runner for that slug). `manual` = a Run button. `webhook` = a POST endpoint that kicks a run.

## 5. Reuse map (why this is cheap to prototype)

| Need | Already in amux |
|---|---|
| Triggers | scheduler (`/api/schedules`) — cron/interval/manual |
| Auth to sites | saved browser profiles (`/api/browser/profiles`) |
| Execution (browser) | `/api/browser/agent`, `/api/browser/action`, `chrome-cdp` skill, Playwright |
| Run evidence | `/api/browser/screenshot` + the `record` skill (MP4/GIF per run) |
| Storage / history | MD files (source) + SQLite `robot_runs` (history) + board (optional per-run issue) |
| Live monitoring | existing SSE + peek/dashboard already on your phone |
| Handoff to coding agents | `POST /api/sessions/<name>/send` |

The genuinely new surface: an **Automations tab** (CRUD over robot MDs) + a **runner** that walks the ladder + a **run log** with the recording attached.

## 6. API (proposed)

```
GET    /api/robots                 list robots (frontmatter + last run summary)
GET    /api/robots/<slug>          full MD + run history
POST   /api/robots/<slug>          create/update MD (body = markdown)
DELETE /api/robots/<slug>          trash (soft delete, like notes)
POST   /api/robots/<slug>/run      kick a run now → returns run_id
GET    /api/robots/<slug>/runs     run history
GET    /api/robots/runs/<run_id>   one run: status, steps, artifacts, error
POST   /api/robots/runs/<run_id>/stop   abort a running robot
POST   /api/robots/<slug>/webhook  (when trigger=webhook) external kick
```

## 7. UI — Automations tab

- **List:** cards, one per robot — name, target badge (browser:profile / desktop), trigger, last-run status dot + timestamp, a **Run** button.
- **Editor:** MD editor (reuse the notes editor) with a live frontmatter form on top (name / trigger / schedule / target / guardrails). A **Dry run** button (see §8).
- **Run detail:** step-by-step timeline with the resolved rung per step, inline screenshots, the run recording (MP4), pass/fail, and — on failure — exactly which step stopped and why.
- Mobile: same breakpoints/touch-target rules as the rest of the dashboard; Run + Stop must be ≥44×44.

## 8. Safety & guardrails

A robot clicking around with saved logins can do real damage. Non-negotiable:

- **Dry run / preview:** resolve the plan and show intended actions *without* executing side-effectful ones.
- **Confirm-before-irreversible:** any destructive or outward-facing step (submit, send, delete, pay) is blocked unless `allow_irreversible: true` **and** — for high-stakes targets — a per-run confirmation. Same discipline as the shared-checkout git guard.
- **Checkpoints:** a failed run resumes from the last good step, not from scratch.
- **Hard stop + clear failure surface:** every run has a stop condition (`## Success when`) and a visible "stopped at step N because X".
- **Scoped auth:** a robot names exactly one profile/target; it can't reach beyond it.
- **2FA / captcha:** stop and surface, never attempt to bypass.

## 9. Local vs cloud (single-codebase rule)

Per `CLAUDE.md`, `amux-server.py` is identical local and cloud — no `IS_CLOUD` branches. Automations respect this via **runtime capability detection**, not build flags:

- **Browser-tier robots** work everywhere (local *and* cloud containers) — Playwright runs headless.
- **Desktop-tier robots** (real mouse/keyboard) need macOS Accessibility + Screen Recording perms and a physical desktop; they take over the machine while running and have **no cloud story** (containers have no desktop).
- Gate desktop tier on *"is there a desktop + perms?"* detected at runtime. The `target: desktop` option simply doesn't light up where no desktop exists. No cloud/OSS code fork.

## 10. Phasing

**MVP (phase 1) — browser only.** You already own the whole stack.
- `/api/robots/*` CRUD over `~/.amux/robots/*.md`
- Runner: rung 1 (CDP/DOM) + rung 2 (`/api/browser/agent`), profile-scoped
- Manual Run + schedule trigger (reuse `/api/schedules`)
- `robot_runs` table + run detail with screenshots and recording
- Automations tab (list + editor + run detail)
- Guardrails: dry run, confirm-before-irreversible, success-when stop condition

**Phase 2 — desktop computer-use (local only).** cliclick/pyautogui actuator behind capability detection; Claude vision loop for native apps.

**Phase 3 — composition.** Handoff to/from coding sessions; webhook triggers; robots that call other robots; board integration (each run optionally logs an issue).

## 11. Open questions

- Do robots live as MD files (proposed, git-friendly) or fully in SQLite? Proposal: MD source + SQLite history.
- Recording every run (storage cost) vs. only on failure / on demand.
- One shared browser vs. per-robot browser context for parallel scheduled runs.
- How much of the plan to cache vs. re-resolve — heuristic for "UI changed, re-plan".
- Auth for webhook triggers (shared secret per robot).
```
