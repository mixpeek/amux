# Harness changes from the 2026-10-31 velocity audit

Source: `~/Dev/mixpeek/research/fleet-audits/2026-10-31-dev-velocity-problems-deep-dive.md`
(five workers, 10-08 to 10-31). That document lists problems only. This one
takes the problems amux owns or can change, and proposes one change for each
with an invariant that would show whether it worked.

The audit assigns origins (its section 1.4): owner direction, worker
execution, task design, supervision, harness, infrastructure. amux can fix
the harness rows directly. It can also make the owner-direction and
supervision rows cheaper and more visible, and that is the only lever it has
there. Mixpeek-only problems (GS-12 row design, MVS shard limits, GCP pools)
are left out.

Every proposal names:

- **Challenge:** the audit's finding, with its evidence ids.
- **Change:** one of remove, KISS (simplify), refactor, clean up, build.
- **Invariant:** a predicate over data amux already keeps, the threshold that
  passes, and what it reads today. Each one ships as an `/api/health/invariants`
  check with `measured` and `n_considered`, per `docs/` and
  `tests/diagnostic_contract.rs`, unless it says test.
- **Breaks if:** the mutation that must turn the check red (ethos rule 7).

## Measured baseline (2026-10-31, before any change)

`scripts/velocity-invariants-probe.py` runs each invariant below against live
amux data (the API, `amux.db` read-only, git, docker, ps and the guard hooks)
and reports pass, fail or unmeasured with `measured` and `n_considered`.
Unmeasured means amux keeps no data that could answer it yet, which is itself
the gap. Run it again after each change: the invariant must move from fail to
pass, and the change's own check then replaces the probe.

Result on 2026-10-31 afternoon: **7 pass, 14 fail, 6 unmeasured** (27
probes, including item 27 added for the board switch). A probe whose source
did not answer (an API timeout, an empty board read, a missing server log)
reports unmeasured; an earlier draft let three of them read as passes, which
the land review caught.

