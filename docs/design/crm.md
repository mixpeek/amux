# amux CRM: one place for every relationship (plan)

Owner, 2026-11-23: "audit how we've been using crm across mixpeek and amux and
personal and create a plan for amux crm to support it all (and we want to get
away from instantly/lightfield eventually)", then "build the CRM tab ... a tab
that is globally accessible in the amux worker list page ... clean on mobile
... all the capabilities needed to continue all our gtm shit ... automatically
sync", and "Include these in the crm: Contact info, ICP grade, lookalikes".

## 1. What the audit found

Four stores hold the same relationships today, and none is complete.

| Store | What it is used for | Who writes it | Volume |
|---|---|---|---|
| **Lightfield** (`api.lightfield.app/v1`) | The system of record since 2026-05-28 (it replaced HubSpot): accounts, contacts, opportunities with stages (Lead, Qualification, Demo, Trial, Proposal, Won, Lost), tasks with an assignee, auto-captured mail | mixpeek `server/ops/gtm/outreach_crm.py`, `inbound_lf_tasks.py`, `task_reconcile.py`, `lightfield_opportunity_sync.py`, `import_to_lightfield.py` | 179 contacts, 141 accounts at migration; 41 opportunities (2026-07-25 digest) |
| **Instantly** (API v2) | Every automated outbound send ("any automated outbound should go thru instantly", 2026-10-25): campaigns, per-lead copy variables, 4 alias-domain mailboxes, warmup, daily caps, reply and bounce status, unsubscribe | `gtm/campaigns/*.py`, `gtm/iconik-sync/`, `server/ops/gtm/icp_outreach.py`, `inbound_guards.py`, `gtm_sync.py` | rb2b-inbound: 338 sends to 176 leads, 0 replies, 19 bounces; stopped since 2026-08-22 |
| **Mixpeek `gtm` namespace** (`ns_af7542b688`) | A dual-write copy and event sink (RB2B visits, sends, replies) | `server/ops/gtm/sink.py`, the RB2B webhook | 5,000 contacts, 2,792 accounts |
| **amux CRM** (`/api/crm`) | Workers' notes on people; calls log here | gtm-engine (68 creates, the main writer), personal, calls | 351 rows (263 live); 100 real people, 74 email-only RB2B imports, 72 LinkedIn-only targets, 14 brands and devices from a 2026-03-28 brand scan, 3 placeholders; 185 with no interaction, at least 4 duplicate people |

Beside them: CSVs under `~/Dev/.gtm-pii` (2,821 consolidated people, 2,594
signups, 523 engaged companies), the cold-outbound pipeline's CSV and JSONL
(about 3k rows), Mongo `gtm_outreach_ledger`, a LinkedIn export (4,383
connections, 31,939 messages), 30 `customers/` account folders and
`gtm/revops/pipeline.md`. Personal relationships run through Google Contacts
schedules (SCHED-1556, 1557) and People notes in the owner's notes folder.

What the amux CRM cannot hold today, each relied on by a live workflow:

- **Companies.** `company` is free text, so a person cannot be grouped under
  an account, and dedupe by domain (outreach ledger, holds) is impossible.
- **Deals.** No stage, amount or next step; `gtm/revops/CRM-SCHEMA.md` packs
  them into tags and notes, and the API drops them silently.
- **Source.** No record of who or what created a contact, which is how 14
  brand names and an RB2B batch got in unnoticed.
- **Sequences.** Multi-touch outreach, per-lead copy, caps, reply and bounce
  stop rules, unsubscribe.
- **Auto-capture.** The reason HubSpot was replaced: mail and meetings attach
  to the person without anyone typing them.
- **ICP fit and lookalikes.** Both are computed today in scripts and notes
  (`icp_outreach.py`, the playbooks) and stored nowhere a person can sort by.
- **A screen.** The People tab was removed on 2026-08-09 (AMUX-2590); the data
  is reachable only through the CLI and workers.

## 2. The model

Everything stays inside amux's primitives (CLAUDE.md: board, workers,
schedulers, filesystem, groups, memories, environment, messages). The CRM is
the data; workers do the judgment (research, grading, drafting); the scheduler
runs cadences; the email API sends, with its approval holds. No ninth thing.

**Contacts** (people; the existing `crm_contacts`, kept):
name, email(s), phone, LinkedIn, X, role, location, notes, tags, plus new:
- `account_id` (the company, below), `source` (`worker:<name>`, `import:<file>`,
  `rb2b`, `lightfield`, `instantly`, `owner`), `owner_lane`
- `status`: `lead`, `active`, `customer`, `partner`, `personal`, `do_not_contact`
- `icp_grade` A to D (and `icp_reasons`, `icp_scored_at`, `icp_by`)
- `unsubscribed_at`, `bounced_at` (outbound stops on either)
- `external` ids: `lightfield_id`, `instantly_lead_id`, `gtm_ns_id`

**Accounts** (companies, new `crm_accounts`): name, domain (unique, the dedupe
key), industry, size, location, website, LinkedIn, notes, tags, `icp_grade`
with reasons, `source`, `external` ids.

**Lookalikes** (new `crm_lookalikes`): account to account (or contact to
contact), score 0 to 1, reason, found_by, found_at. "Accounts like our best
customers" is a filter on the tab and a worker task: grade A accounts seed a
search, the hits land as new accounts with `source=lookalike:<seed>`.

**Deals** (new `crm_deals`): account, contacts, stage (Lightfield's enum:
Lead, Qualification, Demo, Trial, Proposal, Won, Lost), amount, close date,
next step, owner, notes. Proposal, Won and Lost stay human-moved, as today.

**Activity** (the existing `crm_interactions`, widened): email, call, meeting,
DM, note, event, sequence step; with direction (in, out), `message_id`
(threading back to the email API), and the follow-up date it already has.

