# Source requests on the amux board

Slack requests collected by the WorkDesk connector appear as ordinary amux board cards. Open a card to read its source and evidence, request or revise a draft, approve its exact version and destination, and separately request delivery. The dashboard renders these controls natively; it does not load the former WorkDesk dashboard.

amux owns the task ID, visible lifecycle and card actions. The headless Python connector retains the existing Slack source cache, assessment, artifact versions, source/content hashes and approval/delivery ledger. These records are not copied into a second editable task manager. The Python authorized-draft queue remains the sole execution owner for these cards; native auto-claim/board-drive cannot independently dispatch them.

## Configuration and cutover

Set `AMUX_WORKDESK_URL` to the connector's HTTP loopback IP origin, for example `http://127.0.0.1:8080`, and `AMUX_WORKDESK_TOKEN_FILE` to a private token file. The connector reads the matching token through `WORKDESK_AMUX_TOKEN_FILE`. Tokens belong in runtime configuration, never source files or browser requests. Connector redirects and non-loopback origins are refused.

The existing periodic runtime registry imports source changes every 60 seconds. The board's synchronization button runs the same bounded importer. The unique connector-instance/candidate binding prevents duplicate cards across repeats and restarts. Imported requests do not authorize draft generation. Existing manual mode and source checks still apply.

After native UI and source-count verification, set `WORKDESK_AMUX_MODE=1` for the connector and its automation service. This disables the old dashboard, public task writes and independent owner notifications. Keep the signed Slack ingress, poller and authorized execution queue. The connector's durable instance ID must change if a database clone is intended to represent a distinct source; otherwise it represents the same source identity.

Back up both stores, artifacts and runtime configuration before cutover. Rollback disables native source actions before enabling the old UI, after pending operation receipts are reconciled. Do not restore an old database over newer user work.

## Data and approval boundaries

Native card reads use `/api/board/{id}/source`. Commands use `/api/board/{id}/source/actions` with an operation UUID, kind and expected source fingerprint. Revision, approval and delivery also bind the exact artifact ID/content hash. Approval binds the chosen channel and thread. The connector records immutable operation payloads so retries recover the same action rather than starting another one.

The source adapter alone may update a bound task's source, ownership and lifecycle. Database guards apply even when a generic board caller requests force. Ordinary task claiming and generic worker capture do not take over source cards. Bound dispatch records the existing task on the durable steering message; it does not run semantic intake or mint a second WE card.

Approval and delivery require same-origin dashboard intent, a named approver and no declared worker-origin header. These are the existing local owner conventions. A shared privileged host bearer is not a cryptographic distinction between a human and an agent; do not describe the header checks as hostile-agent isolation.

The connector revalidates source and artifact hashes at the action boundary. A model result is reviewable work, not evidence of external delivery. A source task closes as delivered only when its current artifact has a nonempty delivery receipt. Ambiguous delivery stays unknown and is not blindly retried. Connector errors retain the task and show unavailable state instead of an empty successful queue.

## Verification

Run `node --test e2e/work-requests.test.mjs` for the shipped request-detail component with an isolated fixture API. It covers frozen source/content fields, confirmation-before-approval, separate delivery, retry IDs, stale source rejection, rapid card navigation, escaping and 375px layout. This component test does not prove the full dashboard/router composition.

Run `scripts/safe-cargo.sh test -p amux-server --lib work_requests::tests` for source binding, route and SQL takeover guards, source completion evidence and explicit bound-dispatch deduplication. These are focused tests, not the entire server suite. Use the repository's normal workspace, lint and broader test gates as well.

Before reporting deployment, verify the live `/health` commit, imported identity counts, connector authentication and disabled legacy UI. Inspect the actual dashboard in a browser. A canary must produce one task, one native worker submission and one reviewable artifact; replay and restart must not create another task. Test outbound delivery only with a stub transport unless the owner expressly authorizes a real send.