| # | Verdict | Reading | n |
|---|---|---|---|
| 1 | pass | 4 lanes idle 30 min with eligible To Do (long-form-videos, mixpeek-ops-server, studio-plg, social-activities); each has a fresh board-drive reason. The reason is per lane, so the per-card half is unbuilt | 20 idle lanes |
| 2 | fail | 15 parked cards whose waiting_on card is already terminal (MGR-40, MGR-31, MS-1650, MF-4578, MI-5991, GCA-54, MGR-21, MGR-22, ...); 51 of 73 parked cards name no card at all (prose) | 73 parked |
| 3 | fail | 5 doing cards on lanes quiet 45 min with other eligible work (GE-1142, LFV-1, GE-1116, GE-1102, GE-1057) | 15 doing |
| 4 | pass | 0 fleet-cap refusals in the last 40 MB of server log. Weak: the probe greps for the refusal's wording | 1 |
| 5 | pass | 129 owner messages in 24 h, all delivered. One earlier run caught MSG-102505 queued 10 min behind amux-app's turn; it was delivered later, so it counts toward item 6's latency | 127 |
| 6 | unmeasured | no priority flag exists, so no priority latency can be timed | 0 |
| 7 | fail | 20 messages repeated to the same recipient by the same sender within 6 h | 934 messages, 24 h |
| 8 | fail | 660 messages in 7 days typed `user` with origin `api (no worker identity)`, which render as the owner | 1,763 |
| 9 | fail | 59 messages in 24 h carry the "no reply path" footer | 934 |
| 10 | fail | background share 86.0% of 11,496 USD rate-card spend in 24 h; board-drive nudges 3,476 USD, peer messages 3,265 USD, owner prompts 1,608 USD | 13,661 turns |
| 11 | pass | no enabled tmux schedule fired more than 5 times in 24 h. No-change detection is unbuilt, so this is an upper bound | 40 schedules |
| 12 | fail | 0 of 1,662 verified cards name a check whose result can be read | 1,662 |
| 13 | fail | 548 done-to-discarded moves in 7 days, 527 of them by `backend` in one hour (10-30 23:00) | 548 |
| 14 | unmeasured | owner rulings are not recorded as rulings anywhere amux reads | 0 |
| 15 | fail | longest run of consecutive "not met" goal-keeper prompts in 7 days: 10 | 17 |
| 16 | fail | 1,492 of 2,750 land logs from 7 days have no outcome line (landed, refused or failed) | 2,750 |
| 17 | pass | 0 schedules past their `until`, 0 plan-reserve refusals in the recent log. Expiry notices are unbuilt, so a future expiry would still go unannounced | 463 |
| 18 | unmeasured | no path-triggered schedule kind exists | 0 |
| 19 | unmeasured | no restart context message exists to look for | 1 restart in the log window |
| 20 | pass | all 42 non-isolated agent processes carry `AMUX_SESSION`. The audit's empty-identity reports came from shells inside a pane, which this does not read | 42 |
| 21 | unmeasured | the watchdog's server-pid file is absent: the fix (a6ab9dc49) is on main, but launchd runs the watchdog from the shared checkout, which is behind | 1 |
| 22 | fail | the installed guards let `pkill -f node`, `killall python3` and `rm -rf` of a repo folder through; no boundary in `boundaries.toml` covers them | 4 cases |
| 23 | fail | 9 of 9 running containers lack an owner label, and memory limits are missing on most (ts-dropin-mx, ts-appstate-pgrst, ffe-wt-postgrest, ...) | 9 |
| 24 | unmeasured | a scan needs vault values, which the probe must not read | 0 |
| 25 | fail | amux's own machine-commit share is 0.2% (passes); 1,479 land logs older than 14 days are not archived (fails) | 640 commits |
| 26 | fail | `/api/usage/attribution?session=mxp-gs12` returns 9 rows from other sessions; the window does honour 168 hours, so the 24-hour half of the audit's claim is stale | 10 |
| 27 | pass | 16 workers have the board switched Off; none got a card or a board delivery in 24 h. See item 27 | 16 |

Existing invariants that already overlap and fail today:
`board.todo_is_reachable_by_dispatch` (items 1 to 4),
`board.nonterminal_has_disposition` (item 2), `board.repeat_offers_are_visible`
(item 7) and `schedule.target_can_receive` (item 9).

