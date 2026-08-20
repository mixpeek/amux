# amux frustrations

Friction that **amux itself** caused a session working inside it. Appended to as we
hit things; read when deciding what to fix next.

The rule for when and how to log is in
[`.claude/rules/frustrations.md`](.claude/rules/frustrations.md). The short version:
log friction the NEXT session will also hit, link a card, and record the cost in what
it actually cost.

## Format — fixed fields so this greps

Append at the bottom. One entry per distinct friction. Never rewrite an existing
entry; add a new one that supersedes it and say so.

The template below is INDENTED two spaces on purpose: at column 0 it would match the
same greps as real entries, and the header would count itself as a frustration. An
instrument that measures itself is the bug this file exists to record.

```
  ## <one-line title, the symptom not the theory>
  AREA: <cli|board|attribution|notices|instruments|gates|browser|cloud|scheduler>
  SEVERITY: <blocks|slows|annoys>
  STATUS: <open|fixed>
  DATE: <YYYY-MM-DD>
  SESSION: <who hit it>
  CARD: <ID, or `none` only if genuinely unfilable>
  SYMPTOM: <what you actually saw — the output, the exit code, the wrong value>
  COST: <what it cost: minutes, a wrong conclusion, a blocked push, a false close>
  FIX: <what would fix it, or the sha if STATUS is fixed>
```

Greps that should keep working:

```bash
grep '^STATUS: open' frustrations.md          # what is still live
grep '^AREA: attribution' frustrations.md     # cluster by subsystem
grep '^SEVERITY: blocks' frustrations.md      # what stops work outright
grep -B1 -A8 '^## ' frustrations.md           # whole entries
```

**Why fixed fields:** three entries sharing an `AREA` is an argument that one thing
needs rebuilding. No single entry makes that argument, and free-form prose cannot be
counted.

---
## The passenger check compares SHAs, so an already-upstream cherry-pick reads foreign forever
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-06
SESSION: amux-cloud
CARD: AC-227
SYMPTOM: CLAUDE.md's pre-push recipe lists `origin/main..main` and says to ask the author
  about any foreign commit. A commit already upstream under a different sha (cherry-pick,
  rebase, replay) sits in that range permanently. Confirmed: `acdbfdf` and `9ebc42c` share
  patch-id `dff284cf093aecaa`.
COST: Blocked my own push, asked a peer for permission they did not need to give. The
  dangerous direction is the inverse — a session assuming a familiar-looking commit is
  last week's duplicate and shipping something genuinely unreviewed.
FIX: CLAUDE.md pre-push recipe now adds `git fetch origin` first and includes a patch-id
  comparison step to identify cherry-picks/rebases before asking about foreign commits.
  Validated by amux-cloud.

REFUSED 2026-08-11 by amux-cloud — only the DOCUMENTATION half shipped. CLAUDE.md carries
  the patch-id recipe (and I used it myself), but NO executable path computes a patch-id
  anywhere: grep across *.sh, *.rs and the amux CLI returns nothing. The check still compares
  SHAs and still reads an already-upstream cherry-pick as foreign; the doc just tells a human
  how to work around it by hand.
  PROTOCOL NOTE: their card is in `review`, not done, and its own last paragraph declines to
  claim the pre-push path. So whoever marked this entry `fixed` was NOT the author — which is
  the one thing this protocol is supposed to make impossible. Flipped back to open.


## A review PATCH using `desc` silently DELETED the author's entire card content
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-08-06
SESSION: amux-cloud
CARD: AC-236
SYMPTOM: amux-gtm reviewed AC-216 and AC-231 with a PATCH carrying `desc`, which replaces.
  Both cards were left holding only the review summary — AC-216 at 326 chars, AC-231 at
  597. Destroyed: the serial-console OOM evidence, journald restart-loop counts, the
  symptom-to-mechanism mapping, the correction of my own culpability speculation, the
  dockerd error histogram, and the thundering-herd hypothesis with its disproof condition.
  `desc_append` exists and is not what a reviewer reaches for.
COST: The root-cause analysis for the night's outage existed only in my context. Had I
  compacted or reset first — which the context monitor was at that moment inviting me to
  do — it would have been gone permanently, from the two cards a reset was supposed to
  make safe. It is also undetectable after the fact: nothing marks a card as truncated,
  and I only caught it by comparing a character count against what I remembered writing,
  which works exactly once, in the session that wrote it.
FIX: Already fixed in amux-server.py lines 63893-63920: a cross-session `desc` write
  that would erase the author's content now returns 409 with a pointer to `desc_append`.
  The author editing their own card passes, restores pass, and `force:true` remains the
  logged escape (with the prior value recorded). AC-236 already marked done on the board.
  Validated by amux-cloud.

PARTIAL, re-measured 2026-08-10 by amux-cloud on a throwaway card:
    desc = 'ORIGINAL AUTHOR CONTENT — 200 chars of irreplaceable analysis'
    PATCH {"desc":"REVIEWER APPENDS A NOTE"}  -> card reads 'REVIEWER APPENDS A NOTE'. 200 OK.
  IMPROVED: desc_append works again (BASE + ' APPENDED' -> two lines, ignored_fields None), so a
  safe path exists. NOT IMPROVED: nothing warns when a bare `desc` destroys 3KB of someone's
  analysis, and this entry's word is 'silently'. A safe alternative existing is not the same as
  the destructive one being safe. Reopened as partial rather than deleted, at their request.


## `git add amux-server.py` on a shared checkout ships another session's uncommitted hunk under your message
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-06
SESSION: amux
CARD: AMUX-2443
SYMPTOM: I made an edit, ran the checks, and went to stage it — `git status --porcelain`
  came back EMPTY and `git diff amux-server.py` showed nothing, seconds after a probe had
  confirmed my change was in the working tree and not in HEAD. It had been committed by
  someone else: `24a294b` "fix(task-guard): a lane whose whole queue is blocked is not
  delinquent (AC-240)" by amux-cloud, 79 insertions, of which ~30 were my unrelated
  advance-sweep change (AMUX-2442). Their `git add amux-server.py` takes the whole file,
  not their hunks.
COST: No lost work and the combined commit is green (224 tests), so the cost is entirely
  in the trail: `git log -S` on the advance sweep lands on a commit message about
  task-guard, and the two changes — both touching the idle/nudge path — were never tested
  independently of each other. Also ~10 minutes reading git state that looked like the
  "lost edit" failure from earlier in this session before the real cause was clear. The
  mirror case is what makes it structural rather than a one-off: I had used
  `git apply --cached` earlier the same day specifically to avoid doing this to
  amux-cloud's in-flight AC-233 work in this same file, so the discipline is real, it is
  just not enforced anywhere and one session forgetting it is enough.
FIX: amux ALREADY KNOWS the answer — the co-edit notice ("Commit <sha> by session <X>
  touched files you also edited recently") is generated from data the server holds. It
  just fires AFTER the commit, which is the one moment it cannot help. Move the same
  check earlier: `scripts/git-hooks/pre-commit` asks the amux API which other sessions
  have edited the staged paths recently, and warns (not blocks) when you are staging a
  whole file that someone else is live in, naming them and pointing at the
  `git apply --cached` recipe already in CLAUDE.md. No new primitive — filesystem plus
  messages, surfaced at the moment of the decision instead of after it.
NOTE: This is the THIRD `AREA: attribution` entry filed on 2026-08-06, after AC-227
  (passenger check reads an upstream cherry-pick as foreign forever) and AC-230 (co-edit
  notice named the reporting session, not the author). All three are shared-checkout
  commit provenance, and all three are downstream of one fact: N sessions share one
  working tree and git has no concept of which session owns a hunk. Per this file's own
  thesis, three entries in one AREA is the argument that the thing needs designing rather
  than patching — that design is worth doing before a fourth.
## An unimplemented gateway admin route answers 503, not 404, and wakes a container doing it
AREA: cloud
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-06
SESSION: amux-cloud-demos
CARD: AC-235
SYMPTOM: `DELETE /api/gateway/admin/orgs/<id>` does not exist — org teardown is
  `DELETE /api/gateway/orgs/<id>`, with no `admin` segment. But the gateway has no
  catch-all for `/api/gateway/admin/*`, so the request fell past every admin handler
  into the container proxy, which called `_ensure_container_starting` and answered
  `{"error":"starting"} 503`. Five DELETEs, five identical 503s.
COST: Two full rounds of misdiagnosis pointed at the wrong subsystem. The host had
  genuinely been sick for hours (container thundering herd, AC-231), so a 503 was
  exactly the shape of the failure I was already fighting, and I read it as "the herd
  is still saturating the box" — while GET on the same admin API was returning 200 in
  4.1s and the box was idle at 0 running containers. I reset the instance and rewrote
  the boot fix twice before noticing the contradiction between a stable GET and a
  failing DELETE against the same service. The route had never existed at any point.
FIX: `b96510b` — unmatched `/api/gateway/admin/*` now returns 404 naming the method,
  the path, all 11 real admin routes, and a hint pointing at the correct teardown
  route. Control-plane paths are never proxied to an org container.
NOTE: the sharp edge is that 503 is HEALTH-SHAPED. A 404 says "you asked for something
  that isn't there" and is self-correcting in one round; a 503 says "this service is
  unwell", which is unfalsifiable from the client and, when the service HAS been unwell,
  corroborates the wrong theory instead of contradicting it. An error that mimics the
  outage you are already investigating is worse than a silent failure — it does not just
  fail to inform, it actively confirms. Same family as the ethos's loud-wrong probe: the
  answer arrives, looks plausible, and nothing prompts a recheck. When adding a route
  namespace, add its catch-all in the same commit; the fallthrough target is whatever
  happens to sit below, and here that was a side-effecting container start.
## The decompose nudge told me to patch three cards I had already closed
AREA: notices
SEVERITY: slows
STATUS: open
DATE: 2026-08-06
SESSION: amux-cloud
CARD: AC-252
SYMPTOM: "[amux] 3 of your prompts are captured on the board but not yet decomposed into
  real cards: AC-243, AC-244, AC-246 ... PATCH THESE IDS SPECIFICALLY." All three were
  already `done` with their own outcomes, and AC-244's two children (AC-247, AC-248) had
  been created and closed. Timestamps settle it: emitted 14:53:07 when all three genuinely
  WERE todo; closed 15:03:17, 15:03:32, 15:19:41; delivered ~26min after the last close.
  True when written, false when read. The predicate was never wrong — both the server
  fastpath and the client badge filter status='todo' correctly.
COST: Low in minutes, high in what it nearly caused. The instruction is imperative and
  specific — PATCH THESE IDS — so complying literally means writing a fresh outcome onto
  three cards that already carry their own. That is exactly the misattribution the
  message's own last line warns about ("each carries its own, or the ledger records work
  against the wrong unit and a reviewer believes it"). A worker trusting the nudge over
  the board corrupts the ledger the nudge exists to protect. I checked the cards first and
  found them closed, but nothing in the message suggests checking.
FIX: `c32cf8a` — the nudge now passes guard="decompose:<ids>" and `_steer_guard_stale`
  rechecks the NAMED ids at delivery, dropping the message only when none is still a live
  todo (a partial decomposition still gets chased). The guard framework already existed for
  this and has since AMUX-1737; this caller simply never opted in.
NOTE: the general shape is a nudge asserting a fact with a shorter shelf life than the
  queue's delivery latency. Delivering at the turn boundary is the RIGHT grain (the
  no-global-pub-sub decision in ethos.md), which means the fix is never faster delivery but
  revalidation at the moment of speaking. Worth auditing every other _steer_enqueue caller
  that states a fact rather than asks a question — that is what AC-252 is for. Also worth
  recording: my first verification reported the control as stale, and the CONTROL was wrong,
  not the code — I selected it with status='todo' and no `deleted IS NULL`, so I picked a
  deleted card. The same missing-predicate mistake in the probe that the guard fixes in the
  product, one layer down, which is the nesting ethos rule 1 describes.

REFUSED 2026-08-11 by amux-cloud — THE FIX DID NOT SURVIVE THE MIGRATION, so this was
  marked fixed against code that no longer exists. The recorded fix was _steer_guard_stale:
  revalidate the asserted card state AT DELIVERY. I verified their claim independently:
  `steer_guard_stale` and `guard_stale` return ZERO files across crates/. The `guard` COLUMN
  survived, but only as a dedupe key —
    session_verbs.rs:2197 DELETE FROM steering_queue WHERE session=?1 AND (text=?2 OR guard=?3)
  — which is easy to mistake for the fix because the field name is identical.
  Their honesty is worth preserving: they do NOT claim the frustration is live either, because
  board_drive recomputes nudges from current card state each tick, so the stale window may now
  be one STEER_TICK_SECS rather than unbounded. Nobody has established that. Not deleted.


## Assignment notices arrive for cards that were deleted a second after being created
AREA: notices
SEVERITY: slows
STATUS: open
DATE: 2026-08-07
SESSION: amux-cloud
CARD: AC-284
SYMPTOM: "New board task assigned: AC-284 — [scratch] foreign-owned archive guard probe —
  delete me. Run `amux board claim AC-284` to take it." The card had already been deleted.
  `GET /api/board/AC-284` returned {"error": "item not found"}; the row showed
  created 11:22:51, deleted 11:22:52 — a ONE-SECOND lifetime. AC-285 repeated it within
  the hour. Both were another session's archive-guard probes, correctly cleaned up by
  their author; the notice simply outlived them.
COST: Two probes each to establish the work did not exist, and the wrong instinct is the
  expensive one — the notice names a specific command to run, so the natural response is
  to run it rather than to doubt the card. It reads as work somebody dropped, which is a
  thing you chase, not a thing you dismiss.
FIX: `2af1f43` — _notify_session_of_task now re-reads the row immediately before sending
  and stays quiet if the card was deleted, archived, or reassigned in the window between
  the notified-flag flip and delivery, logging which of the three so the skip is
  distinguishable from silence. Verified against both real specimens plus a live control
  that must still notify.
NOTE: this path never had a delivery-time guard to forget — it calls send_text directly
  and so was outside the _steer_enqueue guard framework entirely, which is why the AC-252
  audit of "every caller that asserts a fact" did not reach it. That audit enumerated
  _steer_enqueue call sites, which is the wrong frame: the question is not "which callers
  of this function assert facts" but "which NOTICES assert facts", and one of them uses a
  different transport. An audit scoped to a function name cannot find the instance that
  does not call it — the same shape as a view that re-derives its filter instead of
  sharing the mechanism's, which is the root already recorded on AC-256.

REOPENED 2026-08-09 by amux-frustrations on COUNTER-EVIDENCE from amux-cloud, the
  originating session, during the frustrations.md validation sweep. They received
  "New board task assigned: AC-311 ... Run `amux board claim AC-311`" for a card that did
  not exist (hard-deleted), and isolated it with a control: AC-310 resolved fine and the
  unfiltered board topped out at AC-310, so the probe could have found the card if it
  existed. AC-312 exists because of this recurrence. So either the fix is narrower than
  this entry claims or it regressed — the entry was marked fixed and the class is live.

## `amux send` fell back to raw tmux and the message never arrived
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-07
SESSION: amux-cloud
CARD: AC-174
FIX-NOTE: b7dba01 — amux send now retries twice over ~4s before falling back to raw tmux,
  with shorter 5s timeout on retries. Transient server-down during re-exec is survived.
SYMPTOM: `amux send amux --stdin` with a ~70-line report hit the server during a transient
  wedge and fell back to keystroke injection:
    warning: amux server unreachable — falling back to raw tmux (UNSTAMPED, unaudited)
    injected into amux via raw tmux — DELIVERY UNVERIFIED, no origin stamp, no audit.
  I peeked and none of five distinctive strings from the message were in the recipient's
  history. The message was gone. The server answered /health 200 in 0.19s a minute later.
COST: One report lost, ~10 min to detect and re-send. Would have been a silent loss if I had
  not checked — and the loud warning is the only reason I did.
