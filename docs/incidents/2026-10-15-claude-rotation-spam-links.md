# October 15 to 16: Claude account rotation stalled on sign-in links filed as spam (two outages)

`scripts/claude-rotate.py` switches the machine's Claude login to another subscription before the current one runs out. A switch signs in through the browser with a one-time link that Claude emails to the target account; every account forwards to ethan@mixpeek.com, and the script reads the link through `/api/email/search` and `/api/email/message`.

## Impact

- **Oct 15, about 21:40 to 23:03 EDT (about 80 minutes): the fleet was rate limited.** ethan@getmixpeek.com reached 100% of its 5-hour and weekly limits at about 21:40. The switch to ethan@runmixpeek.com failed 40 times in a row and only landed at 23:03.
- **Oct 16, about 12:17 to 12:25 EDT: the fleet ran out again** until the owner switched to mixpeekvectors by hand. Details under "Second outage".
- **Oct 16, 01:09 to 10:03 EDT: 75 failed switches to ethan@mixpeekvectors.com.** No outage, because the current accounts still had room and later switches to other accounts worked (03:04 and 08:04). Each failed attempt still held the switch lock for up to about 4 minutes.

## Timeline (EDT)

| Time | Event |
|---|---|
| Oct 15 20:50 | getmixpeek at 93% weekly; switching to runmixpeek starts failing (`steps` end at `continue-email`, about 258 s each) |
| 21:27 | First `login page did not advance after 3 email submissions`; repeated sign-in requests start failing faster (about 26 s) |
| about 21:40 | getmixpeek at 100%; workers hit the limit prompt |
| 23:03 | Switch to runmixpeek succeeds (153 s) |
| Oct 16 01:09 | Switching to mixpeekvectors starts failing the same way |
| 10:01 | `2007693d9` searches `in:anywhere` and skips an account after 3 failed switches |
| 10:04 | Rotation skips mixpeekvectors and switches to joinmixpeek (82 s) |
| 10:38 | `bc682d768`: `/api/email/message` searches `in:anywhere` (root fix) |
| 10:53 | `3067a22da` pushed; its `rust` run fails on a flaky e2e shard |
| 11:13 | Deploy gate saves `red` for 3067a22da |
| 11:36 | 3067a22da's `rust` re-run passes; the gate keeps using its saved `red` |
| 11:45 | joinmixpeek (5h 86%) preempts to ethan@mixpeek.com (5h 81%) |
| 12:05 | ethan@mixpeek.com (5h 85%) preempts back to joinmixpeek; both 5h windows reset at 13:00 |
| 12:17 | joinmixpeek at 98%; the only target left is mixpeekvectors; `link_unreadable`, then the switch waits 259 s and fails |
| about 12:22 | Owner switches to mixpeekvectors by hand |
| 12:27 | Stale `red` cleared by hand; gate deploys 3067a22da with the root fix; the spam link reads in 3.1 s |

## Root cause

**Gmail filed the forwarded sign-in emails as spam, and the amux email API could not open a message in spam.**

1. The emails arrived on time. Gmail metadata, searched by recipient over 3 days: runmixpeek had 43 sign-in emails, 39 of them in Spam, including every one sent between 20:50 and 23:00. The 23:02 email landed in the Inbox, and that is the switch that succeeded at 23:03. mixpeekvectors had 100, all of them in Spam.
2. The script searched without `in:anywhere`, and Gmail search leaves out Spam and Trash by default, so it never saw them.
3. `2007693d9` added `in:anywhere` to the search. That made the emails visible, and `/api/email/message/<id>` still answered `message not found` for each one. `find_message_by_rfc822` in `crates/amux-server/src/integrations/email.rs` looked the id up with the query `rfc822msgid:<id>`, which has the same Spam and Trash exclusion. After the fix, the lookup for mixpeekvectors checked all 20 results at about 6 s each, found no link, and took 122 s.

**Why the fix did not reach the 12:17 outage.** The deploy gate only activates commits whose `rust` and `checks` workflows both passed. It saved `red` for 3067a22da after the first attempt and never asked again, so the passing re-run at 11:36 was ignored and the server stayed on 19e901238 without the fix.

**Second outage (12:17).** The early-switch rule moved between two accounts whose 5-hour windows were both above 80% and both reset at 13:00. joinmixpeek ran out at 12:17, leaving mixpeekvectors as the only account with room, and its link could not be read until the fix deployed. The script also waited the full page timeout after it had already logged that the link was unreadable.

**Why one missing email became an 80-minute outage.** Each tick retried the same target, because it was still ranked best. Repeated sign-in requests then made Claude's login page stop advancing, so even the attempts that might have gotten a link failed early. Nothing skipped the failing account and nothing raised an alarm.

## Fixes

| Commit | Change |
|---|---|
| `2007693d9` | Search `in:anywhere`. Skip an account after 3 failed switches within the backoff window. |
| `19e901238` | Never pick a switch target that is already over the 85% preempt line. |
| `68e24f975` | Switch before the limit; resume workers stopped at the limit prompt. |
| `bc682d768` | **Root fix.** `/api/email/message` looks ids up with `in:anywhere`, so a message in Spam or Trash opens. Test `rfc822_lookup_reaches_spam_and_trash` fails when `in:anywhere` is removed. |
| `162e78e03` | Deploy gate re-asks CI for a saved `red` every 10 minutes, so a passing re-run goes live. |
| `55eb0daa2` | A preempt or expiring-capacity switch only goes to an account with 5h under 65% and weekly under 85%. An unreadable link fails the switch at once and backs the account off. When the current account is spent and its target fails, the same tick tries the next account. |
| `d11d38571` | Search by recipient (`to:`), so piled-up spam for one account cannot push another account's fresh link out of the newest 20. Log `claude_rotate_link_unreadable` when search finds a link email that cannot be opened. Repair two policy tests broken by `19e901238`. |

## Detection

Every attempt logs a `claude_rotate_switch_failed` line with its `steps` to `~/.amux/logs/claude-rotate.log`. The new `claude_rotate_link_unreadable` line names this exact cause. Nothing alerts on repeated failures while the current account is at its limit. With the skip-after-3 rule, a missing link now costs one account, and the rotation moves on to the next one.

## Open

- Gmail still files these emails as spam. A Gmail filter on ethan@mixpeek.com (subject "Your secure link to Claude", never send to spam) would remove the cause entirely. That changes the owner's mailbox settings, so it is left to the owner.

## Verification (Oct 16, live)

- `/api/email/message` opens a spam-filed sign-in email that answered `message not found` before the deploy, and the script reads the mixpeekvectors link in 3.1 s (it gave up after 122 s before).
- 12:40: the scheduled tick switched runmixpeek (spent) to mixpeekvectors in 76 s, through a link Gmail had filed as spam. This is the path that failed 115 times overnight.
- 13:00: with both 5-hour windows reset, the tick preempted mixpeekvectors (weekly about 95%) to joinmixpeek (5h 4%) in 85 s. The new margin rule allowed it because joinmixpeek was cool.
- Tests: rotation suite 21 passed; email tests 117 passed; launcher test 25 passed. Each new rule was mutated on its own and a named test failed.