Removals come first because the audit's strongest pattern is workarounds
stacked on harness defects (Part 5, recurring theme 3: "Waker, watches, note
delivery and card moves all stand in for dispatch").

## Summary

| # | Change | Kind | Audit theme | Invariant id |
|---|---|---|---|---|
| 1 | A lane with eligible work is never idle without a recorded reason | build | 9, 10 | `dispatch.idle_lane_has_reason` |
| 2 | Structured `waiting_on` replaces prose triggers | remove + KISS | 10 | `board.parked_card_trigger_resolves` |
| 3 | A waiting `doing` card gives up its slot | KISS | 10 | `dispatch.doing_card_is_progressing` |
| 4 | Remove the global dispatch cap in favour of per-lane slots | remove | 5, 10 | `dispatch.no_cross_lane_slot_wait` |
| 5 | Owner text always reaches the pane | KISS | 10 | `message.owner_send_reaches_pane` |
| 6 | Priority owner messages land mid-turn | build | 10 | `message.priority_send_latency` |
| 7 | Identical messages are delivered once | build | 8 | `message.duplicate_suppressed` |
| 8 | Owner voice needs a verified stamp | remove | 8 | `message.owner_label_is_stamped` |
| 9 | Any delivered message can be answered | KISS | 8, 10 | `message.reply_path_exists` |
| 10 | Background model spend is measured and capped | build | 7, 8 | `usage.background_share` |
| 11 | A no-op scheduled model turn pauses itself | remove | 8, 9 | `schedule.model_fire_changes_state` |
| 12 | `verified` follows a live check | build | 4 | `board.verified_has_passing_check` |
| 13 | Finished and dropped stay distinct | clean up | 10 | `board.done_not_bulk_discarded` |
| 14 | Owner rulings are recorded once and superseded explicitly | build | 1 | `ruling.reissue_rate` |
| 15 | A goal that cannot move stops re-prompting | KISS | 10 | `goal.keeper_stops_on_flat` |
| 16 | Every land either reaches main or leaves a receipt | build | 10 | `land.request_has_outcome` |
| 17 | Schedules announce expiry; critical ones skip the plan reserve | build | 10 | `schedule.expiry_announced`, `schedule.critical_not_refused` |
| 18 | Path-triggered schedules replace re-proof request posts | build | 2 | `schedule.path_trigger_latency` |
| 19 | A restarted worker gets its open work back | build | 13 | `worker.restart_restores_context` |
| 20 | Every worker knows who it is | clean up | 13 | `worker.identity_env_present` |
| 21 | Fleet recovers when tmux is replaced | build (done) | 10 | `session.tmux_replacement_recovered` |
| 22 | Destructive pattern commands are refused for subagents | build | 11 | test |
| 23 | Long-running containers have an owner and a cap | build | 11 | `host.container_has_owner_and_cap` |
| 24 | Known secret values never reach a transcript | build | 11 | `transcript.no_vault_values` |
| 25 | Machine commits and land logs stop flooding | clean up | 10 | `repo.machine_commit_share`, `land.log_retention` |
| 26 | Usage attribution honours its filter and window | refactor | 7 | test |
| 27 | Off on the board switch means the board is not used | KISS | owner, 10-31 | `board.off_switch_means_unused` |

---

## Lanes stop, and the owner is the watchdog (themes 9 and 10)

The audit's single most expensive harness pattern: lanes sat idle with full
boards, and the owner found out by asking. Idle with todo waiting showed in
36, 23, 17 and 8 of 171 samples (Part 5, quantities). In the 344-minute
overnight window on 10-28, tubescience-parity was idle 295 minutes, mxp-gs12
248, mixpeek-cicd 315. Owner status questions were 86 of 465 rows on
mxp-gs12. Two scheduled wakers (SCHED-813, 106 fires; SCHED-880, 24 turns)
were built to cover it and did not.

### 1. A lane with eligible work is never idle without a recorded reason (build)

- **Challenge:** "there should be no reeason these workers (with a full board)
  dont continue their shit" (MSG-101734). "nothing in the harness acts on an
  unmet goal when a lane has no eligible card" (Part 5, 10-30).
- **Change:** board-drive writes one reason per non-terminal card it did not
  offer to an idle lane (parked: trigger X; slot held by Y; WIP limit; owner
  hold; isolated). An idle lane with an eligible card is offered it. The
  reasons are the lane's idle explanation in the dashboard, so the owner
  can read why.
- **Invariant `dispatch.idle_lane_has_reason`:** for every running,
  non-isolated, non-paused worker idle 30 minutes or longer that has at least
  one non-terminal card, every such card has a dispatch reason recorded inside
  the last 10 minutes. Pass: 0 lanes without reasons. `n_considered` is the
  number of idle lanes with cards.
- **Breaks if:** board-drive stops writing reasons for parked cards.

### 2. Structured `waiting_on` replaces prose triggers (remove + KISS)

- **Challenge:** prose triggers parked 79 of mxp-gs12's 83 backlog cards; a
  fired trigger left cards parked 23 hours (TP-196 to 200); eight cards kept
  re-listing because their trigger named a discarded card (MSG-102126); any
  `source_ref` write re-parks a card for 24 hours (Part 5, dispatch defects).
- **Change:** remove free-text triggers. A park names one structured target:
  a card id, a commit sha reaching main, a time, a schedule id, or a named
  owner question. The resolver checks the target directly. Existing prose
  triggers migrate once: parsed if they parse, otherwise the card returns to
  `todo` with a note.
- **Invariant `board.parked_card_trigger_resolves`:** every parked card has a
  structured target, the target exists, and the target is unmet. A card whose
  target is met (card terminal, sha on main, time past) and is still parked
  10 minutes later fails. Pass: 0.
- **Breaks if:** the resolver stops reading card terminal status.

### 3. A waiting `doing` card gives up its slot (KISS)

- **Challenge:** "doing card holds the only slot while waiting" (Part 5,
  dispatch defects). Row 17 waited 1 h 53 min and row 5 two hours on serial
  pools while their cards held the lane.
- **Change:** a `doing` card that the lane marks as waiting on an external
  event moves to `blocked` with that `waiting_on`, which frees the slot. When
  the event lands, the card returns to the head of `todo`.
- **Invariant `dispatch.doing_card_is_progressing`:** no `doing` card has had
  zero lane activity (tool calls, commits, card notes) for more than 45
  minutes while the lane has another eligible card. Pass: 0.
- **Breaks if:** the waiting state stops releasing the slot.

### 4. Remove the global dispatch cap in favour of per-lane slots (remove)

- **Challenge:** "cap 10 with 49 active" (Part 5, 10-28 04:24), so briefs
  became cards that were never delivered.
- **Change:** remove the fleet-wide dispatch cap. Concurrency is one slot per
  lane, which is what a lane can actually use. Host pressure is handled by the
  existing resource guard, which measures the machine instead of guessing
  from a count.
- **Invariant `dispatch.no_cross_lane_slot_wait`:** no lane with an eligible
  card goes undispatched because of another lane's activity. Pass: 0
  refusals with reason `fleet_cap`.
- **Breaks if:** a global cap is reintroduced.

## Messages and supervision (themes 8 and 10)

### 5. Owner text always reaches the pane (KISS)

- **Challenge:** briefs "turned into board cards and never reached you"
  (MSG-99246), at least three times; a revert directive needed "not queu not
  board" (MSG-100952); adherence swallowed owner messages (AF-979).
- **Change:** owner text to a worker is typed into its pane every time. Board
  capture may also file a card, but never instead of delivery. Partly landed
  10-29 (`48a13282a`, `6ef07793d`) and 10-30 (`bad230bb8`, which resends a
  message the server never answered).
- **Invariant `message.owner_send_reaches_pane`:** every owner message in the
  last 24 hours has `delivered_at` within 60 seconds, or a visible failure the
  owner was shown. Pass: 0 owner messages that became card-only.
- **Breaks if:** a staged branch returns without the pane delivery.

### 6. Priority owner messages land mid-turn (build)

- **Challenge:** "rulings wait for turn end. Local-first took about 40 minutes
  to reach mxp-gs12, zero-dollar about 45" (Part 5).
- **Change:** an owner message marked priority (or containing a stop or
  redirect) is delivered at the next tool boundary rather than at turn end.
  Claude Code already accepts input typed mid-turn; amux has to type it
  instead of holding it in the queue.
- **Invariant `message.priority_send_latency`:** p95 time from a priority
  owner send to the provider recording it (UserPromptSubmit or mid-turn
  queue receipt) is under 2 minutes over the last 7 days. `n_considered` is
  the number of priority sends.
- **Breaks if:** priority sends go back into the turn-boundary queue.

### 7. Identical messages are delivered once (build)

- **Challenge:** 24 identical re-proof queue posts (Part 3, 3.8); WASTE lines
  arriving 2 to 4 times each; "Stop relaying the per-minute LOCK WATCH
  lines" (MSG-97886); restart notices delivered twice. At about 530k context
  per turn, each duplicate is a paid turn.
- **Change:** the send path drops a message whose normalized text matches one
  from the same sender to the same recipient in the last 6 hours, and tells
  the sender it was a duplicate.
- **Invariant `message.duplicate_suppressed`:** in the last 24 hours, 0 pairs
  of delivered messages with the same sender, recipient and normalized text
  inside 6 hours.
- **Breaks if:** the dedupe key drops the recipient.

### 8. Owner voice needs a verified stamp (remove)

- **Challenge:** helper sends with an empty `$AMUX_SESSION` arrived unsigned
  and were adopted as owner input (MSG-94211); a 48-hour soak nobody ordered;
  `owner-rulings-1024.md` was built to cope. Of 144 rows typed `user` with no
  worker identity, 126 were Root (Part 3, caveats).
- **Change:** remove the fallback that renders an unstamped send as the owner.
  The owner label requires the owner's own auth; anything else renders as
  `api (unstamped)` with its origin.
- **Invariant `message.owner_label_is_stamped`:** every message shown as the
  owner in the last 24 hours carries an owner-auth stamp. Pass: 0 unstamped.
- **Breaks if:** the unstamped fallback returns.

### 9. Any delivered message can be answered (KISS)

- **Challenge:** 46% of mixpeek-cicd's rows carry a "no reply path" footer;
  its answers were refused for about a week (Part 1, theme 10); STATUS.md
  grew relay sections; image ids moved through handoff files.
- **Change:** a recipient may always reply to the stamped origin of a message
  it received, for 24 hours, whatever the groups say. Discovery and unsolicited
  sends keep the group rules. This keeps isolation's promise (nobody can find
  you) and removes the one-way trap.
