# Demo video audit, 2026-11

Shipped, user-facing amux features with no demo clip on /demos/, ranked by how much a 15 to 30 second clip would help someone choose amux. Checked against `origin/main` on 2026-11-08: every evidence path below exists there. Rows 3, 10, 11 and 20 were recorded on 2026-11-09 (see below); the rest are still open.

## Current set, 2026-11-09

Ethan asked for /demos/ to show amux features only, no use cases. It now has 11 clips, one feature each:

| Area | Demo | Page |
|---|---|---|
| Workers | Worker Cards | /demos/session-cards-overview/ |
| Workers | Send to Any Worker | /demos/send-command-dashboard/ |
| Workers | Live Status and Restarts | /demos/self-healing-watchdog/ |
| Workers | Messages (new) | /demos/messages/ |
| Board & schedule | Board | /demos/kanban-board-overview/ |
| Board & schedule | Scheduler (new) | /demos/scheduler/ |
| Board & schedule | Calendar | /demos/ical-calendar-sync/ |
| Files & apps | File Viewer (new) | /demos/file-viewer/ |
| Files & apps | Apps (new) | /demos/apps/ |
| Files & apps | Draw | /demos/draw-realtime-sync/ |
| Phone & sync | Phone App | /demos/mobile-pwa/ |

Removed: the 15 use-case demos (overnight, parallel features, tests, refactoring, bug triage, code review, docs, API generation, migration, modernization, monorepo, dependency updates, security audit, CI/CD, research). Folded into the feature they belong to: atomic task claiming into Board, agent orchestration and agent coordination into Messages, conversation fork into Send to Any Worker (fork has no dashboard entry, AMUX-6801), the dashboard tour into Worker Cards. Every removed URL is a noindex redirect.

Found while recording:

- A Scheduler tab open on a second device never shows a schedule made elsewhere until the tab is reopened. The SSE `invalidate` handler in `app.js` acts on board, journal, messages and sessions only.
- Run now prefixes the delivered message with an `[amux] Run-now ...` note that reads as internal plumbing in a terminal; a cron fire delivers the bare message.
- A peek on a loaded host can lag the phone by several seconds and show "Reconnecting... retrying terminal updates" (AMUX-6802). This sank the Offline Sync clip: in all three takes the banner showed on the laptop while the phone's queue replayed. Offline sync itself works (the phone saves the message offline, then syncs and the agent starts); re-record it on a quieter host. A second round of three takes at lower load showed no banner, but in every take the laptop's terminal header kept reading WORKING after the agent finished (AMUX-6796), and the one composed for review showed a Claude Code startup warning on the phone. Seven takes, none shipped.

