# Audit, legal hold, erasure and the kill switch

Governance PR 19 (`crates/amux-server/src/api/governance.rs`). The record itself is the ledger
(`api/ledger.rs`, governance PR 7); this page is what a person does with it.

| Need | API | CLI | Who |
|---|---|---|---|
| Export the record | `GET /api/audit/export?format=csv\|ocsf\|json&since_h=&since=&until=&actor=&worker=` | `amux audit export --format ocsf -o out.jsonl` | owner, auditor, admin |
| Keep rows past retention | `POST /api/audit/holds {name, actor?, worker?, from?, to?, all?}`, `POST /api/audit/holds/<id>/lift`, `GET /api/audit/holds` | `amux audit hold --actor member:x@y`, `amux audit unhold <id>`, `amux audit holds` | owner places and lifts; auditor reads |
| What amux keeps about a person | `GET /api/audit/person?subject=<email>` | `amux audit person <email>` | owner, auditor, admin, the person |
| Erase a person | `POST /api/audit/person/erase {subject, dry_run}` | `amux audit erase <email> [--yes]` | owner |
| Stop all automation now | `POST /api/audit/kill-switch {engaged, reason}`, `GET` for state | `amux audit kill-switch on "<reason>"` / `off` / `status` | owner engages and lifts |

Every use is a ledger row of kind `governance` (`audit.export`, `legal_hold.place`,
`legal_hold.lift`, `person.export`, `person.erase`, `kill_switch.engage`, `kill_switch.lift`).
Counters: `GET /api/contract/counters` under `governance`, and `ledger.held_back_by_legal_hold`.

## Auditor

A member whose role is `auditor` (`PATCH /api/org/members/<id> {"role":"auditor"}`, owner only).
They may `GET` the ledger, `/api/trace/*`, `/api/approvals/cards`, `/api/security/at-rest`,
`/api/audit*` and `/api/contract/counters`. Every write and every other read is refused with
`auditor_read_only`, whatever scope the member was invited with, and counted
(`governance.auditor_refused`).

## Export

Rows oldest first. Every row has `chain_verified`: the chain walk reached it intact. The response
headers carry the verdict for the whole export (`X-Amux-Ledger-Chain-Ok`,
`X-Amux-Ledger-Verified-Upto`, `X-Amux-Ledger-Rows`), so a CSV read on its own still says whether
the chain held. OCSF lines are API Activity (class 6003); fields OCSF has no place for are under
`unmapped.amux`. A CSV cell starting with `=`, `+`, `-` or `@` is prefixed with `'` so a
spreadsheet does not run it. To check the export against signed checkpoints, use
`amux ledger verify --against` (governance PR 10).

## Legal hold

A hold matches ledger rows by actor (actor, claimed or on-behalf-of), worker (`worker:<w>` as
actor, claimed or resource) and time range; at least one, or `all: true`. A trigger refuses to
delete a matching row, so the retention prune (`AMUX_LEDGER_RETAIN_DAYS`) stops at the first held
row: the ledger stays a prefix-pruned chain, which means rows after the held one are also kept
until the hold lifts. Holds are never deleted, only lifted once.

## Erasure

The person is an email (`member:<email>`). In every ledger row that names them in actor, claimed,
on-behalf-of or resource, that column becomes `erased:<erasure id>`. The row keeps its
`prev_hash` and `hash`, so the chain still links and signed checkpoints still match. Its content
can no longer be re-hashed, so the erasure writes its own ledger row whose `output_hash` is a
digest of every redacted row as it now stands; verify checks each redacted row's link and that
digest, and reports `redacted`. Changing a redacted row afterwards, or marking a row redacted
without an erasure, fails verify.

Also: their interaction receipts (`_amux_interactions`, working state) are deleted and their
name is replaced in policy receipts. Not reached, and listed in every answer as `not_erased`:
Messages posts and uploads, terminal transcripts, the request log and server logs, and their
`org_members` row (offboard them for that). An erasure is refused, changing nothing, when any of
the person's rows is under an active hold.

## Kill switch

State lives in `<AMUX_HOME>/governance/kill-switch.json`, so the git pre-push hook reads the same
fact with the server down. While engaged:

- the server refuses `publish` (`POST /api/page-links*`), `email_send` (`/api/email/send`,
  `/reply`, `/approve/<id>`, including in-process scheduled sends and auto-approvals),
  `connector_mint` (`/api/connectors/<id>/token`) and `social_publish`, with 403
  `kill_switch_engaged`, for any caller that is not a person; the owner and members keep acting;
- the social publish job leaves queued posts queued;
- `scripts/git-hooks/pre-push` refuses a push from inside a worker (`$AMUX_SESSION` set), with no
  override. A repo whose hooks are not amux's is not covered.

An unreadable switch file reads as engaged. `AMUX_KILL_SWITCH=off` makes the switch inert.