- **Invariant `message.reply_path_exists`:** 0 refused sends where the
  recipient sent the refused sender a message in the previous 24 hours.
- **Breaks if:** the reply allowance is removed from the send gate.

### 10. Background model spend is measured and capped (build)

- **Challenge:** on 10-31 about 10,900 USD of rate-card attribution in 24
  hours, 87.7% background. Board-drive nudges 3,385 USD and peer messages
  3,065 USD against 1,338 USD of typed owner prompts. tubescience-parity took
  761 USD of nudges (Part 1, theme 7).
- **Change:** attribute every turn to its trigger (owner, peer, nudge,
  schedule, goal keeper) and show the split per lane. A lane whose
  non-owner-triggered spend passes a scoped budget (default 50% of the lane's
  24-hour spend) stops accepting nudges and peer asks until the owner raises
  it, and says so on its card.
- **Invariant `usage.background_share`:** fleet background share of the last
  24 hours is published with `measured`. Pass: at or under the scoped target
  (start at 50%; the audit measured 87.7%).
- **Breaks if:** nudges stop being attributed to `nudge`.

### 11. A no-op scheduled model turn pauses itself (remove)

- **Challenge:** SCHED-813 fired 106 times "and the workers still stalled";
  SCHED-880 ran 24 turns against "7 cycles at most"; the idle waker SCHED-868
  was disabled, re-enabled and patched three cycles running (Part 5).
