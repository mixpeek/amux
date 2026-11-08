# amux orchestration against Factory Missions and Devin (AGH-5)

Written 2026-10-31. Every claim about Factory or Devin below comes from their
own documentation, read on 2026-10-31, and links the page. Every claim about
amux comes from this repository at the time of writing or from this machine's
live data, with the command that produced it.

An earlier pass (2026-10-08, on AGH-5) had two claims that no longer hold:
Factory's Missions overview does not describe validator workers or per-role
models (those are on the reference page), and Devin's
`automation-templates/circleci-failure-fix` page now returns 404. CI-failure
handling is part of Devin's general Automations page instead.

## Side by side

| Area | Factory Missions | Devin | amux |
|---|---|---|---|
| Decomposition | "a structured plan: a set of features organized into milestones", built in a planning conversation ([overview](https://docs.factory.com/missions/overview)) | Playbooks hold reusable procedures ([creating playbooks](https://docs.devin.ai/product-guides/creating-playbooks.md)) | A request becomes board cards, one per unit of work; `/orchestrate` drives a plan to verified completion |
| Model roles | Separate settings: `missionOrchestratorModel`, `missionModelSettings.workerModel`, `missionModelSettings.validationWorkerModel` ([reference](https://docs.factory.com/missions/reference)) | Not described on the pages read | Coordinator and executor profiles (`crates/amux-core/src/project.rs`); `AMUX_HELPER_MODEL` for helpers |
| Independent validation | "scrutiny" and "user-testing" validation milestones, each skippable ([reference](https://docs.factory.com/missions/reference)); "runs user-facing QA testing against your application to validate each feature" ([overview](https://docs.factory.com/missions/overview)) | "Devin can test your application end-to-end after creating a PR", with a screen recording attached to the session ([testing and recordings](https://docs.devin.ai/work-with-devin/testing-and-recordings.md)) | Column gates on every card (`done` needs evidence, `verified` needs prod proof); the land gate runs tests, e2e and a fresh-context review before main |
| Event triggers | Not described on the pages read | Automations fire on a CI check completing, PR and issue events, Linear and Jira tickets, Slack, schedules and webhooks, then start or message a session ([automations](https://docs.devin.ai/product-guides/automations.md)) | The scheduler, wake-watches on CI runs, and autofix cards filed from red CI, failing invariants and 5xx groups, routed to a lane |
| Recovery | Manual: "pause the orchestrator, describe what you are seeing in plain language, and ask it to recover"; "Missions are not fire-and-forget" ([overview](https://docs.factory.com/missions/overview)) | Sessions can be resumed from the CLI ([essential commands](https://docs.devin.ai/cli/essential-commands.md)) | Crash-safe recovery under real SIGKILL (`scripts/test-harness-recovery.sh`, PR #239); a stalled lane is detected and re-driven by board-drive |
| Spend joined to outcomes | Agent Effectiveness links "a session to the pull requests, tickets, and documents it worked on" and exposes "the agent spend behind each pull request, ticket, project, or session"; coverage is limited to "the systems you connect" ([agent effectiveness](https://docs.factory.com/agent-effectiveness/overview)) | Usage is reported per session and per user; the usage page describes no join to PRs or tickets ([usage](https://docs.devin.ai/admin/billing/usage.md)) | `token_ledger.task` joins spend to a card only while that card is claimed (`runtime_jobs::token_ledger::attribute_tasks`) |

## The gap chosen, with its baseline

Of the ranked candidates on AGH-5, crash recovery has merged (PR #239,
c0cff3f30f, with its recovery suite). Spend attribution has no fix on main:
AGH-6's `/api/harness/efficiency` never landed (the route answers 404) and its
lane is unregistered. Factory's attribution is the closest comparison, and it
is also the gap with a number.

Baseline, the 7 days to 2026-10-31 ~08:30Z, read-only from `token_ledger`:

| Population | Spend | Joined to a card |
|---|---|---|
| All lanes | $96,104 | $28,887 (30.1%) |
| Isolated lanes (no board by design) | $14,289 (14.9% of all) | not attributable |
| Lanes with a board | $81,816 | $28,782 (35.2%) |

The largest unattributed lanes with a board: mxp-gs12 $25,967 (19% joined),
tubescience-parity $7,523 (38%), mixpeek-cicd $5,143 (39%), amux $4,537 (8%,
its auto-pickup is off, so it rarely claims a card), and $1,403 under an empty
session name.

So in a lane with a board, about two dollars in three are spent outside any
card claim, and nothing reports that. Factory names its coverage limit to the
user ("Attribution and the Output view only cover the systems you connect");
amux has no equivalent statement.

```bash
sqlite3 -readonly "file:$HOME/.amux/amux.db?mode=ro" \
  "SELECT SUM(cost_usd), SUM(CASE WHEN COALESCE(task,'')<>'' THEN cost_usd END)
   FROM token_ledger WHERE ts > strftime('%s','now','-7 days')"
```

## Next

The fix belongs in attribution itself, so this number moves rather than only
being reported: find what an unclaimed turn in a board lane was doing (a
delivered owner message carries its captured card, an auto-pickup or a
deploy-wake names its card, a schedule fire names its schedule), join that
spend to the card when one exists, and report coverage beside the totals with
`measured` and `n_considered` so an unattributed share is visible where spend is
read. Before and after are measured with the query above, on the same window
length.
