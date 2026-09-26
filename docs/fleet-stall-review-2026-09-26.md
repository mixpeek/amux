# Fleet stall review, 2026-09-26

Why each running worker has or has not driven its task to completion, and the amux harness change that would have prevented each stall.

Method: for the 24 running workers with owner messages in the history window (Sep 24 to Sep 26 15:05 EDT, capped at 500 rows), read the owner's last 15 messages and compared them with the worker's pane history, most recent first, plus its session status. Read-only. Tracked as AMUX-5233.

## Per worker

| Worker | State | Why not complete |
|---|---|---|
| celery-retirement | In progress | Real work left (lifecycle matrix 7/17 failing, load round 2, prod roll). Lost ~80 min to the weekly limit, 15:11Z to 16:32Z. |
| gs-4-gke-minimization | In progress | Grant #1 (actAs on node SA) still unrun by Ethan. Said "last lever I can pull alone", opened AskUserQuestion that sat ~19 min, then found more levers it could pull alone. |
| mvs-infra | Idle, watchers armed | Incident work was legitimate. Deferred auto-fix canary with "unless you want it sooner" despite authority. Lost ~76 min to the weekly limit. SCHED-494 preflight output unreadable. |
| mixpeek-frustrations | In progress | Holds ~145 touchpoint cards for Ethan when mixpeek-general already gave the approval the goal required; only 4 needed Ethan. Lost ~81 min to the weekly limit. |
| gs-3-bucket-objects | In progress | Waiting on mvs-infra clearance, watched by regex over mvs-infra's pane (fired a false positive). Promotions failed canary under load; retry scheduled. |
| gs-10-zero-base-cicd | In progress, partly blocked | Two asks for Ethan live only in its STATUS.md as BLOCKED-ASK lines (roll MVS prod primary without warm standby; enable paid Staging E2E). None became needsyou cards. |
| tubescience-parity | In progress | Bandwidth-bound vector backfill (1,200/3,310). Needs a semantic-search sign-in from Ethan, stated only in prose. TP-35/TP-36 stale in todo. |
| mac-ops | Done | Said a Monitor would notify it but none was armed; next turn died on "API Error: The response stopped arriving" and sat idle until Ethan's continue at 13:14. |
| gtm-engine | Done, waiting on Ethan | 192 emails rewritten and loaded. Needs "go" on Programmatic I/O and the $749 AW pass. Cost one round trip asking "say go" for an in-boundary rewrite. |
| launch-videos | Done, waiting on Ethan | Publish approvals and a music sync licence are Ethan's. LV-109 epic reads Closed but status is backlog; 7+ LV needsyou cards, oldest 08-15. |
| mixpeek-ops-server | Mostly done | Category catalog exists (MOS-219) but runtime enable/disable 503s in prod (MOS-20) and it did not pick that up. Ethan asked the same thing twice. |
| random | Done | Nothing outstanding. |
| mixpeek-cicd | Answered, fixes parked | Ends turns with "awaiting your word" on MC-2092/2094/2096, all in-boundary code fixes. |
| mixpeek-general | Audit done, follow-ups stuck | Owning lanes (mixpeek-observability, backend) are paused, sends refused, so it left their items instead of owning them. Board writes returned 000 five times. |
| mixpeek-finances | Done | Correctly refused to pay (money). Left 2025 amounts "unknown" though the source was already found; offered to draft an email instead of queuing it. |
| gtm-videos | Idle after /clear | Nothing pending. Remote Control "account changed" banner on all panes. |
| desktop | Done | First scoring round failed because amux silently refuses automated sends into an isolated lane. |
| amux-frustrations | Done, waiting on Ethan | Outward PR replies drafted for Ethan. The instruction that started the work was typed in the pane and never reached history. |
| amux-helper | Done, unverified in UI | Everything pushed; UI checks deferred until the build deploys, and nothing wakes it when it does. |
| gtm-ticker | Waiting on Ethan | Email approvals expire after an hour and get re-requested every fire; four have expired twice. A push-retry loop swallowed a guard's error text for hours. |
| mixpeek-homepage-claude | Done | Latest asks live and verified on prod. 11 auto-captured intake cards left in todo for shipped work. |
| mvs-research | Partly blocked, partly stalled | MVS build/deploy workflows off since 9/21 and MP-106 needs sign-off (Ethan). MR-288: said "Next I'll build the streaming loader" and never did; card sits in doing. |
| social-activities | Blocked on Ethan | Gmail reauth needs the OAuth app published to Production. The diagnosis was appended to an archived card for another account, so nobody owns it. |
| primis | Done, waiting on Ethan | Draft to an external contact delivered. Six capture cards open; a send-pipeline test probe was injected into this customer pane. |

