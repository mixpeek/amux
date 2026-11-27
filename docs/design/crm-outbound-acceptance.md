# CRM outbound: the acceptance test plan

Owner, 2026-11-27: "use this sequence lifecycle as a test plan ... so that we can always
use this test plan as the acceptance criteria for the CRM being finished", then "we also
need to have the ability to send from different domains ... so that we can fully replace
Instantly ... and then we can do external enrichment using, for example, Hunter.io or
Apollo ... Exa for LinkedIn search, or even Google search".

The CRM is finished when every row of the summary below reads **Pass**, and the
lifecycle in sections 2 to 9 runs end to end on the three test companies with no step done
by hand outside amux. Design background: `docs/design/crm.md`.

Owner, 2026-11-27, on review: the plan "proves the opening motion well. The gap is that it
barely covers closing, and closing is the point. As written, all 34 rows could read Pass and
a deal would still never move past `lead`." Rows 35 to 54 and sections 10 to 16 add closing,
reply intelligence, measurement, approval throughput, suppression, address validation and
the human boundary. This plan runs the go-to-market engine for the next month: the IP check
and creative-dna focus, on the three test companies.

Statuses:

- **Pass**: built, and a test exercises it (the evidence column names the test).
- **Built**: the capability exists in amux, but this plan has not been run through it end to end yet.
- **Partial**: some of it exists; the gap is named.
- **Missing**: amux cannot do it today.

## 1. Executive summary: every requirement against what amux has