- **Change:** with item 1 in place, remove the waker schedules. For any
  remaining tmux-kind schedule, 5 consecutive fires that change nothing
  (no commit, no card status change, no message with an ask) pause it and
  tell the owner once.
- **Invariant `schedule.model_fire_changes_state`:** no enabled tmux
  schedule has more than 5 consecutive no-change fires. Pass: 0.
- **Breaks if:** the no-change detector counts the schedule's own card note
  as a change.

## Claims against evidence (theme 4)

### 12. `verified` follows a live check (build)

- **Challenge:** 12 of 19 rows-lane cards in `verified` were not VERIFIED by
  the checker (Part 4, 3.10); MC-2272 reads verified at 80 standing reds;
  only 1 of 240 mxp-gs12 cards is `verified`, and it is a pricing note.
- **Change:** a card moves to `verified` only with a named check (an
  invariant id, a CI job, or a scheduled check) whose latest result passes.
  When that check fails, the card returns to `done` with the failing result
  attached.
- **Invariant `board.verified_has_passing_check`:** every `verified` card
  names a check, and that check's latest result is a pass no older than its
  cadence. Pass: 0.
- **Breaks if:** demotion on a failing check is removed.

### 13. Finished and dropped stay distinct (clean up)

- **Challenge:** "415 `done` cards went to discarded in one sweep, erasing
  the line between finished and dropped" (Part 1, theme 10). Discard shares of
  62%, 75% and 45%.