**Sequences** (new `crm_sequences` and `crm_enrollments`): steps (delay, copy
template or a worker drafting each touch), stop rules (reply, bounce,
unsubscribe, meeting booked), daily cap per mailbox. A shell schedule advances
due enrollments; each send goes through `/api/email/send` with its normal
approval hold, so the owner's "Draft it and show me. I send it" holds until a
standing approval covers a named sequence.

## 3. Sync: automatic, both ways, until the exits

- **Mail and meetings in.** The email API already logs every send and reads
  the inbox; a server job attaches any message to the contact whose address
  it carries (activity row with `message_id`), and calendar events to their
  attendees. No typing, which is what Lightfield is used for.
- **Lightfield, both ways (until cutover).** Import accounts, contacts,
  opportunities and tasks once, keyed by `lightfield_id`; then a schedule
  mirrors changes each way, last-writer-wins per field, every conflict
  logged. Cutover is a switch that stops the mirror and makes amux the only
  writer; `outreach_crm.py` and friends move to `/api/crm`.
- **Instantly (until its exit).** Import leads with their campaign status,
  reply and bounce state as activity; a schedule pulls new events. Its exit
  needs three things amux does not do today: mailbox rotation across the
  alias domains, warmup, and deliverability monitoring. Until those exist,
  Instantly stays the sending rail and amux is the record of what was sent.
- **Mixpeek `gtm` namespace.** Stays an event sink and the search index for
  lookalikes (it already embeds accounts); amux writes to it and reads
  lookalike hits from it.
- **Phones.** The tab reads the same API, and the dashboard's realtime
  channel refreshes it, so the phone and the Mac agree without a reload.

## 4. The tab

A **CRM** tab beside Workers, Board and Calendar, reached from the worker list
page and from Cmd-K (contacts, accounts and deals are palette results).

- **Views:** People, Companies, Deals (a stage board), Follow-ups (due today
  first). One row of controls on a phone: a search box, the view switch, and a
  filter menu behind `⋯` (status, ICP grade, tag, source, owner lane).
- **A person:** contact info with tap-to-call, mail and LinkedIn; company;
  ICP grade with its reasons; status; tags; deals; the activity timeline
  (mail, calls, meetings, sequence steps); follow-up; lookalikes of their
  company. Actions: log, follow up, ask a worker (the same ask as the markdown
  task card), add to a sequence.
- **A company:** domain, size, ICP grade, its people, deals, activity,
  lookalikes (with "add as lead").
- **Bulk:** select rows to tag, grade, merge duplicates, enroll, or export CSV.

## 4b. The agent side (owner, 2026-11-23, 4:33 PM)

"A very agentic-oriented CRM ... fields that allow the agent to take action
on ... multi-touch ... LinkedIn, Twitter ... real time ... subscribe to a user
... websites ... career posting ... the event subscription."

- **Channels** (`/api/crm/channels`): where a person or company is reached or
  watched: LinkedIn, X, Instagram, website, careers page, email, phone, and so
  on, each with a handle or URL and a watch switch with its own interval.
- **Watching:** the `crm-watch` runtime job fetches watched public pages
  (websites, careers) itself. A worker checks LinkedIn, X and Instagram through
  a signed-in browser route (`amux crm due`, then `amux crm observed`), because
  those sites need a login and limit automation. The first check records; a
  later one with new items emits `crm.channel.changed` and adds a `signal` to
  the person's timeline.
- **Touches** (`/api/crm/touches`): one planned action on one channel (dm,
  comment, connect, email, follow, like, call, note), with text, a due date and
  the worker that does it. A worker plans and drafts; the owner approves
  anything a person outside the company reads (the "To approve" view, one tap);
  approval emits `crm.touch.approved` to the assignee; the worker does it and
  records it done (`crm.touch.done`), which lands in the timeline. An outward
  touch cannot be marked done before it is approved.
- **Automatic capture:** `crm-watch` attaches new `email.received` and
  `linkedin.message_received` events, and delivered sends in the sent-mail log,
  to the people they are from or to, and emits `crm.contact.replied` with how
  many touches are still open, so a sequence stops when someone answers.
- **Real time** is amux's event subscriptions: the four `crm.*` events are in
  the catalog, emitted as source `crm`, so `amux subscribe "a new job at a
  company I watch"` or "when the owner approves my touch" wakes a worker.

## 5. Phases

1. **Foundation (now).** Schema: accounts, source, status, ICP grade,
   lookalikes, deals, wider activity; API for each, owner and worker
   attributed; the CRM tab with People, Companies, Deals and Follow-ups,
   clean at 375 px; Cmd-K results; contact info, ICP grade and lookalikes on
   every person and company. Data cleanup as a reviewed list for the owner
   (the 14 non-people, 4 duplicate groups, 3 placeholders), never a sweep.
2. **Auto-capture.** Mail and meetings attach to people; the timeline fills
   itself.
3. **Lightfield mirror, then cutover.** Import, two-way sync, then amux the
   only writer; mixpeek's GTM scripts point at `/api/crm`.
4. **Sequences on the email API.** Enrollments, stop rules, caps, unsubscribe
   footer and list; workers draft each touch; owner approval per send until a
   standing approval names the sequence.
5. **Instantly exit.** Only after mailbox rotation, warmup and deliverability
   monitoring exist (or a cheaper sending rail is chosen). Spending money on
   mailboxes or a rail is the owner's decision.

## 6. Verification

Every phase is checked as a person would use it (`/e2e-test`): the tab at
desktop and 375 px, a contact created, graded, linked to a company and a deal,
its activity captured from a real send, synced to the phone, and found in
Cmd-K; plus failure cases (offline, a duplicate email, an unsubscribed contact
in a sequence).