| # | Requirement | amux capability | Status | Evidence, or what is missing |
|---|---|---|---|---|
| **Targeting** | | | | |
| 1 | Each company and person carries an ICP grade and why | `icp_grade` (A to D) and `icp_reasons` on `crm_accounts` and `crm_contacts`; ICP filters in the CRM tab | Pass | `e2e/crm-tab.spec.ts` grades a person and reads it back |
| 2 | Companies are one record each, by domain | `crm_accounts.domain` is the dedupe key; people link by `account_id`; a company with no domain gets its people's work-mail domain | Pass | `crm_outbound` unit test `..._companies_get_their_domain` |
| 3 | People of three kinds: already signed up, already talked to, LinkedIn connections | `crm.signup` makes a person, company and deal from a Cloud order; logged interactions; imports carry `source` | Built | `crm_signups` unit tests; the LinkedIn connection import is a one-time worker job, not a sync |
| 4 | Lookalike companies | `crm_lookalikes`, "Add lookalike", and "Ask a worker" to find more | Pass | `e2e/crm-tab.spec.ts` adds a lookalike and reads it from both sides |
| 5 | New people found from signals land in the CRM with where they came from | `POST /api/crm/contacts` with `source` (`worker:<name>`, `lookalike:<seed>`) | Built | Nothing yet checks that a signal-found person's `source` names the signal |
| 6 | Enrichment from Hunter.io or Apollo (emails, titles, company data) | none | Missing | No connector and no `enrich` verb. Needs a connector per provider (keys in `server.env`, paid: the owner's call) and a field recording which provider supplied each value |
| 7 | People search through Exa (LinkedIn) and Google | Workers have web search and the signed-in browser route | Partial | Works only as worker judgement; no CRM-native search that returns candidates with provenance |
| **The sequence** | | | | |
| 8 | A sequence of steps with day offsets, mixed channels, one worker drafting each | `crm_sequences` (`steps: [{day, channel, action, template}]`), enroll by ids or by filter (grade, tag, status, source) | Pass | `e2e/crm-tab.spec.ts` "a sequence mixes online and offline steps" |
| 9 | Email step | channel `email`, action `email`; the worker sends through `/api/email/send` after approval | Pass (draft and approve), Built (the send) | The e2e approves; the send itself goes through the email API's own approval hold |
| 10 | LinkedIn connect and DM | channel `linkedin`, actions `connect`, `dm`; the worker acts in the signed-in browser; browser sends are held in the Outbound tab | Pass (draft and approve) | `e2e/crm-tab.spec.ts` approves a LinkedIn DM; `e2e/outbound-tab.spec.ts` covers the held browser send |
| 11 | X follow, like and DM | channel `x`, actions `follow`, `like`, `dm` | Built | Same path as LinkedIn; no X-specific e2e |
| 12 | Public comments on their LinkedIn and X posts | action `comment` with `target_url` (the post); `{signal}` names their latest post | Partial | Nothing picks the post to comment on: the worker must find it and set `target_url` |
| 13 | Offline: a letter, a handwritten note, a gift, a gift card | channel `post` with actions `letter`, `note`, `gift`; recorded done from the tab | Partial | Recording works (`e2e/crm-tab.spec.ts` records a letter done). No fulfillment service (for example Lob, Handwrytten, Sendoso, Tremendous) that prints, mails or buys. Spending goes through `amux order` and the owner's card rules |
| 14 | Events and in-person touches | channels `event`, `in_person`; actions `invite`, `meet`, `visit` | Built | |
| 15 | Stops: a reply, a booked meeting, an unsubscribe, a bounce, do-not-contact; no double-booking a channel; a cooldown between touches | stop on reply; a logged `meeting` or `booking` (gtm-engine logs each /demo booking that way) ends the person's sequences and emits `crm.contact.booked`; a reply asking to stop marks them unsubscribed; the guard refuses outward touches; `AMUX_CRM_TOUCH_COOLDOWN_DAYS` | Pass | `crm_outbound` unit tests, including `a_booked_meeting_stops_the_persons_sequences_and_is_an_event`; `e2e/crm-tab.spec.ts` blocks a draft after its person opts out. What a booking should also do to the deal is row 35 |
| 16 | The step that is due wakes its worker | `crm.touch.due`, once per touch | Pass | `crm_outbound` unit tests |
| 17 | The owner approves every outward touch | `POST /touches/{id}/approve` is owner-only; the CRM's Outbound review | Pass | `e2e/crm-tab.spec.ts` outbound review test |
| **What each touch says** | | | | |
| 18 | Every touch is written from the current notes, opportunity and assets | Templates fill `{first_name}`, `{name}`, `{company}`, `{role}`, `{signal}`, and from the person's open deal `{deal}`, `{next_step}` and `{asset_url}`, read when the step is planned; the brief carries each deal's notes, MRR, app link and lost reason | Pass | `a_deal_runs_from_reply_to_won_...` fills `{asset_url}` and `{deal}` from the live deal. A draft already written keeps its text: a worker redrafts it when the deal changes |
| 19 | A static app per opportunity (no backend, loads instantly) that shows their use case | `crm_deals.asset_url` (the static app made with `amux publish`), shown as "Open the app" on the deal and given to every draft as `{asset_url}` | Built | A password for the app goes in the deal's notes; nothing yet checks the link answers before a send beyond the voice check's link check (row 21) |
| 20 | Founder voice and anti-slop on every draft | the voice check (VOX-1) on create and edit; the slop attestation; both shown as checks in the review | Pass | `e2e/crm-tab.spec.ts` voice test; `crm_voice` unit tests |
| 21 | Accuracy: every link in a draft answers | the voice check's link check, shown in the review | Built | |
| 22 | Research before each touch | the brief; "Ask a worker"; "Ask the worker to redraft" from the review | Built | |
| **Signals** | | | | |
| 23 | Watch a careers page or website and act on what is new | `crm_channels` with `watch`; the `crm-watch` job fetches; `crm.channel.changed` | Pass | `crm_agent` unit test; `e2e/crm-tab.spec.ts` watched careers page reaches the timeline |
| 24 | Watch their LinkedIn, X and Instagram posts | channels with `how=browser`: a worker checks through the signed-in browser and reports `/observed` | Built | Needs a schedule per worker; no e2e |
| 25 | PR and announcements | watch the company's news or press page as a `website` channel | Partial | No news or PR feed connector; a press page that renders in script may not fetch |
| 26 | A worker subscribes to a person or a company | `amux subscribe` over the `crm.*` events; the Activity view lists them | Pass | `e2e/crm-tab.spec.ts` checks the event in Activity |
| 27 | Replies captured into the timeline | `crm.contact.replied` from mail and LinkedIn messages | Built | For connected Gmail accounts only |
| **Sending, to replace Instantly** | | | | |
| 28 | Send from several domains and mailboxes (trymixpeek.com, getmixpeek.com, ...) | `/api/email/send` sends `from` any connected Google Workspace or Gmail account (`/api/gmail/auth?account=`) | Partial | A sequence cannot choose the mailbox per step or per person |
| 29 | Rotate across mailboxes with a daily cap per mailbox | none | Missing | Needs a mailbox pool per sequence and a cap the send path enforces |
| 30 | Warm-up | none | Missing | Needs a warm-up schedule per new mailbox (send and reply among the pool, ramping volume) |
| 31 | Deliverability: bounce and reply rate per mailbox, spam placement | `bounced_at` per person | Partial | No per-mailbox rates and no placement test |
| 32 | An unsubscribe link in every cold email, honoured on the spot | a reply asking to stop marks them unsubscribed | Missing | No unsubscribe link or footer. Cold email needs one (CAN-SPAM, and Gmail's bulk sender rules) |
| **The owner's view** | | | | |
| 33 | One review queue with full context, on desktop and phone | the CRM's Outbound: message, brief, sequence step, checks; Approve, Edit, Decline, Redraft | Pass | `e2e/crm-tab.spec.ts`, `e2e/crm-layout.spec.ts` (every section at 375 to 1920 px, both themes) |
| 34 | A record of everything that happened | the Activity view over `crm.*` events; each person's timeline | Pass | `e2e/crm-tab.spec.ts` |
| **Closing the loop** (section 10) | | | | |
| 35 | A booking moves the deal to `demo` and plans a pre-demo prep touch | a booking stops the sequences, moves the person's open deal (or a new one) to `demo` with `next_step` "prep the demo", and plans a `note` touch for whoever last worked them; `crm.contact.booked` carries the move and the prep touch | Pass | `crates/amux-server/src/api/crm_close.rs` tests; `e2e/crm-tab.spec.ts` "closing" on desktop, mobile and ios-safari |
| 36 | A positive reply advances the deal to `qualification` | a reply opens the deal at `lead` with `next_step` "read their reply and record its intent"; an interested, book-later or objection intent moves it to `qualification` | Pass | `crates/amux-server/src/api/crm_close.rs` tests; a hand-logged reply (direction in) counts the same as a captured one |
| 37 | After the demo: `trial` or `proposal`, with `next_step` | the owner moves it from the deal record; a worker may move a deal up to `trial` | Partial | No demo-done verb yet: the move after the call is a PATCH (worker to `trial`, owner to `proposal`) |
| 38 | A signature closes the deal `won` with amount and close date | `won` refuses to save without `amount` (the contract value) and `close_date` (`crm_deal_won_incomplete`); `mrr_usd` is optional, for a deal that really is a subscription | Pass | `crates/amux-server/src/api/crm_close.rs` tests; the e2e signs a deal from the deal record only after both are in |
| 39 | A no closes the deal `lost` with a reason | `lost` refuses to save without `lost_reason` from no_budget, no_need, timing, competitor, no_response, other (`crm_deal_lost_reason`); a select on the deal record | Pass | `crates/amux-server/src/api/crm_close.rs` tests |
| **Reply intelligence** (section 11) | | | | |
| 40 | A worker reads each reply and records its intent: interested, book later, objection, not interested, referral | `POST /api/crm/replies/{interaction_id}/intent`, stored on the interaction (`intent`, `intent_why`), emitting `crm.reply.classified`; `crm.contact.replied` now carries the `interaction_id`; the timeline shows the intent | Pass | `crates/amux-server/src/api/crm_close.rs` tests `each_intent_makes_its_move`; `e2e/crm-tab.spec.ts` shows the pill |
| 41 | The intent drives the next move | interested: `qualification`, next step the booking push (`AMUX_CRM_BOOKING_URL` when set). Book later: a follow-up date on the reply. Objection: next step names it. Not interested: sequences stop, `do_not_contact` when asked, the deal `lost`. Referral: a new person with `source: referral:<id>` | Pass | Drafting the booking push, the answer or the intro stays the worker's job, from the event |
| **Measurement** (section 12) | | | | |
| 42 | The campaign funnel: enrolled, contacted, replied, booked, demoed, won, with conversion per step, per sequence and per cohort | `GET /api/crm/funnel?group=sequence|cohort&sequence=&cohort=&since=` on `crm.funnel.v1`; enrollments carry a `cohort`; the CRM tab's Funnel section | Pass | `crates/amux-server/src/api/crm_close.rs` tests; `e2e/crm-tab.spec.ts` reads the cohort's card |
| 43 | Value signed per sequence or cohort (contracts; MRR only where a deal is a subscription) | `won_amount_usd` sums the won deals' contract value per group; `mrr_usd` (optional on a deal) sums apart, for subscriptions such as amux Cloud | Pass | `crates/amux-server/src/api/crm_close.rs` tests |
| 44 | "Demoed" and "won" are counted from what happened, even after a deal moves on or is lost | `crm_deal_stages` (from, to, at, by, why), backfilled with each deal's stage when it began; `GET /api/crm/deals/{id}/stages`; `crm.deal.stage`; the deal record shows the history | Pass | `crates/amux-server/src/api/crm_close.rs` tests: a deal won then lost still counts as demoed |
| **Approval throughput** (section 13) | | | | |
| 45 | Low-risk touches (like, follow, watch) run without the owner while email, DM, comment, connect with a note, letter and gift still hold | `AMUX_CRM_AUTO_ACTIONS` in `~/.amux/server.env` (default `like,follow,watch`, plus an internal `note`): those run and are recorded done with no approval; the guard and cooldowns ignore them | Pass | `crates/amux-server/src/api/crm_close.rs` tests: a like is done with no approve while a DM waits |
| 46 | The owner clears a batch quickly | Select drafts on Pending, then Approve N or Decline N with one note (desktop; a phone decides one at a time) | Pass | `e2e/crm-tab.spec.ts` "bulk" |
| **Suppression** (section 14) | | | | |
| 47 | No outreach to an existing customer | the guard refuses a cold touch to a person with `status=customer`, at an account with a `won` deal, or tagged `customer` (`crm_touch_customer`) | Pass | `crates/amux-server/src/api/crm_close.rs` tests |
| 48 | No outreach on a deal the owner runs himself | an open deal at their account whose `owner` is in `AMUX_CRM_OWNER_NAMES` (default `owner`) refuses workers' touches (`crm_touch_owner_deal`) | Pass | `crates/amux-server/src/api/crm_close.rs` tests |
| 49 | No outreach to competitors | an account tagged `competitor` refuses cold touches (`crm_touch_competitor`) | Pass | `crates/amux-server/src/api/crm_close.rs` tests |
| 50 | Account-level cooldown: two workers never reach two people at one small account in the same week | `AMUX_CRM_ACCOUNT_COOLDOWN_DAYS` (default 7) for accounts with at most `AMUX_CRM_ACCOUNT_COOLDOWN_MAX_PEOPLE` (default 200) people (`crm_touch_account_cooldown`; `force` passes it, like the per-person cooldown) | Pass | `crates/amux-server/src/api/crm_close.rs` tests |
| **Address validation** (section 15) | | | | |
| 51 | An address is checked before the first cold send | before the first email is approved: syntax, role address, disposable domain, and the domain's MX records over the system resolver, null MX included; stored as `email_check`; `POST /api/crm/contacts/{id}/check-email` | Pass (free half) | `crates/amux-server/src/api/crm_close.rs` tests. The paid mailbox-level check waits on row 6's provider. `AMUX_CRM_MX_CHECK=0` turns the lookup off (the e2e servers) |
| 52 | A failed check holds the send and says why | a failed check refuses the email (`crm_touch_address_unverified`) and the review shows it as the Blocked reason | Pass | `crm_touch_address_unverified` in the unit tests; found live in e2e when a made-up domain had no mail server |
| **The human boundary** (section 16) | | | | |
| 53 | The agent drives to booked and nurtured; the owner runs the demo, sets pricing and signs | workers are refused moving a deal to or from `proposal` or `won` (`crm_deal_owner_stage`); a booking moves the deal to `demo` and emits `crm.deal.stage` | Partial | The refusal is built and tested. The hand-off is the `crm.deal.stage` event to `demo`, which a subscription can turn into a notice; there is no Needs-you card yet |
| 54 | Every hand-off reaches the owner with the full context | the brief, the reply thread and the app link sit on the person and the deal | Built |  |

**Count, over the 54 rows: 32 Pass, 10 Built, 8 Partial, 4 Missing.** Rows 1 to 34 can all pass while a deal never moves past `lead`, so the plan is finished only when rows 35 to 54 pass too. Closing, reply intent, the funnel, suppression and the free address check landed on 2026-11-27; what is left in them is a demo-done verb (37) and a Needs-you hand-off card (53). Waiting on the owner: enrichment and verification providers (6, 7, 51), offline fulfillment (13), and the sending domains (28).

## 2. The test companies

Three companies, two for the IP check use case and one for creative-dna, each with one or
two people. Use real records the owner names from the CRM; the rows below are the shape
each must have before the run starts.

| Company | Use case | ICP | Domain | People | How we know them |
|---|---|---|---|---|---|
| Company 1 | IP check (rights and licensing on a media library) | A, with reasons | set | 2 | one signed up for amux Cloud or Mixpeek; one a LinkedIn connection |
| Company 2 | IP check | B, with reasons | set | 1 | already talked to (a logged call or meeting) |
| Company 3 | creative-dna (creative analysis across ads or brand assets) | A, with reasons | set | 2 | one a LinkedIn connection; one found from a signal during the run |

**Pass:** each company has a domain, an ICP grade with reasons and a lookalike; each person
has an email or a LinkedIn or X channel, a role, a `source`, and notes on why they matter.

**Check:** `GET /api/crm/accounts/<id>` and `GET /api/crm/contacts/<id>/brief` for each;
the CRM tab shows each company's Account fit card and each person's Brief card.

## 3. The opportunity and its asset

For each company:

1. A deal (`POST /api/crm/deals`) with the opportunity in plain words: the use case, their
   pain, what we would show them, and the next step.
2. A static app published with `amux publish`: a single page, hard-coded, no backend and no
   compute, that shows their use case on their own material (for example the IP check run
   over a sample of their public images). It loads in under a second on a phone.
3. The app's link and any password recorded on the deal (today: in the deal's notes; row 19
   asks for a real field).

**Pass:** the app answers at its link from a phone with no sign-in beyond the password; the
deal names it; the person's brief shows the deal and the link.

## 4. The sequence

One sequence spans online and offline, so there is no week in which they do not hear from
us. The worker drafts every step from the brief, and the owner approves every one.

| Day | Channel | Action | What it does |
|---|---|---|---|
| 0 | email | email | A short note on their use case, with the static app's link |
| 1 | linkedin | connect | Connection request with a one-line note |
| 2 | x | follow, like | Follow, and like their latest post |
| 3 | linkedin | comment | A real comment on their latest post (`target_url`) |
| 5 | x | dm | A DM that points at something they posted |
| 7 | email | email | Follow-up with one new thing from the app |
| 10 | post | note | A handwritten note by post |
| 14 | post | gift | A small gift or gift card |
| 21 | linkedin | dm | A direct ask for 20 minutes, with `https://mixpeek.com/demo` |
| any | event | invite | An invite when a meetup they would attend comes up |

**Pass, for every step:**

- The touch is planned on its day (`crm.touch.due` fires once).
- The worker drafts it from the brief, and the draft names something true and current about them.
- The voice check passes or the worker states why not.
- The slop pass is attested, and every link answers.
- The owner approves it in Outbound.
- The worker does it through the connector or the browser and records it done, with a result link where there is one.
- The next step is planned.

## 5. Each touch, as a loop

1. **Research.** Read the brief: notes, deal, app link, the last signal, the last reply.
2. **Draft.** Write in the founder voice. Mention one thing that is true now; use the app when it helps.
3. **Check.** Run the voice check, the anti-slop pass and the link check. A failing draft says
   why it should go anyway, or it is fixed.
4. **Approve.** The owner approves, edits, declines with a note, or asks for a redraft
   (CRM, Outbound).
5. **Do.**
   - Email goes out through the email API from the chosen mailbox.
   - LinkedIn and X go through the signed-in browser route; the send is held for the owner in the Outbound tab.
   - Post and gift steps go through a fulfillment service (row 13, Missing), or the owner does them and records them done.
6. **Record.** `POST /touches/{id}/done` with the result. It appears on the timeline and in
   Activity.

**Pass:** for one person, every step from day 0 to day 21 shows on their timeline in order,
with nothing done outside amux.

## 6. Signals: hyper-targeting from what changes

For each company and person, watch:

| Source | How | Event |
|---|---|---|
| Careers page | `careers` channel, fetched by amux | `crm.channel.changed` with the new roles |
| Website, news or press page | `website` channel | `crm.channel.changed` |
| LinkedIn and X posts | `linkedin`, `x` channels, checked by a worker in the browser | `crm.channel.changed` |
| Replies | captured from mail and LinkedIn | `crm.contact.replied` |
| Signups | Cloud orders | `crm.signup` |

A worker subscribes (`amux subscribe "a new job at a company I watch"`). On each event it
researches the change, decides whether it is worth a touch, and if so drafts one that
names the change. A new hire or a newly found person is added to the CRM with the
signal as `source`, graded, and enrolled.

**Pass:**

- A new role on a watched careers page produces a drafted touch within the worker's next check, naming the role.
- A new person found that way appears in People with `source` naming the signal.
- Every signal appears in Activity.

## 7. Replies and stops

**Pass:**

- A reply stops that person's sequences, and a deal opens.
- A booked meeting (a /demo booking, logged as a `meeting` or `booking`) stops that person's sequences, moves the deal to `demo` and plans a prep touch (section 10).
- A reply asking to stop marks them unsubscribed, and no further outward touch can be drafted or approved for them.
- A bounce does the same for email.
- No two open touches share a channel.
- No outward touch lands within the cooldown of another.

## 8. Sending at scale, without Instantly

**Pass:**

- Two or more domains (for example trymixpeek.com and getmixpeek.com), each with its own Google Workspace mailboxes, are connected as Gmail accounts.
- A sequence names its mailbox pool.
- Sends rotate across the pool under a daily cap per mailbox.
- A new mailbox warms up on a ramp before it carries cold mail.
- Each cold email carries an unsubscribe link that works on one click.
- Bounce and reply rates show per mailbox.
- Replies to any mailbox reach the person's timeline.

Today only the first point and the reply capture hold (rows 28 to 32).

## 9. The owner's view, on every device

**Pass:**

- The CRM tab shows every section with no content spilling out of its box and nothing past the right edge, at 375, 820, 1024, 1280, 1440 and 1920 px, in light and dark.
- A draft's actions stay on one row.
- The phone keeps one row of controls above the list.

**Check:** `e2e/crm-layout.spec.ts` on the `desktop`, `mobile` and `ios-safari` projects,
plus a look at its screenshots.

## 10. Closing the loop

A deal moves on what happens, and the worker sets `next_step` at every move.

| What happens | Stage | `next_step` set by the worker | Who moves it |
|---|---|---|---|
| A reply classified interested or book later | `qualification` | the booking push, or the follow-up date | the worker, from the reply intent (section 11) |
| A booked meeting | `demo` | "prep the demo": a prep touch with the brief, the app and the questions to ask | the server, on the booking |
| The demo is done and they want to try it | `trial` | the trial's success test and the check-in date | the worker, from the call notes; the owner confirms |
| The demo is done and they want terms | `proposal` | what the proposal covers | the owner (section 16) |
| A signature | `won`, with `amount` (the contract value) and `close_date`; `mrr_usd` only for a subscription | onboarding | the owner |
| A no, at any point | `lost`, with `lost_reason` | when to look again, if ever | the worker for a clear no; the owner for a lost proposal |

**Pass:**

- For one test company, a booking logged for its person moves their deal to `demo` within a minute.
- That booking plans a prep touch for the assignee.
- A worker-recorded demo moves the deal to `trial` with a `next_step`.
- An owner-set `won` refuses to save without `amount` and `close_date`.
- A `lost` refuses to save without `lost_reason`.
- Every move appears in the stage history and in Activity as `crm.deal.stage`.

## 11. Reply intelligence

Each reply is read before anything else happens.

1. `crm.contact.replied` wakes the assigned worker.
2. The worker reads the whole reply and the thread.
3. It records the intent with `POST /api/crm/replies/{interaction_id}/intent` as one of `interested`, `book_later`, `objection`, `not_interested` or `referral`, with a one-line `why`, and a `referral` object (name, email or LinkedIn) for a referral.
4. It acts on the intent:

| Intent | Action |
|---|---|
| `interested` | Draft the booking push with `https://mixpeek.com/demo`; deal to `qualification` |
| `book_later` | Plan a follow-up touch on the date they gave (or two weeks) |
| `objection` | Draft the answer, in the founder voice, for approval |
| `not_interested` | Stop. If they asked not to be contacted, `do_not_contact`; otherwise `lost` with `lost_reason` |
| `referral` | Add the named person with `source: referral:<contact id>`, and draft the intro that names the referrer |

Classifying is the worker's judgement. The server stores the intent, checks the value is one
of the five, emits `crm.reply.classified`, and applies the stage and the stop.

**Pass:**

- Five test replies, one of each intent, each end in the action above.
- Each is visible on the person's timeline with its intent.
- The funnel counts the interested ones as replied (section 12).

## 12. Measurement: did the campaign work

The rows above say what the CRM can do. The funnel says how the campaign is doing: who moved
from enrolled to won, per sequence and per cohort.

**The contract**, `crm.funnel.v1`. The mixpeek side's `gtm/engine/crm_pilot_report.py` and
`gtm/strategy/CRM-PILOT.md` read these names. A change to a name or a definition ships as a new
version (`crm.funnel.v2`) served beside the old one, which keeps its names and meanings.

```
GET /api/crm/funnel?group=sequence|cohort&sequence=<id>&cohort=<tag>&since=<YYYY-MM-DD>
{
  "contract": "crm.funnel.v1",
  "measured": true, "n_considered": <enrollments counted>, "why_unmeasured": null,
  "since": "2026-11-01", "as_of": "<RFC 3339>",
  "groups": [{
    "group": "sequence", "key": "SEQ-1", "name": "GitHub stargazers",
    "enrolled": 0, "contacted": 0, "replied": 0, "booked": 0, "demoed": 0, "won": 0, "lost": 0,
    "conversion": {"contacted_per_enrolled": 0.0, "replied_per_contacted": 0.0, "booked_per_replied": 0.0,
                   "demoed_per_booked": 0.0, "won_per_demoed": 0.0, "won_per_enrolled": 0.0},
    "mrr_usd": 0, "won_amount_usd": 0
  }]
}
```

Definitions, each counted once per person (a person is the unit):

| Field | Counts a person who, after enrolling |
|---|---|
| `enrolled` | has an enrollment in the group |
| `contacted` | has at least one outward touch recorded `done` |
| `replied` | has a reply (`crm.contact.replied`) |
| `booked` | has a booked meeting (`crm.contact.booked`) |
| `demoed` | has a deal that reached `demo` or later in the stage history, even if it later moved on or was lost |
| `won` | has a deal that reached `won` |
| `lost` | has a deal whose latest stage is `lost` |
| `won_amount_usd` | the sum of `amount` (the contract value) over the group's won deals, in USD: the revenue measure for contracts |
| `mrr_usd` | the sum of `mrr_usd` over the group's won deals that are subscriptions; 0 when none are |

Each conversion is the later count over the earlier one, and `null` when the earlier one is 0.
A cohort is a tag on the enrollment (`cohort` on `POST /sequences/{id}/enroll`, for example
`ip-check-2026-12`); people enrolled with none group as `(none)`. `crm.funnel.changed` fires
when a number may have moved (a stage change, a booking, a classified reply) and names the
person and their sequences, so a worker or the report re-reads the endpoint instead of
polling it.

**Pass:**

- For the three test companies, the funnel's counts match a hand count of the timelines.
- A deal that went to `demo` and then `lost` still counts as demoed.
- `measured` reads `false` with a `why_unmeasured` when the stage history is missing, never a 0.

## 13. Approval throughput

The owner approves anything a person outside the company reads. A like, a follow or a watch
reads nothing, so it runs on its own.

| Runs on its own | Holds for the owner |
|---|---|
| `like`, `follow`, `watch` (and an internal `note`) | `email`, `dm`, `comment`, `connect` with a note, `letter`, `gift`, `text`, `call`, `invite` |

The list lives in one setting, `AMUX_CRM_AUTO_ACTIONS` (default `like,follow,watch`), in
`~/.amux/server.env`. An auto action is still recorded done with its result and appears in Activity.

**Pass:**

- With the default setting, a sequence's like and follow steps are done and recorded with no approval.
- An email step in the same sequence still waits in Outbound.
- Removing `like` from the setting makes the next like wait.

## 14. Suppression

The guard that refuses outward touches today (unsubscribed, bounced, do-not-contact,
duplicate channel, per-person cooldown) also refuses:

| Rule | Verdict |
|---|---|
| The person or their account is a customer (`status=customer`, or the account has a `won` deal) | `crm_touch_customer` |
| The account has an open deal the owner runs (`owner` is the owner) | `crm_touch_owner_deal` |
| The account is tagged `competitor` | `crm_touch_competitor` |
| Another person at the same account had an outward touch within `AMUX_CRM_ACCOUNT_COOLDOWN_DAYS` (default 7), for accounts under `AMUX_CRM_ACCOUNT_COOLDOWN_MAX_PEOPLE` (default 200) | `crm_touch_account_cooldown` |

Each refusal says why, shows as the Blocked reason in the review, and is logged with its verdict.

**Pass:** one test person per rule is refused with the right verdict, and the review shows why.

## 15. Address validation before the first cold send

A bounce costs the mailbox's reputation, which the whole of section 8 exists to build.

1. **When an address is added, and again before the first cold send:**
   - check the syntax
   - check the domain's MX records
   - flag role addresses (info, sales)
   - flag known disposable domains

   All of this is free, and the result is stored as `email_check` on the person.
2. **With a verification provider connected (row 6's decision),** a mailbox-level check also runs.
3. **A failed check refuses the send** with `crm_touch_address_unverified`, and the review shows it as the Blocked reason.

**Pass:**

- An address at a domain with no MX record is refused before any send.
- A role address is flagged in the review.
- A valid address passes.

## 16. The human boundary

Mixpeek closes on a call with the owner and a signed contract. "Agentically close" means the
agents drive every opportunity to a booked meeting and keep it warm; the owner runs the
demo, sets the price and signs.

| The agents do | The owner does |
|---|---|
| Find, research, grade and enroll people | Approve every outward message (section 13 lists the exceptions) |
| Draft and, once approved, send every touch | Run the demo |
| Read replies and act on them (section 11) | Set pricing and write the proposal |
| Book the meeting and prepare the brief for it | Sign, and move a deal to `proposal` or `won` |
| Keep every deal's `next_step` current | Decide on a lost proposal |

At `demo` the worker files a hand-off card for the owner with the brief, the reply thread and
the app link. A worker's attempt to move a deal to `proposal` or `won` is refused with
`crm_deal_owner_stage`.

**Pass:** a worker's PATCH to `won` is refused; a booking produces the hand-off card.

## 17. What to build to finish, in order

Built on 2026-11-27:
- closing, with stage history, `won` and `lost` fields, the owner's stages, and booking to demo with the prep touch
- the funnel, on `crm.funnel.v1`
- reply intent
- suppression (customer, owner's deal, competitor, account cooldown)
- `AMUX_CRM_AUTO_ACTIONS`, and bulk approve
- the free address check
- `{deal}`, `{next_step}` and `{asset_url}` in templates

Still to build, with no key or decision needed (in this order):

1. **The rest of closing (rows 37, 53).**
   - A demo-done verb that a worker calls after the call notes land.
   - A Needs-you hand-off card at `demo` for the owner.
2. **The mailbox pool's software (rows 28 to 32).**
   - A sequence names mailboxes.
   - The send path rotates them under a daily cap.
   - There is a warm-up schedule per mailbox.
   - There are per-mailbox bounce and reply rates.

   The unsubscribe link also needs a public address for amux, which is the owner's call (amux Cloud or a tunnel). This is what retires Instantly.
3. **Comment targeting and press (rows 12, 25).**
   - A comment step picks the person's latest post from their watched channel.
   - A press feed is added beside the page watch.

Waiting on the owner (a paid key, money, or a call only the owner makes):

- **Enrichment and mailbox-level verification (rows 6, 7, 51's paid half):** which providers (Hunter.io, Apollo, Exa, Google, a verifier), and their keys.
- **Offline fulfillment (row 13):** which print-and-mail and gift services, and the card rules for them under `amux order`.
- **The sending domains themselves (row 28):** buying trymixpeek.com and getmixpeek.com style domains and their Google Workspace seats.
- **Confirmations:** the auto-run list (`like`, `follow`, `watch`), and that `proposal` and `won` are owner-only. (Revenue is the contract value, `won_amount_usd`: owner, 2026-11-27, "it's not a subscription".)
