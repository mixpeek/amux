# amux issues found while orchestrating goal spec 12 (2026-09-30 to 2026-10-08)

GS-12 ran as 20 `gs12-*` lanes under one orchestrator (`mixpeek-override`), with amux-helper owning harness health. On 2026-10-08 at 21:35Z Ethan stopped the fleet and moved GS-12 to a single isolated worker.

This is the list of amux defects and design gaps that run exposed. Sources:
- amux-helper's session and its AH cards.
- The 35 GS-12 entries in `frustrations.md`.
- The server log.

Status is as of 2026-10-08 22:00Z. "Fixed" names the commit. "Open" names the card.

## Summary: what actually limited GS-12

1. **Coordination cost exceeded work.** Lanes closed about 135 cards a day, but on 2026-10-08:
   - The plan stayed at 233 items.
   - 345 of 718 open cards named no plan item.
   - 41 cards sat waiting on Ethan, most of which did not need him.
   - 33% of reviews failed and cost another round.
2. **amux had no scope primitive.** Nothing stopped cards from multiplying around a fixed plan. The plan doubled in board terms (575 to 652 tracked cards in one day) while its item count barely moved.
3. **Owner asks over-escalated.** Lanes parked decisions inside Ethan's standing authority, and one notice rule was read as an approval gate for 13 hours during a customer-facing incident.
4. **The fleet could not be stopped by an agent.** `amux stop` does not stick, because board-drive restarts any stopped lane holding a doing card, and only a human can archive a lane.

The proof-run bottleneck (one proof leg per ephemeral plane, 40 runs a day) is a mixpeek CI design issue and is not listed below.

## 1. Scope and card sprawl

| # | Issue | Evidence | Status |
|---|---|---|---|
| 1.1 | No scope lock: nothing ties a goal's cards to its plan items, so findings, follow-ups and incidents become new cards without limit | 2026-10-08: 345 of 718 open GS-12 cards named no plan item or proof row; 134 created and 135 closed that day | Open. Interim: amux-helper's hourly script counts plan items against a 233 baseline and cards that name no item |
| 1.2 | Board intake turns owner messages into "Structure request" cards that nobody acts on | amux-gs12-helper's mission (MSG-87814, MSG-87827) became AGH-1 to AGH-3 and sat in todo. amux-helper has about 20 discarded "Structure request" and "Owner ask" cards from the same run | Open |
| 1.3 | The turn-end classifier turns a worker's status sentence into an owner ask, and a blanket approval then lands on it | A production drop that was never requested got approved this way | Open, MO-4469 |
| 1.4 | Semantic intake can fold a create into an unrelated card (documented in CLAUDE.md) | Recurring during the run | Known; callers must read `card_created` |

## 2. Owner asks (needsyou) and approvals

| # | Issue | Evidence | Status |
|---|---|---|---|
| 2.1 | Lanes park decisions inside the owner's standing authority | 41 GS-12 needsyou cards on 2026-10-08. A triage by title found about 20 inside authority. GM-154, the MVS recreate fixing a 2 to 9 s customer search p99, waited about 13 h because the 30-minute notice rule was read as an approval | Open. No mechanism checks an ask against the boundary list |
| 2.2 | The needs-input auto-approval fired on an ask that excludes itself from auto-approval | GS-199 named a production data migration and a customer API change and was auto-approved | Open, GS-246 |
| 2.3 | The needs-input policy bounced a group-scope configuration ask back to the worker as a key or sign-in ask | One instance, 2026-10-08 | Open, MO-4465 |
| 2.4 | The automated "[amux owner-policy]" nudge reads as the owner's voice, saying "Ethan's standing authority covers this, so proceed now" | Fired after every question amux-helper asked, including fleet-wide stops, priority changes and re-enabling schedules the owner had turned off. Easy to mistake for consent | Open. The nudge should say it is automated and must never be used for scope, priority or overriding an owner action |
| 2.5 | Standing approval SA-6 suppressed an owner page during a production incident | MO-4009 | Fixed, AH-280 |
| 2.6 | No relay for owner asks once the relay schedule is off | SCHED-608 (the ask relay) was turned off, after which needsyou cards reached Ethan only when he asked for status | Open |

## 3. Contracts, review and verification