A footer check (the recorder kit's `footcheck.py`: in every frame, OCR the single line under each terminal at 3x) found that banner in three clips already live from the first round: Worker Cards, Send to Any Worker and Phone App. Whole-frame OCR had missed it. All three were re-recorded on 2026-11-09 with the footer check as part of the recorder gate, and every clip on the page now reads clean on it.

Next to record: Offline Sync (see above), then needs-you approvals (1), Git panel (15), Vault (19) from the table below.


**Hard to capture** means it needs something the private demo server does not have: a real outside account, a device, or a native app build.

| # | Feature | Why it sells amux | Who cares | What the 15 to 30s clip shows | Capture effort | Evidence |
|---|---|---|---|---|---|---|
| 1 | Needs-you inbox and phone approvals | The fear with agents is that they stall or do something unwanted. This answers both. | Both | Agent asks to run a command, the phone bell lights up, one tap approves, agent continues on the laptop | Easy | `crates/amux-server/src/api/needs_input.rs`, `standing_approvals.rs` |
| 2 | Keeps going through usage limits | Unique reason to leave agents running overnight | Dev | Worker hits a limit, card reads held, resumes at the reset with the queued message delivered | Medium: needs a stubbed limit | `crates/amux-server/src/api/agent_limit_resume.rs` |
| 3 | Scheduler | Recurring work without a person is the whole Concierge pitch | Both | Type "every weekday at 9am", a run fires, the worker wakes and reports | Easy | `crates/amux-server/src/api/schedules.rs`, `#tab-scheduler` |
| 4 | Email handled by an agent | The clearest non-technical outcome: inbox triaged, reply drafted, you approve | Non-technical owner | Inbox tab, agent drafts a reply, owner approves from the phone | Hard: needs a demo mailbox | `crates/amux-server/src/api/email.rs`, `email_approval.rs`, `#tab-email` |
| 5 | Browser automation with saved logins | Agents can work in software that has no API | Both | Agent opens a site in a saved profile and fills a form while you watch | Medium: needs a demo site | `crates/amux-server/src/api/browser.rs`, `browser_routing.rs`, `#tab-browser` |
| 6 | Any model, per worker | Answers "am I locked in to one vendor?" | Dev | Claude, Codex and Gemini cards side by side, one model switched from the card | Medium: needs Codex and Gemini logged in | `crates/amux-server/src/api/worker_create.rs`, README "Any model" |
| 7 | Usage and Optimization | Shows where the spend goes and what it bought | Both | Usage tab, spend per worker, a recommendation opened | Easy with seeded history | `crates/amux-server/src/api/usage.rs`, `#tab-usage` |
| 8 | Big goals that run for days | Strongest "the agent keeps working" story | Dev | Set a goal, a checker reports not met, the worker continues, the goal closes | Medium: long run, sped up | `skills/big-goal.md`, `crates/amux-server/src/api/goal_loop.rs` |
| 9 | Voice: dictate and talk to a worker | Hands-free steering is memorable and shows well on a phone | Both | Hold to dictate on the phone, words land in the composer; Converse answers aloud | Medium: audio capture | `crates/amux-server/src/api/dictation.rs`, `converse.rs` |
| 10 | Apps: publish what an agent builds | Turns agent output into a link you can share | Both | Agent builds a small app, `amux publish`, the link opens on the phone | Easy | `skills/amux-publish.md`, `crates/amux-server/src/api/pages.rs`, `#tab-pages` |
| 11 | Agents messaging each other | Shows real teamwork without a person relaying | Dev | One worker asks another a question, the reply appears in Messages | Easy | `crates/amux-server/src/api/worker_messaging.rs`, `messages.rs`, `#tab-messages` |
| 12 | Memory and scoped rules | Agents remember how your team works | Both | Add a rule to a group, the next worker follows it | Easy | `crates/amux-server/src/api/memories.rs`, `scope.rs` |
| 13 | Board gates and evidence | Work cannot be marked done without proof | Dev, team leads | Agent tries to close a card, the gate refuses, evidence added, it closes | Easy | `crates/amux-server/src/api/board.rs`, `criteria.rs` |
| 14 | Land gate | Tests and a fresh review run before anything reaches main | Dev | `amux land` replays commits, review finding appears, fixed, pushed | Medium: real repo and CI | `amux` (`cmd_land`), `scripts/land_gate.py`, `crates/amux-server/src/api/land_queue.rs` |
| 15 | Git panel on a worker | Review an agent's diff without a terminal | Dev | Open a worker, Git tab, read the diff, commit from the browser | Easy | `crates/amux-server/src/api/sessions_git.rs`, `peekGit*` in `app.js` |
| 16 | Translate view | A plain-language summary of a terminal for people who do not read code | Non-technical owner | Same worker, terminal on the left, Translate in plain English on the phone | Easy | `Translate` tab in `crates/amux-dashboard/static/index.html`, `crates/amux-server/src/api/session_verbs.rs` |
| 17 | Steering | Change an agent's direction mid-run | Dev | Steering tab, add a note, the running worker adjusts | Easy | `crates/amux-server/src/api/session_verbs.rs`, Steering tab in `app.js` |
| 18 | Connectors | Plug agents into the tools a business already uses | Non-technical owner | Connectors tab, one connected, an agent uses it | Hard: needs a demo account | `crates/amux-server/src/api/connectors.rs`, `#tab-connectors` |
| 19 | Vault | Agents use credentials without ever seeing them | Both | Add a secret, a worker uses it, the value never appears in the terminal | Easy | `crates/amux-server/src/api/vault.rs`, `#tab-vault` |
| 20 | Files and previews | Read what agents produced without leaving the browser | Both | Files tab, open a markdown file and a deck, edit one | Easy | `crates/amux-server/src/api/files.rs`, `file_viewer.rs`, `#tab-files` |
| 21 | MDAI writing surface | Docs edited with an agent beside them | Both | Open a doc, ask for a change, it applies in place | Easy | `crates/amux-server/src/api/mdai.rs`, `#tab-mdai` |
| 22 | Cmd-K everywhere | Speed for people who live in the dashboard | Dev | Cmd-K, jump to a worker, a file, then an action | Easy | `crates/amux-server/src/api/palette.rs`, `app.js` |
| 23 | iPhone and iPad app | Native polish over the phone browser | Both | Real device: push notification, open worker, reply | Hard: device recording | `ios/AmuxApp` |
| 24 | Mac app panes | Several workers tiled in one native window | Dev | Native window, drag panes, snap zones | Hard: native capture | `desktop/amux-panes.swift`, `desktop/amux-snapzones.swift` |
| 25 | Computer use on a sandboxed desktop | Agents can drive software with no browser | Dev | Agent opens a desktop app in the sandbox and completes a task | Hard: sandbox and capture | `crates/amux-server/src/api/computer.rs` |

## First three to record

Numbers 1, 3 and 16. All are easy on the existing capture kit, and together they tell the non-technical story the Concierge and solutions pages now sell: it runs on its own, it asks before anything risky, and you can read what it did in plain English.