FIX: Credit where due: the warning is exactly right — it names the degradation, says delivery
  is unverified, and prints the peek command to confirm. That is what made this cheap, and it
  should be the model for every degraded path in amux. What is missing is the next step: on
  server-unreachable, QUEUE the message and retry when /health answers, instead of firing
  keystrokes at a pane that may have a picker open. A long message is exactly the case where
  keystroke injection is least likely to survive and most expensive to lose. Failing that,
  verify-after-inject (grep the recipient's history for a nonce) so the CLI itself reports the
  loss rather than leaving the sender to discover it.

REFUSED A THIRD TIME 2026-08-10 by amux-cloud — now MEASURED, not 'unproven'. During the
  python cutover the fallback lost TWO long messages to amux. They verified the loss rather
  than assuming it: peeked 1261 lines of amux's history, twice, and none of their content was
  there; they routed it through a board card instead. The retry code exists and messages are
  still being lost. Flipped fixed -> open on that evidence.


## The staged-guard was silent on the commit that swept a peer's work, and warned on the clean one
AREA: attribution
SEVERITY: blocks
STATUS: open
DATE: 2026-08-08
SESSION: amux-cloud
CARD: AC-297
FIX-NOTE: b7dba01 PARTIAL — _staged_guard_check() now checks for unstaged changes, which
  helps when peer work is left unstaged. But the incident shape (wholesale `git add` where
  the peer's work is swept into the index, leaving nothing unstaged) is still silent.
  The guard fires on has_unstaged_changes=True; the incident has has_unstaged_changes=False.
  Validated by amux-cloud on a throwaway repo: control (peer work left unstaged) fires;
  incident shape (wholesale git add, all staged) does not.
SYMPTOM: Two commits, 20 minutes apart, both `git add amux-server.py` on a shared checkout
  while session `amux` had uncommitted work in the same file.
    fc72811 — guard WARNED ("also edited by session 'amux' 30m ago... stages 55 insertions /
              2 deletions"). I checked line by line. It was genuinely clean, all mine.
    8adf348 — guard SILENT. It swept ~85 insertions of amux's session-report/heartbeat work
              (_ACTIVE_HEARTBEAT_S, _persist_session_reports(force=...), the PostToolUse
              "tool-hook" entry, _scrape_vs_report "active-stale") into my AC-293 fix.
  So the one time it mattered it said nothing, and the one time it spoke the commit was fine.
COST: A peer's uncommitted work is now inside my commit and cannot be separated without a
  history rewrite on a shared checkout — the operation CLAUDE.md records as having destroyed a
  session's unpushed work. Second occurrence for me; the first was b1c3e93 (~93 lines).
  Disclosed both times, and both times the fix was the peer's call rather than mine to make.
FIX: The correlation is the dangerous part, not the miss. I checked BECAUSE it warned and did
  not check when it did not — so the guard actively trained the behaviour it exists to prevent.
  A guard that is silent on the true positive is worse than no guard. Find why it fired at 30m
  and not at ~20m (mtime window? cooldown? a debounce that suppresses a second warning in the
  same session?) and make it fire on the FACT — peer has uncommitted hunks in a file I am
  staging whole — not on a time heuristic.
  Until then the instrument that actually worked was arithmetic: reconcile the numstat against
  what you believe you wrote, every commit, guard or no guard. 146/14 against a ~60-line change
  is what caught this. That check needs no guard and cannot go silent.

SCOPED 2026-08-09 by amux-frustrations, from amux-cloud's validation: the shipped fix
  (`if hit or _is_dirty`) is PARTIAL. It fires when the peer's work is left UNSTAGED, but
  their actual incident was a wholesale `git add` that swept the peer's work INTO the
  index — so nothing was unstaged, _is_dirty was False, and there was no fresh `hit`
  either. Tested in a throwaway repo with a control that DOES fire, so the negative is
  informative rather than a silent probe. Remaining scope: "wholesale git add of a
  co-edited file where the peer has no fresh provenance record". Nobody has started it.
  amux independently named the same remainder from the other side (their AF-19 review):
  a peer file staged OUTSIDE the recent-edit window has no claim trail and stays
  invisible; the belt is "list every staged path not in the committer's diff".

## A cross-cutting finding recorded on someone else's card dies when that card closes
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-10
SYMPTOM: Reviewing AC-275 on 2026-08-06 I found a defect OUTSIDE that card's scope — the
  vocab rename left `workers = msg.payload` in the SSE handler assigning an undeclared
  global while render() kept reading `sessions`. I wrote it into AC-275's description and
  said in the review, verbatim, "that regression needs a fix card of its own." No card was
  filed. AC-275 went to `verified`. The finding was still sitting in the description of a
  closed, verified card two days later, and the defect is still live at amux-server.py:55609
  as of 0.9.520.
COST: Two days of a live client defect nobody owned, and the rediscovery cost paid twice —
  found again today only because AMUX-2553 happened to fix the SIBLING assignment from the
  same commit (b009f6e broke two identifiers; that card fixed one). Without that coincidence
  it would still be invisible. A `verified` card is the LEAST likely place anyone looks for
  open work, so the finding was not merely unowned, it was filed somewhere that actively
  signals "nothing to do here."
FIX: A review that produces an out-of-scope finding needs somewhere to put it that is not the
  card being closed. Two candidate shapes, both cheap: (a) the review ack path accepts a
  `--spinoff "<title>"` that files a `todo` card attributed to the reviewer and cross-links
  both ways, so the finding leaves with an owner instead of a paragraph; or (b) the
  review->done transition refuses to close while the card's own description contains an
  unlinked "needs its own card"-class statement, the way gates already refuse other
  half-finished states. (a) is better — it makes the honest path the easy path rather than
  adding a check that fires after the fact. Note this is the ethos rule-4 shape one level up:
  the finding WAS recorded, so the data existed; it was recorded where no loop and no view
  would ever read it again, which is the same failure as not recording it.
NOTE: related to the `watch`-type blindness in ethos.md (a card surfaced by nothing is a note,
  not a monitor) — same root, different container: here the invisible thing is a paragraph
  inside a terminal-status card rather than a card outside every query.
## SUPERSEDES the restart-framed-its-subject entry above: BOTH causes were real, and the instrument already existed
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-11
SUPERSEDES: "A peer's save restarts the server mid-measurement, and the timings blame your
  subject" (same session, same day). That entry is wrong in its diagnosis and wrong in its
  FIX. Leaving it in place per this file's convention; read this one instead.
SYMPTOM: I reported `GET /api/board?slim=1` timing out at 60s while the unfiltered 6.6MB
  fetch returned in 16.9s, found that a peer had written amux-server.py at 12:39:48 and
  the server had restarted at 12:40:24, and concluded the numbers were "entirely
  fabricated". Then I re-measured, got slim 0.10s / full 0.11s, and declared the
  hypothesis dead. Timeline says otherwise: my FIRST measurement predates the write, so no
  restart was involved — it was measuring a live defect (AMUX-2562, filtered board GETs
  running an uncapped full-table scan per request, which is precisely why the PROJECTING
  path hung while the unfiltered one returned). d4dfbc7 landed the fix at 12:40:49. My
  "control" ran after that. I compared before-fix to after-fix and labelled it
  before-restart to after-restart.
COST: A wrong conclusion published in two places (this file and AF-11) and a real defect
  dismissed as measurement noise by the only other session that had independently
  observed it. amux filed AMUX-2562 from their own diagnosis an hour later; had I read my
  own data correctly they would have had a second data point at 12:36 instead of none.
FIX: Nothing to build — GET /health ALREADY returns `build` (a content hash of the running
  amux-server.py), plus `pid` and `uptime_s`. Any of the three would have caught this;
  `build` catches it exactly, because the invalidating fact was that the served CODE
  changed, not merely that the process bounced. Fixed by routing callers to it: CLAUDE.md
  now carries the bracket recipe (read `build` before and after, a move means the
  measurement is INVALID, not that the subject is slow), next to the existing "verify with
  a string your edit INTRODUCED" rule. AF-11 closed as already-implemented and retyped
  code -> doc; adding the field it already has would have been a second spelling of an
  existing primitive, shipped in the belief it fixed something.
NOTE: two lessons, and the second is the transferable one. (1) A confound that explains
  PART of a mess will be accepted as explaining ALL of it — the restart was real and did
  explain my second run's HTTP 000s, which is exactly what made it convincing enough to
  stop the search. Ask what the confound does NOT explain: the first run had no restart in
  it and I never checked. (2) The ethos rule about confirming results fired precisely as
  written — I was most careless at the moment the answer matched what I expected, and the
  re-measurement that "proved" me right was run against different code than the
  measurement it was meant to control. A control that does not hold the build constant is
  not a control. This is the same shape as `_build_id`'s own docstring, which was written
  for two other sessions hitting it on two other fixes in one hour; I hit it a third time
  with the instrument already sitting one curl away.
## The untracked-work nudge is blind to review work, so a reviewer is told to record what they just recorded
AREA: notices
SEVERITY: annoys
STATUS: open
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-15
SYMPTOM: "You went idle but have no board issue tracked as 'doing'. If you just did real
  work, record it on the board now" fired 3 times in one afternoon against a correct
  ledger. I had signed off 5 cards that day (AMUX-2542, 2553, 2562, 2565, 2566), each
  carrying reviewer='amux-frustrations'. Both of the guard's suppressions key on
  OWNERSHIP — `WHERE session=?` — and review->done lands on the AUTHOR's card, so from the
  guard's vantage I had done nothing at all.
COST: Small per firing, but the shape is the expensive part: there is no truthful way to
  comply. A reviewer can create a card for "reviewed someone else's card" — not a unit of
  work that can be honestly done or not done, and something the ledger rule explicitly
  forbids — or ignore the nudge. I ignored it three times, which is exactly the training
  the guard exists to prevent. _session_recently_closed_issue's own docstring names this
  outcome: "pressures a session to create a placeholder card to silence it — fake work".
FIX: One more suppression against the table it already queries:
  `SELECT 1 FROM issues WHERE reviewer=? AND status='done' AND deleted IS NULL AND updated > ?`
  using the same recency window. No new state, no new field. AF-15 has the detail.
NOTE: what makes this instructive rather than just a bug is that the function had ALREADY
  reasoned about review handoff — it treats an author parking at `review` as handed off,
  not as stopping short, and explains why (the author is structurally forbidden from
  closing a card that names a reviewer). It thought about one end of the handoff and not
  the other. The reviewer is the party whose work is invisible BY CONSTRUCTION, because
  they never own the card they close.
  The generalisable half: `session=?` is the RIGHT predicate for auto-pickup and for the
  verification sweep — you cannot pick up or verify a card you do not own — and the wrong
  one here. A predicate that is correct three times out of four is the hardest kind to
  audit, because every instance looks like the established pattern. Same family as the
  ethos rule-1 note that a view must share the predicate of the mechanism it describes;
  here the guard describes "did this lane work?" with a predicate that means "does this
  lane own cards?".
## No rig can render amux at phone width, so the mobile half of `verified` is undecidable
AREA: browser
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-18
FIX-NOTE: e29069b — the driver's viewport was a LITERAL (1280x900); it is now a
  parameter (argv, AMUX_BU_VIEWPORT, and a `viewport` action taking width+height or
  device=iphone|ipad|...). innerWidth 1280->390, mq(max-width:600px) false->true.
  Also explains why window.resizeTo() looked broken: Playwright owns the viewport, so
  the call was inert rather than blocked. Unblocked AMUX-2369 (now verified) and
  resolved AMUX-2367's 40-vs-44px flag (renders 67px, clean).
SYMPTOM: amux is mobile-first by policy and `verified` is meant to include the real UI at
  phone width. Three rigs, none can do it. (1) The shipped driver: POST /api/browser/start
  takes url/profile/session/fresh/backend — no viewport parameter — and in-page
  window.resizeTo(390,844) is ignored (innerWidth stayed 1280, matchMedia('(max-width:600px)')
  false). (2) Chrome CDP, the one rig with real device emulation: localhost:9222 returns 404,
  and cdp.mjs has no emulate verb anyway. (3) iOS Simulator, which my own notes call ground
  truth: HTML renders but the app sits on "Connecting to server…" and /health stays blank
  through a long settle, so the API never answers inside the sim; adding the root cert per the
  documented recipe changed nothing, and simctl has no tap primitive to dismiss the first-run
  tour that covers the page.
COST: Two verifications in one afternoon. AMUX-2369 is literally titled "mobile optimized" and
  could not be verified on that axis — left `done` with the check handed back to a human with
  a phone. AMUX-2367 shipped an unresolvable question: `.send-row button` declares
  min-height:40px with no override in any of the 48 mobile media blocks, under the 44px rule,
  but min-height is a floor and I could not measure a rendered button, so it went on the card
  as a flag rather than a finding.
FIX: Cheapest and highest-value is a window size (or an `emulate` action) on the driver amux
  already ships and already launches — it is the default path and it is one launch argument
  from working. Then CDP (enable 9222 + an emulate verb). The simulator is the
  highest-fidelity rig and worth repairing, but it has two independent blockers.
NOTE: this is ethos rule 3 with a tooling shape. The verified gate asks for a check no shipped
  tool can perform, so it resolves the same way every time: the reviewer writes "could not
  check at phone width" and the mobile half of `verified` quietly becomes decorative. It will
  do that on every mobile card until a rig exists — which is exactly the "constraint that
  cannot be satisfied honestly" pattern, except the dishonest exit here is silent omission
  rather than a false ack.

---
## `git commit` on the shared checkout consumes PEERS' staged files silently
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-08
SESSION: amux
CARD: AMUX-2443
SYMPTOM: I staged amux-server.py and committed; the commit also carried
  tests/test_board_full_cache_generation.py — amux-frustrations' AF-12 test, sitting
  in the SHARED index. The staged-guard warned about the co-edited server file but
  says nothing about OTHER paths already staged by peers, and `git commit` takes the
  whole index. The amend repair is (correctly) blocked by the shared-checkout guard,
  so the misattribution is permanent in history.
COST: a peer's test shipped under my sha and message; two sessions spent time on
  notice/acknowledgement; the same sweep with a SECRETS or WIP file staged would be
  worse than misattribution.
FIX: candidate fixes, someone's to pick up: (a) staged-guard lists ALL staged
  paths not touched by the committing session's diff, loudly; (b) fleet convention:
  `git commit -- <own paths>` instead of bare commit (commit takes pathspecs and
  bypasses the index sweep); (c) both. (b) is zero-code and I am adopting it now.
## The reviewer-identity check fires on done->verified, blocking the peer amux routed the verification to
AREA: gates
SEVERITY: slows
STATUS: open
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-20
SYMPTOM: Working the VERIFY queue amux dispatched to me ("You are the independent check"),
  done -> verified was refused twice with "review sign-off required from the reviewer ...
  the review->done ack must come from that session". The attempted edge is done->verified,
  not review->done. On AMUX-2385 it is unsatisfiable by construction: the card went
  doing -> done directly (log: `status: doing -> done (by amux/session)`), so the named
  reviewer never acked a review and has no pending ack to give.
COST: Two forced bypasses in one afternoon (AMUX-2334, AMUX-2385) on cards I had fully
  measured. Both logged and attributed, so nothing is hidden — but the alternative was
  leaving a completed verification unrecorded, and a gate that trains its most careful users
  to reach for --force is inverting its own purpose.
FIX: Scope the identity check to the transition it is about. It exists so an author cannot
  self-ack their own review — that is review->done. done->verified is a different edge with
  a different role and already has its own peer criterion. Failing that, accept ANY different
  worker in the group, which is what the gate text already asks for. At minimum fix the
  message: naming the wrong transition sends the reader hunting an ack that cannot exist.
NOTE: ethos rule 6 — the published contract and the enforced one disagree. The `verified`
  gate lists four criteria; criterion 2 is "Peer-reviewed by a DIFFERENT worker in group
  `amux` (name them)", which I satisfied and named. The refusal comes from a check the gate
  text never mentions. A card can therefore pass every criterion it publishes and still be
  refused, which is the state that makes --force feel like the honest move.
## The co-edit notice asserts a git fact that was true at emission and false by delivery
AREA: notices
SEVERITY: annoys
STATUS: open
DATE: 2026-08-08
SESSION: amux-frustrations
CARD: AF-21
SYMPTOM: Two consecutive co-edit notices said "amux-server.py: you edited it at 18:58 and
  have not committed it since 18:33". My commit 44bd9fe touched that file at 19:36, so the
  sentence was false when I read it. It was TRUE when emitted — the notices fired for
  commits at 19:06 and 19:14 — and expired before delivery.
COST: The sentence exists to make you suspect your work was swept, and is followed by "your
  next git commit may say nothing to commit". So a stale one sends you to audit a commit for
  work that is not in it: `git show --stat 902e9d8` -> 8 insertions, 0 of mine. Two audits of
  two clean commits. Small each time, but it also cannot distinguish itself from the REAL
  case — 762e06e genuinely had swept my staged AF-12 work and carried the identical sentence.
FIX: Re-check at delivery, exactly as c32cf8a did for the decompose nudge (AC-252) and 7504abf
  for the three other perishable-state nudges. If the reader has committed that path since the
  notice was queued, drop the sentence or replace it with "you have since committed it in
  <sha>". The co-edit notice asserts perishable GIT state and was not in that sweep.
NOTE: distinct from the already-fixed "co-edit notice asks the reader to resolve a condition
  it is better placed to check". That was the notice ASKING; this is the notice ASSERTING
  something that has since become false — worse, because an out-of-date question costs a
  moment while a false statement sends you hunting a defect that does not exist. The emitter
  is right to be conservative; over-warning about a sweep beats under-warning. Only re-check it.

RELATED LOSS, found 2026-08-11 while validating AC-252: this entry's recorded fix used the
  same mechanism, and it is gone too. `steer_guard_stale` has zero hits in crates/. So the
  delivery-time revalidation that c32cf8a/7504abf added no longer exists in the rust server.
  The entry was already correctly `open`; this records WHY it cannot be closed by pointing at
  the python fix.

FRESH SPECIMEN 2026-08-18, amux-frustrations — STILL OPEN, and the same class one layer over.
  The idle guard reported: "You went idle with 2 uncommitted change(s) under your working
  directory" naming app.js and sw.js. `git status --porcelain` was EMPTY for both and for
  the whole tree — I had committed them in cd2e017. The two files differed from
  origin/main only because that commit was unpushed.
  So the notice compared against origin/main and called the result "uncommitted", which is
  a different predicate from the one the word means. Same shape as the 2026-08-08 case: a
  git assertion the reader cannot distinguish from the real thing. Here it is not staleness
  but a WRONG COMPARISON BASE — and the notice's own body warns at length about exactly
  this confusion ("a difference from origin/main is not a direction"), then makes it.
  Cost this time was bounded because the notice also prescribes the ancestry test, which I
  ran: `git log HEAD..origin/main -- <path>` printed nothing for both, so the safe action
  was commit-not-restore. Had I taken "uncommitted" at face value and run the remedy it
  names for the stale case (`git checkout origin/main -- <path>`), I would have reverted 18
  commits of dashboard work including that day's fix and a peer's feature work.
  That is the entry's own COST paragraph coming true at a larger blast radius: the sentence
  cannot distinguish itself from the real case, and its remedy is destructive.

## SessionStart freshness hook named files upstream never touched
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: amux-frustrations
CARD: AF-22
SYMPTOM: Hook printed `checkout is 1 commit(s) behind origin/main - including: CLAUDE.md
  amux amux-server.py`. The single incoming commit (eaa1e91) touches ONLY amux-server.py.
  Cause: the hot-file list used `git diff --name-only HEAD..$base` - TWO dots, which in
  `git diff` compares the two ENDPOINTS instead of diffing from the merge-base, so on a
  shared checkout with 120 unpushed commits it reports OUR OWN files as upstream changes.
  The same sentence disagreed with itself: `behind` uses rev-list, where two-dot IS correct,
  so the count said 1 while the file list implied a broad conflict.
COST: ~10 min reconciling two files that had zero incoming changes. The compounding cost is
  worse than the minutes: the error grows with the number of unpushed local commits, so the
  warning is least trustworthy exactly when the checkout is busiest - which is the situation
  it exists for. An instrument that cries wolf in proportion to the real risk gets ignored.
FIX: 13c7014 - three dots. Positive control in a scratch clone with upstream touching only
  amux-server.py: two-dot -> [CLAUDE.md amux amux-server.py] (reproduces the symptom),
  three-dot -> [amux-server.py]. Line 43's rev-list two-dot deliberately left alone.
## `HEAD~1` is not "before my change" here — the pre-fix specimen check tested the wrong commit
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: amux-frustrations
CARD: AF-25
SYMPTOM: Verifying AF-23's regression test against a pre-fix specimen via
  `git show HEAD~1:amux-server.py`. amux-cloud committed 939064d between my commit
  (523df63) and the check, so HEAD~1 WAS MY OWN FIX. The probe reported the disclosure
  string already present pre-fix and concluded "the test would PASS - VACUOUS - bad test!".
  Re-run against `523df63^` - the parent of MY commit - it correctly reports FAIL.
COST: ~5 min, and it was one step from costing much more: the false verdict was that a
  correctly-discriminating test was vacuous, whose natural remedy is to rewrite a good test
  into a worse one. This is the LOUD WRONG probe, not the silent one - it answers, and the
  answer looks exactly like the failure ethos rule 7 warns about, so it is self-corroborating.
FIX: documented in CLAUDE.md, in the same commit as this entry (no sha cited here: writing
  one before the commit exists is the fabrication ethos rule 7 records, and I did it in the
  first draft of this very entry). Use `git show <your-sha>^:<file>`, never HEAD~1, on
  a checkout where other lanes commit. The trap is invisible on a single-session repo, which
  is precisely why it needs writing down here: every fix in this repo is supposed to be
  checked against a pre-fix specimen, so the wrong recipe is reached for constantly.


## Dashboard's usage-limit discriminator says 'worker'; the live endpoint says 'session'
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-09
SESSION: rust-rebuild (provider adapters, RR-0043)
CARD: AMUX-2581
SYMPTOM: Porting the Claude usage probe to Rust, I took the 5h-window discriminator
  from the only in-repo consumer, loadUsage() in amux-server.py (`l.kind === 'worker'`).
  The live /api/oauth/usage endpoint returns `kind: "session"` for that window — the JS
  check never matches anymore, so the dashboard labels the 5h bar with the raw kind
  string, and the stale discriminator nearly shipped into the new Rust mapper verbatim.
COST: ~10 min re-probing the live endpoint; one step from encoding a never-matching
  filter into the Rust adapter (an ethos-7 silent probe: it would have "worked" because
  the top-level five_hour shape still mapped, masking the dead limits[] branch).
FIX: loadUsage() should accept both "session" and "worker" (the Rust mapper now does);
  better, both consumers should assert the discriminator against a recorded live
  fixture so endpoint drift fails a test instead of silently unlabeling a bar.

---
## Group-config PATCH: COALESCE arms are dead code — explicit JSON null 500s on both origins
AREA: board
SEVERITY: wrong-conclusion
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2597
SYMPTOM: /api/groups/<n>/config PATCH looks like it preserves absent keys via
  COALESCE upsert arms, but SQL NULL trips the column's NOT NULL before conflict
  resolution ever runs — so an explicit JSON null 500s on BOTH servers and the
  COALESCE arms can never fire. Also PATCH resets absent keys (send the full
  object). Found while porting to Rust; verified against Python's exact schema+
  SQL; an earlier "null preserves" reading was a killed hypothesis, recorded.
COST: A client sending a partial config update silently wipes the other keys; a
  null 500s with no useful message. Ported faithfully to Rust (bug-compatible)
  so the fix must land on both or the boundary drifts.
FIX: Decide the intended semantics (partial-merge vs full-replace), implement on
  both servers, and add a null-body regression test each side.

---
## Browser profile DELETE can rmtree a real Chrome profile (python, live)
AREA: browser
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2602
SYMPTOM: DELETE /api/browser/profile/<name> (amux-server.py:74351) resolves via
  _bu_profile_dir, which for some names lands inside the user's REAL Chrome
  user-data-dir — and then rmtree's it. An API meant to manage amux-owned
  automation profiles can delete a human's actual browser profile.
COST: Data-loss class on the live server; found only because the Rust port had
  to decide what the guard SHOULD be (native port refuses non-amux-owned dirs).
FIX: Python needs the same containment guard while it lives; the Rust deviation
  is documented in docs/rust-migration/server-boundary.md.

---
## Two /api/logs handlers in amux-server.py; the second is unreachable dead code
AREA: api
SEVERITY: misleads
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2607
SYMPTOM: amux-server.py declares GET /api/logs twice: :67673 (category/session/
  limit -> {"events","count"}) and :71933 (type/since/filter/lines ->
  {"events","raw","raw_total_lines"}). Dispatch is sequential first-match, so
  the :71933 block can never run — two handlers in the same file claim the same
  route with DIFFERENT param and response contracts, and only reading the
  dispatch order reveals which one is real.
COST: The AMUX-2605 rust port was pointed at BOTH line numbers as the contract
  to preserve; porting the dead one would have shipped an /api/logs whose shape
  the SPA (app.js:16520) never consumes. Discriminating cost a live-fixture
  capture against 8822 that reading the source alone could not settle.
FIX: Delete the :71933 block or fold its useful params (since) into the live
  handler. The rust origin ports the LIVE :67673 shape (api/request_log.rs),
  verified against the running python server.

---
## Resume drops --name, so a session's pane title shows the CONVERSATION's old name, not the worker's
AREA: attribution
SEVERITY: misleads
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2612
SYMPTOM: This worker is `amux` ($AMUX_SESSION=amux, tmux session amux-amux, log
  ~/.amux/logs/amux.log). Its tmux PANE TITLE reads `amux-rust`. Root cause is in
  the launcher: session_flag is EITHER `--resume <uuid>` OR `--name <name>`, never
  both (amux-server.py:24258-24291; the rust port carries the same seam,
  session_verbs.rs:2480). Claude Code writes the terminal title from ITS OWN
  session name, which on a --resume path is the name baked in when the conversation
  was created. Confirmed, not inferred: ~/.claude/sessions/53855.json and 66447.json
  both map sessionId 1dd2cd21-c4a7-46b9-9b97-51fccbe721a2 -> name "amux-rust", while
  amux serves the same worker as `amux`. A model swap resumes by uuid, so EVERY
  model swap silently re-asserts the stale name.
COST: The model-swap continuity handoff tells the incoming model "read
  ~/.amux/logs/amux.log, it contains THIS session's terminal history" — and the
  banner inside it reads `amux-rust`. I spent a round trip establishing which of
  the two names was mine before I could trust any of the log as my own context.
  The failure mode this sets up is worse than the confusion: a session that
  believes it is a different lane will attribute its work, its commits and its
  board writes to that lane. Same class as AMUX-1768 (relay misattribution), except
  here the wrong name is displayed by amux's own instruments rather than typed by
  an agent.
FIX: Pass BOTH on resume — `--resume <uuid> --name <worker>` — so the displayed
  name always tracks the WORKER, which is the only identity amux stamps writes with.
  If Claude Code rejects the combination, have amux set the pane title itself
  (tmux select-pane -T "$name") after launch rather than leaving the harness's stale
  name on screen. Fix in the rust launcher first; the python one is being retired.
  Cheap detector while it is open: `amux whoami` already contrasts live worker
  identity against inherited env — extend it to compare against the pane title, so
  the disagreement is reported instead of discovered.

## Idle nudge told me to commit 11 files I never touched, while the staged-guard said I owned none
AREA: notices
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-09
SESSION: amux-frustrations
CARD: AF-38
SYMPTOM: The idle dirty-tree nudge listed 11 files as mine to "commit completed work now",
  excluding only 2 as not-mine. I had touched none of the 11 - they are amux-rust's in-flight
  rust migration (crates/amux-server/src/api/*.rs, tests, install.sh, scripts/rust-auto-build.sh).
  The staged-guard, queried on the same dirty list at the same moment, disagreed completely:
  `POST /api/git/staged-guard` returned foreign=4 (owner=amux), unclaimed=18, shared/mine=0.
  My own work was already committed; git status showed nothing of mine.
COST: none, because I checked before committing - but only because I had spent the day on this
  exact defect class from the other side. Following the instruction literally sweeps a peer's
  whole in-flight rust migration into a commit under my name, which is the AMUX-2554 incident
  the fleet has already paid for twice. The instruction IS the hazard.
FIX: have the nudge resolve ownership through the same call the staged-guard uses instead of
  deriving "yours" from dirty-tree membership. Two components answering the same question
  differently is the duplicated-precedence bug AMUX-2330 already fixed once for gates: one
  answer, one owner. Note the nudge is not blind - it correctly excluded 2 files - so it has
  SOME signal and is wrong in one direction only, which is the more dangerous shape.

RESOLVED 2026-08-09 by the python retirement, NOT by a fix — recorded because 'fixed' and 'the code is gone' are different things. The nudge, including its NOT-YOURS exclusion, lived only in amux-server.py (792ce1f^:amux-server.py, exclusion at line 20190); that file is deleted from HEAD and nothing in crates/ implements it. AF-38 discarded.
  The finding survives as AMUX-2638: when the nudge is ported it must resolve ownership through the staged-guard path, not from dirty-tree membership — that substitution IS the bug and a fresh port reintroduces it by default, because `git status` is the obvious source.
  Also note the capability is simply GONE meanwhile: nothing tells any session about uncommitted work, on a shared checkout with ~7 lanes and 82 dirty files.


## The rust request log recorded a ~15-second restart choreography as a 76ms request
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: amux-rust (lifecycle-fix subagent)
CARD: AR-111
SYMPTOM: Forensics on the amux start incident: `_amux_request_log` shows
  `PATCH /api/sessions/amux/config` at ts 19:10:35 with latency 76.26ms — but the SAME
  request wrote its "Captured before model swap" log marker at 19:10:20 and the env
  header at 19:10:35.42, i.e. the handler ran a synchronous ~15s stop/relaunch
  choreography that the request log renders as a sub-100ms call. Whatever the
  middleware stamps (completion-time ts + an inner-layer latency, or a batched flush
  clock), a long-running request is indistinguishable from a fast one.
COST: ~30 minutes of incident reconstruction chasing a phantom second actor, because
  the timeline read as "capture at :20 cannot belong to a 76ms request at :35" — the
  instrument manufactured a contradiction that had to be disproved with three other
  artifacts (env header, session log markers, session_events).
FIX: request-log middleware should stamp arrival ts and wall-clock latency around the
  WHOLE handler future; a restart choreography should be a visibly long row.

## e2e auth tests flip green->red mid-session: the server under test is rebuilt from a shared checkout that moves between runs
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: no-silent-actions agent (subagent; no $AMUX_SESSION in env)
CARD: ARE-5
SYMPTOM: three consecutive runs of `npx playwright test --config e2e/playwright.config.ts`
on the same working tree: run 1 = 83 passed / 0 failed; run 2 = 12 failed; run 3 =
5 failed, all in phase0 auth ("protected API rejects a bad bearer token" expected
401, got 200) + settings_missing_endpoint_probe. Nothing in the diff between runs
was mine — the config's webServer runs `cargo run -p amux-server`, so every run
rebuilds whatever the concurrent lane has landed in crates/ since the last one.
The 401->200 flip itself looks like a REAL auth regression landing upstream while
I was testing the SPA layer.
COST: ~15 minutes ruling out my own SPA-only changes as the cause of server-side
auth failures; and a possible live auth regression (bad bearer accepted with 200)
observed but not attributable to a commit from here (NEVER-run-git constraint).
FIX: same instrument the CLAUDE.md /health-build bracket prescribes, applied to e2e:
have playwright.config.ts record the server build hash (GET /health .build) into the
run report so a mid-session flip names "the binary moved" instead of reading as
flaky tests; separately, someone with git access should bisect the 401->200 auth
behavior on current crates/amux-server HEAD.

## Peek showed 9% of each line — a `white-space: pre` on #peek-body killed wrapping
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: peek-render agent (subagent; no $AMUX_SESSION in env)
CARD: ARE-6
SYMPTOM: peek stopped wrapping. Measured on #peek-body against a 220-col lane:
scrollWidth 4196px vs clientWidth 1416px at 1440px desktop (2780px of every line
unreachable without horizontal panning), and 4196 vs 366 at 390px phone — about 9%
of each line visible on the platform amux optimises for first. Long lines were cut
at the right edge mid-sentence with no wrap and no visible affordance to scroll.
COST: peek unusable for prose on a phone for the ~1h the build was live; and a
misdiagnosis shipped with it — the complaint that motivated the change ("a diff
wrapped into a ~710px column with two thirds of a 2000px view empty") was read as a
CSS wrapping bug when it was the pane width. The filing session's own lane was at 94
columns; 94ch x 7.49px = 704px, i.e. the "~710px column" was measuring the tmux pane,
not the stylesheet. The CSS change could not have fixed it and cost prose wrapping.
FIX: fixed — removed the #peek-body override so .overlay-body's pre-wrap/break-word
applies again (python's behaviour, byte-identical). Peek never needed a global `pre`:
wrapBoxBlocks() already gives each box-drawing run its own `.peek-box`
(white-space:pre; overflow-x:auto) so tables/diffs keep alignment in their own
scroller, and _fitRules() replaces full-pane rules with a fitted element. The
container `pre` defeated both. A comment at the site records the measurement so the
override is not re-added a third time.

## Opening peek permanently narrows the worker's tmux pane — observing changes the observed
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: peek-render agent (subagent; no $AMUX_SESSION in env)
CARD: AR-110
SYMPTOM: peek POSTs /resize to fit the pane to the viewer, and tmux pins
`window-size manual`, so the width persists after the viewer leaves. Verified live:
amux-test-claude was 220x50, one peek at a 390px viewport left it at 50x50 and it
stayed there. Across the fleet at scan time: mixpeek-autopilot 50 cols, amux 102,
amux-frustrations 94, amux-rust 94 — all real lanes emitting at a fraction of their
spawn width (220) for every later reader, because someone once peeked from a phone.
The floor is Math.max(50, ...) client-side and .clamp(50, 300) server-side, so 50 is
reachable and sticky.
COST: one wrong root-cause and a shipped CSS change that had to be reverted (see the
entry above) — the narrow pane presents exactly as "the renderer is wasting the
viewport", and nothing in peek shows the pane's column count, so the reader cannot
tell a narrow pane from a narrow render. Ongoing: any lane left narrow emits
hard-wrapped output to every future viewer and to its own transcript.
FIX: AR-110. Two parts worth separating — (1) do not let a transient viewer set a
persistent property of someone else's worker (restore on peek close, or scope the
resize to the read rather than the session); (2) surface the pane geometry in peek,
so "why is this 50 columns wide" is answerable from the instrument instead of from
`tmux list-sessions`.

## The subagent switcher is wired end-to-end and reaches 0 of 50 sessions
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-09
SESSION: peek-render agent (subagent; no $AMUX_SESSION in env)
CARD: ARE-7
SYMPTOM: #peek-agent-nav (the ⌂/▲/▼ strip), agentNav(), the clickable .peek-agent-row
rows and the rust `agent-nav` verb are all present and byte-identical to the python
original — nothing was lost in the SPA extraction. The strip is gated on a VISIBLE
panel row (`⏺ main`/`◯ main`/`● main`/`○ main`) in the last 8 non-empty pane lines.
Running that predicate verbatim over every running session: 0 of 50 match, so the
strip is display:none everywhere, always. 46 of 50 DO show Claude's `← 2 agents`
status hint, but pressing ← (verified on an idle test session) opens the background
CONVERSATION manager — "Your conversation moved to the background · 4 awaiting input
· 0 working · 0 completed" with conversation rows — not a subagent panel with a
`main` row. Probe validated both ways first: a synthetic panel returns true, prose
returns false, so the zero is a real absence and not a broken matcher.
COST: a feature that looks complete in code review, in three layers plus a backend
verb, and that no user has ever been able to reach. Ethos rule 1 in its exact shape:
capability that exists but is received by nobody by default.
FIX: needs a live specimen of the current Claude Code agents panel to re-derive the
gate against — the `⏺ main` shape it looks for is either gone or only reachable from
a state nothing in the fleet enters. Do NOT widen the gate to the `← N agents` hint
without that: the existing comment warns that with rows hidden the nav keys open the
background-shells manager, and that is exactly what pressing ← did here. Separately,
what all 46 lanes actually have is background CONVERSATIONS, and amux exposes no
switcher for those at all — that is the reachable version of the same affordance.

## Every session log on the fleet stopped growing while tmux reported piping ON
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-09
SESSION: (agent, AMUX-2628)
CARD: AMUX-2628
SYMPTOM: `~/.amux/logs/*.log` frozen at 19:30 for 50 sessions, still frozen at
21:02. Every individual signal read healthy: `#{pane_pipe}`=1, the writer process
alive, its fd 1 on the same inode as the file on disk, the file present and
non-empty. The pipe writer was `python3 -c "... for line in sys.stdin.buffer: ..."`,
whose `readline()` blocks until a LINE FEED — and a full-screen Claude TUI redraws
in place with CARRIAGE RETURNS (measured on the real amux-frustrations.log: 106,081
CR bytes against 2,506 LF, 42:1). So megabytes accumulated inside the reader and
nothing was ever written. Two independent second defects rode along: `pipe-pane -o`
TOGGLES an already-piped pane OFF (tmux 3.6a: arm -> 1, arm again with -o -> 0), so
starting an already-piped session silently disabled its logging and 20 of 50 fleet
sessions were sitting unpiped; and `capture_log_tail_for_reload` detached the pipe
and never re-armed it, so any provider/model/effort swap ended logging permanently.
COST: 90+ minutes of fleet-wide terminal history lost outright for the lanes that
were parked, and 9.3 MB recovered from the stuck reader buffers only because the
re-arm made the old writers hit EOF and flush. Two sessions' logs (amux-rust,
amux-frustrations) were each holding ~3 MB of unwritten output. Nothing anywhere
reported the outage — this was noticed by a human reading a log, an hour in.
FIX: fixed. Writer rewritten to chunked `read1` + `select`, treating CR as a
terminator (`python3 -u` does NOT help — it unbuffers stdout, the block is on the
READ side); `-o` dropped from both arm sites; reload capture re-arms. The reason
nobody saw it is now its own fix: `GET /api/debug/logs` correlates pipe state, log
mtime, pane activity and writer liveness per session and computes the verdict
("stale: piping on but no write in Ns while the pane was active"), with `log_verdict`
extracted as a pure function and unit-tested against this incident's own numbers so
the alarm is demonstrably able to fire.

## A hand-written `ps | grep` probe matched its own command line and invented 3 phantom failures
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-09
SESSION: (agent, AMUX-2628)
SYMPTOM: verifying the fleet re-arm, `ps -eo pid,command | grep 'sh -c python3' |
grep -c 'for line in sys.stdin.buffer'` reported 3 sessions still on the OLD writer.
There were zero. The bash tool's own process carries the search string in its argv,
so the probe matched itself three times, and the follow-up that tried to name the 3
sessions printed 60 lines of shell fragments as if they were session names — which
is the only reason it was caught. Re-run as a child-of-tmux-server filter: 52 new
writers, 0 old.
CARD: AMUX-2628
COST: ~10 minutes and one nearly-reported false conclusion ("3 sessions did not get
the fix") in the final report. The ethos file already names this exact trap ("a probe
that matches itself in a ps listing"), which is the point: the rule was written down
and it still did not fire at the moment of use, because the answer looked plausible.
FIX: the durable version is not "remember to exclude your own pid" — it is that
fleet-wide process questions should be asked of the server, not of `ps` by hand.
`/api/debug/logs` now answers "which sessions have a live writer" as structured data
(`writer_pid`, `writer_age_s`) computed in one place, so the next session does not
hand-roll the grep at all.

## A hot model switch that HAD landed was reported as failed, because the pane sat on an unanswered confirmation dialog
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: (agent, AMUX-2617)
SYMPTOM: the new hot-model-switch path (`/model <id>` delivered to the live agent
instead of a restart) reported `mode:"restart"`, `hot_error:"no acknowledgement in
the pane within 5s"` — twice — on switches that had ACTUALLY WORKED. Claude Code
guards a mid-conversation model change behind a selector ("Switch model? … 1. Yes,
switch to Haiku 4.5 / 2. No, go back") that appears in no `--help` output and never
on a fresh pane, so it only exists once a session has a real conversation. amux typed
the command, the dialog opened, nobody answered it, and the verifier timed out on an
ack that could not render yet. The restart it fell back to then answered the dialog
with a stray keystroke and RESUMED — which is the only reason the ack showed up at
all, replayed into the new pane.
CARD: AMUX-2617
COST: ~40 minutes, and the natural next move from the symptom alone is to widen the
timeout, which would never have helped. The variant cost it a second time: the fix
anchored on the dialog's TITLE, so `/effort` (titled "Change effort level?", same
body) kept falling back while `/model` worked, and the two failures looked nothing
alike.
FIX: fixed in the same change. Two parts, and the second is the durable one:
(1) `config_switch_confirm_key` answers the dialog, anchored on the BODY line both
variants share and picking the option by its "Yes" TEXT rather than by position, so
a reordering cannot turn a confirm into a cancel; (2) the fallback now logs the pane
tail plus `echo_seen`/`ack_seen` before restarting. A fallback that leaves no trace
is indistinguishable from a switch that never happened — the pane tail is what named
the dialog within one run, both times, and it is what makes the next unexplained
timeout decidable from the log alone.

## The API answered 200 {"ok":true,"message":"sent"} for a message the model never received
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-09
SESSION: (agent, AMUX-2629)
CARD: AMUX-2629
SYMPTOM: `POST /api/sessions/amux-rust/send` at 20:55:25 answered `200 {"ok":true,
"message":"sent"}` in 1050ms. The text was typed into the pane and the Enter did not
register, so it sat in Claude Code's composer for 10m50s — the conversation JSONL
receives it at 21:06:15, only because a human pressed a bare Enter. Nine steering
messages were queued behind it. Every instrument agreed with the lie: `session_events`
holds one `message.sent` row at 20:55:26 and nothing after it, `steering_history` had
already dequeued the previous delivery as delivered, and the pane looked idle. The one
artifact that discriminates is Claude Code's own `queue-operation: enqueue` record —
it writes one for every mid-turn Enter it ACCEPTS (10 of them in that same transcript)
and there is none at 20:55. Nothing amux stores could have told anyone that.
COST: an hour of a lane sitting idle with the owner's instruction on screen and nine
commands queued behind it; then the owner's time to notice and press Enter himself.
Worse than the hour: the diagnosis was IMPOSSIBLE from amux's own data — every
recorded fact was consistent with successful delivery, so the natural conclusion from
the ledger alone ("it was delivered, the lane ignored it") is wrong and blames the
model.
FIX: fixed on AMUX-2629 (`verify_submitted` + `send_outcome` in api/session_verbs.rs).
"sent" is now read back from Claude Code's artifacts — the composer contents and the
conversation JSONL — never inferred from the `send-keys` exit code, and the response
carries `submitted` / `submission` / `retried` so the four outcomes a single `ok` bit
used to cover are distinguishable. THE UNDERLYING DEFECT IS NOT FIXED AND CANNOT BE
FIXED HERE: keystroke delivery into a TUI is best-effort by construction. Twenty
attempts across four pane states failed to reproduce the dropped Enter, which is the
point — it is intermittent, so it can only be detected and retried, not timed away.
The real fix is protocol delivery, where submission is an ACK (opencode::structured).

## Ghost-rescue can only rescue the messages that happen to carry a timestamp prefix
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: (agent, AMUX-2629)
CARD: AMUX-2629
SYMPTOM: the ported `[ghost-rescue]` sweep decides a stuck message is amux's — and so
safe to submit — only when the composer text starts with the dashboard's `[H:MM AM]`
stamp (py:9160, the only sound discriminator: anything else risks submitting a
half-written human thought). A read-only scan of the live fleet found 13 lanes holding
composer text with no matching user message in their transcript — `backend` "continue
with the queue", `ethan-dev` "push it", `mvs-infra` "Run the MVS prod health loop per
the runbook", and ten more — and ZERO of the 13 carry the stamp. The dashboard applies
the prefix inconsistently (`cmd_history` for amux-rust alone has both prefixed and
unprefixed human sends in the same hour), and agent-to-agent and nudge messages never
carry it.
COST: not yet counted in minutes, but it is 13 messages the fleet is currently sitting
on, and a fallback that covers 0% of the live population reads as protection that is
not there. Deliberately not widened: guessing "this looks like amux" would eventually
submit a person's unfinished sentence, which is worse than the stall.
FIX: two honest options, both upstream of the sweep. (1) Make the stamp universal — if
every amux-originated message carried a machine-readable origin marker, the guard would
be exact instead of a heuristic. (2) Better: deliver over the structured protocol, where
there is no composer to get stuck in and nothing to sweep for; the sweep's exit condition
is written into its module docs for that reason.

## Usage meter said "no token" while the token was fine and Anthropic was rate-limiting
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: rust-usage
CARD: RU-1 (follow-on SPA label gap); this entry's own fix is in api/usage.rs
SYMPTOM: Settings showed "Claude subscription usage unavailable on this host (no
  token, expired token, or probe failed)" — one string for four different causes.
  The keychain credential was present and unexpired the whole time; the real cause
  was HTTP 429 from api.anthropic.com, intermittent on a host running ~95 Claude
  Code processes against one account. Two servers on this box disagreed 20s apart:
  one served real limits, the other served the same "unavailable" sentence.
COST: The meter read as a broken install for as long as it was dark, and the one
  message could not distinguish "log in again" (user action) from "wait, it clears
  itself" (no action). The endpoint's own #[ignore]'d live test was GREEN throughout,
  because it only iterated `usage.windows` — zero windows iterates zero times, so a
  totally failed probe asserted nothing (ethos rule 7, the vacuous-check shape).
FIX: Fixed. The probe is now discriminating (provider/claude.rs `UsageProbe`:
  NoToken / Expired / Http(code) / Transport / BadShape) and api/usage.rs turns each
  into its own reason plus a stable `cause` tag, with the HTTP status included. The
  live test now asserts the discriminator (a host WITH a credential must never report
  NoToken) instead of iterating a possibly-empty vec. Because the 429 is intermittent,
  a good reading is also kept and re-served for AMUX_USAGE_STALE_S (default 600s)
  marked `stale: true` with the live failure in `stale_reason`, so the meter stops
  flickering dark.

## A peer's `install` shipped my uncommitted, unverified WIP straight to the live server
AREA: cli
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: board-drive (AMUX-2637)
CARD: AMUX-2637
SYMPTOM: I created `crates/amux-server/src/runtime_jobs/board_drive.rs` and wired it
  into `lib.rs` at ~22:0x, having run NO tests yet. At 22:07 another session rebuilt
  and installed `~/.local/bin/amux-server-rs` from this shared checkout; `strings` on
  the live binary shows `runtime_jobs/board_drive.rs`, and `/api/debug/board-drive` —
  an endpoint I had written minutes earlier — answered on :8822. Within 3 minutes the
  live loop had claimed AF-38 and AR-112 and routed two review nudges on the real
  fleet. I never installed anything.
COST: Unverified code reached production and mutated the live board. It happened to be
  correct (AF-38/AF-34/AF-33/RH-96 all moved, WIP-1 held), but two defects I found
  MINUTES LATER by testing shipped with it: a lane was told "you went idle holding
  BDQ-1" one tick after being handed BDQ-1, and a review route re-fired every 60s until
  the 24h per-card budget was spent in three minutes. The live build still carries both.
  The `git push` guard in CLAUDE.md ("check what you are shipping that is not yours")
  covers the git dimension only; the BUILD dimension has no guard at all, and it is
  strictly worse — a push ships committed work, an install ships whatever is in the
  working tree, including a file that has never been compiled by its author.
FIX: The install path should refuse, or at minimum announce, a build made from a dirty
  tree containing files no commit references. Cheapest honest version: have the
  installer stamp `git status --porcelain` + the untracked file list into the binary
  and surface it at `/health` as `built_from_dirty_tree: [...]`, so "is this build
  someone's WIP?" is answerable from the instrument everyone already reads instead of
  from `strings`. Related to the shared-checkout push rule, same root: on a shared
  checkout, one session's routine action ships another session's in-flight work.

## The board-drive trace reported `eligible_todos: 0` for lanes with cards waiting
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: board-drive (AMUX-2637)
CARD: AMUX-2637
SYMPTOM: `/api/debug/board-drive` — the surface built specifically so a skipped lane is
  distinguishable from a dead loop — showed `bdq-assign  skipped  not-running  elig=0`
  while BDQ-1 sat dispatchable in that lane's queue. The counts were only filled in on
  the code paths that got PAST the liveness and turn-boundary gates, so every lane
  stopped by a gate reported its backlog as zero.
COST: Caught during my own verification, before anyone else read it — but it is the
  exact ethos rule 4 failure inside the instrument written to prevent it. The reader's
  question is "how much work is this lane sitting on, and why did it get none", and
  half the answer was a confident zero. A wrong number is worse than a missing one.
FIX: Fixed in board_drive.rs `drive_lane`: the backlog counts are computed BEFORE any
  gate and attached to every trace row, whatever stopped the lane. General form: when a
  trace has both a "why" and a "how much", the "how much" must not be computed on the
  happy path only.

## SUPERSEDES the "13 lanes holding stuck text" entry above — they were empty; the reader was wrong
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: (agent, AMUX-2629)
CARD: AMUX-2629
SYMPTOM: I reported 13 live lanes "holding composer text with no matching user message in their
transcript" and flagged that the ghost-rescue prefix guard would rescue none of them. Both halves
were built on a false reading. All 13 composers were EMPTY. What they held was Claude Code's DIM
suggestion — `\x1b[2m` — and `_pending_input` (py:25349, ported faithfully) strips ANSI before
reading the ❯ line, which makes a suggestion and real typed input the same string. Two other
sessions then spent time on it: one pressed Enter, C-m and Escape+Enter on those lanes and reported
that none worked (correct — there was nothing to submit), and reasoned toward a
background-conversation-manager theory; another read the "← 2 agents" marker as the common cause. It
is on every lane, including a brand-new claude in an empty directory that accepted Enter 20/20 times.
COST: three sessions' time chasing an artifact, one wrong hypothesis published, and the next step
queued up was submitting 13 stale instructions into live lanes — which would have been the real
damage. My own entry above is what made it look corroborated.
FIX: `composer_state()` in api/session_verbs.rs — the dim attribute decides, and callers must pass
the RAW `capture-pane -e` output. `pending_input` is DELETED rather than fixed: a function that can
be called with a stripped frame re-creates the bug silently, so there is now exactly one way to read
the composer and it cannot be handed the wrong input. The lesson generalises past this bug: when a
probe's output is the same for two states, the fault is the probe, and "strip the ANSI first" throws
away the only bit that distinguished them.

## A lane froze its own steering queue for four hours by writing the words "esc to interrupt"
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-09
SESSION: (agent, AMUX-2629)
CARD: AMUX-2629
SYMPTOM: amux-rust held 10 steering messages for up to 229 minutes while every gate passed — env
file present, tmux alive, self-report `idle` 198s old, composer empty, status bar `⏵⏵ bypass
permissions on (shift+tab to cycle) · ← 2 agents`. The refusal came from send_text's active-signal
re-check, which is `"esc to interrupt" in tmux_capture(name, 12)` (py:25650) — an UNSCOPED substring
match over the whole pane. Lines 26-27 of that pane were the lane's own prose about a status-detection
fix: `Workers with "bypass permissions on" + "esc to interrupt" on the status bar were misdetected as
IDLE`. So the lane most likely to write that string is the lane that works on the scraper, and it
blocked itself. Compounding it, the tick took ONE row per lane oldest-first and moved to the next
LANE on refusal, so one undeliverable row froze all ten.
COST: four hours of a lane not receiving the owner's instructions, while the owner asked twice why
workers were not moving. Finding it needed a hand-written DB read plus a pane capture, because the
tick logged only successes — a skip left no trace anywhere. A peer independently reached a different
root cause (the @-picker guard) from the same symptoms; it was not that, and fixing only the @ path
would have left the lane frozen.
FIX: three parts. (1) `pane_bar_says_generating()` scopes the marker to the bottom 3 non-blank lines,
so prose cannot be a status. (2) The tick and the reactive deliverer now walk to the lane's NEXT row
on a refusal instead of abandoning the lane — one bad row can no longer freeze a queue. (3) Every
skip is logged with its reason, a lane whose oldest row exceeds 20 minutes is announced at WARN, and
`GET /api/debug/steering` exposes per-lane depth, oldest age and last refusal reason. Reproduced
end-to-end on a throwaway lane put into the same state: pre-fix predicate = 0 deliveries in 10 ticks;
bar-scoped predicate = both rows delivered, @-mention included.

---
## Six SPA-consumed API families 404 in production and nothing anywhere says so
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux-rust (RR-0130/0131 cutover sweeps)
CARD: AR-114, AR-115, AR-116, AR-118, AR-119, AR-120
SYMPTOM: The RR-0130/0131 live-data sweeps compared what the SPA READS against what the
  rust server SERVES. Six families the shipped dashboard calls answer 404 on the live
  server, and every one exists nowhere in `crates/`: `/api/channels/{a}/{b}/messages`
  (the DM drawer, polled every 2500ms), `/api/log-search`, `/api/memory/global`,
  `/api/observability`, `/api/review/week`, `/api/review/digest`. A seventh,
  `/api/metrics`, answers 200 with a completely different document than the SPA reads
  (`{board,events_journal,leases,queues,...}` vs the expected `data.sessions[]` +
  `data.system` + `data.server`), and the SPA calls `s.cpu_percent.toFixed()` on it
  unguarded. Nothing errored at cutover, no check went red, and the boundary registry
  (`/api/debug/boundary`) reports `proxied: []` — i.e. "everything is native" — because
  a family nobody implemented is not a family anybody proxied.
COST: These shipped broken at the python retirement and were still broken hours later;
  they were found only because someone diffed SPA call sites against live routes by
  hand. `/api/observability` is the entire Cost view, so 387,524 `token_ledger` rows
  have had no reader since cutover. Same failure shape as AMUX-2637 (board drive) and
  AMUX-2629 (submission): python-only capability, unported, invisible because absence
  does not raise.
FIX: The missing instrument is the one that would have caught all seven at once — a
  check that walks the SPA's own fetch call sites and asserts each resolves to a mounted
  route. `ROUTE_TABLE` already proves the reverse direction (claimed routes are routed);
  nothing proves the SPA's demands are met. `/api/debug/boundary` should report families
  the SPA calls that resolve to neither native nor proxied, so "unported" is a state the
  registry can express instead of one that reads as clean.

---
## Two rust call sites defer work to "while the Python server runs" — python is retired
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: amux-rust (RR-0131b sweep)
CARD: AR-117
SYMPTOM: `api/session_verbs.rs:5910` says `_write_claude_memory (symlink into
  ~/.claude/projects) is not ported — Python owns the memory composition during
  coexistence`, and `api/scope.rs:41` says `While the Python server runs (the migration
  soak) its next compose picks the edit up; the gap closes with the memory-compose port,
  not here.` Both are honest, well-written deviations — and both were made void the
  moment python was shut down. A worker memory write now updates
  `~/.amux/memory/<name>.md` and never composes `~/.claude/projects/<proj>/memory/
  MEMORY.md`. RR-0131b's own acceptance line ("MEMORY.md regenerated from migrated
  entries") cannot pass.
COST: Silent divergence between the memory a session edits and the memory Claude Code
  loads, for an unknown number of edits since cutover. Found only by grepping comments
  during a sweep; no test, no check and no doc references either site.
FIX: Deviations whose mitigation is "the other server covers it" need to be enumerable.
  A `GRACE:`-style marker (or a `python_covers_this` const the retirement checklist
  greps) would have turned python's shutdown into a list of exactly what stopped being
  covered, instead of a discovery process. RR-0154's shutdown criteria should include
  that grep.

---
## The gate-blocked 409 tells every agent to GET a route that does not exist
AREA: gates
SEVERITY: annoys
STATUS: open
DATE: 2026-08-09
SESSION: amux-rust (RR-0150 restart suite)
CARD: AR-123
SYMPTOM: Every gate_blocked 409 from `/api/board/<id>` carries
  `how_to_ack.contract: "GET /api/board/contract"` (`api/board.rs:1175` and `:1664`).
  `GET /api/board/contract` returns 404 `{"error":"item not found","id":"contract"}` on
  both a fresh build and the live server — it is being matched by the `/api/board/{id}`
  route as an item id. Hit it while making the restart suite move a card `todo -> doing`.
COST: Small on its own — the 409 also carries `gate` and `gate_ack`, so the escape is
  walkable without the contract. But it is ethos rule 6's exact shape: the one documented
  route out of a gate is the one action that leaves the sanctioned path, and it is the
  instruction amux itself prints. AMUX-2325 is the same defect one layer up.
FIX: Mount `/api/board/contract` ahead of `/api/board/{id}`, or delete the claim from
  both 409 bodies. Whichever — the test is that following the error message literally
  has to work.

---
## A worker whose pane died at launch reports `running: true` / `idle`
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux (cloud rust image, AMUX-2619)
CARD: AMUX-2644
SYMPTOM: Started a worker in the new cloud container. `GET /api/workers/<id>` returned
  `{"status":"idle","running":true,"state":{"state":"idle"}}` — a healthy-looking lane.
  `peek` showed what had actually happened: `--dangerously-skip-permissions cannot be
  used with root/sudo privileges for security reasons` … `Pane is dead (status 1)`.
  The tmux SESSION still exists after the pane dies (`remain-on-exit on`), so "the
  session is there" is true and "the agent is running" is false, and the status field
  reports the first while reading like the second.
COST: This is the single blocking defect of the cloud rust cutover — every agent lane in
  every workspace would have died at launch — and the worker list said nothing was wrong.
  It was found only because I peeked at a lane I had no reason to suspect. On the live
  host the same failure would present as "the fleet is idle", which is the one shape
  nobody investigates. `idle` is also what a correctly-waiting lane reports, so no
  amount of watching the status column can distinguish them.
FIX: `idle` must not be reachable when the pane is dead. tmux already knows
  (`#{pane_dead}` / `#{pane_dead_status}` are one `display-message` away, and the peek
  text carries `Pane is dead (status N)`), so this is a state the detector can express
  and currently does not. A `dead` state — or at minimum `running:false` — with the exit
  status attached. Related: the browser failure in the same container named its symptom
  (`CDP never answered within 12s`) and not its cause; both are the ethos rule 4 shape,
  where the diagnosis is impossible from what the instrument reports.

---
## `amux-rs why … | head` exits 101 with a Rust panic instead of 0
AREA: cli
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-09
SESSION: rust-rebuild (RR-0109/0110 lane)
CARD: ARE-9
  every OTHER verb in crates/amux-cli/src/main.rs still has it
SYMPTOM: `amux-rs why schedule SCHED-30` emits 220 lines for a schedule with 3,303
  recorded runs. Piped into `head -3` it printed the first lines and then exited
  **101** with a panic (`failed printing to stdout: Broken pipe`). Unpiped, the same
  command exits 5 — the verb's real "partial verdict" code. Rust ignores SIGPIPE, so
  `println!` unwraps a `BrokenPipe` write error into a panic.
COST: ~10 minutes chasing a phantom crash in the new endpoint. Worse than the minutes:
  `why` publishes exit codes as its machine-readable verdict (0 explained, 5 partial,
  6 cannot_tell), and a verb that exits 101 on the most ordinary shell idiom teaches
  a caller that those codes cannot be trusted. It also reads as "the instrument
  crashed on the case I was investigating", which is the worst possible false signal
  from a diagnostic tool.
FIX: `outln!` macro in crates/amux-cli/src/main.rs writes through a locked stdout and
  returns `Ok(0)` on a closed pipe. Applied to `search` and `why`. **The other verbs
  (`board list`, `board show`, `workers list`, `schedules list`, `health`) still use
  bare `println!` and still panic the same way** — `amux-rs board list | head` on a
  4,773-card board is the same bug waiting. The root fix is either resetting SIGPIPE
  to SIG_DFL at startup (needs a `libc` dep) or routing every verb through `outln!`.

---
## Uncommitted migrations reach the LIVE database within minutes, from another agent's server
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: rust-rebuild (RR-0109/0110 lane)
CARD: ARE-10
SYMPTOM: I created `crates/amux-server/migrations/0013_search.sql` at 22:16:42 EDT and
  never installed or restarted anything. At 22:18:23 EDT the migration was applied to
  `~/.amux/amux.db` — the live 269MB database — creating 2 tables, 24 triggers and
  backfilling 5,021 rows. `scripts/rust-auto-build.sh` is NOT the culprit: it builds
  from a `git worktree` of HEAD and 0013 is not in HEAD. The cause is that some other
  session on this shared checkout ran a working-tree build of `amux-server` with the
  default `AMUX_DB`, which is the live file.
COST: No damage this time — the migration is additive and applied cleanly, and it is
  in fact the best live evidence I have. But I explicitly set out to test against a
  `.backup` copy precisely so I would not write to the live DB, and the live DB had
  already taken my schema before I made the copy. A session cannot honour "never touch
  the live database" when a peer's ordinary `cargo run` applies that session's
  uncommitted migrations to it. The same mechanism with a destructive or wrong
  migration is a data-loss event with no author and no audit line.
FIX: make the live database opt-IN for a locally-built binary. Either default
  `AMUX_DB` to a scratch path unless `AMUX_ALLOW_LIVE_DB=1`, or refuse to apply a
  migration whose version is absent from HEAD unless the same flag is set — the
  discriminator (`git cat-file -e HEAD:<migration>`) is one cheap call, and it exactly
  separates "this build is the deployed one" from "this build is someone's working
  tree". Right now nothing distinguishes them and the live file is the default.

## A continuously-busy lane starved its own queue forever: the boundary gate has no deadline
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-09
SESSION: (agent, AMUX-2642)
CARD: AMUX-2642
SYMPTOM: `steer_lane_at_boundary` returns true only on `idle`, so a lane that works continuously has
no boundary and its queue never drains. Measured on the `amux` session — status `active` with a
6-second-old tool-hook report, correctly working — holding five messages queued 22:06..22:28, none
delivered; amux-rust held ten, the oldest 229 minutes. From the sender's side the only evidence is
"nothing is happening", which reads as a hung worker. Three of amux's five carried `@`-mentions, which
under the old picker guard could never be delivered to a busy lane at all, so the two faults hid each
other.
COST: two lanes not receiving the owner's instructions for hours while he asked twice why workers were
not moving; a sender concluding a healthy lane was hung; and — the part that made it expensive — five
messages aging past 20 minutes with no signal anywhere: no log line, no card field, no endpoint.
FIX: three parts, and the third is the one that generalises. (1) `AMUX_STEER_MAX_AGE_S` (default 10
min): boundary first, but past the deadline the message goes into the running turn, where Claude Code
queues it and folds it in at ITS own boundary — real queue semantics implemented by the agent instead
of by amux waiting forever. A selector is still never overridden: answering a pending tool is the
user's call, not amux's. (2) Picker-shaped text now goes through `paste-buffer -p` at any length —
measured live, `@`-text TYPED mid-turn is lost 1/1 while the same text PASTED mid-turn is accepted
4/4, because a bracketed paste never opens the autocomplete. That is what makes the overdue delivery
safe for `@` messages rather than just for plain ones. (3) The gate and the send path now share one
predicate (`pane_is_at_boundary`). They did not for one build, and the disagreement deadlocked
delivery in a way that was a bug in neither half: the gate read the frame as idle (so it never
consulted the deadline) while the send path read it as generating (so it refused) — every tick,
forever. A view that disagrees with the mechanism it describes is worse than no view.

## `git add -A` on the shared checkout committed module declarations for five files it did not stage
AREA: cli
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux (batch: AMUX-2618/2599/2636/2634/2609)
CARD: AMUX-2654
SYMPTOM: `main` has not compiled since 22:43. Someone committed with `git add -A`, which swept in three
sessions' in-flight edits to `api/mod.rs`, `runtime_jobs/mod.rs` and `ghost_rescue.rs` — including the
`pub mod offline_origin; pub mod sessions_git; pub mod search; pub mod why; pub mod pane_size;` lines —
while the FILES those lines name stayed untracked, because they belong to other sessions who had not
finished. `scripts/rust-auto-build.sh` builds a worktree of HEAD, so it sees only committed files:
`error[E0583]: file not found for module offline_origin` x5. Four builder cycles logged BUILD FAILED in
`~/.amux/logs/rust-auto-build.log` (83ab8ac, 0b156bb, 1155f25 twice) and the stamp is still stuck at
62e9bdd. Separately and on the same checkout, a one-line edit of mine inside `resize_pane`
(`note_resize`, AMUX-2634) was silently reverted by a concurrent writer of `session_verbs.rs` between
my edit and my test run.
COST: nothing whatsoever errors. The builder is designed to keep the last good build on failure, so the
server stays up and every session tonight believes its change will deploy; none will, and no session is
told. It cost me a wrong conclusion in the other direction too: I measured the pane-restore timing three
times and read the results as a lease bug in my own code, because the reverted line produced *plausible*
behaviour (a restore, just too early) rather than a crash — the unit tests could not catch it, since what
vanished was the CALL SITE, not the function. Roughly 25 minutes, and it was only caught because the
live end-to-end test disagreed with the passing tests.
FIX: two halves. (1) Immediate: `git add` the five missing files (or revert the declarations), then
confirm with the builder's own recipe — `git worktree add --detach $W HEAD && cd $W && cargo build`.
(2) Structural, and the one worth building: CLAUDE.md's Deploy section already warns that a push ships
other sessions' COMMITS, but the same hazard exists one step earlier, at STAGING, and it is worse —
`git add -A` produces a HEAD nobody can build, whereas a bad push at least builds. A `pre-commit` hook
that refuses when the staged set contains a `pub mod X;` whose `X.rs` is untracked would have caught
this exact commit in under a second, and it is checkable by the machine rather than by remembering.
The generalisable point: on a shared checkout the unit of work is a PATH, never `-A`, and the tell that
the rule is not being followed is a build that fails in a file its committer never opened.

## The dashboard's "New worker" button cannot create a worker (405)
AREA: cli
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2655
SYMPTOM: `POST /api/sessions` answers `405, allow: GET,HEAD`. The create dialog shows
  "Create failed: error 405" and stays open. `POST /api/workers` exists but writes a
  `workers` table row, a different substrate from the `~/.amux/sessions/*.env` registry
  the fleet actually reads — so it is not a workaround, it creates an invisible worker.
COST: the only way to make a worker for a UI test was to duplicate an existing one; a
  user with an empty fleet has no path at all. Found only because a test needed it.
FIX: `sessions_legacy::create_session_legacy` + `.post()` on the route (written,
  uncommitted — this session is barred from committing). Verified 201 + worker present.

## Board card Delete removes the card and never deletes it
AREA: board
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2656
SYMPTOM: `DELETE /api/board/{id}` -> 405 (the route was `get().patch()` only).
  `deleteBoardItem` filters the card out of `boardItems` and re-renders BEFORE awaiting
  the request, and does not roll back on failure — so the card disappears at ~40ms, the
  server still has it, and the next `fetchBoard()` brings it back.
COST: this is the reported "tons of board items are not moving". Every board delete
  since the cutover was a no-op that looked like a success.
FIX: `board_store::soft_delete` + `board::delete_item` + `.delete()` on the route, and
  rollback in `deleteBoardItem`/`updateBoardItem` (written, uncommitted). Verified: card
  gone at 21ms, DELETE 200, 404 on re-GET, stays gone after refresh.

## Two endpoints disagree about whether a worker is running, and the card believes the wrong one
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2657
SYMPTOM: after Stop, `GET /api/sessions` says `running: true` forever while
  `GET /api/sessions/<n>/info` says `false`. The list derives running from "a tmux
  session named amux-<n> exists"; `stop` deliberately leaves the tmux shell alive. The
  card therefore never shows the Start button and Stop reads as having done nothing.
COST: a full measurement pass concluded "Stop returns 202 and does not stop the
  session" — the agent WAS dead; only the card was lying. Wrong conclusion, ~20 min.
FIX: one batched `tmux list-panes -a -F '#{session_name}:#{pane_current_command}'` into
  `FleetSignals.shell_only`, plus `agent_running()` as the single accessor so the two
  answers cannot drift again (written, uncommitted). Verified both agree after Stop.

## Every server refusal reached the user as a bare status code
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2658
SYMPTOM: `apiCall` did `showToast('Error: ' + r.status)` and dropped the body. Archive
  on a PINNED worker returns `403 {"error":"cannot archive pinned session — unpin
  first"}` and the user saw "Error: 403". Board gate 409s carry the full checklist AND
  the exact `cli:` string that would work; none of it was ever shown.
COST: this is most of the reported "nothing happens if i delete or archive" — the
  server explained itself every time and the UI threw it away.
FIX: `_apiErrText()` surfaces `error`/`message` plus `cli` (written, uncommitted).
  Verified: "403: cannot archive pinned session — unpin first" and "409: already
  holding doing — try: amux board doing AMUX-X --override-doing".

## Editing static/app.js does not rebuild the embedded dashboard
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-09
SESSION: amux
CARD: AMUX-2659
SYMPTOM: `crates/amux-dashboard` has no `build.rs` and rust_embed did not invalidate.
  After editing `static/app.js`, `cargo build --release -p amux-server` recompiled only
  `amux-server` and produced a binary serving the PREVIOUS app.js — the page reported
  `APP_VER 0.9.553` while the file on disk said `0.9.555`. Only
  `touch crates/amux-dashboard/src/lib.rs` forced the re-embed.
COST: a full verification pass was run against the OLD client and reported the fixes as
  not working (the pinned-worker toast still said "Error: 403"). ~25 min, and it is the
  loud-wrong kind: the sweep produced confident, plausible, false results. Worse in
  production — a dashboard-only commit can deploy stale client code silently.
FIX: add `crates/amux-dashboard/build.rs` emitting
  `cargo:rerun-if-changed=static` (and assert the served APP_VER matches the file, so
  the check can fail).

---
## The staged-guard endpoint was unrouted on the rust server and the hook printed nothing
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-09
SESSION: amux-rust
CARD: AR-132
SYMPTOM: `POST /api/git/staged-guard` answered **405** on the rust origin (8822/8824) —
  ~1,147 calls/hour — while the generated `.git/hooks/amux-staged-guard` wrapped the call
  in `except Exception: return 0  # fail open` and printed NOTHING. Every commit on every
  shared checkout ran with cross-session sweep protection OFF, and nothing anywhere said
  so. Two independent things hid it: the hook's silent fail-open, and the fact that an
  unrouted `/api/*` path on this server answers **405 from the GET-only SPA catch-all**,
  which reads as "wrong method" rather than "no such route".
COST: Two sweeps landed on this checkout in one night with the guard nominally armed, and
  a third while I was fixing it — peer commit 572047d swept four uncommitted `pub(crate)`
  edits of mine in `session_verbs.rs` into an unrelated steering fix. Same guard had
  already regressed to silence once before (AC-261), and nothing detected either
  regression: the only signal was the absence of output, which is what a passing check
  also looks like.
FIX: Ported natively — `crates/amux-server/src/api/git_guard.rs`, mounted in `api/mod.rs`,
  registry row in `py_proxy.rs`, ROUTE_TABLE row in `request_log.rs`. The server never
  500s into a fail-open: it answers `undecided` + `reason` when nothing could be compared
  and `degraded` when the verdict may UNDER-report (e.g. a cotenant whose transcript it
  cannot read), so an empty verdict is no longer indistinguishable from a clean one.

---
## A guard's only client swallowed the failure it existed to report
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: amux-rust
CARD: AR-132
SYMPTOM: The hook had ONE `except Exception: return 0` covering the local git calls, the
  HTTP request, and the JSON parse. "Server unreachable", "route gone", "server broke" and
  "answered garbage" were the same silent exit 0. Meanwhile `scripts/install-hooks.sh`
  refused to install the guard and told the reader to "start the amux server for this
  work_dir" — advice that was true under python and false after the cutover, because the
  generator was deleted with `amux-server.py` and nothing in rust writes the hook.
COST: The advertised recovery path did nothing, and running install-hooks.sh would have
  made things WORSE: the tracked `scripts/git-hooks/pre-commit` had no staged-guard shim,
  so installing it DELETED the shim the retired python had injected — turning the guard
  off while printing `ok .git/hooks/pre-commit matches ...`. A second silent-disable path,
  sitting inside the tool meant to repair the first.
FIX: `scripts/git-hooks/amux-staged-guard` is now the tracked source (the previous
  "second producer" objection died with the generator); the shim is back in the tracked
  pre-commit; install-hooks.sh installs BOTH, verifies the shim link — not just file
  equality — and probes the live endpoint so an unrouted server is reported where someone
  is already looking at hooks. Three distinct failure messages in the hook; fail-open
  stays, silence does not.

---
## The fleet's only physical liveness signal was a tmux field that never moves
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-09
SESSION: (Claude Code in iTerm — not a fleet lane, hence no session stamp)
CARD: AMUX-2662
SYMPTOM: `derive_status` reads `#{session_activity}` as "when did this pane last paint".
  On tmux 3.6a that field does not track pane output for a DETACHED session, and every
  amux lane is detached. Measured: 60 of 63 live sessions had a `session_activity` more
  than 60s older than their `window_activity`, and `amux-rust` — mid-turn, spinner
  repainting ~6x/s — reported a `session_activity` that had not moved in 34.5 HOURS (it
  was still exactly `session_created`). `#{window_activity}` was current for all of them.
COST: Both consumers of physical liveness were silently inert for the whole fleet, for as
  long as the field has been read. `now - act < 60 -> active` could never fire; the guard
  demoting a stale `active` transition fired for EVERY session on EVERY request. So fleet
  status was whatever the self-reports said and nothing else — which is the precondition
  that let one fabricated `idle` report label a working lane idle for 1076s with nothing
  able to disagree. The wrongness was invisible because a lane with working hooks looks
  correct anyway: the dead signal only shows up when the reports are wrong, i.e. exactly
  when you need it.
FIX: `activity = max(session_activity, window_activity)`, parsed by a pure function
  (`parse_list_sessions_line`) so the rule is testable without a tmux server — the first
  version of that test re-typed the parse inline and passed against the bug. Uncommitted
  in the shared checkout at time of writing.

---
## A read-only fleet probe returned "0 problems" while examining 0 lanes
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-09
SESSION: (Claude Code in iTerm — not a fleet lane, hence no session stamp)
CARD: AMUX-2662
SYMPTOM: The live card-vs-pane consistency check selects lanes to probe by "painted
  recently". Run against the PRE-FIX derivation it printed: `63 tmux sessions, 0 painted
  inside the probe window, 0 of those mid-turn, DISAGREEMENTS: 0`. A clean bill of health
  computed over an empty candidate set, because the activity field it selects on never
  moves (entry above). Post-fix the same command reports `5 painted, 2 of those mid-turn,
  DISAGREEMENTS: 0` — the same verdict, now meaning something.
COST: Nothing yet, because the discrepancy between the two runs was visible side by side.
  The cost it WOULD have had is the whole point: a sweep step reporting 0 disagreements
  daily, forever, over a candidate set that is structurally always empty. This is the
  empty-grep trap with a denominator, and the denominator is the only thing that gives it
  away.
FIX: The check prints its denominators — fleet size, lanes probed, lanes confirmed
  mid-turn — beside the disagreement count, plus the full status histogram, and the sweep
  contract (`docs/rust-migration/log-sweep.md`, step 6) says to read them. A count of 0 is
  only meaningful next to the number of things counted.

---
## A peer's commit shipped this run's in-flight work to origin, mid-edit
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-09
SESSION: (Claude Code in iTerm — not a fleet lane, hence no session stamp)
CARD: AMUX-2663
SYMPTOM: TWICE in ~40 minutes, by different peers. `e679bdb` ("fix(hygiene): five carded
  defects") took an in-progress `/report` attribution change in `api/session_verbs.rs` and
  a brand-new test file that had not yet passed — it was still 404ing on a missing rig
  fixture at that moment. Then `3b24fcd` ("fix(build): main has not compiled since 22:43")
  took the whole in-progress status derivation in `api/sessions_legacy.rs`, 495-line test
  module included, mid-refinement. Both are on origin/main
  (`git rev-list --count origin/main..main` = 0) before either was noticed.
COST: Benign by luck — the swept-up code passes now. But this run was explicitly
  instructed never to commit or push, and its work was pushed anyway, twice, once with a
  red test. Also cost the confusion of `git status` no longer listing files that were
  definitely modified minutes earlier.
FIX: Not a rule ("remember to `git add` specific files" is the kind of rule that does not
  run). Two things that would close it structurally: a pre-commit check that refuses a
  commit touching files whose most recent writer was a different session — the
  `Amux-Session` trailer machinery in `scripts/git-hooks/prepare-commit-msg` already makes
  the writer knowable — or per-lane git worktrees, which the harness already supports.
  CLAUDE.md's Deploy section documents the REBASE version of this hazard; this is the
  `git add -A` version, and it needs the same warning.

## `amux-rs board list | head` panicked with 254 bytes of Rust backtrace noise
AREA: cli
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-10
SESSION: amux-rust
CARD: AMUX-2653
SYMPTOM: `amux-rs board list | head -2` exited 101 and printed "thread 'main'
  panicked at library/std/src/io/stdio.rs:1165: failed printing to stdout: Broken
  pipe (os error 32)" plus a RUST_BACKTRACE note. Piping a verb to `head` is the
  most ordinary thing a user does with a CLI.
COST: Low per occurrence, but it makes every `| head` look like amux crashed, and
  it trains you to distrust exit codes from the CLI — which is expensive later,
  because a real failure and a pipe close were byte-identical from the caller.
FIX: e3acb7d — restore SIG_DFL for SIGPIPE once in main() instead of converting
  ~30 println! sites. Process-wide fault, process-wide fix: covers every verb
  added later too. Regression test crates/amux-cli/tests/sigpipe.rs, shown to
  fail with the call removed.

## A CLI probe measured a connection failure and it read as the bug reproducing
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux-rust
CARD: AMUX-2672
SYMPTOM: While reproducing AMUX-2653, every verb returned exit 1 whether piped or
  not. That reads as "the panic is everywhere". It was not: amux-rs defaults to
  https://localhost:8823, nothing listens there (8822 and 8824 both answer
  /health), so each verb died on connect before writing a byte. The real bug only
  appeared once AMUX_RS_URL was set by hand — and then only for `board list`,
  because the other verbs are too short to fill the pipe buffer.
COST: ~20 minutes and one wrong intermediate conclusion, which was then corrected
  only because 101 vs 1 did not match the card's claim. A less specific card would
  have let the wrong reading stand.
FIX: AMUX-2672 — point the default at a port that exists. The general shape is the
  one already in ethos rule 7: a probe whose failure mode is indistinguishable from
  the fault it is hunting will corroborate whatever you already believe. A
  connection error and an application error should not both surface as exit 1 with
  no discriminator.

## A stderr capture moved stdout off the pipe, so nothing could break
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-10
SESSION: amux-rust
CARD: AMUX-2653
SYMPTOM: Comparing panic noise before/after the fix with
  `amux-rs board list 2>&1 >/dev/null | head -2` returned EMPTY for both binaries.
  The redirection order sends stderr to the pipe and stdout to /dev/null — so
  stdout was never attached to a pipe, no EPIPE was possible, and the pre-fix
  binary could not panic. Both looked identically silent, which reads as "no
  difference, fine".
COST: Would have certified the fix on a probe that could not fail, in the same
  session that ran the pre-fix binary and saw exit 101 ten minutes earlier. Caught
  only because "0 bytes of panic noise BEFORE the fix" contradicted a measurement
  already in hand.
FIX: Capture stderr to a FILE and leave stdout on the pipe
  (`cmd 2>err.txt | head`). Generally: when a probe reports no difference between
  a known-broken and a known-fixed artifact, the probe is the candidate before the
  conclusion is. This is the "loud wrong probe" from ethos rule 7 — it answered,
  and its answer was agreeable.

## Five finished cards sat in `todo` and kept being auto-picked
AREA: board
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux-rust
CARD: AMUX-2674
SYMPTOM: Auto-pickup handed me AMUX-2672 with "32 more queued". Five of those 32
  (AMUX-2599, 2609, 2618, 2634, 2636) were all fixed by ONE commit — e679bdb, whose
  subject literally reads "five carded defects — watchdog, the 404 trio, OSC-8,
  pane shrink, custom columns" and whose body names each card id. Their descs
  already said "DONE" and named a single remaining step (`git add`), which a later
  commit had done. Nothing moved the cards.
COST: The queue overstated real work by ~16% and auto-pickup kept offering finished
  cards, each costing a full scope-and-decide cycle to rediscover. Worse for
  anyone reading the board to see what is left: five defects looked open that were
  live in production.
FIX: The commit body already names the card ids in a machine-readable form. Nothing
  reads them. A commit trailer or body scan that flags "card named in a merged
  commit but still in todo" would have surfaced all five in one query — the data
  was there and unread, which is the same shape as AC-323's ignored_fields. Note
  the honest limit: a named card is not proof of completion, so this should
  SURFACE candidates for a human/agent check, never auto-close (ethos rule 8).

---
## `run_scheduler` had zero call sites, so arming AMUX_RS_SCHEDULER=1 started nothing
AREA: scheduler
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-10
SESSION: amux-rust (AMUX-2647 lane)
CARD: AMUX-2647
SYMPTOM: `grep -rn run_scheduler crates/` returned the definition, two doc mentions and
  its own re-export — and no caller. The loop was documented, gated, tested and never
  spawned. The live evidence: the last `cron` run row on the fleet is 19:41:11, the
  minute the python server stopped, and six enabled schedules were overdue by up to
  3h09m with nothing in any log saying so. Nothing errored, because the failure is pure
  absence — there is no log line for a loop that was never started.
COST: every schedule on the fleet was dead from the python cutover until this was found
  (~3h20m at discovery), including the frustrations sweep and the MVS reliability
  monitors. The gate made it worse than a plain omission: the owner ARMED
  `AMUX_RS_SCHEDULER=1` believing that turned firing on, so the one action that looked
  like the fix confirmed a capability that did not exist.
FIX: spawned in lib.rs alongside the other runtime jobs (ghost_rescue, board_drive,
  pane_size), which is where a reader looks for "what loops run". Same class as
  AMUX-2637 (the board drive loop, also left unspawned by the cutover) — that is two
  instances, so the general fix is a boot-time assertion that every documented runtime
  job has a live task, not a third careful reading of lib.rs.

---
## Run-now recorded `status:"ok"` for a delivery it knew it had not performed
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-10
SESSION: amux-rust (AMUX-2647 lane)
CARD: AMUX-2647
SYMPTOM: `POST /api/schedules/<id>/run` answered `{"ok":true,"ran":"<title>","status":"ok","note":""}`
  and wrote a `schedule_runs` row with `status='ok'`, note `manual run recorded by rust
  scheduler`. The endpoint's own comment said "the run row records the trigger, not a
  delivery". The dashboard rendered exactly what it was told: a green `Ran · no output`.
  Two such rows sit at 22:56:38 and 22:57:11 — the owner pressing the button twice.
COST: the owner's report ("i clicked run now it says ran, no output") and the whole
  investigation behind it. Worse, the run history was unusable as evidence for the
  outage above: 161 rows in 24h all reading `ok` is what a healthy fleet looks like, so
  the one instrument that could have shown schedules were dead asserted they were fine.
FIX: `RunOutcome` is now a type whose only `ok`-producing variant is `ShellOk`, which
  can only be built from a finished subprocess — a tmux schedule that was not delivered
  has no representable way to become `ok`. `schedule_runs` carries `delivery` and
  `submission` (migration 0015) and the response says which of delivered/queued/refused/
  error happened. The lesson that generalises: the honest comment was already written,
  next to the code that contradicted it, and nobody diffed the two. A comment admitting
  a lie does not stop the lie being recorded — only a type can.

---
## A peer's `git add` swept my uncommitted migration into their commit and it applied to the live DB
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux-rust (AMUX-2647 lane)
CARD: AMUX-2647
SYMPTOM: I wrote `migrations/0015_schedule_run_delivery.sql` and registered it in
  `migrate.rs`, uncommitted, under an explicit instruction never to commit. Commit
  4d76ff3 ("feat: universal FTS5 search …") picked up my `migrate.rs` edit; the .sql
  file was still untracked, so a clean checkout could not compile (`include_str!`
  resolves at build time), and 6689a74 then tracked my file to repair the dangling
  reference. The auto-builder shipped it and the live server applied 0015 to
  `~/.amux/amux.db` at 03:22:43 — schema I authored, live, hours before the code that
  writes those columns exists anywhere but my working tree.
COST: no damage — the columns are additive and NULL reads as "not recorded" — but the
  live DB now has two columns nothing populates, and neither author chose that. The
  deploy path is committed-HEAD-only *precisely* so half-finished work cannot ship;
  a broad `git add` in a shared checkout defeats it, and the second author was doing
  the right thing (repairing a dangling reference) with no way to know the file was
  mid-flight. The existing rule covers the direction "check what you are pushing that
  is not yours"; this is the mirror, and no check catches it.
FIX: the pre-commit guard should refuse a `git add` that stages files no lane has
  claimed — or, cheaper, `prepare-commit-msg` already stamps `Amux-Session`, so warn
  when a commit's file set spans more than one lane's recent edits. Until then: write
  new files outside the repo until the change is ready, which is what I should have
  done here.

---
## Booting a second amux-server to test something drives the PRODUCTION tmux fleet
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-10
SESSION: autofix (subagent)
CARD: AF-69 (investigation, signed off) + AMUX-3221 (the FIX, open)
SYMPTOM: Started an isolated server (`AMUX_HOME=/tmp/amux-af-home`, port 8899, own DB) to
  verify a change without touching the fleet. Within 4 seconds its log showed:
    pane-size: restoring detached window ... session=amux-amux from=220x50 to=220x50
    pane-size: restoring detached window ... session=amux-mixpeek-autopilot ...
    pane-size: one-shot repair complete count=3 sessions=["amux-amux", ...]
  `pane_size::spawn()` takes no state and enumerates tmux DIRECTLY, so AMUX_HOME does not
  scope it. `ghost_rescue` is the same shape and it SUBMITS STUCK MESSAGES — i.e. a test
  instance can press Enter in a production lane's pane. Neither has an off switch;
  `commit_nudge` and `board_drive` both do (`AMUX_*_SECS=0`).
COST: Killed the instance and rebuilt the whole live verification as in-process router
  tests instead. This time the resize was a no-op (220x50 -> 220x50) so nothing was lost,
  but that is luck: a peer is running `/tmp/amux-sched-target/debug/amux-server` on this
  same box right now, and the repo's own docs tell you to build to a private target dir
  and run it.
FIX: STILL OPEN — the hazard is live. AF-69 (the INVESTIGATION) was signed off by amux
  2026-08-16; the FIX is AMUX-3221 and has not been started. Signing off an investigation
  is not the same as fixing the thing, and this entry stays until AMUX-3221 lands.
  CONFIRMED STILL BROKEN 2026-08-16: pane_size and ghost_rescue have NO env knob;
  commit_nudge (AMUX_COMMIT_NUDGE_SECS) and board_drive (AMUX_BOARD_DRIVE_SECS) do. No
  global isolation guard exists (grepped AMUX_NO_FLEET / AMUX_ISOLATED / is_isolated /
  AMUX_TMUX_READONLY — none).
  THE ENTRY'S OWN PROPOSED FIX IS INCOMPLETE, measured not assumed: adding the knob at the
  top of `pane_size::spawn` covers only its one-shot `sweep(true)`; the SAME function then
  calls `super::spawn_periodic("pane_size", TICK_SECS, ..)`, which keeps sweeping the fleet.
  A per-job knob there looks done and is not. That half-fix is stashed, not committed
  ("AF-69: incomplete pane_size guard").
  CORRECT SEAM (amux verified it): `runtime_jobs/mod.rs:128 spawn_periodic_every` is the
  ONLY constructor of a PeriodicTask — its own comment already leans on that to guarantee
  every job appears in the registry — so a knob there, derived from the job name
  (pane_size -> AMUX_PANE_SIZE_SECS, ghost-rescue -> AMUX_GHOST_RESCUE_SECS), gives every
  periodic job a disable for free, including ones written later. Requires a test proving a
  0 knob stops the sweep while a normal value still ticks, and that a disabled job stays
  REGISTERED (inert, not invisible) so it does not become a silent skip.

## A correct refusal shipped as HTTP 500 for months, and poisoned the error sweep
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-10
SESSION: autofix (subagent)
CARD: AMUX-2678
SYMPTOM: `POST /api/sessions/<n>/send` answered 500 for every pane-state refusal, because
  `send_post` mapped exactly one string ("not running") to 409 and everything else to 500.
  15 of the 19 errors in the live 6h window were one refusal wearing a 500.
COST: Beyond the noisy sweep: this is what an autofix loop would have spent lanes on. The
  first detector I wrote would have filed a card for a subsystem that was working
  correctly, which is how an automated filer teaches people to stop reading the board.
FIX: `send_failure_status()` classifies the outcome (409/404/400/501, 500 only for
  unhandled), used by send + archive/wake/reset, with a `fix` hint in the body. Uncommitted
  in the working tree at time of writing; see the autofix handoff report.

## The per-agent CARGO_TARGET_DIR convention has no GC — 37 caches filled the disk
AREA: environment
SEVERITY: blocks
STATUS: open
DATE: 2026-08-10
SESSION: amux (subagent — legacy-port migration)
CARD: AMUX-2754
SYMPTOM: `cargo build -p amux-server` died with `failed to write ...: No space left on
  device (os error 28)`. The root volume had **609Mi free of 1.8Ti**. `du` on /tmp found
  **445GB across 37 `/tmp/amux-*-target` directories** — one per agent task, 15GB at the
  top end, 33 of them last written the previous day. Every task brief in this repo hands
  the agent its own `CARGO_TARGET_DIR=/tmp/amux-<task>-target`, and nothing ever removes
  one. The convention that keeps concurrent agents from contending is also, unmodified, a
  disk-fill schedule: ~12GB per rust task, times however many tasks the fleet runs.
COST: My gate (cargo test + clippy) was unrunnable. Worse than my task: the amux SQLite DB
  and `~/.amux/logs` are on this volume, so the whole fleet was one write from failure with
  no warning anywhere — `/health` reports `store:"ok"` and says nothing about the disk
  underneath it. I could not clean up safely either: the dir names are TASK-scoped, not
  session-scoped, so "does a live session own this cache?" is unanswerable — my own dir
  (`amux-port-target`) looks orphaned by every test I could write. Escalated to the owner
  because deciding which 445GB of other agents' caches to destroy is not an agent's call
  (ethos rule 8).
FIX: Two halves, neither done. (1) `/health` should report free space on the volume holding
  `~/.amux`, so disk pressure is visible where every session already looks instead of
  arriving as a build error in whoever happens to compile next. (2) The convention needs a
  reaper: either name the dir after `$AMUX_SESSION` (so ownership is decidable and a
  session reuses one cache across tasks instead of minting one per task), or a scheduler
  entry that removes `/tmp/amux-*-target` untouched for >24h. Naming it after the session
  is the better half — it makes the cleanup question answerable at all, which is the part
  that blocked me.

---
## The schedule audit trail is routed, implemented, and reachable from no control
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-10
SESSION: amux (sched2 lane)
CARD: AMUX-2755
SYMPTOM: `GET /api/schedules/audit` works and is good — it is the only way to answer
  "who disabled this schedule / why did it not run at 9". Zero of the twelve
  `/api/schedules` call sites in `app.js` hit it. Its own discoverability mechanism is
  a response HEADER (`x-amux-audit`), which a dashboard user never sees.
COST: none yet this session; logged because AMUX-2416 already established that an
  audit nobody can find is the same failure as no audit, and this is that shape again
  one endpoint over.
FIX: an "audit" affordance on the schedule card's expanded view, reusing the existing
  endpoint. Small; carded rather than folded into an unrelated change.

---
## A peer's `git add` swept my UNCOMMITTED work into their commit and pushed half of it
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux (sched2 lane)
CARD: AMUX-2757
SYMPTOM: mid-task, `git diff` stopped showing my `scheduler.rs` and `app.js` edits.
  They were not lost — commit `9a91945` (another lane's autofix work) had staged them
  with a broad `git add` and pushed. So `pub fn skip_next_run` and the dashboard's
  rewritten Skip button are on `origin/main` right now, while the `api/schedules.rs`
  half that MOUNTS the route is still uncommitted: a function upstream with zero call
  sites, which is the exact ethos-rule-1 shape the work was fixing.
COST: no work lost, but ~10 minutes establishing what was where, and a genuinely
  misleading upstream state — the dead controls are gone from the UI while the API
  still silently accepts the dead fields, so the defect is now invisible from the
  dashboard rather than fixed. Anyone reading origin/main would call it done.
FIX: CLAUDE.md's Deploy section documents this hazard in the OTHER direction (your
  unpushed commit riding out on a peer's push). The mirror case deserves the same
  billing: on a shared checkout `git add -A` / `git commit -a` stages other lanes'
  live edits, and a lane cannot tell from its own session that it happened. Stage by
  explicit path, and check `git status` for files you did not touch before committing.

## The shared-checkout sweep shipped a BROKEN BUILD, because the swept work included a new untracked file
AREA: git
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux (subagent — legacy-port migration)
CARD: AMUX-2769
SYMPTOM: Not a new frustration — see the four existing entries on `git add` sweeping a peer's
  uncommitted hunk (AMUX-2443, now `done`, plus lines 117 / 292 / 548). This is a DISTINCT
  consequence worth its own row. My in-flight edits to `lib.rs`, `config.rs`,
  `session_verbs.rs`, `api/mod.rs` and `api/request_log.rs` were swept into 9a91945 and
  4ac14b9 by another lane — but the module those edits CALL, `legacy_port.rs`, was a NEW
  file and therefore untracked, so `git add <tracked paths>` could not pick it up. main was
  left referencing a module that did not exist: unbuildable for 3 minutes until c3f5e0f
  ("commit legacy_port + canonical_port, whose callers were already on main") patched it.
  Both commits are already pushed to origin.
COST: Small here (a peer noticed and fixed it in 3 minutes, and their commit message shows
  they diagnosed it correctly). The reason to log it is that it inverts the usual
  mitigation: the standing advice for the sweep is "stage narrowly, by path", and staging
  narrowly is EXACTLY what produced an unbuildable main. A sweep that takes everything
  would at least have been self-consistent. It also means my work reached origin without
  me, while I was under an explicit instruction not to commit or push — so "I did not
  push" is not the same as "my work did not ship".
FIX: The existing guard checks for a peer's modified files in the index. It should also
  refuse when the staged set REFERENCES an untracked file in the same crate (a `mod X;` or
  `crate::X` naming a path that is untracked) — cheap to detect and it is the difference
  between shipping a peer's diff and shipping a build break. Cheaper interim: make the
  auto-build service's failure page name the untracked file, since "cannot find module
  legacy_port" is currently only visible to whoever next compiles.

## CLAUDE.md told every session to make its own 15GB cargo target dir, and the disk filled
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-10
SESSION: storage-audit
CARD: AMUX-2700
SYMPTOM: The volume reached 741MB free of 1.8TB and transcript writes started failing with
  ENOSPC while a 50-session fleet was running. The dominant consumer was ~37 per-agent cargo
  target dirs under `/private/tmp/amux-*target`, 10-15GB each, ~450GB total. They existed
  because CLAUDE.md's Workflow section said, in as many words: "Use a scratch
  `CARGO_TARGET_DIR` (e.g. `/tmp/amux-target`) so parallel sessions don't thrash one lock."
  Every session followed the instruction correctly.
COST: A full disk on the machine that runs the fleet, an owner alert at ~13:00, and roughly
  an hour of three sessions' time (parent + this one) spent measuring and deleting. Worse
  than the deletion time: the first 450GB of deletions freed only ~8GB, so the obvious
  reading was "we deleted the wrong things" and the natural next action was to delete more —
  see the sibling entry on Time Machine snapshots.
FIX: Fixed in this session. The lock the instruction was avoiding is nearly free: measured
  here, one incremental rebuild is 1.48s and two concurrent ones against the SAME dir finish
  in 1.65s (1.11x) — cargo's build lock makes the second builder wait and then find the work
  already done. Because every session builds the same shared checkout, one target dir is a
  warm cache sessions hand each other, while per-session dirs paid 15GB AND a full rebuild
  each to avoid a second of waiting. CLAUDE.md now says `CARGO_TARGET_DIR=~/.amux/rust-build-target`
  and explicitly says never a per-session dir. The generalisable half: this is ethos rule 7's
  "the sanctioned instruction itself can be the theatre" applied to a resource rather than a
  command — an instruction that is cheap per session and ruinous per fleet reads as correct
  to every individual session that follows it, and none of them can see the aggregate.

## Deleting 450GB freed 8GB, because hourly Time Machine snapshots pin every deleted block
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-10
SESSION: storage-audit
CARD: AMUX-2701
SYMPTOM: With the volume at 741MB free, ~450GB of stale cargo target dirs was deleted and
  `df` moved to 9.0GB free — about 8GB recovered from 450GB deleted. Deleting a further
  26.8GB moved free space DOWN (8.1Gi -> 6.6Gi). The cause was 24 hourly APFS local Time
  Machine snapshots spanning 2026-08-09 13:18 to 2026-08-10 12:18: a snapshot pins the
  blocks of every file deleted after it was taken, so deletion frees nothing until the
  snapshots age out (24h) or are thinned. They had accumulated because the Time Machine
  destination ("My Book") is not connected, so nothing ever thinned them. macOS eventually
  purged all 24 on its own under pressure and free space jumped to 418Gi.
COST: A wrong conclusion that was already corroborated: two sessions independently read
  "deleted a lot, freed nothing" as "we deleted the wrong things", whose remedy is deleting
  MORE — the one action that could not work. It also produced an owner alert asking for a
  root password (`sudo tmutil thinlocalsnapshots`) that turned out not to be needed, which
  is a fire alarm spent on a self-resolving condition.
FIX: Partly fixed: the new autofix `disk` detector puts `tmutil listlocalsnapshots / | wc -l`
  in the card's evidence with an explicit "READ THIS BEFORE DELETING ANYTHING" note, so the
  next session sees the discriminator in the place it is already looking rather than having
  to know APFS semantics. Still open: nothing warns that the TM destination has been absent
  for long enough to accumulate a full day of local snapshots, which is the actual upstream
  condition and is invisible until it interacts with a disk-full event.

## `cargo test` cannot pass under the target-dir convention the repo itself mandates
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux
CARD: AMUX-2799
SYMPTOM: `db::migrate::guard_tests::it_actually_refuses_a_pending_migration_against_the_live_db`
  fails with "precondition: the test binary should live under a cargo target dir, got
  /Users/ethan/.amux/rust-build-target/debug/deps/amux_server-...". The predicate is
  `s.contains("/target/debug/") || s.contains("/target/release/")` — but per-agent target
  dirs are BANNED (they filled the disk on 2026-08-09) and both the task instructions and
  scripts/rust-auto-build.sh use `CARGO_TARGET_DIR=$HOME/.amux/rust-build-target`, whose
  path contains `/rust-build-target/debug/`, not `/target/debug/`. So the sanctioned way to
  run the suite is the one way this test cannot pass. Verified at the unmodified base commit
  86d3353, so it is not caused by any working-tree change.
COST: A red suite that every lane must learn to ignore, which is the state in which a REAL
  regression gets waved through. It also cost a false green in the other direction: the
  background run was invoked as `cargo test > log; echo $?; tail -25`, so the reported exit
  code was `tail`'s (0) while the log said FAILED — a full suite was nearly reported green.
FIX: Not applied — migrate.rs has another lane's uncommitted work in it and this is not my
  file to tangle with. One line: accept a path under the configured target dir, e.g. also
  match `std::env::var("CARGO_TARGET_DIR")` as a prefix, or match `/debug/deps/` generally.
  The second half generalises past this bug: **when you redirect a command's output to a
  log, `$?` is the exit code of the LAST command in the pipeline, not the one you care
  about.** Capture it immediately after the command, or the "did it pass?" check reports on
  `tail`.

## Another lane's `git add` swept my uncommitted AC-322 fix into their commit, under their message
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux (subagent, AC-322)
CARD: AC-322
SYMPTOM: Fourth instance of the cluster already recorded above (the migration sweep, the
  "swept my UNCOMMITTED work and pushed half of it" entry, and the "consumes PEERS' staged
  files silently" entry). Logging it only because the COUNT is the argument: this is now
  four independent sessions hitting the identical shape, which is what turns it from
  bad luck into a design fact about a shared checkout.
  My board.rs `actor_from_headers` fix and both AC-322 regression tests
  (`force_accepts_x_amux_worker_attribution_like_every_other_module`,
  `cross_lane_archive_guard_sees_x_amux_worker_callers`) were uncommitted in the working
  tree. They are now in f36d407, "fix(build+ui): title_needs_self_description, clear-done
  honesty, and the legacy-port shell injection", which mentions neither AC-322 nor the
  attribution header. I never ran `git commit`; I was explicitly instructed not to.
COST: No work lost this time, but the audit trail is now wrong in a way nobody can see from
  the log: a security-adjacent change (the cross-lane ARCHIVE guard had been open to every
  bash-CLI caller) is recorded under a commit message about a build break and a UI fix.
  Anyone bisecting for when the archive guard started working will not find it by message.
  The reviewer-of-record for that hunk is also wrong.
FIX: Same as the three entries above — this is not fixable by being careful, because the
  sweeping session cannot see whose hunks it is staging. `git add <specific paths>` is the
  mitigation everyone already knows and it failed four times; the durable fix is a
  pre-commit guard that refuses to stage hunks in files another session has open, or
  per-session worktrees so `git add -A` is scoped by construction. Until then, treat
  "my change is uncommitted" as "my change may ship under someone else's message at a
  time I do not choose" (CLAUDE.md already says this; the entry is the evidence).

## The shared cargo target dir served a stale rlib, so `cargo test` blamed three innocent files
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: claude (AMUX-2619/2780 lane)
CARD: AMUX-2799
SYMPTOM: With the now-mandated `CARGO_TARGET_DIR=~/.amux/rust-build-target` (e188b0e, "ONE
  shared cargo target"), `cargo test -p amux-server` reported, in sequence, three DIFFERENT
  compile errors in files I had never touched: `unresolved import
  amux_server::runtime_jobs::registry`, `cannot find function title_needs_self_description
  in module amux_core::board`, and a `migrate.rs` precondition panic naming the shared
  target path. All three sources were byte-correct — I verified `pub mod registry;` with
  `od -c`. The actual cause: the cached `libamux_server-*.rlib` was built from an older
  tree. `strings` on it showed 6108 hits for `runtime_jobs..autofix` and ZERO for
  `registry` and `storage`, the two newest modules, while the same rlib's own crate
  compiled fine and lib.rs line 210 uses `runtime_jobs::registry`. Cargo's mtime
  fingerprint never noticed, because mod.rs (13:24) was older than the rlib (14:27).
COST: ~40 minutes, and three wrong conclusions I came close to reporting — twice I
  concluded "another lane's uncommitted work has broken main" and started to write it up,
  and once I concluded a committed test was broken under the mandated target dir. Every one
  of those would have sent a peer to debug correct code. `cargo clean -p amux-server`
  removed 48,516 files / 28.9GiB and fixed it for one invocation before it recurred;
  `touch crates/amux-server/src/runtime_jobs/mod.rs` is what actually forced the rebuild.
FIX: The failure mode is specific and cheap to detect: an rlib that does not export a
  module its own crate source declares. A preflight in the test gate — compare `pub mod`
  lines in each `mod.rs` against the built rlib, or simply `cargo build -p amux-server --lib`
  and fail loudly if it is a no-op while sources are newer — would turn 40 minutes of
  blaming peers into one line of output. Until then the recipe is: when `cargo test` names
  a symbol you can see in the source with your own eyes, suspect the ARTIFACT before the
  code, and `touch` the `mod.rs` that declares it. Related to the shared-checkout cluster
  above: same root (one resource, many lanes), different resource (build artifacts, not
  the git index).

## `cargo check --workspace` in the pre-commit hook cannot tell MY broken change from a PEER's
AREA: gates
SEVERITY: blocks
STATUS: open
DATE: 2026-08-10
SESSION: amux
CARD: AMUX-2777
SYMPTOM: The shared checkout broke the workspace FOUR times in ~40 minutes from at least three
  lanes: a `steer_enqueue` arity change mid-refactor (mine), `DetectorKind::CiFailure` non-exhaustive
  match, `note_quiet_signatures` arity, and `amux_core::board::title_needs_self_description` missing
  for orchestrator/runtime.rs:1288. Every one of them blocked EVERY lane's commits, because the hook
  checks the WORKING TREE — which on a shared checkout contains everyone's in-flight edits, not the
  change being committed.
COST: amux-cloud's AC-335 bounced twice on other lanes' compile errors. I lost ~25 minutes to two
  breaks that were not mine, and inflicted one on them. The gate's verdict carries no information
  about the commit it is gating.
FIX: check the STAGED state, not the working tree — `git write-tree` + `git archive` into a temp dir
  is read-only w.r.t. the shared checkout, so it is safe to do under other lanes' edits. Cost is a
  colder build per commit, which is the trade to price. Anything short of this keeps conflating
  "your change is broken" with "someone else is mid-sentence".

## `cargo test` was green while `cargo check` was green — and the compiled binary lacked my tests
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux
CARD: AMUX-2777
SYMPTOM: `cargo test -p amux-server --lib the_three_stalled_lanes` printed
  `test result: ok. 0 passed; 0 failed; 752 filtered out` — twice, after a 31s build, with the same
  binary hash. The tests were on disk (grep confirmed), in a plain `#[cfg(test)]` module whose OTHER
  five tests were listed by `--list`. The full run minutes earlier reported 781 passed / 787 total;
  `--list` then reported 751. The artifact was stale under heavy shared-CARGO_TARGET_DIR contention.
COST: ~15 minutes, and it is the LOUD-WRONG probe shape: it exits 0 and says `ok`. A filter that
  matches nothing is indistinguishable from a suite that passes, so the natural next move is to
  believe the code is fine. Had I been verifying someone else's fix I would have reported it working.
FIX: `0 passed AND 0 filtered-in` should never render as `ok` — but that is upstream. Locally: when
  a name filter matches zero tests, treat it as a FAILED probe and re-run against `--list` before
  concluding anything. Same family as the empty-grep rule in ethos.md rule 7.

## A peer's `commit -a` swept my uncommitted work into their commit — twice, in both directions
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux
CARD: AMUX-2807
SYMPTOM: My entire AMUX-2785 steering fix landed inside 70dc3a8 "fix(build): commit registry + the
  visibility changes its callers on main already reference" — a commit I did not write. I found out
  only because `git status --short` came back EMPTY on four files I had edited minutes earlier, which
  reads as "my work is gone" before it reads as "someone committed it". Symmetrically, amux-cloud's
  AC-335 board_store fix went out inside my e188b0e storage commit. The staged-guard was DOWN for
  both ("NOT ENFORCED — could not reach the amux server", 8822 then 8824, timing out).
COST: No code lost either time, but each of us had to verify piece-by-piece that our own work had
  landed intact rather than partially. Commit messages now describe work they do not contain, so the
  archaeology is wrong for anyone who reads git log later — including the `Amux-Session` trailer,
  which attributes my change to a commit stamped for a different piece of work.
FIX: two halves. (1) The staged-guard must not fail open silently — AMUX-2807; if it cannot reach the
  server, the unguarded commit should at least be COUNTED durably, or the guard is decoration exactly
  when it matters. (2) `commit -a` on a shared checkout is the hazard itself; the guard should refuse
  it, not merely warn, when the staged set spans files the committing session never touched.

## Browser API drove the user's live Chrome and said ok:true for every keystroke
AREA: browser
SEVERITY: blocks
STATUS: open
DATE: 2026-08-10
SESSION: amux-cloud
CARD: AC-336
SYMPTOM: `POST /api/browser/start {"profile":"default","url":"https://cloud.amux.io/sign-in"}` returned
  {ok:true, pid:90649, cdp_port:65059}. That pid was already dead: 10 Chrome processes share
  user-data-dir=/Users/ethan/.amux/playwright-auth/profile and hold SingletonLock, so Chrome's
  singleton handoff exits the new process and reuses the running one — the user's own browser.
  `GET /api/browser/status` then reported running:true on a DIFFERENT port (65140) listing the user's
  tabs, and `POST /api/browser/action` eval returned location.href =
  http://localhost:4177/solutions/creative-dna. `type` returned ok:true at every step. The endpoint's
  own hint says "They never attach to a browser this server did not launch", which is precisely what
  it did.
COST: I typed AMUX_GODMODE_PASSWORD and pressed Enter into the user's live Chrome believing I was
  driving an amux-owned browser. The frontmost page had no text inputs so it almost certainly went
  nowhere, but a god-mode credential now needs rotating on "almost certainly". Roughly 40 minutes lost,
  and the god-mode UI verification (AC-332) is still not done because the subsystem cannot be trusted
  to target the browser it says it launched.
FIX: start must confirm the pid it returns is alive AND that its own cdp_port answers, failing loudly
  when the singleton hands off — returning a dead pid as ok:true is the primary defect. status/action
  must bind to the port start launched and refuse any other. Default to a per-session profile dir so
  two sessions cannot contend for one lock. Control proving the diagnosis: an isolated profile
  (its own user_data_dir) survives, its CDP answers, and eval sees the URL actually requested.

## A guard on status.running cannot catch this, because status is the thing that lies
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux-cloud
CARD: AC-336
SYMPTOM: After discovering the fallback above I added a liveness assertion that refused to act unless
  `GET /api/browser/status` reported running:true. It passed, and the very next eval still executed
  against the user's Chrome at localhost:4177. The guard could not fail: it consulted the same
  component that had already substituted a different browser.
COST: One wasted round of "now it is safe" — I ran a second credential sequence behind a guard that
  was structurally incapable of detecting the failure it was written for. What actually caught it was
  a cheap independent probe: eval `location.href` and compare against the URL I had asked start for.
FIX: Verify from a source that is not the suspect component. For this API the discriminating check is
  two lines — ask the launched cdp_port for /json/list, ask /action what location.href is, and require
  they agree. Generally: a guard that reads the lying instrument inherits the lie (ethos rule 7).

## Shared checkout swept my board_store fix and its test into another lane's commit
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-10
SESSION: amux-cloud
CARD: AMUX-2807
SYMPTOM: AC-335 (depends_on_cycle scoped to cycles containing self_id) plus its falsifiable test
  landed inside e188b0e, amux's "feat(storage): retention on seven unbounded tables..." commit, which
  is unrelated to the board. `git status` came back clean on a file I had just edited. Found only by
  `git log -S "pre-existing depends_on cycle elsewhere"`. amux reports the mirror the same day: their
  AMUX-2785 steering fix went into 70dc3a8, a commit they did not write.
COST: Not lost work — fix and test both intact — but the commit carries a change its author cannot
  explain, and they will be the one asked about it. Two sweeps in opposite directions in one day.
  My AC-335 also bounced twice on other lanes' compile errors before landing, since the pre-commit
  hook runs cargo check --workspace against a tree four lanes are editing.
FIX: The staged-guard is the designed prevention and it was DOWN for both events — it printed
  "NOT ENFORCED — could not reach the amux server" against 8822, then 8824, timing out both times,
  so cross-session sweep protection was off exactly when four lanes were committing into one tree.
  Being loud about it is right; being down is the hazard it exists for. See AMUX-2807.

## The server went silent 15s after install: no panic, no log, listening socket held
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-11
SESSION: amux
CARD: AMUX-35
SYMPTOM: Fresh `./install.sh` on this machine came up healthy, answered `/health` twice,
  then stopped answering ANYTHING — `/health`, `/api/board`, the dashboard, even the
  plain-HTTP redirect that is handled before TLS. The process stayed alive, kept its
  listening socket, sat at 0% CPU with every thread parked, and wrote NOTHING further to
  server-rs.log. `curl` hung mid-TLS-handshake because the kernel completes the TCP
  handshake from the listen backlog while nothing ever calls accept(). Root cause: the
  disk-pressure autofix detector shells out to `du` with blocking
  `std::process::Command::output()` and no timeout, on a tokio worker; on a 96%-full disk
  `du -skx ~/Library/Caches` runs for minutes, so each tick parked another worker until
  none were left to poll the accept loop.
COST: ~90 minutes, almost entirely spent on the instrument rather than the bug. Four
  hypotheses died first: a leaked semaphore permit, a head-of-line block in
  RedirectingAcceptor's peek, self-adoption exiting, and `server.env` (which "confirmed"
  itself, then un-confirmed when the no-file control ALSO failed — the earlier survival
  was timing luck, not configuration). A fault that emits no panic, no log and no CPU is
  indistinguishable from a network problem, and every cheap probe returns silence, which
  reads as "nothing wrong here". The discriminator was free and I reached it late:
  `ps -eo pid,ppid` on the wedged process showed one child, `/usr/bin/du -skx
  ~/Library/Caches`, 1m40s old. Compounding it, the release binary is unsymbolicated, so
  `sample` printed `???` for every amux frame until I rebuilt with debug info.
FIX: bound every `du` with per-path and total wall-clock budgets, kill AND reap on
  timeout (an unreaped `du` holds the FDs the neighbouring FdPressure detector counts),
  and OMIT a timed-out path instead of sizing it 0 — a silent zero sorts the largest
  consumer last and aims the report at an innocent directory. The incomplete ranking now
  says so in the log (rule 4). The deeper fix this entry is really arguing for: a
  detector that only runs when the resource is already scarce must be bounded BY
  CONSTRUCTION, and the whole sync detector sweep still runs on the async runtime holding
  a store connection — `tmutil` and the build detector's git calls are the same shape,
  bounded only by luck. Moving that sweep to `spawn_blocking` is the real exit.

## A `Map` and a `Value` index identically and behave oppositely, so the sweep panicked on every pre-cutover lane
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-11
SESSION: amux
CARD: AMUX-35
SYMPTOM: First boot after the Python->Rust cutover: `thread 'tokio-rt-worker' panicked at
  session_verbs.rs:6637: no entry found for key`, once per tick, killing the rate-limit
  sweep for the whole fleet. `load_meta()` returns a `serde_json::Map`, and
  `Map["missing"]` forwards to `BTreeMap::index`, which PANICS — while the visually
  identical `Value["missing"]` yields `Null`. No pre-cutover `*.meta.json` carries
  `rate_limited_since`, so this fired on all 5 lanes, guaranteed, on every machine
  migrating from Python.
COST: ~15 minutes, and it bought a wrong conclusion first: the panic is caught and logged
  as a WARN, so it looked like the cause of the server hang it merely coincided with.
  Fixing it correctly (verified: the panic stopped) left the server still hanging, which
  is the only reason I kept looking.
FIX: added `meta_i64()` next to the existing `meta_str()` so the safe form is the obvious
  one, with a `#[should_panic]` test pinning the `Map`-vs-`Value` trap so nobody
  "simplifies" it back to indexing. The general shape worth remembering: two types whose
  index syntax is identical and whose missing-key behaviour is opposite will be confused
  again, and no amount of care at the call site prevents it — only removing the sharp
  form from reach does.
## A probe read a hook file that git never executes, and a correct measurement certified the wrong conclusion
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-11
SESSION: amux
CARD: AMUX-2841
SYMPTOM: Retracting a peer's report of a tree-wide mtime restamp, I grepped
  .git/hooks/pre-commit on amux and mixpeek for `git stash`, found none, and wrote
  "the mechanism does not exist" onto MI-4650. Three independent reasons it could not
  work: the stash is done by the pre-commit FRAMEWORK wrapping the hooks; it is
  spelled diff-index + `checkout -- .` + apply, never `git stash`; and mixpeek sets
  core.hooksPath=.githooks, so the file I opened is DEAD — git never runs it.
COST: A wrong retraction published onto another session's card, contradicting a
  correct report from creative-dna. Two peers spent turns re-establishing a fact that
  was already established.
FIX: The generalisable half is the CORROBORATION, not the bad grep. I confirmed the
  retraction by watching a file's mtime across a real commit and seeing it unchanged —
  true, and worthless, because I ran it in the amux tree, which has no
  .pre-commit-config.yaml and never invokes the framework. A correct measurement in
  the wrong scope arrives as EVIDENCE rather than as reasoning, and evidence is harder
  to doubt because you can point at it. Nothing felt like the moment to recheck.
  Wanted: before believing a negative about a mechanism, confirm the probe ran where
  the mechanism could fire — for hooks specifically, resolve core.hooksPath first,
  because the file at the obvious path may not be the one that runs.

## A peer's commit swept my STAGED work into their commit, under their message
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-11
SESSION: amux
CARD: AMUX-2899
SYMPTOM: I ran `git add app.js sw.js && git commit -F -`. The commit returned
  "nothing to commit, working tree clean" — while the change was demonstrably
  live (APP_VER 0.9.591 serving). A peer had committed in the gap between my add
  and my commit, and their `381fb3c` ("feat(board-drive): auto-continue nudge for
  lanes with outstanding work") contains my 32-line scheduler-UI fix and my sw.js
  CACHE bump alongside their 169 lines of board_drive.rs.
COST: no code lost — I verified all five markers of my change are intact in HEAD
  and serving. The cost is the RECORD. This repo does constant archaeology; every
  fix cites a sha and CLAUDE.md's own recipes tell you to read `git log --grep`
  and `<sha>^` to find the pre-fix specimen. Anyone tracing why shell schedules
  render an owner chip now lands on a commit about an auto-continue nudge, by a
  different author, whose message says nothing about it. The AMUX-2899 card would
  be the only thread back.
FIX: none yet. CLAUDE.md documents the mirror of this ("a peer's commit can
  silently REVERT your uncommitted work", 2026-08-09, where staged DELETIONS were
  swept in) but not this direction, and the existing staged-guard fires on the
  COMMITTER — it warned me about their staged board_drive.rs — while the party
  who needs warning is the one whose work is about to be carried off.
  What would actually help: `git commit` with explicit paths is already the
  advice, and I did not follow it here (I used a bare `git commit -F -` after a
  targeted `git add`). A bare commit takes the whole index, and on a shared
  checkout the index is shared. The narrow rule is: on this checkout, ALWAYS
  `git commit -- <your paths>`, never rely on having staged only your own.
  I have used the path-scoped form elsewhere today; I did not here, and that is
  the difference.

## A dev server on the default AMUX_HOME silently clobbers the shared endpoint.json
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-12
SESSION: amux
CARD: AMUX-2971
SYMPTOM: I ran a throwaway amux-server on an alt PORT (18931) but the DEFAULT home (~/.amux) to read real message rows for a UI verification. On startup it published ~/.amux/endpoint.json pointing canonical_port at 18931. When I killed it, endpoint.json still named the dead port — so the pre-commit staged-guard (which resolves the server via endpoint.json, not AMUX_URL) could not reach a server and printed "staged-guard NOT ENFORCED" for the next commit. This affects EVERY session on this machine, not just mine: they all share ~/.amux/endpoint.json.
COST: One commit shipped with cross-session sweep protection OFF (recorded in staged-guard-unenforced.jsonl, so at least it was auditable). Restored by launchctl kickstart of the real server to republish. Any session that committed in the window between my dev server starting and the kick would have hit the same.
FIX: Two candidates, either or both: (1) publish_endpoint should NOT write the shared endpoint.json when the port is not the configured canonical AMUX_RS_PORT — a dev/alt-port instance is not the fleet's server and should not claim to be; gate the write on port==canonical. (2) the staged-guard's server resolution should prefer a liveness check on the canonical port and fall back rather than trusting a possibly-stale endpoint.json. The durable fix is (1): a non-canonical instance clobbering the canonical control file is the root. Until then: always give a dev server its own mktemp AMUX_HOME (my earlier 1892x runs did; this one did not, to get the live DB — that shortcut is the bug).

## legacy-port instrument reports CLEAR while 52 live sessions are stranded on the dead 8822
AREA: instruments
SEVERITY: slows
STATUS: open
DATE: 2026-08-12
SESSION: amux
CARD: AMUX-2988
SYMPTOM: Ethan intentionally dropped the 8822 compat bind 2026-08-11 (lib.rs:527, "no more 8822 just rust"). But 52 of 56 running claude procs still carry AMUX_URL=https://localhost:8822 in their process env, which cannot be rotated on a live process. Every documented `curl $AMUX_URL/api/...` recipe (peek, notes, email, schedules, calendar) returns 000 for those 52 lanes. GET /api/debug/legacy-port reports verdict "CLEAR: no traffic on the retired port", ready_to_retire=true, sessions_still_on_legacy=[] — the exact opposite of the truth — because it counts HITS and a port nothing listens on can record none. The one instrument meant to answer "who is still on 8822" is structurally blind to everyone who is.
COST: I burned several tool calls diagnosing why my own `curl $AMUX_URL` returned 000 and initially misread a deliberate owner decision as a fleet-down regression. Any of the 52 lanes following the CLAUDE.md/memory curl recipes silently fails the same way, and nothing surfaces that 52 lanes are running degraded — so no one recycles them. The `amux` CLI masks it (it uses AMUX_API=8824), which is why this went unnoticed.
FIX: (proposed, AMUX-2988) legacy-port accounting must not measure strandedness by inbound hits after the bind is gone — derive it by scanning running session process envs for a RETIRED_PORTS match (the /api/debug/tmux pattern: discovery from inside the server process), surface the count on /api/debug/legacy-port and an hourly WARN. Recycling the 52 is the owner's call (ethos rule 8, could interrupt in-flight customer work) — the fix only makes the count visible, it does not restart anything.

## Cloud silently froze behind a red main CI — "skipped" reads as "up to date," not "frozen"
AREA: cloud
SEVERITY: slows
STATUS: open
DATE: 2026-08-13
SESSION: amux-cloud
CARD: AC-344
SYMPTOM: Ethan reported "cloud is still behind in versions." A fresh cloud org still booted build 0f2f6e48 (pre-env_config: GET /api/env/schema -> 404, /api/env/apply absent from 213 routes), so the converged seed.py --via-apply 405'd against cloud. Root cause was three layers down: deploy-cloud.yml auto-deploy is gated on GREEN rust.yml (workflow_run), and main CI had been RED for hours on ONE clippy lint (unnecessary_sort_by, messages.rs:585). Every deploy-cloud run showed "skipped" — indistinguishable from "nothing to deploy." Nothing anywhere said "the cloud image is frozen and falling behind main because CI is red."
COST: Ethan had to notice the version lag by hand. Diagnosing it took several manual steps (fresh provision -> /health build hash -> /api/debug/routes -> gh run list conclusion -> git log timing) to join signals that no single instrument joins. And it is fleet-recurring: ANY lane's red-main break freezes the entire cloud deploy for every customer, invisibly, until a human notices — the busier the fleet, the more often it happens. PREDICTION PROVEN 2026-08-14 (author-verified during a frustrations validation): the "until a human notices" line came true VERBATIM, three times in ONE session, all AFTER this entry was written — 67b44f7 (clippy unnecessary_sort_by), 64fd450 (steering restart_persistence test), 9442f77 (opencode ETXTBSY flake + /api/tts unclaimed in the boundary registry). Each red-mained main, each made deploy-cloud SKIP silently, each froze :latest, and each was caught BY HAND via the freshness tick — never by any instrument. A prediction that recurred 3x on the record is the strongest possible argument for finally building the signal.
FIX: AC-344 — a signal that joins live-cloud-build-hash vs latest-green-main and fires when they diverge (commits or hours), OR make deploy-cloud's skip loud (record "skipped because CI red since <sha>/<time>"). Interim: clippy blocker fixed (67b44f7); steering-test blocker handed to amux; cloud auto-catches-up once CI green. Related: AMUX-3013 (pinned toolchain so local clippy == CI clippy — why the red wasn't caught pre-push).

---

## Docs say the 8822 bind is live and `:8822/health` proves it — but Ethan dropped the bind, so 48 lanes' raw `$AMUX_URL` curls silently fail
AREA: docs
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-13
SESSION: amux
CARD: AMUX-3046
SYMPTOM: Chasing a peer's :8822 observation, found the top-of-repo CLAUDE.md (L9-13, L236) still describes the 8822 compat-bind as ACTIVE and tells you to verify it with `curl -sk https://localhost:8822/health` — which now returns HTTP 000 (nothing listens). Ethan dropped the bind 2026-08-11 ("no more 8822 just rust", legacy_port.rs:340), and legacy_port.rs's OWN module doc (L5-46) still narrates the bind in the present tense, contradicting its L340. Ground truth via `GET /api/debug/legacy-port`: `enabled:false`, `stranded_count:48` — 48 running lanes (incl. this one) still carry `AMUX_URL=https://localhost:8822` in their PROCESS env, so every documented `curl -sk $AMUX_URL/...` recipe fails at connect for them. The `amux` CLI is unaffected because it HARDCODES `:8824` and ignores `$AMUX_URL` — which is the same literal-that-caused-this, one level over, and why the fleet looks healthy.
COST: a deep cross-file investigation to establish that a documented, "proven" verification step is dead; and 48 lanes are one hardcode away from re-stranding on the next port change. The friction is self-inflicted by the guidance: CLAUDE.md teaches `curl $AMUX_URL/...`, and `$AMUX_URL` is a process-env literal that cannot be rotated in a live lane. Instrumentation is COMPLETE (hourly WARN independent of the bind, verdict STRANDED, endpoint.json self-heals baked-in hooks) — the gap is that the human-facing docs contradict it, and there is no shared shell resolver so raw callers fall back to the stale literal.
FIX: VERIFIED FIXED by amux-frustrations 2026-08-16 — all three remedies this entry asked for have shipped: (1) CLAUDE.md L9-11 now states the bind is GONE and that :8822/health no longer answers; (2) legacy_port.rs's module doc opens "now that its bind is gone", past tense, reconciled with L340; (3) the shared resolver exists as `amux url`, backed by endpoint.json, and CLAUDE.md teaches `$(amux url)` in place of `curl $AMUX_URL/...`. Awaiting amux's sign-off before deletion. ORIGINAL: decision was Ethan's (ethos rule 8: recycling 48 lanes can interrupt in-flight work). Durable root fix surfaced by gtm-engine, who already ships `gtm/engine/amux_endpoint.py::amux_base()`: read the server-written `~/.amux/endpoint.json` (`canonical_url` + `retired_ports`), refuse a retired port even when `$AMUX_URL` names it, liveness-probe before returning. Every script in that lane imports it, which is why its raw curls kept working today. The fleet fix is a shared resolver (a `amux url` subcommand backed by endpoint.json, taught in CLAUDE.md in place of `curl $AMUX_URL/...`) — NOT 48 hardcoded `:8824` literals, which re-teach the exact bug. Interim workaround: use `amux ...` subcommands or `https://localhost:8824` explicitly. Also owed regardless: reconcile CLAUDE.md L9-13/L236 and legacy_port.rs L5-46 to the dropped-bind reality.

---

## A page.route stub defeated by a service worker fails LOUDLY and blames the wrong subsystem
DATE: 2026-08-13
AREA: instruments
SEVERITY: slows
STATUS: open
SESSION: amux-frustrations
CARD: AF-47
SYMPTOM: Isolation gave each project a CLEAN browser profile, which surfaced two failures the
  shared one had masked — and both lied about where the fault was. (1) system-jobs.spec.ts
  stubs /api/system-jobs with page.route; a registered service worker defeats that, because
  the request passes through the worker's fetch handler where page.route cannot see it. It
  did not error — it rendered the REAL job list and diffed it against the stub, so it read as
  "the stalled-row styling is broken under WebKit". (2) sw.js reloads the page on
  `controllerchange` as soon as a fresh worker claims the client, landing mid-page.evaluate:
  "Execution context was destroyed" on two specs about CSS geometry.
COST: Both point at the wrong subsystem by construction. (1) is the dangerous one: a stub
  that silently does not apply produces a confident, specific, wrong failure about rendering,
  and the natural response is to go read the CSS. Roughly an hour across the two before the
  common cause was visible.
FIX: `test.use({ serviceWorkers: 'block' })` on the specs that do not test the worker, in
  b31bcac. STILL OPEN as a class: nothing warns that a page.route stub never matched a
  request. A stub that matches zero requests is almost always a bug and is currently
  indistinguishable from one that matched — same green-looking machinery, no output either
  way. The generalisable guard is an assertion that each route was actually hit; amux has no
  such helper today and every future page.route stub inherits the same silence.

---

## Verified gate rejects a cross-group reporter's verification, so the strongest evidence cannot close the card
AREA: gates
SEVERITY: slows
STATUS: open
DATE: 2026-08-14
SESSION: amux
CARD: AMUX-3119
SYMPTOM: AMUX-3116 and AMUX-3117 (amux CLI fixes) were verified end-to-end by gtm-engine
  with negative controls, field-level CC_* diffs and a server-API cross-check, which is
  stronger than a typical same-group review. But the code verified-gate criterion is
  "peer-reviewed by a worker in group `amux`", and gtm-engine is group `gtm`. Acking it
  would be untrue, so both stay `done`.
COST: Two genuinely-verified cards cannot reach `verified`; the strongest verification
  available (the affected user, who also reported the bug) does not count toward the gate.
FIX: The verified gate should accept verification by the originating reporter, or by any
  worker when the card records who plus their evidence (AMUX-3119).

## Create-modal start prompt dropped silently when a worker boots in the default (manual) permission mode
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-14
SESSION: amux
CARD: AMUX-3055
SYMPTOM: Creating a worker from the dashboard modal with a start prompt, then peeking the
  new session: Claude Code was up and idle ("❯ Try ...", footer "⏸ manual mode on · ? for
  shortcuts"), but the start prompt was never delivered (composer empty, status stayed idle,
  the prompt token never appeared). send_after_ready polled claude_ui_visible for its whole
  timeout and returned with NOTHING logged (no server line, no session event, no board card).
COST: The initial prompt for every non-bypass worker created from the modal was lost, and
  the loss was invisible: the only symptom was an empty session a human had to notice. Root
  cause was a footer allow-list (claude_ui_visible matched only ⏵⏵/"bypass permissions"/"plan
  mode"), so a default manual-mode worker read as "UI not ready" forever.
FIX: Fixed at root. claude_ui_visible now also matches "manual mode" / "for shortcuts"
  (the mode-independent idle footer), send_after_ready now WARN-logs and emits a
  session.prompt_dropped event on timeout so the next drop self-announces, the create-start
  timeout was widened 30s->60s for first-run boots, and a regression test asserts the real
  manual-mode frame reads as visible. crates/amux-server/src/api/session_verbs.rs (uncommitted).

## Pressing Enter at a non-numbered/footered picker pasted the option label instead of pressing Enter, so the key never landed
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-14
SESSION: amux
CARD: AMUX-3054
SYMPTOM: A worker sat at an interactive input picker (input list / menu). Pressing the
  dashboard "Enter" affordance did nothing, silently, while the API answered ok. The Enter
  affordances route through an empty send (POST /api/sessions/<n>/send {text:""}) which
  extracts the highlighted ❯ line. AMUX-2952 pressed a real Enter only when the option was
  NUMBERED; a non-numbered highlighted option ("❯ Yes") was extracted and DELIVERED AS TEXT,
  and since AMUX-2909 pastes picker-shaped panes it landed as one bracketed-paste event the
  picker swallowed whole. A footered picker returned "no suggestion found" and the composer
  empty-send (no client fallback) also did nothing.
COST: Every attempt to answer certain pickers from the dashboard was a silent no-op; the
  worker stayed blocked at the picker and the human had to fall back to a raw keypress. The
  next occurrence left no trace (info-level log at best, only on the numbered path).
FIX: Fixed at root in send_text_inner's empty-send path: gate on the SELECTOR STATE
  (detect_claude_status == "waiting" && !is_rate_limit_menu), not on the option's numbering,
  and press Enter to accept the highlighted option for every picker shape. Emits a WARN
  [picker-enter/AMUX-3054] so the class is countable; the numbered fallback now WARNs too
  (a hit there means a detect_claude_status gap). Rate-limit menus are excluded so their
  dedicated handler keeps stamping credit_limited. Unit test locks the discriminator.
  crates/amux-server/src/api/session_verbs.rs (uncommitted).

## litestream DR replication died fleet-wide and nothing in amux could express it; it was found by grepping container logs on the box
AREA: cloud
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux-cloud (hit) / amux (diagnosed)
CARD: AC-349
SYMPTOM: All 5 real-org litestream sidecars failing with "attempt to write a readonly
  database (8)" on _litestream_seq, consecutive_errors 300+, after a disk-full container
  recreate pulled a non-root litestream:latest. No /api endpoint, invariant, or job report
  expresses DR-replication health: /api/logs/analyze, /api/debug/*, and
  /api/health/invariants all say nothing about a sidecar that has stopped replicating. The
  signal lived only in the litestream container's own stderr and its Prometheus metrics,
  neither of which amux reads.
COST: The failure was invisible until a human noticed and hand-diagnosed it: reproducing on
  the box, rm-ing state dirs, and reading container logs per org. A DR-coverage gap ran
  overnight (08-14 into 08-15) with nobody able to see it from amux; had a customer db
  actually corrupted in that window, the first signal would have been data loss rather than a
  probe.
FIX: Root cause fixed at the template (AMUX-3127, b8b358f: pin litestream 0.5.16 + user:0,
  plus a deploy guard that trips on reintroduction). The OBSERVABILITY half is AC-349 (routed
  to amux-cloud): the gateway should poll each sidecar's replica lag / consecutive_errors and
  expose it via /api/observability or a health invariant, so the next DR failure
  self-announces. Open until that runtime signal exists; the CI guard only catches the repo
  reintroduction, not a live replication stall.

## The idle-drain nudge escalates forever and never names the one command that stops it
AREA: notices
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-15
SESSION: amux
CARD: AMUX-3140
SYMPTOM: After draining every clean card and staging the rest (delicate / design-change /
  epic-child) to backlog, the `[amux]` idle-drain nudge kept firing and escalating ("repeats
  faster the larger your backlog is"). The mechanism to stop it exists — `drainable_backlog`
  excludes any card with a `source_ref` trigger from both firing and escalation — but the
  nudge message lists "backlog with a trigger" as an option WITHOUT naming the `--trigger`
  flag that sets it or saying it excludes the card. So the sanctioned escape was invisible
  from the message: I parked cards with a prose "TRIGGER:" in the desc (sets no source_ref)
  and kept getting re-listed, then nearly filed a card blaming the nudge for "not respecting
  triggers" when it does.
COST: A multi-turn loop of finish -> nudge -> stage -> re-nudge, and a near-miss filed card
  against a mechanism that was correct (caught only by verifying the mechanism, ethos rule 7).
  The same shape hit mixpeek-autopilot (2026-08-13, per the drainable_backlog code comment):
  three false nudges on a standing tripwire and two externally-triggered chores.
FIX: c7fe156 — the message now spells out `amux board <status> <id> --trigger "what unblocks
  it"` and that it excludes the card from the nudge, and says "drainable" throughout (renamed
  the misnamed `total_backlog` param; the call site already passed the drainable count). The
  exclusion behavior was already correct; this makes the honest path discoverable (ethos rule
  6). Underlying nudge friction is the same family as [[amux-project-reference]] board churn.

## amux send to a bare REPL worker: origin header is submitted as its own message, prompt body is not
AREA: notices
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux-cloud
CARD: AC-354
SYMPTOM: Driving a qwen3.8:27b ollama worker, `amux send qwen-eval "<prompt>"` returned
  `sent (origin-stamped): sent`, but the peek showed the model had received and answered only
  the `[amux-origin: amux-cloud ...]` HEADER (qwen reasoned about it as a possible
  social-engineering attempt and asked what I wanted), while the real prompt sat in the REPL
  input typed-but-unsubmitted (`Press Enter to send`). I had to `tmux send-keys Enter` by hand
  to get an answer. The steering/delivery choreography is claude-UI-shaped: it injects an
  origin header the bare REPL treats as content, and it does not submit the body.
COST: The send reported success while the payload never ran — a false "delivered" (ethos rule
  4). Every eval prompt needed a manual Enter, so the amux worker plumbing could not drive the
  model unattended; I fell back to tmux for the model eval.
FIX: REPL-aware delivery (AC-354, routed to amux, who owns the send/steering path): for
  bare-REPL providers, do not inject the origin header as a submitted message (omit it or make
  it a non-submitted preamble), and ensure the body is actually submitted. Verify by peeking
  that the model answered, not by trusting `sent`. Same message->worker seam as [[amux-project-reference]]
  AC-353 (env-apply can't message a not-yet-started worker).

## A peer's `git commit -a` swept my uncommitted feature edits into their unrelated commit
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux-cloud
CARD: AC-355
SYMPTOM: Mid-edit on session_verbs.rs (14 lines enabling ollama as a provider, AMUX-3145),
  another lane committed `13d66f4 fix(ledger)` with a `git commit -a`-shaped `git add
  <file>`, which staged the WHOLE shared file — sweeping my uncommitted ollama edits into
  their ledger-fix commit. `git diff` went empty under me mid-`git add`; `git status` showed
  the file clean while `grep -c ollama` still found my lines — they were committed, under a
  peer's authorship, in a commit whose message is about the ledger. The CLAUDE.md documents
  this exact class (2026-08-09) and it recurred here.
COST: ~8 min diagnosing where my edits went (were they reverted? committed? by whom?),
  splitting my remaining work into a second commit (b992c99), and a commit whose message does
  not describe half its diff. No code lost, but the attribution + commit-message coherence is
  wrong and only a manual peer heads-up reconciled it.
FIX: The durable fix is not "remember not to `git add <sharedfile>`" — it is per-lane
  isolation for in-flight edits (worktrees), or a pre-commit guard that refuses to stage a
  file another live session has uncommitted hunks in (the staged-guard already KNOWS cotenant
  edits — it warned about env_config.rs minutes earlier — but it warns the committer, not the
  victim, and does not block). Filed AC-355. Same family as [[amux-project-reference]]
  shared-checkout races; three attribution entries now share this seam.

## The shared git INDEX let my `git commit` sweep a peer's STAGED work (mirror of AC-355)
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux
CARD: AC-355
SYMPTOM: I ran `git add <my 3 files>` then `git commit -F` for AMUX-3148. The commit landed
  14 files, not 3 (440 insertions), sweeping 11 files of amux-cloud's and amux-frustrations'
  work (provider/*, backend/*, workers.rs, opencode/*, app.js, the amux CLI) into 4becada
  under MY message and trailer. Root mechanism is worse than AC-355's `git add <sharedfile>`:
  the git INDEX (.git/index) is SHARED across every session in this checkout, so a peer who
  had `git add`-ed their files but not yet committed had their STAGED work committed by MY
  commit — path-scoped `git add` gives no isolation when the index already holds a cotenant's
  staged hunks. The staged-guard NOTE fired ("also edited by amux-frustrations 37m ago") but,
  exactly as AC-355 says, warned me and did not block. When I tried to un-sweep, the
  shared-checkout guard correctly BLOCKED `git reset --soft HEAD~1` (moving shared HEAD can
  decapitate peers' commits), so there was no clean recovery: revert would delete their work.
COST: The work is safe (compiles, not pushed, preserved in 4becada) but attribution is wrong
  on 11 files and there is no in-repo way to fix it without owner sign-off; reconciliation was
  a manual heads-up to two peers. ~15 min. The un-sweep being unreachable from the sanctioned
  tooling is itself an ethos-6 gap.
FIX: Two concrete, either closes it: (1) before committing, assert the staged set equals your
  intended paths — `git diff --cached --name-only` must match what you added, and any extra is
  a cotenant's staged work to `git restore --staged` (scoped, guard-allowed) BEFORE commit; a
  pre-commit hook could enforce this automatically (refuse a commit whose staged set contains a
  file with another live session's uncommitted/staged hunks). (2) per-lane worktrees so the
  index is never shared. Same seam as AC-355; four attribution entries now share it, which is
  the argument for worktree isolation rather than another warning nobody can act on.

---

## `tmux send-keys ... Enter` does NOT submit a codex TUI prompt — amux sessions cannot send tasks to codex workers via raw tmux
DATE: 2026-08-15
SESSION: amux-homepage
AREA: codex-integration
STATUS: open
CARD: AH-81
SEVERITY: slows
TITLE: `tmux send-keys ... Enter` does NOT submit a codex TUI prompt — amux sessions cannot send tasks to codex workers via raw tmux
SYMPTOM: Tested qwen worker (codex --oss --local-provider ollama). Used `tmux send-keys -t "amux-qwen" "task text" Enter` to send prompts. Enter appended a NEWLINE to codex's multi-line input buffer rather than submitting — the prompt accumulated silently, never reached the model. Discovered only after ~45 min of apparent "no response" — the model was idle, not processing. Same issue hit xhigh reasoning effort (qwen does not support extended thinking), which added ~30 min of wasted wait time. Eventually discovered that `POST /api/sessions/<name>/send` correctly submits (amux uses the pane's send protocol that delivers Ctrl+Enter or similar). After switching to the API send, the agent immediately started Working and produced correct output.
COST: ~75 min (45 min for unresponsive session + 30 min debugging xhigh), wrong conclusion that the worker was broken (it was not — the submission method was wrong).
NORMALISED 2026-08-17 by amux-errors-and-bugs, not rewritten — see AEAB-19. This entry
  used `WHAT HAPPENED:` where the contract says `SYMPTOM:`, so `grep '^SYMPTOM:'` could
  not find it, and carried no `SEVERITY:`, so `grep '^SEVERITY: ...'` could not either.
  Renaming the label changed no words. The one INFERRED value is `SEVERITY: slows`,
  derived from this entry's own COST line ("~75 min ... wrong conclusion that the worker
  was broken"); amux-homepage should correct it if that is wrong. The non-contract
  `TITLE:` line duplicating the heading was left alone as harmless.
FIX: `POST /api/sessions/<name>/send` is the correct way to send tasks to codex/ollama workers. `tmux send-keys ... Enter` is wrong for codex TUI — it inserts a newline, not a submit. No amux docs or session-card says this; it is an easy mistake for any session testing a codex worker. Also: codex's global config `model_reasoning_effort = "xhigh"` is incompatible with local qwen models (qwen does not support extended thinking API); workers using `--oss --local-provider ollama` need `-c model_reasoning_effort=low` to be responsive.

---

## Three copies of "report state to amux" exist and global settings.json pointed at the poorest one — model + tokens silently regressed to zero
AREA: hooks
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AMUX-2936
SYMPTOM: `~/.claude/settings.json` Stop/UserPromptSubmit/PostToolUse all ran an inline one-liner posting exactly `{"state":"idle","source":"stop-hook"}` — no model, no tokens, no conversation id — while `~/.amux/hook-report.sh` sat on disk extracting all three. The server-side consequence was already recorded in code: "292 report POSTs, 0 carrying tokens".
  Fixing the blind-cotenant window I went to add one field to the state-report hook and found THREE implementations on this machine: an inline one-liner baked into ~/.claude/settings.json (posts {state, source} only), ~/.amux/hooks/amux-report.sh (a delegate), and ~/.amux/hook-report.sh (the real one — parses the payload, extracts model and real token count). settings.json pointed at the INLINE one, so every session started since that regression reported no model and no tokens, and auto-compact (AMUX-2829) lost its only input for the second time. amux-report.sh's own header documents this exact fork happening in 2026-08-11 and says "two implementations of one thing is what produced this bug; do not re-fork it" — and I still nearly shipped a FOURTH copy, because that warning lives in an unversioned runtime file nobody reads before editing. The reason it keeps recurring is structural: hook-report.sh was untracked, so there was no reviewable, diffable, rollback-able canonical copy, and no check could compare what is running against what was intended.
COST: ~25 min to discover the existing script and unwind my duplicate, plus an unknown number of days of model/tokens reporting zero fleet-wide, which silently disables auto-compact. The near-miss is the real cost: a fourth copy would have regressed model+tokens AGAIN while looking like a fix.
FIX: SHIPPED (ce87481). hook-report.sh now lives in the repo at scripts/hooks/hook-report.sh and is installed from there with a recorded sha256, mirroring the git-shared-guard treatment at install.sh:134 that exists for exactly this reason. settings.json repointed at it (restores model+tokens AND adds the conversation id). Remaining gap, not closed: there is no invariant comparing the RUNNING ~/.amux/hook-report.sh against the committed copy the way `hooks.shared_guard_matches_committed` does for the git guard — so drift is now detectable by hand but still not self-announcing. Worth adding; it is a near-copy of an invariant that already exists.

---

## A shared CARGO_TARGET_DIR is mandated, and concurrent builds in it evict each other's artifacts
AREA: build
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AMUX-2936
SYMPTOM: `error: extern location for serde_core does not exist: ~/.amux/rust-build-target/debug/deps/libserde_core-0d2476c6ed9be3cc.rmeta`, and separately 42 errors inside the `nix` crate ("cannot find type `ControlFlags` in this scope") — artifacts deleted underneath an in-flight build, three times in one session.
  CLAUDE.md requires ONE shared build dir (~/.amux/rust-build-target) and the reasoning is sound — per-session dirs filled the disk with ~37 copies at 10-15GB each. But with several lanes plus the auto-builder building concurrently, I hit repeated hard failures of the form "extern location for serde_core does not exist: .../libserde_core-<hash>.rmeta" and 42 errors inside the `nix` crate, i.e. artifacts deleted underneath an in-flight build. Not a lock contention wait, which is what the CLAUDE.md note measured and correctly called cheap; this is cache eviction, and the only recovery is a full rebuild. Hit it three times in one session, roughly 4 minutes of rebuild each.
COST: ~12 min of pure rebuild, and worse, it masqueraded as a code error twice — the first failure looked like my own change had broken the build, which is exactly the wrong instrument reading (a red result on code you just verified by hand means the instrument is a candidate before the code is).
FIX: Not fixed; needs a decision, not a workaround. Options: (a) leave it — the failure is loud and self-recovering, just expensive; (b) give the auto-builder its own target dir, since it is the one builder that runs unattended every 60s and is the most likely evictor, accepting ~15GB for the one process that never benefits from a warm shared cache; (c) find whether this is cargo GC (CARGO_GC / cache auto-clean) rather than eviction, in which case pinning the retention setting fixes it outright and costs nothing. (c) is worth checking first because it would be a one-line fix, and nobody has established WHICH of the three is happening — the diagnosis is missing, not the remedy.

## /api/health/invariants cannot tell you a check is running, only that nothing failed
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AF-55
SYMPTOM: after adding two invariants and deploying them, `GET /api/health/invariants` returned `checks: {pass: 409, fail: 17, unknown: 0, total: 426}` and named neither new check. `failures` lists only failing rows and `unknowns` only Unknown ones, so a PASSING invariant appears nowhere. A check that is green and a check that was never wired into `evaluate_all` produce a byte-identical response. Polled the endpoint eight times across a builder swap looking for a string that could never have been there.
COST: ~8 minutes and a wrong path, on a fix whose whole point was making a silent failure self-announce. Worse in the general case: the natural next move is to conclude the wiring did not take and go re-edit working code. `/api/debug/invariants` -> `latest_per_invariant` had the answer the entire time (`status=pass`, `age_s=2.4`) and the observability table in CLAUDE.md does not mention it.
FIX: both halves shipped. 2eceea7 documents `/api/debug/invariants` in the CLAUDE.md observability table and states plainly that a PASS is invisible on the health endpoint. feb7ea7 adds `GET /api/health/invariants?id=<invariant_id>`, which returns an explicit `ran` flag plus, on a miss, `known_ids` -- because a typo and a genuinely-unwired check are both empty results and only one of them is a bug. `ran` is about evaluation and not verdict: a failing check ran, and that is asserted, since collapsing the two is the obvious way to reintroduce the ambiguity. Mutation-verified (`ran := true` turns the test red). Same shape as the rule this file exists for: an empty result read as evidence, where a positive was never expressible.

## `include_str!` reaching outside crates/ compiles locally and breaks builds that COPY a subset — third instance
AREA: cloud
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AF-56
SYMPTOM: I added `include_str!(concat!(CARGO_MANIFEST_DIR, "/../../scripts/hooks/hook-report.sh"))` in 06683ee. `cargo check --workspace --all-targets`, `clippy -D warnings` and 1010 lib tests all passed, and the change deployed locally and ran green. The cloud image build then failed, because `cloud/docker/Dockerfile` COPYs `Cargo.toml`, `crates` and `amux` but not `scripts/`. Every gate I ran compiles from the FULL checkout, where the path exists, so not one of them could have caught it. rust CI's `check` job has the same blindness, which is why it stayed green.
COST: ~40 minutes of amux-cloud's evening, on the first green-main deploy in days. My commit was the second instance, not the first — ea2a573 (2026-08-14) added the same pattern for `scripts/git-hooks/git-shared-guard.py` and the image had been unbuildable for a day, invisible because deploy-cloud skipped on red main every time. Greening main is what exposed both at once. This is the THIRD `include_str!`-resolution incident in a week: 2026-08-10 (an uncommitted .sql swept into a peer's commit, AMUX-2647, still `STATUS: open` above), 2026-08-12 (the amux-CLI build), and this one. Same root each time — a compile-time include whose path is present in the author's tree and absent in some other build's inputs — reached by three different mechanisms, which is exactly why no single entry made the argument.
FIX: 910e668 (amux-cloud) adds `COPY scripts scripts` to the build stage AND `tests/dockerfile_build_inputs.rs`, which scans amux-server for `include_str!` reaching outside `crates/` and asserts the Dockerfile COPYs each root. It runs in `check`, so a NEW external include fails on a green checkout before any deploy build sees it. I ran it here and read its negative control rather than trusting the pass: it drops "scripts" from the copied set and asserts the check then reports it missing, with "the check cannot detect a missing COPY — it is theatre". Scanning for the pattern rather than hardcoding one path is what makes it a class kill. Remaining gap, latent not live: the check is scoped to amux-server, and the 2026-08-12 instance was the CLI. I grepped before claiming it — today NO crate outside amux-server uses `include_str!` at all, so nothing is currently unguarded; the gap is that the first one added to amux-cli would be.

## amux-launched browser does not survive a server self-adopt
AREA: browser
SEVERITY: slows
STATUS: open
DATE: 2026-08-15
SESSION: amux
CARD: AMUX-3184
SYMPTOM: Driving the dashboard for the ollama UI E2E, the amux-launched Chrome (POST /api/browser/start, a Playwright/CDP child of the server) vanished twice mid-test. Each time the trigger was the local auto-builder adopting a fleet commit: the server self-adopts (exits for launchd to relaunch) and the Chrome child dies with it. On a shared checkout where ANY session's commit swaps the binary every ~60s, any browser-driven task longer than a build cycle loses its session.
  CORRECTION (verified after filing, and it is the more useful lesson): my first report also claimed the failure was SILENT, that /api/browser/screenshot returned {"path": null} with no error. That was MY probe, not the endpoint. The handler returns a clear, actionable body, {"error":"no amux-launched browser is running, POST /api/browser/start ... first", "hint": ...}, and it already WARNs on wedged captures. My extraction was `python3 -c "print(json.load(sys.stdin).get('path'))"`, and an error response carries no `path` key, so it printed "None" and I read the None as a silent null. Exactly the ethos rule 7 trap: a blank result on code I had not yet read means the INSTRUMENT is the candidate before the code is. The instrument half of this card is a non-bug; the endpoint errors clearly today.
COST: ~8 minutes. ~6 across two browser restarts (re-open the peek via openPeek eval; the tmux pane re-rendered its shell setup so the worker's response had to be read from the peek history API), plus ~2 chasing a "silent failure" that my own extraction script invented and I filed a card for before reading the handler.
FIX: The real residual is lifecycle, not instrumentation. Launch Chrome DETACHED (not a server child) and persist its cdp_http/cdp_port/pid (the start response already returns all three), so a freshly self-adopted server re-attaches to the still-alive Chrome instead of orphaning it. Until then, a browser-driven task must expect to restart the session across a builder swap. The instrument half needs nothing.

## Compressed error bodies were logged as mojibake, so half the 5xx in a sweep were undiagnosable
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AF-57
SYMPTOM: running the daily log sweep, `GET /api/logs/analyze?since_h=24` returned `error_body` values like `\x1f\xef\xbf\xbd\x08...` for the 502/503 groups (browser, tts) while other groups on the same page showed clean JSON. One row read `\x1f...{"error":"CDP Page.captureScreenshot timed out after 30s"}\x53\x40` — readable text embedded in binary, which is what gave it away. The bytes are gzip: the request-log middleware is the OUTERMOST layer so it runs after `CompressionLayer`, and `String::from_utf8_lossy` was applied to an already-compressed body.
COST: the field exists precisely so a 5xx is diagnosable without a repro, and it silently failed for the subset of clients that negotiate gzip. `/api/why` and autofix both read `error_body`, so all three consumers got noise. ~2KB of destroyed bytes were written per affected row to hold it. Measured on the live log: 27 of 264 error bodies in a 24h window, ~10%. The corruption is IRREVERSIBLE, not merely ugly — 875 of ~3.8KB became U+FFFD, so `1f 8b` is now `1f ef bf bd` and no reader will ever recover those bodies; the 502s and 503s already in the window are permanently undiagnosable. Worst property: it only sometimes fires, so the same endpoint reads fine from curl and as mojibake from the dashboard, which makes it look like a weird payload rather than a logging bug.
FIX: f683a40 honours `Content-Encoding` before storing — decode gzip, and on an undecodable encoding or a corrupt stream store an explicit marker instead of bytes that read like content, so every branch is honest. Output capped at 1MB because a compressed body is an amplification vector and this runs on every 4xx/5xx. 993f5e4 adds a WARN on both marker branches so the next instance reaches a log sweep without anyone thinking to inspect `error_body`. Live-verified on the deployed build: 27 mojibake in the 24h before, 0 after, with `content-encoding: gzip` confirmed on the wire for the probe rows and the stored bodies read back read-only from `_amux_request_log`.

## A full `cargo test` in the shared checkout reports phantom failures when a peer is mid-edit
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AF-60
SYMPTOM: `cargo test -p amux-server --lib` returned `1009 passed; 4 failed` — `api::branding::tests::manifest_follows_branding_prefs`, `api::sse::ping_tests::ping_carries_the_embedded_app_ver`, `api::static_files::tests::unknown_api_path_is_a_json_404_not_the_spa_shell`, `invariants::monitor::extractor_wiring_tests::extract_caller_paths_includes_the_cli`. My only edit was `crates/amux-server/src/api/request_log.rs`, which none of those four touch. Re-running the same four minutes later: all pass. Full suite re-run: 1013 passed, 0 failed. The cause was a peer (amux) writing dashboard static files and the `amux` CLI while my run was reading them — these tests read repo files at RUNTIME, so they see whatever the shared working tree contains at that instant.
COST: ~6 minutes and a worktree round-trip to disambiguate, on a run whose whole purpose was deciding whether MY change broke something. The failure points at four subsystems the author never touched, so the honest first hypothesis is "I broke something in a way I do not understand" — the expensive direction. It also produces the inverse risk: a session that sees 4 unrelated failures, shrugs, and commits anyway is right this time and wrong the time it matters. `ethos.md` names the tell ("a red test on code you just verified by hand ... means the instrument is a candidate before the code is") but nothing in the test output says the shared tree moved underneath it.
FIX: bf01bdd — enabled rust-embed's `debug-embed` feature, taking option (b)'s spirit (remove the race) rather than (a) (report it). ROOT, and it is ONE root not four: all four tests go through `DashboardAssets::get()`, and rust-embed falls back to reading `static/` from disk at runtime in DEBUG builds only. Reproduced deterministically instead of by timing — truncate `app.js` WITHOUT rebuilding and two of them fail; with the feature they pass, and still pass when `app.js` is DELETED, because the binary carries its own copy. The cost is debug hot-reload of dashboard assets, which nothing here uses: the auto-builder ships `cargo build --release`, so what DEPLOYS always embedded at compile time, and CLAUDE.md already states the rule this restores ("editing the working tree changes nothing that is live; COMMITTED source is what ships") — which the debug fallback was quietly contradicting. Options considered and not taken: (a) have the file-reading tests record the mtime/sha of the repo files they read and print "the working tree changed during this run" on failure, so the phantom announces itself instead of being re-diagnosed each time; (b) have them read from `git show HEAD:<path>` rather than the working tree, so they test the COMMITTED artifact, which is what actually ships (the deploy is committed-source-only, so this is arguably more correct anyway); (c) document the disambiguation recipe — re-run the failures alone, and if they pass, run the full suite in `git worktree add --detach HEAD` before believing them. (b) looks right to me and is a small change, but these are not my tests and the choice belongs to whoever owns them.

## An unknown /api path answers a bare empty 405 on non-GET, so a guessing caller learns nothing
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-15
SESSION: amux-frustrations
CARD: AF-61
SYMPTOM: `POST /api/board/AF-49/backlog` returned `405` with an EMPTY body and no content-type, while `GET` on the same nonexistent path returns `{"error": "not found"}` as JSON. `/{*path}` was mounted GET-only, so axum's method router answered before the JSON-404 branch could run. Found via `/api/logs/analyze`, whose verdict already said "no route exists at this path — the 405 is the GET-only SPA catch-all answering a non-GET".
COST: small per occurrence, but it lands on a caller who is ALREADY wrong and gives them nothing to correct with. 9 rows over two days from two lanes (8 from `backend` in one batch). The route was invented — nothing advertises it — most likely generalised from `POST /api/board/{id}/claim`, which does exist. Worth contrasting with the gate-409 body, which names the exact CLI command to run and is why those callers recover; this one names nothing. The capability was never missing: `amux board backlog <ID>` exists and works.
FIX: 362fc4d — mount `any(serve_path)` and take `Method`, so an unknown `/api/*` path JSON-404s on every method. Non-API non-GET still 405s, because handing the SPA shell back for a POST would be worse than the bare 405 it replaces. The guard that existed could not fail on this: `unknown_api_path_is_a_json_404_not_the_spa_shell` exercised GET only, the method that already worked. Proved rather than argued — reverting to `get()` turns the new test red while the old one stays green.

## The decompose nudge re-fires on a card already reshaped out of a capture
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-15
SESSION: amux
CARD: AMUX-3187
SYMPTOM: I reshaped an auto-captured card (AMUX-3185) into a proper unit of work: retitled it "Pull the current top local LLM (qwen3-coder:30b) into ollama", retyped it `ops`, wrote a desc with an explicit done-condition ("done when the model shows in GET /api/ollama/models"), and set it doing while the pull ran. The `[amux]` decompose nudge fired anyway, twice, calling it "a captured chat prompt, not a unit of work" that "cannot move through the gates as it stands". The card carried no capture flags (flags={}), owner_type=agent, a clean title with no "**Prompt:**" prefix, and an ops gate it can satisfy. The nudge is keying on the card's auto-captured ORIGIN, not its current shape.
COST: two false nudges plus a verification tool-call to confirm the card was properly shaped and the nudge wrong. Small here because I had just reshaped it, so I knew it was fine. The hazard is the general case: a session that did NOT just reshape the card reads "not a unit of work, split it" and either splits an already-good card into duplicates or discards real in-progress work. It is the ethos rule 5 shape, a nudge disagreeing with the artifact it describes, and it re-fires forever because reshaping never clears whatever it keys on.
FIX: 2802107. The origin signal was concrete: pickup_junk_reason evaluated `desc + log`, and `capture: session prompt` is a DURABLE LOG marker minted once at capture (session_verbs.rs:2602) that never clears, so the blob carried it forever and re-branded a reshaped card on every 6h tick. Now pickup_junk_reason takes (title, desc, log) separately: the capture brand (the marker check and the anchored **Prompt:** check) reads the CURRENT desc; the structure veto and fold count still read the blob, where log content is legitimate signal. A fresh capture's desc always begins "**Prompt:** " so every real capture is still caught, and reshaping the desc, the sanctioned exit, now clears the brand. Regression test a_reshaped_capture_is_no_longer_branded_a_capture with a positive control (still-raw capture IS branded; same log marker under a reshaped desc is NOT). Sibling class filed AMUX-3188 (the AMUX-3000 stale-file nudge prescribes an unconditional git checkout without a hash-check, social-media).

## Cloud freshness tick's served-APP_VER probe returns empty because app.js 302s to /sign-in
AREA: cloud
SEVERITY: slows
STATUS: open
DATE: 2026-08-16
SESSION: amux-cloud
CARD: AC-360
SYMPTOM: The CLOUD FRESHNESS TICK step-1 probe `curl -sk https://cloud.amux.io/app.js | grep APP_VER` returns EMPTY. Unauthenticated `app.js` now 302s to `/sign-in` (http=302, size=0). `/health` and `/version` also 302; `/api/health` and `/api/version` 401. There is no auth-free endpoint on the gateway that reveals the served build, so the recipe's `served=` is always blank for whoever runs the tick.
COST: A blank `served` compared against a non-empty `head` reads as "cloud is behind origin/main" and, taken literally, would dispatch `recreate=yes` — which STOPS every worker container and does not restore them. That is precisely the false-positive-recreate-before-a-demo harm the 2026-08-12 guard was added to prevent, and here the trigger is a broken probe rather than a real drift. Caught only because I recognised the empty read as a probe fault, not a signal (ethos rule 7: an empty grep is not a measurement). A less careful run recreates prod to "fix" a drift that does not exist.
FIX: (proposed on AC-360) Drop the app.js scrape. The robust, auth-free freshness signal already exists: the newest SUCCESSFUL `deploy-cloud` run's headSha vs origin/main. `gh run list --workflow=deploy-cloud.yml --json headSha,conclusion` -> newest `success` sha, then `git rev-list --count <sha>..origin/main`; 0 means the deployed image is current. Used exactly that this tick (last success 31914150334 built 73fce92; origin/main tip == 73fce92, 0 behind). The recipe change is Ethan's to make (it is his standing scheduler prompt, ethos rule 8) — proposed, not silently rewritten. Distinct from AC-344 (deploy-cloud SKIP is silent); this is the tick's own comparison being unable to read the served version at all. FIXED 2026-08-16 (c026008 log + SCHED-344 command PATCH): the tick now runs the deploy-sha probe, verified live on a later firing.

## `amux alert` reached NO channel during a real prod-down incident — the fire-alarm is silently dead
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-16
SESSION: amux-cloud
CARD: AC-362
SYMPTOM: cloud.amux.io was fully down (502 on every endpoint, AC-361). I fired `amux alert "<prod down + what I need>" "<why now>"` and it paged NOBODY, exit 3: "email: failed: token refresh failed (400): invalid_grant, push: no push subscriptions, sms: no phone configured (AMUX_OWNER_PHONE is empty)". All three fire-alarm channels dead at once, so the one path meant to reach the owner in an emergency failed at the exact moment it was needed. The top-of-repo CLAUDE.md still advertises SMS/iMessage as "wired and confirmed working" returning channels push:sent sms:imessage — now false.
COST: The owner was NOT paged during a live customer-facing outage. I only reached Ethan because /api/email/send (Gmail API, a DIFFERENT credential than the alert email path) still worked and delivered a direct email (msg 1a00b525ec2baa84), plus a board escalation (AC-361). Without noticing the alert had failed and falling back by hand, prod would have stayed down and silent. A fire-alarm that fails only when pulled is worse than none, because every runbook (including CLAUDE.md's) tells you to trust it.
FIX: (AC-362) three channel repairs — re-auth the alert email OAuth token [Ethan's Google], register a push subscription for the owner, set AMUX_OWNER_PHONE in ~/.amux/server.env [Ethan]. DURABLE (the real fix): `amux alert` must SELF-TEST its channels on a schedule and surface "0/3 channels healthy" as its own signal BEFORE an incident needs it — a dead fire-alarm currently leaves no trace until someone pulls it. And reconcile the CLAUDE.md claim that SMS is confirmed-working with reality.

## Diagnosing deployed behaviour against source that had not shipped yet
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-16
SESSION: amux-frustrations
CARD: AF-82
SYMPTOM: `ai-video-editor` read `running=False` in `/api/sessions` while the agent was demonstrably alive (claude pid 8605, 321k tokens, 2 subagents, `self_report {age_s: 57, state: active}`). I traced it to `shell_only` classification, then found `sessions_legacy.rs:467` ALREADY contained a `pgrep -P <pane_pid>` rescue for exactly this — with an RCA comment naming `ai-video-editor` by name. I ran the rescue's own command by hand (`/usr/bin/pgrep -P 8600` -> `8605`), confirmed it should fire, and filed a card saying the misclassification "survives the child-check". It did not survive anything: the rescue was committed at 14:12:27 and the running server had started at 13:08:59, so the live binary predated it by an hour.
COST: a card filed on a false premise, and a stretch of investigation spent on a mystery that did not exist. The deeper cost is the shape: I was reading source, confirming its logic was correct, and treating deployed behaviour that disagreed as an unexplained defect — when the two were simply different builds. This repo's own rule ("editing the working tree changes nothing that is live; COMMITTED source is what ships") is stated for UNCOMMITTED edits, and I have quoted it at other sessions; the same gap opens for a commit that exists but has not been BUILT yet, which is a window of up to a builder cycle plus a 4.5-minute release compile.
FIX: the check is one call and belongs before any "why does deployed behaviour disagree with the code" investigation: compare `/health`'s `build` against the commit time of the code you are reading (`git log -1 --format=%ad <sha>`), and if the fix postdates the running binary, stop — there is nothing to diagnose. CLAUDE.md already tells you to bracket TIMING measurements with `build`; this is the same instrument for a different question, and it is not written down for this one. Worth adding to the Deploy section: a fix you can see in `git log` is not a fix the server is running.

## Metrics disk gauge read 0.6% used while the volume was 90% full and failing writes
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-16
SESSION: desktop
CARD: DESKT-3
SYMPTOM: `/api/metrics` reported `disk_used_gb: 11.4`, `disk_total_gb: 1858.2`, `disk_percent: 0.6` — a green "low" gauge on the dashboard — while `df -h /System/Volumes/Data` showed 1.7Ti used of 1.8Ti, 26Gi free, 99% full. Writes were already failing elsewhere on the box. Cause: `collect_system_metrics` ran `df -k /` and parsed its Used column. On APFS `/` is the SEALED READ-ONLY system snapshot (~11GB, essentially static); everything a user owns lives on `/System/Volumes/Data`. So the gauge was reading a different volume than the one that fills up, and it will read ~0% forever no matter how full the machine gets.
COST: the disk filled to 99% with the dashboard showing green the whole way, and nobody had a reason to look. The failure mode is the expensive one: not a missing instrument (which prompts a manual check) but a confident wrong one that is trusted and read first. CLAUDE.md already records a related incident where ~37 abandoned target dirs took the volume to 741MB free with a 50-session fleet running — the gauge that was supposed to make that visible could not, by construction, ever have shown it. Also cost this session a wrong initial read of how much headroom the machine had.
FIX: fixed in the same breath as building the reclaim scanner. `/api/metrics` now derives disk from `statfs($HOME)` through `reclaim::df_bytes` — asking about a PATH rather than a mount point is correct on APFS by construction, and sharing the helper with the scanner means the two views can never disagree (a second spelling would drift). Adds `disk_free_gb` because free space is the number you act on, and emits `tracing::warn!(disk_percent, free_gb, "disk critically full")` at >=90% so the NEXT disk-fill announces itself in a log sweep instead of waiting for someone to open the dashboard. Regression test `disk_metrics_measure_the_data_volume_not_the_sealed_system_snapshot` was verified to FAIL against the pre-fix code with "metrics reports 0.6% used but the data volume is at 90.56%".

## `git commit` after `git add`-ing only my files still ships a peer's pre-staged file
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-16
SESSION: amux (file-manager subagent)
CARD: AMUX-3249
SYMPTOM: I `git add`-ed exactly my four dashboard files, then `git commit`. The
  commit landed FIVE files: my four plus `crates/amux-server/src/runtime_jobs/mod.rs`,
  a peer's (`desktop`/`amux`) comment-only em-dash cleanup that was already sitting
  staged in the shared index. `git commit` commits the whole index, not just the paths
  I added, so a file another session left staged rides along under my SHA and my card.
COST: a foreign Rust file shipped in a dashboard-UX commit (8b802e1) against an explicit
  "do not touch Rust files / only git add your exact files" instruction. Harmless here
  (comment-only, compiles, same-session attribution) but the mechanism ships un-reviewed
  peer code silently. Caught only because `git show --stat` listed 5 files, not 4.
FIX: the durable habit is `git commit -- <exact paths>` (path-scoped) or
  `git diff --cached --name-only` + unstage foreign paths BEFORE committing — the second
  AMUX-3242 commit used `git commit -- <paths>` and landed exactly 2 files. Worth putting
  the path-scoped form in CLAUDE.md's Deploy section as the default on this shared checkout,
  since a headerless `git add` of your files does not protect you from a peer's pre-staged index.

## staged-guard can't see a subagent's own edits, so it blocks the subagent's real work as "foreign"
AREA: attribution
SEVERITY: slows
STATUS: open
DATE: 2026-08-16
SESSION: amux (file-manager subagent)
CARD: AMUX-3249
SYMPTOM: The pre-commit staged-guard bases its verdict on per-session EDIT RECORDS in a
  time window, not on the staged diff. Running as a subagent, my Edits to app.css /
  index.html / sw.js produced no edit record under my session, so the guard reported
  "they wrote it (transcript); you have no edit record on this path" and BLOCKED the
  commit, naming `desktop` as sole author of files I had just rewritten this session.
COST: the commit was blocked; I had to read the FULL staged diff of app.css and index.html
  by hand to confirm every hunk was mine, then use `AMUX_VERIFIED_SOLO=1` to override. The
  guard's own advice ("keep only your hunks") assumed the peer's work was mixed in when it
  was not. The dangerous edge: a subagent conditioned to reach for AMUX_VERIFIED_SOLO on
  every commit will eventually rubber-stamp a diff that DOES carry foreign hunks, since the
  guard cries wolf on every subagent commit.
FIX: the guard needs a signal a subagent's edits actually exist — attribute Edit-tool writes
  to the running (sub)agent session, or fall back to the staged diff (not edit records) when
  no edit record exists for EITHER party. Basing the verdict on the staged diff directly
  would make it correct regardless of who recorded what.

## Idle guard called a CLEAN tree dirty, then prescribed a 44-commit revert as the "safe" action
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-16
SESSION: desktop
CARD: DESKT-10
SYMPTOM: The idle dirty-tree notice reported "2 uncommitted change(s)" for app.css and app.js while `git status --porcelain` was EMPTY. Both worktree blobs were byte-identical to HEAD; they differed only from origin/main, which this checkout sits ~44 commits ahead of. The notice then ran its direction test, `git cat-file -e $(git hash-object <path>)`, got "object exists" for both, and classified them STALE, whose prescribed remedy is `git checkout origin/main -- <path>`. Running that would have reverted app.js by 1153 insertions and deleted crates/amux-server/src/api/reclaim.rs entirely, a feature shipped hours earlier. I tested five committed-but-unpushed paths (app.js, app.css, reclaim.rs, api/mod.rs, frustrations.md) and every single one classified STALE.
COST: no work lost, because the tree being clean vs HEAD was checkable in one command and I checked before acting. The cost is the trap itself and how well disguised it is. The notice opens by warning that a difference from origin is not a direction, and then uses a test carrying exactly that blind spot, so the warning reads as evidence the test already accounts for it. It also states that roughly 1 in 4 differing paths are novel mid-edits a checkout would destroy, which frames "STALE" as the safe verdict and pushes toward the destructive branch. Any session that follows it literally on this checkout reverts every file it names.
FIX: the direction test must be ANCESTRY, not blob existence. Blob existence cannot tell an old revision from a current one that is merely unpushed; both answer yes, and on a permanently-ahead checkout every committed file answers yes. `git merge-base --is-ancestor $(git log -1 --format=%H -- <path>) origin/main` separates them exactly: false means committed and unpushed, so leave it alone; true plus a worktree difference means genuinely older. Second, gate the notice on `git status --porcelain` being non-empty, so a tree that is clean against HEAD never triggers it at all. Both are one-line changes and either alone would have prevented this.

## SUPERSEDES the entry above: the guard's classifier was right, only its printed ADVICE was wrong
AREA: instruments
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-16
SESSION: desktop
CARD: DESKT-10
SYMPTOM: Same incident, corrected diagnosis after reading commit_nudge.rs instead of reasoning from the notice alone. Two claims in my entry above were wrong. FIRST: the guard does NOT classify with blob existence. `freshness_from_repo` uses `git log HEAD..origin/main -- <path>`, which is proper ancestry and correctly returns not-stale for a committed-but-unpushed file. What prescribes `git cat-file -e $(git hash-object <path>)` is the message TEXT the guard prints, in its two direction-unknown branches. The classifier and the advice disagreed, and the advice is the half a human acts on. SECOND: I reported it firing on a CLEAN tree. `dirty_paths` reads `git status --porcelain`, so it cannot. The real explanation is a race: at nudge time the amux lane had app.css and app.js uncommitted, and by the time I ran git status they had committed them in 2ec671b. The notice itself said CONTESTED, also edited by amux, which fits. So the "gate the notice on porcelain non-empty" fix I proposed was unnecessary.
COST: nothing beyond my own time, and it would have cost the amux lane theirs: they picked the card up and were about to hunt for a second code path that does not exist. Worth recording because of HOW the wrong diagnosis was produced. I ran the blob test, watched it misclassify five real paths, and concluded the guard classified that way, when all I had actually established was that the printed recipe was wrong. The notice's text was treated as evidence of the code's behaviour. Reading the 40 lines of commit_nudge.rs would have separated them in a minute, and I filed a card and a frustrations entry before doing it.
FIX: 5b923db. Both direction-unknown branches now print the ancestry test the classifier already uses, state which way each outcome points, and name blob-existence as the thing not to substitute plus why. The STALE section's use of blob-existence is deliberately kept: there the path is already proven behind, and the open question is pure-old-copy vs novel-mid-edit, which blob existence answers correctly. Regression test asserts on the message text and was verified to fail against the old recipe. The durable lesson is narrower than my first entry: when a notice and the code disagree, read the code before filing against either, and say which one you actually measured.

## The card log names no lane on auto-pickup, so a same-owner claim reads as a cross-owner dispatch
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-17
SESSION: amux
CARD: AF-79
SYMPTOM: amux-frustrations filed AF-79 reporting that auto-pickup dispatched a card owned by session=amux (AF-78) into the amux-frustrations lane. Tracing it against the DB and request log showed the dispatch was correct: AF-78 was created owned by amux-frustrations, correctly picked up by its owner at 09:40:39, and reassigned to amux one second LATER at 09:40:40. The report was a false positive. The reason it looked like a bug: claim_card logs a bare "Auto-picked up from queue" naming no lane, and it silently overwrites the card session to the claimer, so a legitimate same-owner pickup followed by a reassignment is un-reconstructable from the card and reads as a cross-owner dispatch. select_pickup keys on i.session=?1 bound to the lane in all three callers, so a cross-owner dispatch cannot actually happen.
COST: a peer's false-positive bug card plus a multi-step investigation (DB timeline reconstruction, request-log correlation, reading three claim_card call sites) to establish that the dispatch was correct. The card's own log could not answer "which lane claimed this", which is the one fact that would have settled it in seconds.
FIX: dd8d6ab. claim_card now reads the prior owner in the same transaction and logs "Auto-picked up from queue by <lane>", plus "(reassigned from <prior>)" and a tracing::warn when prior != claimer (unreachable from every caller, so it is the self-announcing signal if a real mis-dispatch ever lands). Regression tests: pickup_offers_a_card_only_to_its_session_owner and claim_log_names_the_lane_and_records_a_reassignment (the latter fails on the pre-fix bare log line, verified by reverting).

## SUPERSEDES both entries above on DESKT-10: blob existence is unsound in the STALE section too
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-17
SESSION: desktop
CARD: DESKT-10
SYMPTOM: My fix 5b923db moved the direction-unknown branches to the ancestry test but DELIBERATELY kept `git cat-file -e $(git hash-object <path>)` in the STALE section, with a comment arguing it was correct there because the classifier had already proven the path was behind. cold-outbound proved that wrong and I reproduced it: commit v1, edit to v2, `git add` without committing, and cat-file -e reports EXISTS while `git log --all --find-object=<blob>` is empty. `git add` writes the blob into .git/objects, so cat-file -e answers "ever written to the object DB", not "ever committed". The prescribed `git checkout origin/main -- <path>` then deletes the never-committed mid-edit. cold-outbound hit a live 4-minute near-miss on server-fast-checks.yml, mid-keystroke.
COST: a destructive false positive shipped into standing advice for every lane, for about 14 hours, and a near-miss on someone else's uncommitted work. The gap is not exotic: any session that stages incrementally produces it constantly, and it fires in the delete direction rather than the redundant-commit direction.
FIX: `git log --all --find-object=<blob>`; empty means never committed anywhere. `--all` matters, since a blob committed only on origin or another branch reads empty under a HEAD-only search, which errs safe but still misclassifies. amux has a fix agent in flight across commit_nudge.rs, the shell guards and session-freshness.sh, with a regression test; I am staying off those files rather than being a second editor. What generalises past this bug: I decomposed the question correctly (once a path is known behind, ask pure-old-copy vs novel-mid-edit) and then never checked that the instrument answered the sub-question I had just posed. A correct decomposition makes the wrong instrument feel already-validated, because the reasoning that selected it was sound. Verify the mechanism, not the verdict, applies to the sub-question too, and I had quoted that rule at another session hours earlier.

---

## push-guard reports "unknown (api unreachable)" instead of reading the Amux-Session trailer from the commit
DATE: 2026-08-17
AREA: attribution
STATUS: open
SEVERITY: annoys
SESSION: amux-homepage
COST: 5 minutes of false diagnosis and a wrong message sent to amux
CARD: AEAB-21
SYMPTOM: push-guard emits "currently: unknown (api unreachable)" when it fails to resolve the owning session's identity via the API. The Amux-Session trailers ARE present and correct in git log, so the attribution data exists — the guard just cannot reach the API at that moment and falls back to "unknown" instead of reading the trailer directly from the commit. This caused me to message amux asking them to push as if something was wrong with their commits, when the real issue was a guard API lookup failure.
REPRODUCE: trigger a push that the guard will block; if the server is mid-restart (builder cycle), the guard resolves "currently: unknown (api unreachable)" even for sessions with fully-attributed commits.
FIX: the guard should fall back to the Amux-Session trailer in the commit itself when the API is unreachable, rather than reporting "unknown". The data is already in the commit; the API is just a secondary confirmation. Never let an API timeout degrade a git-native source.
NORMALISED 2026-08-17 by amux-errors-and-bugs, not rewritten — see AEAB-19. This entry
  arrived in 18590ca8 with no `## ` heading, so the audit's parser folded it into the
  entry above and the file's own greps could not see it. Changes were: added the heading
  (worded from this entry's own first clause), `DESCRIPTION:` -> `SYMPTOM:`,
  `FIX DIRECTION:` -> `FIX:`, `SESSION:` filled from the commit's own Amux-Session
  trailer, and `CARD: (file one)` -> AEAB-21, which I filed on this entry's behalf
  because it asked for one. Not a word of the account was altered. The one INFERRED
  value is `SEVERITY: annoys`, derived from this entry's own COST line ("5 minutes of
  false diagnosis"); amux-homepage should correct it if that is wrong.
## A pure status-question prompt is captured as code/doing, so it can never pass a gate and holds the WIP slot
AREA: board
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-17
SESSION: amux
CARD: AMUX-3330
SYMPTOM: Three prompts this session that only ASK for information ("status on MSG-29602?", "whats the status of MSG-29588?") were auto-captured as type=code, status=doing cards (AMUX-3325, AMUX-3323, AMUX-3328). A code card is gated on "implemented and merged"; a question can never truthfully satisfy that (ethos rule 3), and status=doing consumes the single WIP slot, so the `[amux]` "captured prompt, not a unit of work" nudge fires and the only resolution is a manual `curl PATCH ... status=discarded`.
COST: ~1 tool-call + a manual discard per occurrence, 3x this session, and each one interrupts the answer flow with a WIP nudge. Small individually, but it recurs on every information-only prompt, which for a session Ethan is querying is a large fraction of turns.
FIX: Fixed in 145ee4b. amux_core::board::is_status_query detects the pure status-query shape and mint_capture_card returns None for it (mints NO board card), exactly like the steering/[no-board] prompts title_from_prompt already declines — so no WIP hold, no gate, no manual discard. The prompt still lands in cmd_history (the Messages ledger keeps every prompt). Deliberately tight (false positive = silent work): a match needs BOTH a status-query opener AND a trailing '?'. Chose mint-nothing over a non-gated type because AMUX-2613 forces captures to mint in `doing` (a `todo` capture is re-dispatched and double-runs), so a non-doing capture was not an option; and a status question is genuinely not work, like the steering prompts already skipped. Surface-in-logs: 'ledger: status-query not carded'. Distinct from the reshape-rebrand class (2802107 / AMUX-3188). Card AMUX-3330.
## The verify nudge names `needs:you` as its escape and does not honour it
AREA: board
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-17
SESSION: amux (AEAB)
CARD: AEAB-9
SYMPTOM: The done-verification nudge closes with "Cards that genuinely cannot be
  verified by you ... should be tagged `needs:you` so they surface in the owner digest
  rather than sitting here indefinitely." AEAB-5 was tagged via `amux board needsyou`
  on 08-14 — the sanctioned command, which succeeded — and the nudge fired on it again
  on 08-17. `done_verify_candidates` / `done_card_count` filter on session, status,
  deleted, archived, owner_type and type, and never join `issue_tags`.
COST: Three nudge cycles spent re-deciding a card that was already correctly parked,
  each one re-reading the card to reach the same verdict. Worse than the minutes: the
  nudge is the mechanism that is supposed to make a stuck done card visible, so an
  instruction it issues and then ignores trains the reader to discount it. It also
  double-nags — `needsyou.renag` already chases the HUMAN on these cards every 3d, so
  the same card asks both the party who owes the answer and the party who cannot give
  one.
FIX: PR #106 / a9aa7177 — exclusion factored into `needs_human_sql()` next to the
  existing `unverifiable_types_sql()` and applied to BOTH queries (they feed the
  "... and N more" arithmetic and the file already warns they must not drift). Same
  `LIKE 'needs:you%'` form as `select_pickup`, so `needs:you:decision` cannot slip
  past. Test verified to FAIL on the pre-fix code, listing all three tagged cards.
  NOT A NEW DEFECT CLASS: this is AC-223 one consumer down. That card closed the
  auto-pickup hole and its own FIX note said the tag-vs-status representation question
  stayed open. Every query that reads `issues` without joining `issue_tags` is a
  candidate for the same bug — worth a sweep rather than waiting to hit the next one.

## `amux board add` folds every flag into the title, and `--help` files a card
AREA: cli
SEVERITY: blocks
STATUS: open
DATE: 2026-08-17
SESSION: amux-errors-and-bugs
CARD: AEAB-17
SYMPTOM: `amux board add` takes only a positional title. Unknown flags are concatenated
  into it, exit 0, no warning — proved with a control rather than inferred from missing
  help: `amux board add "PROBE..." --totally-bogus-flag xyz` filed AEAB-16 with the flag
  in the title. Separately, `amux board add --help` does not print help; it creates a
  card titled `--help` (AEAB-15). The verbs that DO take flags are inconsistent with it:
  `retitle` accepts `--stdin/--file/--desc-stdin` and `--type`, every status verb accepts
  `--checked/--ack/--outcome-file`; `add`, the verb you reach first, accepts none.
COST: All five cards filed in one log-review run (AEAB-9..AEAB-13) were created with
  desc_len 0 or near-0 — every diagnosis written for them went nowhere — titles up to
  259 chars carrying raw `--desc-file /private/tmp/claude-501/.../c26.md` paths that
  will not exist tomorrow, and the wrong TYPE, so two cards about a production outage
  and a dual-server fault were gated on "Implemented and merged" + "Tests / lint pass".
  An empty card is worse than no card: it asserts the work is tracked. And the
  discovery path is the polluting action, so the mistake is unlearnable in advance —
  the AMUX-2140 shape (`amux board claim`) with a new verb.
FIX: Cards repaired via the documented path (`retitle ... --desc-stdin`) and verified
  from fresh reads; probe cards discarded after confirming they were mine. CLI not
  fixed — not mine. Wanted: reject unknown flags, print help for `--help`, accept
  `--type`/`--desc-file` on `add` like `retitle` does, and a title-length sanity check
  (a 259-char title containing `/private/tmp/` is never intentional).
MY OWN HALF, recorded because ethos.md predicted it verbatim: I verified `type` and
  `status` on those cards from fresh reads and never checked `desc` — the exact
  desc_append/AMUX-2161 failure rule 7 describes ("the habit gave the feeling of rigour
  while pointing at the wrong field"). I had read that line earlier in the same session.
  Reading a rule does not install the habit; verify the operand you just wrote.

## The auto-builder ships any branch to the live fleet with no announcement
AREA: deploy
SEVERITY: blocks
STATUS: open (the live deviation is fixed; the hazard is not)
DATE: 2026-08-17
SESSION: amux-errors-and-bugs
CARD: AEAB-12
SYMPTOM: `~/amux` is the BUILD SOURCE, and the builder rebuilds on any local HEAD move
  regardless of branch; the server self-adopts in 5s. I committed a9aa7177 on a feature
  branch there at 00:02; at 00:03:43 the builder installed it and it served the whole
  fleet until 09:45 — 9h42m of an unreviewed, un-CI'd commit in production. The same
  condition left the machine 29 commits behind origin/main, so SCHED-1 ("keep me on the
  latest") fired at 09:00 and could not do its job.
COST: 9h42m of unreviewed code live, plus the owner's standing "keep me on the latest"
  request silently unmet while every indicator looked healthy. Diagnosing it took the
  first ~30 minutes of a log review that was supposed to be about something else.
FIX: Live deviation fixed — ~/amux back on main, fast-forwarded to 9d5aebf4, verified
  by build-stamp change (663a3a84 -> ec3228af), store=ok, 0 panics/0 ERRORs since. The
  hazard is NOT fixed and should not be fixed by refusing non-main HEADs: this machine
  survived weeks deliberately pinned to an unmerged fix branch, so that is a supported
  mode. The defect is that a deliberate pin and an accidental feature branch are
  byte-identical to the builder and the accidental one is announced nowhere. Wanted:
  one line in rust-auto-build.log naming the branch when the revision is off main, and
  the same fact on /health or the dashboard. Workaround that works today and belongs in
  CLAUDE.md: never develop in ~/amux — `git worktree add` and leave its HEAD on main.

## Two amux servers on one SQLite DB, and endpoint.json points at the wrong one
AREA: port
SEVERITY: blocks
STATUS: open — owner's decision
DATE: 2026-08-17
SESSION: amux-errors-and-bugs
CARD: AEAB-11
SYMPTOM: Two launchd jobs both run the Rust server against `~/.amux/amux.db` —
  `com.amux.server-rs` (pid 22521, port 8824, last exit -9) and `com.amux.serve`
  (pid 22053, port 8823, exit 0) — same binary, same build, both logging "schedule loop
  starting (FIRING)". Every `starting amux-rust` line before today was 8824 and single;
  8823 starts begin 2026-08-17 03:53:41.
COST: One batch of request-log rows was DROPPED (`request-log insert failed; rows
  dropped error=database is locked`, 04:07:34) — the first and only lock error in the
  file, all time, inside the dual-instance window. `endpoint.json` now advertises 8823,
  so every hook self-healing a stale AMUX_URL off it reaches the OTHER server; my own
  sync-github.sh resolver (frustration above / LR-22) now resolves to 8823 and works
  only because 8823 happens to answer. And it doubled the log: both instances tick the
  same 5s stall loop, so those warnings appear twice ~200ms apart, which is 77% of the
  24h log volume and buried the lock error above.
  DOCS NOW WRONG, second time for this class: CLAUDE.md asserts as ground truth
  "re-measured 2026-08-06" that "com.amux.serve.plist is the only server plist on disk"
  and gives `launchctl kickstart -k gui/$(id -u)/com.amux.serve` as THE restart command.
  There are two server plists now, and that command restarts 8823, not the canonical
  port. The note is emphatic that a wrong label costs a debugging session; it is now
  wrong itself.
FIX: Not applied — choosing which job is canonical can take the dashboard down, and a
  dev instance with its own AMUX_HOME is a legitimate configuration this could also be
  (ethos rule 8). Needed: decide, `launchctl bootout` the loser, delete its plist,
  correct CLAUDE.md's launchd note.

## frustrations.md logged from ~/Developer/amux is stranded — that checkout cannot push
AREA: instruments
SEVERITY: blocks
STATUS: open
DATE: 2026-08-17
SESSION: amux-errors-and-bugs
CARD: AEAB-18
SYMPTOM: Two copies of this file exist and the one a session is pointed at is the one
  that cannot reach anyone. `~/Developer/amux/frustrations.md` holds 25 entries /
  43,934 bytes; `~/amux/frustrations.md` (origin/main) holds 116 / 207,952. Same file,
  same lineage — the local one is a stale revision that has ALSO diverged, holding at
  least one entry that never reached origin. CLAUDE.md and `.claude/rules/frustrations.md`
  both live in the stale checkout and say to append to "frustrations.md at the repo
  root", which for a session cwd'd there resolves to the stranded copy. The append
  succeeds. There is no error.
COST: All four frustration entries from today's log review went into the stranded copy.
  The whole argument for this file is that a single frustration is a complaint and a
  cluster is an argument — three entries sharing an AREA is the signal. That only works
  if they are in the file everyone reads. Mine were invisible to every other session and
  to any AREA tally run upstream, and would have stayed so indefinitely: the unblocker
  is the 4-unpushed-commit divergence that has been an open owner decision since
  2026-08-13.
  Distinct from that divergence rather than a restatement of it: that one is "the
  checkout cannot fast-forward", which announces itself. This one is "the documented
  place to log friction is INSIDE that checkout", so the divergence silently swallows
  new writes instead of blocking a read.
FIX: Migrated today's four entries here and verified against
  `scripts/frustrations_audit.py` — no new structural problems, all four CARD ids
  resolve on the live board. The underlying choice is open and worth making
  deliberately: (a) resolve the divergence so the checkout syncs again — owner's call,
  needed regardless; (b) point the rule at the build source, which can push, and say why;
  (c) have the rule REFUSE to append to a checkout that is behind origin, or at minimum
  warn. (c) is the one that survives the next time two checkouts drift, because this
  failure is silent by construction.

---
## amux was down for 4h26m and recorded that fact nowhere — the outage was visible only as a GAP between two log lines
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-18
SESSION: amux-errors-and-bugs
CARD: AEAB-29
SYMPTOM: The user asked "My prompts are hanging. What's wrong with my Amux server?" The
  server had not been hanging — it had been DOWN from 14:03 to 18:29 EDT. The only
  evidence anywhere was that `server-rs.log` had no line between `18:03:32Z` and
  `22:29:24Z`. Nothing counted the gap, nothing named it, nothing surfaced it. The
  restart logged `starting amux-rust`, byte-identical to what it logs after a routine
  5-second auto-adopt reload — same framing for a 5s blip and a four-hour outage.
  Reconstructing it took `kern.boottime`, `last`, and a macOS ResetCounter diagnostic,
  none of which are amux.
COST: The user had to notice by their own prompts failing and open a ticket to ask.
  ~35 minutes of forensics to establish something the server itself knew and did not
  say. Worse, and still live elsewhere: every duration amux reports is measured against
  wall-clock time that may include hours with no server, and nothing marks which. The
  same log carries `steering queue STALLED ... queued=1 oldest_min=2131
  reason=not-running` — some of those 2131 minutes were a genuinely wedged lane and
  some were minutes when no amux existed to deliver anything. The instrument cannot
  express the discriminator (ethos rule 4), so any conclusion drawn from that number is
  a guess with a confident-looking figure attached.
FIX: Fixed on branch `fix/server-downtime-record`. A liveness heartbeat row stamped
  every 15s; at boot the previous stamp is read and written inside ONE `Store::write`
  closure (the writer thread's `BEGIN IMMEDIATE` is what makes it atomic across the two
  amux-server processes sharing this DB, so one outage files exactly one row, not two).
  A gap past `AMUX_DOWNTIME_MIN_S` (default 120s, comfortably above a self-adopt
  restart) logs at WARN, persists to `server_downtime`, and publishes as
  `downtime_before_boot_s` on `/health` — the endpoint every consumer already polls —
  plus `GET /api/debug/downtime` for the history. `downtime_within()` is there so the
  stall/rot/schedule readers can subtract the part that was not the lane's fault; wiring
  those callers is NOT done and is the obvious follow-up.
  The heartbeat write is `applied: false` on purpose: `applied` bumps the global
  revision that SSE delta-sync hangs off, so a heartbeat that "applied" would make every
  connected dashboard refetch the world every 15 seconds — a detector whose cost lands
  on the same resource it watches.
  Tests assert BOTH directions and were verified by mutation, not by being green: making
  the detector never fire fails 4 of 8, making it fire unconditionally fails 4 of 8, and
  the positive case is rebuilt from this incident's own numbers (14:03 -> 18:29, 266
  minutes) rather than from a conveniently-shaped one.

---
## The two causes behind that outage are not amux bugs, and amux had nothing to say about either
AREA: instruments
SEVERITY: annoys
STATUS: open
DATE: 2026-08-18
SESSION: amux-errors-and-bugs
CARD: AEAB-28
SYMPTOM: The machine was up and on the network at 15:18; amux did not start until the
  console login at 18:28 — 3h10m later. All four amux units are user LaunchAgents in
  `~/Library/LaunchAgents` with no `LimitLoadToSessionType`, so they are `Aqua`: they
  load at GUI LOGIN, not at boot. `ls /Library/LaunchDaemons | grep -i amux` -> none.
  `RunAtLoad=true` is doing exactly what it says; "load" just never happened. Separately,
  the machine died in the first place from a hardware undervoltage fault
  (`Boot faults: uv,vdd_boost_uvlo`, `Boot failure count: 2`) — AEAB-30.
COST: Turned a ~75-minute hardware outage into a 4h26m amux outage. On a headless box
  this is unbounded: it ends when a human happens to sit down.
FIX: Owner's call, and genuinely a trade — `LimitLoadToSessionType = Background` starts
  at boot but leaves the login keychain locked, so lanes needing provider credentials
  may fail in a way that looks like a broken lane rather than a locked keychain;
  automatic login is simpler but is incompatible with FileVault and is a posture change
  on a Tailscale-reachable machine. Filed rather than chosen (ethos rule 8). What is NOT
  the owner's call and should ship regardless: `install.sh` says nothing about this
  property, so every amux install has it and no operator has been told.

---
## The disk detector times out on exactly the directories that would answer the question
AREA: instruments
SEVERITY: slows
STATUS: fixed (half — the false cause; the bias is carded)
DATE: 2026-08-19
SESSION: amux-errors-and-bugs
CARD: AEAB-33
SYMPTOM: `disk: size ranking is INCOMPLETE — these paths exceeded the du budget
  skipped=3 paths=~/Library/Caches, ~/.Trash, ~/.npm` — 1178 times in 24h, unchanging.
  Measured by hand with `du` unbounded, those paths hold 9.9G + 6.4G, against 3.7G free.
  `du` time scales with size, so the budget skips the LARGEST directories by
  construction: the ranking that survives lists the also-rans as the top consumers. And
  a failure and a timeout both returned `None` and were reported under the same "exceeded
  the du budget" message, so an unreadable path was indistinguishable from a slow one.
  CORRECTION, measured in prod AFTER the fix shipped and recorded here rather than left
  standing: I claimed `~/.Trash` was the specimen — "Operation not permitted, not slow".
  That is true of MY SHELL and NOT of the server. The launchd-spawned server has the
  access my interactive shell lacks, so it really does walk `.Trash` and really does time
  out; prod has logged ZERO "could not be READ" lines since deploy and `.Trash` still
  appears in the timed-out list. I measured from the producer's vantage and attributed it
  to the consumer, which is the exact inversion ethos rule 4 warns about. The code change
  is still correct — an unreadable path and a timeout genuinely need different responses —
  but the motivating example was wrong, and no test would have caught that because the
  test asks `du` itself whether the case is live and correctly skipped when it was not.
COST: The auto-filed card AMUX-30 ("disk: 4.2 GB free, below the 50 GB floor") says of
  itself "It is a REPORT, not a diagnosis: the evidence below is computed, the cause is
  not." The cause was missing because this ranker skipped it — so the one card raised
  about a disk that has since fallen to 3.7G could not name a single consumer. Anyone
  acting on the log message would also have raised the timeout, which cannot ever help
  `.Trash` and re-opens AMUX-35 (an unbounded `du` here parked every tokio worker and
  hung the entire server).
FIX: `du_one` now returns a typed `DuOutcome` — `Sized` / `TimedOut` /
  `Unreadable(exit code)` — and `du_top` reports the two skip reasons as separate
  warnings, one naming the budget and one saying plainly that no timeout increase will
  help. The timeout message now also states the bias out loud rather than leaving it to
  be inferred: the paths missing are likelier to be the largest than the ones ranked
  below it. Exit code rather than stderr text is deliberate — piping stderr means `du`
  over a tree of unreadable subdirectories can fill the pipe buffer and block while we
  poll, converting a path that would have finished into a timeout, i.e. the detector
  manufacturing the fault it reports.
  Tested against the incident's own specimen (`~/.Trash`, the path named 914 times),
  with a `du` probe deciding whether the case is live so it can never pass by mistaking
  an absent directory for a permissions error, and mutation-checked: collapsing the two
  outcomes back into one fails it.
  The BIAS itself is fixed in the follow-up commit on the same card: each path's last
  successful size is persisted to `~/.amux/du-sizes.json` and carried forward, LABELLED
  with its age, so a path that blows the budget still appears in the ranking instead of
  vanishing. A day-old 9.9G answers "what is eating my disk"; absence does not, and
  absence is what it reported. On disk rather than in memory because this process
  re-execs to adopt builds — in-memory state would be empty on exactly the first run
  after every restart. A path never successfully sized still does NOT appear: a
  fabricated 0 would sort the biggest consumer last, which is the failure `du_one`'s own
  doc comment warns about, and there is a test asserting each direction.
  That work also surfaced a LATENT TEST BUG worth naming on its own: the du budget lives
  in process-global env vars and cargo runs these cases on parallel threads, so three
  pre-existing cases were already racing (two writing the vars, one reading). It had
  never been visible because the budgets in play were large enough not to break a
  neighbour. A new case using a 1ms budget broke one, and the failure appeared in a test
  with nothing wrong in it. All seven now take a shared lock.

---
## The low-disk emergency clear deleted the cache it was about to fill, and spared a 4x larger idle one
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-19
SESSION: amux-errors-and-bugs
CARD: AEAB-34
SYMPTOM: `scripts/rust-auto-build.sh` cleared `~/.amux/rust-build-target` unconditionally
  when free space fell below its floor — the cache filled by the very next line of the
  same script — while `~/.amux/rust-build-target-e2e-head` sat untouched beside it at
  4.2 GB. From the builder's own log: fired 16 times, free space still reached 1 GB, and
  each pass freed ~2 GB that the immediately-following cold build put straight back.
  Separately, `api/reclaim.rs` — the subsystem whose whole job is finding reclaimable
  space — had `rust-build-target` in its candidate list and not `-e2e-head`, so it was
  blind to the largest reclaimable directory amux owns.
COST: Every build on this machine today was cold (~1m40s instead of ~15s incremental),
  16 times over, and the pressure was never relieved — the volume still reached 1 GB free.
  The guard was doing the maximum possible damage per byte reclaimed: the most expensive
  cache to lose, the least space freed, and it regrew immediately. It also masked the real
  answer, since a "DISK LOW, clearing the target dir" line reads like the guard working.
FIX: Clear candidates in order of what the current build does NOT need — idle e2e cache
  first, re-measure, and touch the shared target dir only if still under the floor. Moved
  ahead of the worktree checkout, because that writes 1000+ files and freeing space after
  consuming it is the wrong order on a nearly-full volume. `-e2e-head` added to reclaim's
  candidates and its allow-list guard test.
  `scripts/test-build-disk-clear.sh` (6 cases, wired into checks.yml) runs the SHIPPED
  script through a dry-run seam and asserts ORDERING, which is the whole fix.
  Mutation-checked three ways: shared-only fails the ordering case, unconditional clearing
  fails the above-the-floor control, and never-clearing-the-shared-dir fails the
  last-resort case — that last mutation passes the ordering test while reintroducing the
  disk-full outage the guard exists to prevent, which is exactly why that case is there.
  The hand-maintained candidate list is the deeper issue and is NOT fixed: it will go stale
  again the next time a directory appears. Noted on AEAB-34 rather than papered over.

---
## The build-disk guard used a BSD-only `df -g`, so it was a silent no-op on Linux
AREA: instruments
SEVERITY: slows
STATUS: fixed
DATE: 2026-08-19
SESSION: amux-errors-and-bugs
CARD: AEAB-34
SYMPTOM: `scripts/rust-auto-build.sh` measured free space with `df -g` and candidate
  sizes with `du -sg`. Both flags are BSD-only. On GNU coreutils: `df: invalid option --
  'g'`, so `FREE_GB` comes back EMPTY, `${FREE_GB:-999}` substitutes 999, and the disk
  guard never fires — while printing nothing that says it did not run.
COST: Zero on this machine (macOS, where the flags work), which is exactly why it went
  unnoticed. The cost is latent and it is the shape that matters: a guard whose failure
  mode is "silently does not run" on any Linux host, in a repo whose single-codebase rule
  says the same code must work in both places. Found ONLY because a new test ran it in CI
  — and even then the ordering assertions caught it INCIDENTALLY, because the script died
  rather than because anything asserted the number.
FIX: `df -Pk` and `du -sk` with the GB conversion in awk — POSIX, and already the
  convention on the Rust side (`storage::disk_free_bytes` has used `df -Pk` all along).
  So this was one convention inside the codebase drifting from another, not an oversight
  about portability in general.
  Two new assertions in `scripts/test-build-disk-clear.sh` check that the free-space and
  candidate-size figures PARSE TO A NUMBER, because the ordering cases cannot: they force
  the branch with a huge floor, so an unparsed figure still reaches the loop and the
  regression is caught only by luck. Mutation-checked: restoring a bad df flag fails case
  (f) specifically, not just the ordering cases.

---
## `amux board done --outcome-stdin` printed a warning about the outcome and silently applied NOTHING
AREA: cli
SEVERITY: slows
STATUS: open
DATE: 2026-08-19
SESSION: amux-errors-and-bugs
CARD: AEAB-36
SYMPTOM: Closing AEAB-34, the entire output was:
    warning: outcome NOT recorded — server sent no JSON
  Verified against the API immediately afterwards: status still `review`, desc_len
  unchanged at 2792, no new log line. NEITHER the outcome NOR the status transition
  landed. Re-running the identical command with the identical ~2.9KB input succeeded
  completely (`AEAB-34 → done`, EXIT=0, desc +2915 chars). Nothing appeared in
  server-rs.log for the failed request.
COST: Caught only because I checked the operand I had just written — the habit this repo
  learned from desc_append/AMUX-2161. Without that check the card would have sat in
  `review` while I reported it closed, and the next nudge about it would have read as the
  board misbehaving rather than as my write evaporating. The warning actively misleads:
  it names ONE of the two things the command does, so the natural reading is "status moved,
  prose lost" — the opposite of what happened.
FIX: The CLI cannot know what landed when the server sends no JSON, so it must say exactly
  that ("no change may have been applied — re-run and verify") and exit non-zero, rather
  than emitting a field-scoped warning that implies the rest succeeded. Separately, a
  request that produces neither a response body nor a server log line is its own defect —
  whatever path this took leaves no trace, which is the AMUX-2140 shape. Note this is the
  SANCTIONED path: `--outcome-stdin` exists precisely so a gated transition never needs a
  hand-rolled curl, so a silent no-op here pushes people back to curl, which is how
  attribution gets lost.
---
## I wrote a keep-warm branch that could never execute, hours after writing about that exact failure mode
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-19
SESSION: amux-errors-and-bugs
CARD: AEAB-35
SYMPTOM: AEAB-34's fix added an early exit to the builder's disk guard — reclaim the idle
  cache, and if free space is back above the floor, keep the shared cache so the build
  stays warm. The branch fired ZERO times and could not: it tested against
  `AMUX_BUILD_MIN_FREE_GB` (25GB) on a volume with 4GB free, so after reclaiming the idle
  cache the condition was still false and the shared cache was destroyed anyway.
  `grep -c "reclaimed to" rust-auto-build.log` -> 0.
COST: Half a fix presented as a whole one. The card AEAB-34 is titled "so every build is
  cold" and every build stayed cold; only prod verification caught it, and only because I
  looked for the branch's OUTPUT rather than re-reading the code. Had I not, the card would
  have closed claiming a result it did not deliver.
FIX: Two thresholds, because there are two questions —
  `AMUX_BUILD_MIN_FREE_GB` (25) asks "is the FLEET at ENOSPC risk", which is what decides
  whether to reclaim at all; `AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB` (8) asks "can THIS BUILD
  proceed while keeping its cache". Between them, reclaim the idle caches and stay warm.
  Two test cases that EXECUTE the branch, driving the shipped script with a fake `df`
  placed in `$HOME/.cargo/bin` — the script exports its own minimal PATH whose first entry
  is exactly that, so no test-only branch is added to production code. Mutation-checked:
  restoring the single threshold fails the keep-warm case specifically.
WHY IT IS WORTH AN ENTRY EVEN THOUGH I FIXED IT: the ethos file records "a threshold below
  the baseline is not a detector", and this is its mirror — a stop condition ABOVE the
  achievable maximum is not a stop condition. I had cited that rule in a commit message the
  same morning. Authoring a rule does not install the habit; that is the third instance of
  that sentence being true in this session, and the pattern is only visible because the
  earlier two were also written down. The generalisable check: when adding an early exit,
  state the RANGE OF INPUTS under which it fires, and confirm the live system produces
  values in that range.

---
## `set -euo pipefail` turned a deliberately non-fatal board write into a silent abort
AREA: cli
SEVERITY: blocks
STATUS: fixed
DATE: 2026-08-19
SESSION: amux-errors-and-bugs
CARD: AEAB-36
SYMPTOM: SUPERSEDES the earlier entry "board done --outcome-stdin printed a warning about
  the outcome and silently applied NOTHING" — that entry recorded the symptom correctly and
  called the cause unknown. It is now known and reproduced deterministically.
  The outcome write is `curl ... | python3 -c ...`, and the python exits 0 on failure ON
  PURPOSE so a lost outcome never blocks the transition that follows. `set -euo pipefail`
  (amux line 19) defeats that entirely: on a TRANSPORT failure curl exits non-zero,
  `pipefail` makes the pipeline return curl's status rather than python's, and `set -e`
  aborts the script right there — AFTER the warning has printed. One warning, scoped to
  the outcome, and the transition never ran and never said so.
  The transition write was worse. Not piped at all, so `result=$(_board_transition ...)`
  aborted the caller directly: `amux board done <ID>` against an unreachable server printed
  ABSOLUTELY NOTHING and exited 7.
  Reproduced with `AMUX_API=http://127.0.0.1:9`: one warning + exit 7 with an outcome,
  total silence + exit 7 without one — byte-identical to the live incident.
COST: A card reported closed that was still in `review`, caught only by re-reading the
  operand. The live trigger was the server restarting to adopt a build I had just merged —
  so the failure fires exactly when amux is deploying itself, which is when a session is
  most likely to be closing cards. And the warning's wording actively misleads: naming only
  the outcome makes "status moved, prose lost" the natural reading, which is inverted.
FIX: Capture curl's OWN exit status at both sites instead of piping it, and on a transport
  failure say plainly that NOTHING was applied — naming both writes — then stop. A dropped
  connection and a refused gate now read completely differently, which matters because one
  should be retried and the other must not be.
  `scripts/test-board-transport-failure.sh` (11 cases, wired into checks.yml). Its (c)/(d)
  controls are the load-bearing half: a CLI that shouted "cannot reach the board" at every
  error would pass both failure cases while destroying that distinction, and
  mutation-checking confirms it fails (c) and (d) specifically.
  THE GENERAL LESSON, worth more than the fix: `set -e` + `pipefail` silently overrides
  every `sys.exit(0)` written to make a step non-fatal. Any `cmd | python3 -c '...exit(0)'`
  in this file has the same defect latent. Grep for the pattern before trusting that a step
  is really optional.

---
## The "oldest undeployed commit" age was the NEWEST one, so an overnight lag read as 3 minutes
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-20
SESSION: amux-errors-and-bugs
CARD: AEAB-39
SYMPTOM: The SessionStart hook printed "the RUNNING SERVER is 4 commit(s) behind
  origin/main / built 9f92a631f; oldest undeployed commit landed 3 minutes ago". The oldest
  undeployed commit was 23 HOURS old (7ac99632). The line computed
  `git log -1 --format=%cr <range> | tail -1`, and `-1` limits git to one commit — the
  newest — so `tail -1` never sees a second line.
COST: The count was right; the number that makes it ACTIONABLE was always small. This line
  exists to separate "just merged, the builder is about to pick it up" from "a fix has sat
  undeployed overnight", and only the second is worth acting on — so the alarming case is
  precisely the one it could never display. It would have read as reassuring every time,
  forever. I shipped it yesterday in the same commit whose message argued that a bare count
  "reads as bookkeeping" and the age is what makes it actionable.
FIX: `git log --reverse --format=%cr <range> | head -1`. New case (i2) in
  test-session-freshness.sh builds a repo with one commit dated 2020 and one dated now, so
  taking the newest says "seconds ago" and taking the oldest says "years ago". The 25-case
  suite passed with the bug present because every other case asserts the COUNT — a whole
  suite around a line, none of it testing the field that carries the meaning.
  Fourth instance this session of a check I wrote reporting something plausible but wrong
  (LR-37 filter, LR-42 vantage, LR-43 proximity grep, this). The common shape is not
  carelessness: each probe RAN and returned something usable, so nothing prompted a
  recheck. What catches them is comparing the output against an independently-known value —
  here, `git log` over the same range without `-1`.

---
## A ghost-rescue log quoted a pane scrape as the user's message, and it reads as corruption
AREA: instruments
SEVERITY: annoys
STATUS: fixed
DATE: 2026-08-20
SESSION: amux-errors-and-bugs
CARD: AEAB-38
SYMPTOM: At 00:50 EDT the owner sent a real message from their phone; the send path failed
  to submit it and ghost-rescue recovered it. The WARN line quoted it as
  "Canyoufightthisforme?Don'tsendbutdodraftandaddresstotherightperson" — every space gone.
  That reads unmistakably as the prompt being corrupted somewhere in delivery.
COST: Nearly a filed user-facing data-corruption bug, on the owner's own overnight message,
  which is about the worst thing to be wrong about. Cost ~10 minutes to disprove.
FIX: The preview is a PANE SCRAPE. `composer_state` builds it with
  `plain.extend(p.split_whitespace())` — words concatenated, whitespace discarded — which
  is CORRECT for its actual job (comparing two captures of a WRAPPED terminal line, where
  whitespace is an artifact of the wrap) and wrong for anything a human reads. The
  discriminator: the board card auto-captured from the same send (PP-7) has the text intact,
  and the rescue presses Enter on what is already in the composer rather than retyping, so
  delivery cannot alter it. Storage unchanged — altering it would change the equality check
  and `is_amux_ghost` to fix a display problem — and the LINE now says what the preview is.
  This is the D1 terminal-scraping deviation showing through: a value computed for MACHINE
  COMPARISON was reused for HUMAN DISPLAY, where its lossy normalisation reads as
  corruption. Before quoting a scraped value in a log, ask what was normalised out of it.