- **Change:** `done` and `verified` are terminal for sweeps. A move from
  `done` to `discarded` takes a per-card reason, and a bulk move of more than
  10 is refused.
- **Invariant `board.done_not_bulk_discarded`:** 0 `done` to `discarded`
  transitions without a reason in the last 7 days, and no single actor moves
  more than 10 in an hour.
- **Breaks if:** the sweep path bypasses the reason check.

## Moving targets (theme 1)

### 14. Owner rulings are recorded once and superseded explicitly (build)

- **Challenge:** rulings had to be repeated: scale to zero 7 times, primitives
  4, "do not depend on TubeScience" 4, local first twice 10 days apart. The
  local-first ruling of 10-21 was in memory and still came back as an
  outburst on 10-31 (Part 5). Spend rules changed six times in five days.
- **Change:** built on the memories primitive. When the owner
  states a rule, it is written once as a ruling memory scoped to the group,
  with its date. A later ruling on the same subject supersedes it by link.
  When the owner issues a ruling that reverses a live one, the reply shows the
  earlier ruling and its date, so the owner decides with the history in view.
- **Invariant `ruling.reissue_rate`:** over 7 days, the share of owner
  rulings that restate a live ruling. Published with `n_considered`; the
  target is a falling trend, and every superseded ruling links its successor
  (0 orphans).
- **Breaks if:** ruling memories stop being linked on supersede.

### 15. A goal that cannot move stops re-prompting (KISS)

- **Challenge:** "/goal cleared by error 4 times" (MSG-91541 to 91697); a goal
  that could no longer pass kept re-prompting from 10-21 to 10-29 (MG-2302);
  27 of 34 goal-keeper rows were "not met yet".
- **Change:** after 6 consecutive "not met" checks with no change in the
  goal's measured state, the keeper stops and tells the owner once, with the
  number it is stuck on.
- **Invariant `goal.keeper_stops_on_flat`:** no goal keeper sends a 7th
  consecutive prompt with an unchanged measured state.
- **Breaks if:** the flat counter resets on each prompt.

## Harness faults (themes 10 and 13)

### 16. Every land either reaches main or leaves a receipt (build)

- **Challenge:** a queued shard fix landed by batching after its waiter was
  killed, sat 1 h 45 min and failed rows until reverted; `amux land --detach`
  lost MG-2290's be857a5273f; land rewrites shas, so an ancestry pin nearly
  held a repair (MSG-94991). Seen again 10-30 in this session: two lands from
  one lane reported the same result.
- **Change:** each land request gets an id. Its outcome is one of: landed
  (with old sha to new sha map), refused (with reason), or dropped (with
  reason). A detached land lands the commit it was given, never the clone's
  HEAD at its turn.
- **Invariant `land.request_has_outcome`:** every land request older than 30
  minutes has an outcome, and every `landed` outcome's patch id is on
  `origin/main`. Pass: 0 requests without outcome, 0 landed without patch id.
- **Breaks if:** the detached path reads HEAD instead of the requested sha.

### 17. Schedules announce expiry; critical ones skip the plan reserve (build)

- **Challenge:** schedules expired on their own `until` unnoticed; the owner
  found end dates himself. A rollback ran 63 minutes late because the plan
  reserve refused time-critical schedules (Part 3, 10-19).
- **Change:** a schedule one fire from its `until` or `max_runs` sends its
  lane one notice. A schedule marked `critical` is never refused by the plan
  reserve; it is logged instead.
- **Invariants:** `schedule.expiry_announced`: every schedule that expired in
  the last 7 days sent its notice. `schedule.critical_not_refused`: 0 refusals
  of critical schedules.
- **Breaks if:** the reserve check runs before the critical flag.

### 18. Path-triggered schedules replace re-proof request posts (build)