| # | Issue | Evidence | Status |
|---|---|---|---|
| 3.1 | Superseded contracts with no review state were never reviewed | 28 done cards looked unreviewed | Fixed, 58500fc5 |
| 3.2 | Frozen or failed checks at done stayed stuck forever | | Fixed, 02f1a4ab (re-checks after 6 h) |
| 3.3 | A third of reviews fail, mostly on gaps visible at plan time | 2026-10-08: 40 of 122 reviews failed (28 cards). 9 had missing evidence, 7 a verify command that checks only some criteria, 6 an unmet criterion | Fixed in part, 214eaf08: the pre-run review now reads every frozen-contract card and names unchecked criteria. Measure round-1 fail rate before and after |
| 3.4 | A done request on an escalated contract card skips the server check, so no round-4 review is ever queued | GD-75 (proof 12) | Open, MO-4466 |
| 3.5 | Only the owner can amend a frozen contract, so cards sit on wording the orchestrator has already ruled on | | Open, MO-4468 |
| 3.6 | A verify_cmd write on a todo contract card answers 200 with applied:false and no hint | Lanes read it as a refusal | Open, MO-4464 |
| 3.7 | Contract verification refuses on its own stale locked worktree under `~/.amux/tmp/contract` | | Open, GS-247 |
| 3.8 | Quota failures reopened passing work and consumed review rounds | | Open, AF-968 |
| 3.9 | Advisory review claims came before completed proofs | | Open, AF-968 |
| 3.10 | A session-swap auto-pickup reopens a card the contract granted done 69 s earlier | GE1-33 | Open |

## 4. Fleet lifecycle: stop, restart, recycle, capacity

| # | Issue | Evidence | Status |
|---|---|---|---|
| 4.1 | `amux stop` does not keep a lane stopped: board-drive resumes any stopped worker holding a doing card (`cause="stopped-worker-exact-claim"`) | 2026-10-08 21:32Z: all 20 lanes stopped, and 16 were running again within two minutes | Open. Needs a "stopped by owner" state that board-drive respects, or a group-level drive-off switch |
| 4.2 | Archiving a lane is human-only, with no agent path even on an explicit owner instruction | The handoff could not be completed by amux-helper | Open, by design. A brokered "owner said archive group X" path would close it |
| 4.3 | A provider usage-limit hold did not release at the reset time | gs12-extra-1 held overnight | Fixed, b716dc3a |
| 4.4 | A conversation recycle cut off by a server restart was never finished | | Fixed, 465c8e03 (boot resume, bounded retries) |
| 4.5 | Several lifecycle holds: a failed foreground held behind surviving background work, a passed quota reset still holding the parent, an expired reset re-parking its continuation, resume dedup suppressing a lost continuation, and a recycle retry bound silently cancelling the replacement | Six frustrations entries, 2026-10-08 | Open, AF-968 |
| 4.6 | A worker swapped to another model spent its first 45 minutes on its working directory: CC_DIR said repo root, workspace isolation picked a worktree, and there is no per-worker opt-out | amux-gs12-helper, 2026-10-08 | Open |
| 4.7 | `amux stop` printed "worktree removed" for three lanes during the fleet stop, with no statement of whether they were clean | gs12-compute, gs12-ops-deputy, gs12-deputy | Unverified. The stop should say why a worktree was safe to remove |

## 5. A2 pool and dispatch

| # | Issue | Evidence | Status |
|---|---|---|---|
| 5.1 | The A2 group cap was read at the wrong scope | | Fixed, 5b5f77b7 |
| 5.2 | TooLarge reported the wrong group size | It led amux-helper to a wrong cap change, reverted | Fixed, 51bb273b |
| 5.3 | `a2_pool_group_too_large` and `move_refused` logged every pass | | Fixed, 32a14e76 |
| 5.4 | Lanes sat idle while proof cards were parked on run triggers, with no fallback work | 2026-10-08: 4 of 20 lanes idle, A2 skipped parked cards 216 times | Open. Idle lanes should pull reviews or other lanes' failures |

## 6. Messages and steering