## Harness fixes, grouped by root cause

Ordered by how many workers each would have helped.

1. **Owner asks stuck in prose (9 workers: gs-4, gs-10, tubescience-parity, mvs-infra, mixpeek-cicd, gtm-engine, mixpeek-finances, social-activities, mixpeek-frustrations).** Add a turn-end classifier on the Stop hook. When the final paragraph asks the owner something ("say go", "awaiting your word", "unless you want it sooner", "needs one thing from you", a BLOCKED-ASK line), check it against the standing-authority boundary. In-boundary: steer back "proceed, standing authority covers this". Boundary (money, external send, prod data): auto-create a needsyou card with the question and the unblock line. While a /goal is active, turn AskUserQuestion into a needsyou card and keep the worker going.
2. **No automatic resume after a limit or API error (6 workers: celery-retirement, mvs-infra, mixpeek-frustrations, gs-3, gs-10, mac-ops).** When a limit's reset passes or the signed-in account changes, send "continue" once to every lane paused on it. When a pane ends on "API Error: The response stopped arriving" and stays idle 2 minutes with no background task, send "continue" once and log it.
3. **Promised next step never taken (2 workers: mvs-research, mac-ops).** When a turn ends with "Next I'll..." or "I'll let the Monitor notify me" and the lane is idle 2 minutes with a doing card and no live background task, re-prompt once with the card id.
4. **Cross-lane coordination by scraping and refusal (3 workers: gs-3, mixpeek-frustrations, mixpeek-general).** Add a named gate on cards that a peer can clear with one PATCH, waking every subscribed lane; clear a peer-approval gate automatically when the peer's answer lands. A send to a paused lane should queue for resume and tell the sender it owns the work meanwhile.
5. **Capture cards never reconciled (4 workers: mixpeek-homepage-claude, primis, tubescience-parity, launch-videos).** Close an intake card when a landed commit or turn-end cites its MSG id. Surface capture cards still open after 24 hours as unreconciled. Give an epic whose children are all done a distinct awaiting-publish state.
6. **No wake-up on deploy (amux-helper).** When the builder deploys a sha, message the lane that pushed it: "<sha> is live, run your UI check".
7. **Approvals that expire faster than the owner reads them (gtm-ticker).** Keep an email approval pending until acted on; send one daily digest instead of re-requesting on every fire.
8. **Observability gaps (mvs-infra, amux-frustrations, desktop, primis, mixpeek-ops-server).** Store stdout tail and exit code for shell schedules without .bash_profile noise. Record prompts typed directly into a pane as history rows. Return a distinct 403 isolated_target for automated sends to isolated lanes, and warn at schedule creation. Keep delivery probes out of real worker panes. Flag a near-duplicate owner message as a repeat and attach the earlier result.

## Waiting on Ethan

- gs-10: may it roll the MVS prod primary without a warm standby; enable paid Staging E2E.
- gs-4: run grant #1 (actAs on the node service account).
- tubescience-parity: semantic-search sign-in.
- social-activities: publish the Google OAuth app to Production.
- gtm-ticker: four expired email approvals (Rami, Abraham, John, AssemblyAI).
- gtm-engine: "go" on Programmatic I/O; buy the $749 Advertising Week pass.
- launch-videos: publish approvals and the music licence.
- mvs-research: re-enable MVS build/deploy workflows; sign off MP-106.
- mixpeek-ops-server: RB2B and Ghost webhook URLs; rotate OPS_SLACK_WEBHOOK_URL.
- mixpeek-frustrations: MF-3230, MF-3554, MF-1737, MF-1963.
- mixpeek-finances: pay the NY warrants.
- primis: send the Garik reply.