- **Challenge:** GS-12 rows stale when a commit touches their paths; helper
  posted the same re-proof request 24 times; the lane that stales a row is
  usually not the lane that pays to re-prove it (Part 1, theme 2).
- **Change:** a schedule kind `on_change` fires when a commit reaching
  `origin/main` touches its listed paths, with the commit range as input. It
  is one more scheduler trigger. Mixpeek's re-proof job moves
  onto it, and the request posts stop.
- **Invariant `schedule.path_trigger_latency`:** p95 from a matching commit
  reaching main to the schedule's run starting is under 10 minutes, with
  `n_considered` the matching commits.
- **Breaks if:** the watcher reads the local checkout instead of
  `origin/main`.

### 19. A restarted worker gets its open work back (build)

- **Challenge:** "Restarted with no memory of the prior turn" (Part 5);
  29 compactions on tubescience-parity in 14 days; handoff state in
  `~/.amux/tmp`; the 10-30 tmux crash lost a wake that was delivered by hand.
- **Change:** on start or restart, amux delivers one context message: the
  lane's `doing` card, its open owner requests (`amux requests --open`), and
  undelivered queued messages. It is built from the board and the message
  store, so it needs no new state.
- **Invariant `worker.restart_restores_context`:** every worker restart in
  the last 7 days was followed by that message inside 2 minutes. Pass: 0
  restarts without it.
- **Breaks if:** the restart path skips the context send for queued
  messages.

### 20. Every worker knows who it is (clean up)

- **Challenge:** `$AMUX_SESSION` empty after restarts (Part 5); unsigned sends
  followed (item 8). This session saw the same thing: the staged guard printed
  "`$AMUX_SESSION` was empty; using 'amux-admin' from the tmux session name
  (MR-43)" on every commit.
- **Change:** set `AMUX_SESSION` from the tmux session at every spawn and
  respawn path, including shell restarts inside the pane.
- **Invariant `worker.identity_env_present`:** every non-isolated running
  worker's agent process has `AMUX_SESSION` equal to its name. Pass: 0
  mismatches. The MR-43 fallback stays and logs a counter that must stay 0.
- **Breaks if:** a respawn path drops the env.

### 21. Fleet recovers when tmux is replaced (build, done)

- **Challenge:** tmux crashed 10-30 00:08 and stopped about 30 workers; it
  crashed again 10-29 20:10 (tmux 3.6a segfault in control mode).
- **Change:** landed. Control mode is off by default (`0edcdd049`) and the
  watchdog restarts the fleet when the tmux server pid changes
  (`a6ab9dc49`).
- **Invariant `session.tmux_replacement_recovered`:** after a tmux server pid
  change, every worker that was running is running again within 5 minutes.
  The watchdog's own test covers the trigger; this checks the outcome.
- **Breaks if:** the watchdog's pid file is not written.

## Shared machine (theme 11)

### 22. Destructive pattern commands are refused for subagents (build)

- **Challenge:** subagent `pkill` killed the shared Docker VM (10-21);
  subagent `rm -rf` deleted a live chain folder (10-23); a cleanup script
  deleted 74 tracked files (10-20). The same class killed 184 processes on
  10-19 (memory: never kill by pattern).
- **Change:** the boundary guard refuses `pkill`, `killall` and `kill` by
  pattern, and `rm -rf` outside the worker's own scratch or worktree, for
  every worker and subagent, with the safe form in the refusal.
- **Invariant:** test in the guard's suite: each of those commands is refused
  and each safe form passes.
- **Breaks if:** the guard matches only the first command in a chain.

### 23. Long-running containers have an owner and a cap (build)

- **Challenge:** `ts-dropin-mx` ran 394% CPU for 5 hours on 10-20; on 10-30 it
  ran 350% CPU at 20.3 of 21 GiB for 10 hours while the server's writes
  queued for seconds, which caused dashboard stalls and dropped sends (seen in
  this session). The Mac ran about 50 workers at 99% memory and was reset
  twice on 10-23.
