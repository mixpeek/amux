# Connector lifecycle E2E

Runtime-declared OAuth connectors now use the existing durable grant broker. The
flow is declaration → credentials → account-labelled consent with PKCE S256 →
public callback → worker-scoped mint → provider call → rotation → crash recovery
→ disconnect → explicit reconnect. Account labels are owner-supplied, not a
verified provider identity (`identity_unverified: true`); a worker must check the
provider identity for account-sensitive work. OAuth security references:
[RFC 9700](https://www.rfc-editor.org/rfc/rfc9700.html) and
[RFC 7636](https://www.rfc-editor.org/rfc/rfc7636.html).

## Automated production-server checks

Use the shared Cargo target and sanctioned wrappers:

```sh
CARGO_TARGET_DIR="$HOME/.amux/rust-build-target" CARGO_BUILD_JOBS=1 \
  scripts/test-contended.sh -p amux-server --test restart_persistence connector -- --nocapture
node scripts/test-connector-account-health.mjs
```

`declared_oauth_connector_lifecycle_survives_sigkill` runs an actual TLS server and
HTTP provider. It checks PKCE and exact redirect binding, single-use callbacks,
real account-bearer canaries, account scope enforcement, refresh rotation,
SIGKILL/restart without duplicate refresh, revoked-account reconnect reporting,
disconnect cancelling outstanding consent, and definition deletion/redeclaration
without resurrecting grants. `connector_scope_patches_survive_concurrency_and_sigkill`
checks 24 concurrent additions, preservation of account/MCP fields, and refusal to
replace malformed saved state. The existing acknowledged-state crash test also
runs under the `connector` filter. A wrapper exit 75 means no test ran.

## Every builtin connector

Run the checked-in matrix through the production router, authentication and durable
store. Only the provider network boundary is translated to loopback fixtures; each
request still crosses a real HTTP socket. A separate Amux child process is killed
with SIGKILL and restarted against the same home and SQLite database. Its test
executable is copied once into the private home and SHA256-checked at completion,
so shared Cargo cache cleanup cannot replace the restart image. No real
account credentials or customer messages are used.

```bash
CARGO_TARGET_DIR=~/.amux/rust-build-target CARGO_BUILD_JOBS=1 \
  scripts/test-contended.sh -p amux-server --test connector_matrix -- --nocapture
```

| Connector | Flows in the matrix |
| --- | --- |
| Granola | Missing/invalid/saved API key, provider canary, unsupported OAuth mint, restart |
| Gmail | Google consent with PKCE, registered Gmail callback delegation, cancel/replay, two accounts, pinned mint/denials, canary, rotating refresh/revocation/reconnect, restart, signed service-account delegation/refusal |
| Calendar | Same Google family lifecycle and its own canary/mint/delegation scope |
| Drive | Same Google family lifecycle and its own canary/mint/delegation scope |
| Admin | Same Google family lifecycle and its own canary/mint/delegation scope |
| Slack | Code exchange, cancellation/replay, two accounts, pin/denials, HTTP200 application errors, rotation/revocation/reconnect, restart |
| Telegram | Missing/invalid/saved key, getMe, immediate status/send use, poll /link, private-before-group gate, scoped send, HTML-to-plain retry, HTTP200 rejection, durable mapping/cursor and no duplicate acknowledged link after SIGKILL |
| Mattermost | Missing/invalid login, Token response header, two saved logins, pin/denials, both-account canaries, expired login/reconnect, restart |

The test inventories all builtin IDs and fails if a new connector lacks a matrix
entry. A declared API-key adversarial canary also verifies that a provider-echoed
bearer is absent from the response, inventory and persisted last-test result.
Granola and Telegram do not mint user OAuth grants. Mattermost uses login
and password; a personal access token is not a password in this implementation.
The existing declared-connector restart suite covers custom OAuth and API-key
registry/credential persistence, disconnect, pending-consent cancellation, deletion
and redeclaration. This matrix does not claim that a local disconnect revokes a
provider-side grant.

For an explicitly authorized native worker pass, set `AMUX_MATRIX_OWNER_HANDOFF`
to an output receipt path. After initial scopes/grants/refresh checks, the test
publishes only the private origins/home and waits up to ten minutes for the owner
to create `release_file`. Use a new isolated native Codex worker with direct owner
input, no MCP or injected harness, and explicitly configured worker-only network
access. Give it the synthetic broker credential in a private file, pin its grants
to Beth, and ask it to discover accounts and correct a stale Alice invoice report.
The provider returns duplicates, revisions and a void invoice: Beth must yield
IDs A/C/D and 137 through all six OAuth/login connectors, while explicit Alice
requests return 403. Granola must pass its server-managed canary; Telegram must
pass getMe and a synthetic send. The Chat companion must read the actual report;
a second browser-composer turn must render the same result in the same native
conversation. Stop the worker, remove the temporary network flag, preserve
sanitized receipts/rollout hashes, close test tabs, and release the matrix to
finish crash/revocation/delegation checks. The default CI run never waits for a
worker and never starts one.

## Actual browser and native Codex worker

The example uses the production router/assets/auth/store, a private home/database,
read-only existing trusted TLS certificate, and no fleet jobs or adoption. It
refuses the live home and production port. It serves a synthetic consent page
whose code exchange goes to the separate loopback HTTP provider; no intercepted
routes or injected HTTP transport.

1. Create a private directory. Build the example with `scripts/safe-cargo.sh build
   -p amux-server --example connectors_e2e_server`; copy the executable out of the
   shared cache before another test cleans it. Start the provider:
   `python3 scripts/fixtures/connectors-provider.py --state-dir <private-dir> --port 19122`.
2. In the private home's `server.env`, set `CONNECTORS_REDIRECT_ORIGIN` to an
   already-trusted HTTPS origin on port 19121. Launch the example with private
   `AMUX_HOME`, `AMUX_RS_PORT=19121`, `AMUX_NO_SELF_ADOPT=1`, and
   `AMUX_E2E_PROVIDER_ORIGIN=http://127.0.0.1:19122`. Its authentication uses the
   owner's existing token; keep it in memory and headers, never command arguments.
3. In Connectors, add `fixture-oauth` (OAuth2): `FIXTURE_CLIENT_ID`,
   `FIXTURE_CLIENT_SECRET`, authorize URL `<trusted-origin>/e2e/authorize`, token
   URL `http://127.0.0.1:19122/token`, scopes `invoices.read`, test URL
   `http://127.0.0.1:19122/data`. Save **synthetic** values `fixture-client` and
   `fixture-secret`. Connect labels `alice` and `beth`, selecting the matching
   fixture identity on consent. Reloading a successful callback must fail.
4. Seed a sentinel connector scope with an account and MCP field. Create an API
   key connector, save `fixture-api-key`, and test against `/data`. Creation and
   toggles must preserve the sentinel and OAuth scope. The dashboard sends
   `{connectors:{id:patch},merge:true}` through `PUT /api/scope` atomically.
5. Configure the **private broker** worker scope to enable `fixture-oauth` and pin
   account `beth`. Create a new owner-controlled isolated native Codex worker in a
   disposable Git directory containing a stale Alice/9 report. Explicitly
   configure network access for this test worker if its CLI sandbox denies local
   HTTP. Direct owner prompts instruct it to POST the private broker's token
   endpoint with its worker header and **no account parameter**, then call `/data`
   with the returned bearer in memory. Require fresh identity `beth`, billable
   IDs `A,C,D`, total **137** after deduplicating records, choosing the highest
   revision, and excluding void rows. Explicit Alice and disabled-worker requests
   must return 403. Never copy real grants into the fixture.
6. Disconnect Beth in the browser. SIGKILL only the private process and restart
   the same private state. A fresh worker mint must return 404 `needs_auth`, even
   though Alice remains connected. Reconnect Beth through consent; a fresh owner
   worker turn must compute Beth/137 again. Provider `/revoke` with synthetic
   `identity=beth` must yield an Expired/reconnect account after an uncached
   health probe, while Alice stays Active.
7. Preserve sanitized request/response receipts, worker owner prompts/delivery
   history/native completion and report files, provider event counts, binary
   hash, and crash signal/PIDs. Scan transcripts/logs for token leakage without
   printing tokens. Stop the test worker and remove its temporary network flag;
   stop private processes and close only test tabs.

Isolation intentionally excludes injected MCP/harness context. This proves direct
owner-to-native-Codex broker use and recovery, not automatic MCP delivery. A local
disconnect forgets Amux's grant and cancels pending consent; provider-side
revocation is separate (`provider_revoked:false`). Only explicit new consent may
reconnect. Never run destructive scenarios against saved real accounts. Live
read-only canaries/inbox calls are separate evidence, and missing credentials,
revoked grants, or service-account delegation failures must remain visible.
