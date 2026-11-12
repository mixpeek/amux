---
description: Compliance work from amux's own record - access reviews, evidence packs and incident timelines, built from the ledger, traces and commits. Read-only; it recommends and the owner decides. e.g. /compliance access review, /compliance evidence pack for SOC 2, /compliance what happened with trace 4bf9...
argument-hint: access-review [stale-days] | evidence [controls] | timeline <trace id | worker | member:<email> | window>
allowed-tools: Bash, Read, Write
amux-managed: true
---

# /compliance: access reviews, evidence and incident timelines

amux keeps one record of what happened: the **ledger** (append-only, hash-chained,
every decision and effect with its proven actor, authority and trace id), **traces**
(one id carried from the message that started a piece of work through schedules,
deliveries, commits and publishes), and **commits** whose trailers say which worker
made them, for whom and on which host. This skill turns that record into the three
things a reviewer, an auditor or an incident lead asks for.

You are reading, never changing. Every revocation, offboarding or setting change is
the owner's decision: write it as a recommendation with the exact command, and stop.

## What to read

| Need | Read | Notes |
|---|---|---|
| Who can do what | `amux compliance access-review [--stale-days N] --json` | People, workers (with effective capabilities), apps, SCIM tokens, standing approvals, MFA/SSO policy, and computed `findings` |
| One incident | `amux compliance timeline --trace ID` (or `--worker W`, `--actor member:<email>`, `--since 7d --until ...`) `--json` | Ledger rows, trace nodes and commits merged by time, with the chain's verify result |
| Evidence pack | `amux compliance evidence --out <dir>` | Ledger verify, signed checkpoints and key, audit export (CSV and OCSF), at-rest report, access review, kill switch, legal holds, counters, plus `manifest.json` (sha256 per file) and `CONTROLS.md` |
| Anything else | `amux ledger verify`, `amux audit export`, `amux audit holds`, `amux audit kill-switch`, `amux security at-rest`, `GET /api/trace/<id>`, `GET /api/ledger?actor=...` | All readable by the owner and by auditors |

Before trusting any number, read the `measured` beside it. A section that says
`measured: false` was not read, and its empty list means nothing: say so in the
report rather than reporting "no findings". `ledger.oldest_at` bounds every "never
seen": the ledger only remembers back to that time.

## Access review

1. Run the access review. Read `findings` as leads, not verdicts: each names the
   rule that flagged it.
2. Check what the rules cannot see. A member flagged `person_inactive` may work only
   through a worker they sponsor (look for `on_behalf_of` rows in the ledger). A
   worker flagged `outbound_without_approval` may be doing exactly the routine email
   it was set up for. Look before you recommend.
3. Look for what no rule flags: a person whose scope is wider than the groups they
   actually touch (compare scope with the resources in their ledger rows), an app
   allowed amux routes it never calls, a sponsor who has been offboarded.
4. Write the review: a table per subject type, then the recommendations ranked by
   risk, each with the evidence (ledger rows, dates) and the exact command the
   owner would run. Save it under the folder you were asked to, or
   `compliance/access-review-<date>.md`.

## Evidence pack

1. Run `amux compliance evidence --out <dir>`. Exit 3 means incomplete: the
   manifest's `missing` names what failed and `ledger_chain_ok` says whether the
   chain verified. Report an incomplete pack as incomplete.
2. If you were given a control list (SOC 2, HIPAA, a customer's questionnaire), map
   each control to the files that evidence it. `CONTROLS.md` is a starting map, not
   the answer: add controls it does not cover, and say plainly where amux holds no
   evidence (for example, physical security or HR screening).
3. To let someone else check the pack: the `sha256` in the manifest for each file,
   and `amux ledger verify --against <checkpoints>` against the signed checkpoints
   held outside this host.

## Incident timeline

1. Start from whatever you were given: a trace id, a worker, a person, or a window.
   No trace id? Find one: `GET /api/ledger?actor=...&since_h=...` rows carry
   `trace_id`, and so do commit trailers (`Amux-Trace:`).
2. Run the timeline and read it in order. Follow `parent_event_id` and span links
   when one trace started another (a schedule that fired, a message that was
   delivered).
3. Note the gaps: the trace only covers work that carried it, commits made before
   the bot identity existed are not matched, and anything outside amux (a laptop,
   a provider's console) is invisible. Say what the record cannot show.
4. Write the timeline: what happened, who acted (proven actor) and on whose behalf,
   what was allowed or refused and by what authority, what changed (commits,
   publishes, emails), whether the chain verified, and what is still unknown.

## Rules

- Read-only. Never revoke, offboard, erase, place a hold or flip the kill switch.
- Quote evidence: ledger `seq` and `event_id`, commit hashes, file paths, dates.
- Unmeasured is not zero. Say which sections did not measure, and why.
- Personal data: a timeline or review names people; keep it where the owner put the
  request, not in a public folder or a published page.