- **Change:** containers started by a worker carry an owner label and a CPU
  and memory limit from the worker's scope. amux lists them on the owner's
  worker card.
- **Invariant `host.container_has_owner_and_cap`:** every running container
  older than 30 minutes has an owner label and limits. Pass: 0 unowned or
  uncapped.
- **Breaks if:** the label is set only on create and lost on restart.

### 24. Known secret values never reach a transcript (build)

- **Challenge:** a customer Redis password printed into subagent output
  (10-10); a pasted Postgres password in a meta-worker transcript (10-29).
- **Change:** a scan of new transcript lines against the vault's values
  (hashes only) redacts matches and records the event without the value.
- **Invariant `transcript.no_vault_values`:** 0 matches in the last 24 hours
  of transcripts after redaction; `n_considered` is the number of lines
  scanned.
- **Breaks if:** the scan reads only the main transcript and skips subagent
  transcripts.

## Clean up and refactor

### 25. Machine commits and land logs stop flooding (clean up)

- **Challenge:** 24% of mxp-gs12's 4,606 commits are ledger commits, 652 with
  one subject; 70 commits are `amux land: regenerate`; 5,355 land log files.
- **Change:** generated files regenerate inside the commit that needs them,
  never as a separate commit. Ledger appends batch into one commit per lane
  per hour. Land logs older than 14 days are compressed into one archive per
  week.
- **Invariants:** `repo.machine_commit_share`: share of commits on
  `origin/main` in 7 days whose subject matches a generated pattern, at or
  under 5%. `land.log_retention`: 0 land log files older than 14 days
  outside an archive.
- **Breaks if:** the land path commits regenerated files separately.

### 26. Usage attribution honours its filter and window (refactor)

- **Challenge:** "amux usage attribution covers 24 hours at most and ignores
  its per-session filter" (audit, limits on the evidence), so the audit could
  not attribute spend per lane past one day.
- **Change:** the usage query applies its session filter and accepts any
  window the stored data covers.
- **Invariant:** test. A query for one session returns only that session's
  rows, and a 7-day window returns 7 days.
- **Breaks if:** the filter is applied after the row cap.

---

## The board switch (added 2026-10-31)

### 27. Off on the board switch means the board is not used (KISS)

- **Challenge:** the owner, 2026-10-31: "make sure enable/disable board also
  makes it so board is not used if its disabled. this is critical". The
  worker page's switch reads Off whenever steering is off, but the server
  called the board unused only when every board key was off. Measured the same
  day: 7 workers with steering explicitly off still had a legacy
  `CC_AUTO_CONTINUE=1` or `CC_STANDING_ORDERS=1` the switch never wrote
  (amux-gtm, gainz, mixpeek-ops-server, mixpeek-override, mixpeek-research,
  mxp-marketplace-retrievers, social-activities), and 11 running workers with
  no board setting showed Off while the board kept their cards (gtm-engine 73
  open, backend 17).
- **Change:** an explicit Off at any scope makes `board_unused` true whatever
  the other keys say; the switch also writes the legacy `CC_AUTO_CONTINUE`;
  the session API publishes `board_used` from the gate every board path
  enforces, and the switch reads it, so a worker whose board is in use reads On.
- **Invariant `board.off_switch_means_unused`:** for every worker with
  steering explicitly off, 0 cards created for it and 0 board-labelled
  deliveries to it in 24 hours.
- **Breaks if:** `board_unused_in` stops checking steering first.

## Order

1, 2, 3 and 4 remove the reason the wakers exist, so 11 can follow. 5, 7, 8
and 10 cut the supervision cost the audit measured at 87.7% of spend. 12 and
13 make the board say what shipped again. 16 to 20 close the harness faults.
21 is done. 22 to 24 protect the shared Mac. 14 and 15 make moving targets
visible to the owner.

Each change lands with its invariant in the same commit, and the invariant
must be red on main before the change and green after it, or the commit says
why it could not be.