| # | Issue | Evidence | Status |
|---|---|---|---|
| 6.1 | Messages to a busy lane queue for a long time | 36 stalled deliveries at one hourly check, 6 to gs12-mvs during its incident work | Open |
| 6.2 | A lane blocked on a permission dialog cannot receive any message | gs12-retrievers missed the fleet checkpoint notice | Open |
| 6.3 | Steering was delivered mid-turn after waiting past AMUX_STEER_MAX_AGE_S | gs12-data, 2026-10-08 21:30Z | Open |
| 6.4 | Concurrent steering notices could overwrite each other's queue receipt | | Open, AF-968 |
| 6.5 | An isolated worker cannot be messaged, so after the handoff there is no incident path to GS-12 except through Ethan | By design | Accepted. Worth an owner-visible incident channel |

## 7. Landing and git guards

| # | Issue | Evidence | Status |
|---|---|---|---|
| 7.1 | `amux land` sits in "running" for 15 to 50 minutes on a worktree start timeout, holds the queue, and the holder cannot cancel it | | Open, MO-4463 |
| 7.2 | The land queue serializes a docs-only commit behind every lane's checkpoint | The handoff file waited at queue position 6 during the 21:35Z handoff | Open. A docs-only fast path would help |
| 7.3 | mixpeek's CLAUDE.md requires a push slot from `mixpeek-orchestrator`, which is archived, so the rule has no honest path | 2026-10-08 | Open, in mixpeek CLAUDE.md |
| 7.4 | The staged-guard times out at 18 s while the board answers in 0.1 s, so commits go unguarded | Seen on every amux-helper commit on 2026-10-08 while the server was rebuilding | Open, GC-183 |
| 7.5 | The git-shared-guard's sweep-consent escape cannot be walked from a tool call, and it reads a merge commit as a sweep | | Open, AH-401 |
| 7.6 | The push-guard used isolation as a proxy for "cannot consent" and refused a yes the author had given | | Open, AMUX-4972 |

## 8. Schedules and automation

| # | Issue | Evidence | Status |
|---|---|---|---|
| 8.1 | A schedule disabled from the dashboard records who but not why | SCHED-543, 550, 608 and 623 were turned off at 14:57Z within 3 s. Neither lane nor owner could tell whether it was deliberate | Open. Ask for a one-line reason on disable |
| 8.2 | A shell schedule's timeout kills only the outer bash, and the job tree runs on as an orphan | | Open, GG-104 |
| 8.3 | The bottleneck detector crashed under the scheduler's non-login PATH | | Fixed in SCHED-550's command (PATH export). Open as a general scheduler default |
| 8.4 | The lever loop's own measure stayed flat ("proof_stalled 22 to 22, no_effect" repeatedly) without escalating differently | 2026-10-08 | Open. The ladder fired once a day, then deduped |

## 9. Observability and server

| # | Issue | Evidence | Status |
|---|---|---|---|
| 9.1 | The server restarts on every commit, so long diagnostic reads and API calls fail intermittently | Several empty responses and a "store hung" health read on 2026-10-08 during builds | Known. Callers retry |
| 9.2 | Commands stalled behind database maintenance and retained-history scans | | Open, AF-968 |
| 9.3 | A successful board archive was undone in the dashboard by an older poll | | Open, AF-968 |
| 9.4 | A proof-progress number depended on which population it counted | amux-helper reported "22 of 97 proof cards" for a day. The finish-line measure was 14 of 49 proof rows | Fixed in practice. The pace tool should name its population |

## 10. Adjacent: harness tooling outside amux

| # | Issue | Evidence | Status |
|---|---|---|---|
| 10.1 | The orchestrator's queue scripts hardcode live paths in sourced helpers, so a test with an overridden queue path still wrote the live queue | 2026-10-08 16:28Z: the live proof queue was wiped; 5 entries restored, the lanes re-added 41 | Lesson recorded. The scripts should take the path from one variable |
| 10.2 | A repeating GCP log alert (MI-4014) emailed twice an hour for 12+ days on a static condition | Noise to the owner | Mitigated: the metric excludes that index; AH-405 has the real fix |

## What a better harness would have done

- **Scope as a primitive.** A goal's plan items are the only parents its cards may have. A card with no parent is refused or parked automatically.
- **Owner asks checked against the boundary.** An ask that names no boundary category goes back to the lane, with the category list, before it reaches the owner.
- **One owner-visible stop.** "Stop group X" holds until the owner lifts it, and board-drive and auto-resume respect it.
- **Throughput measured on the finish line.** Every pace and lever measure reads proof rows passed, not cards closed.
