# Browser profiles: one per identity, chosen by the harness, kept signed in

Owner ask (2026-10-07): consolidate the saved browser profiles so it is obvious
which one a worker uses, keep their logins active, and make it automatic in the
harness rather than a recipe a worker has to follow.

## What was there

77 profiles, about 12 GB, in four groups:

| Group | Count | Problem |
|---|---|---|
| Single-site saves (`github`, `x`, `brex`, `slack`…) | ~25 | 9 held zero cookies; the rest lost most of theirs |
| Grab-bags (`default`, `Default`, `consolidated`, `clean`) | 4 | Dozens of sites, no owner, no label; `default` was the documented recipe |
| Studio test personas (`persona-*`) | 40 | Unregistered; the reaper removes them at 30 days |
| Studio identities, customer, probes | ~8 | Mixed |

Three defects made the set unusable whatever its shape:

1. **"Signed in" was a lie.** `signed_in_to` listed every host with any cookie,
   so a tracking cookie or an expired session counted.
2. **Saved logins evaporated.** Chrome drops session cookies when a profile's
   browser closes, and the save path closed it right after copying them in:
   brex 9 of 9 copied and 0 kept, GitHub 3 of 14, Cloudflare 3 of 43.
3. **Nothing chose a profile.** A worker read 77 rows and guessed; `start`
   with no profile took the grab-bag.

## The model

**One profile per identity.** A profile is who you are signed in as, not which
site you visit. Each mirrors the owner's Chrome profile signed in as the same
account, for a named list of sites only.

| Profile | Role | Mirrors (owner's Chrome) | Who may use it |
|---|---|---|---|
| `ethan` | primary | ethan@mixpeek.com | every worker; the default |
| `ethan-money` | restricted | ethan@mixpeek.com, money sites only | the finance lane |
| `ethan-personal` | personal | esteininger21@gmail.com, events and social only | social-activities |
| `mixpeek-studio-ethan`, `mixpeek-studio-info` | | Studio test logins | every worker |
| `ethan-tubescience` | customer | TubeScience environment | customer lanes |
| `persona-*` | test | synthetic Studio users | tests |
| everything else | deprecated | | still usable, ranked last |

The access column describes the intended policy. Workers must use the current
registry scope; a role or label does not enforce that policy. In the 2026-10-08
audit, personal and finance still allowed all 189 workers, so their login imports
and sync remain held until the owner chooses narrower worker/group allowlists.

Roles live in the registry (`playwright-auth/profiles.json`, `role` and
`identity`), set with `POST /api/browser/profile/meta`. Nothing is deleted.

## How a worker chooses

Workers choose by account identity, declared purpose, allowed scope and observed
site access. `amux browser profiles` includes those semantics; QA and deprecated
profiles remain discoverable with `--all`. `amux browser for <url>` (or `GET /api/browser/profile-for?url=`)
recommends a profile for that worker's scope and explains its cookie evidence.
`GET /api/browser/profile-for?url=...&identity=...&role=...` constrains the
recommendation to the task's required identity/purpose and includes semantic
selection cards and alternatives. Verify the live account before acting. `POST /api/browser/start` with no profile uses
the default identity (`AMUX_BROWSER_DEFAULT_PROFILE`, else the registry's
primary). The profile list is sorted by role, but workers still match the organization and purpose; two profiles may share an email while serving different customers.

## How logins stay active

The `browser-login-sync` job (every 6 h; `AMUX_BROWSER_LOGIN_SYNC_SECS`, `0`
off; `POST /api/browser/sync-logins` runs it now) copies the listed sites'
cookies from the owner's Chrome into each opted-in profile:

- **Listed sites only.** The owner's Chrome holds banking, insurance and health
  logins; nothing reaches an agent-usable profile unless a human named the site.
- **Mirror, not accumulate.** A sign-out in Chrome removes the site from the
  profile on the next pass.
- **Session cookies kept** for 30 days, which is what defect 2 needed.
- **Restricted and personal profiles wait for their scope.** Their logins are
  not copied while every worker may use them; narrowing access is owner-only
  (Scope tab), and the next pass after it fills them.
- Each pass logs `verdict=browser_login_sync` per profile with what was copied,
  removed, signed in and not signed in. A site the owner is not signed in to in
  Chrome is reported as `not_signed_in`: signing in there once fixes it.

`signed_in_to` now counts only unexpired session or auth cookies, and `logins`
gives each site's expiry, so a dead session reads as not signed in without
launching anything.
### Browser routing acceptance

The owner may set `AMUX_BROWSER_CHROME_USER_DATA_DIR` to select a Chrome profile
root; it defaults to the platform's normal Chrome directory. Routing still
copies the selected profile into its private automation directory.

For acceptance, launch an isolated server with its own `AMUX_HOME`, port and
`AMUX_BROWSER_CHROME_USER_DATA_DIR` beneath that home. Set
`AMUX_ROUTING_E2E_BASE`, `AMUX_ROUTING_E2E_HOME`, `AMUX_ROUTING_EVIDENCE`, and
`AMUX_ROUTING_CHROME_ROOT` to those matching paths. Then run:

```
node tests/browser-routing-api-e2e.mjs
AMUX_ROUTING_CUA=1 node tests/browser-routing-e2e.mjs
node tests/browser-routing-stress-e2e.mjs
AMUX_ROUTING_UI_CHROME_PROFILE='<chrome_profile from api-result.json>' AMUX_ROUTING_UI_WORK_IDENTITY=api-proof@example.test node tests/browser-routing-ui-e2e.mjs
```

The API test uses actual native profile contention and a temporarily unavailable
fixture Chrome directory to force CDP and CUA through the shipped endpoint. The
driver test independently injects transport failures, verifies no mutation
replay, kills the isolated CDP Chrome with SIGKILL and verifies persistence after
reopening. Both require an available Docker computer sandbox. For a Docker VM,
set `AMUX_ROUTING_FIXTURE_HOST` to a host address reachable from that VM. Receipts
include measured check counts, HTTP submissions and real browser screenshots.

The stress suite launches eight workers simultaneously against one saved
account while preserving its native owner. It checks distinct tabs, Unicode
input, exact HTTP submissions and page effects, rerendered/disabled/covered
element references, SPA navigation, popup ownership, scope revocation, an
actual driver SIGKILL during an action, and durable authentication after the
last CDP tab closes. It separately reports capability limitations rather than
counting unsupported behavior as successful task execution: current CDP
observations omit iframe/shadow DOM controls, and CSS-selector pointer dispatch
does not establish actionability or the intended page effect. Playwright role
locators are used only as an independent comparison for those fixture controls.
The current scope parser splits on whitespace as well as commas; exact
allowlist tokens cannot contain spaces. These controller tests are not an
agent/model benchmark.

### Goal-level recovery and real workers

The Browser tab saves Chrome and CUA choices independently for each selected
Amux profile. Selecting another profile restores its choices. An unconfigured
profile does not inherit another account's last-saved fallback.
`amux browser route config` exposes those owner choices and Chrome identities.
Ordinary Claude and Codex CLI workers receive the same selection/recovery guide
as orchestrated tasks at launch; raw isolated workers receive no injected guide.

Transport errors advance automatically. If a functioning browser still cannot
complete the goal, the worker uses
`amux browser route advance '{"reason":"describe the unmet goal"}'`. The Browser
tab exposes the same operation as **Try next route**. Each advance records
`goal_unmet`, preserves the selected account, and never replays an action.
Workers observe again before acting. Exhausted CUA reports a terminal refusal.
Stopping an advanced route also stops its original native browser only if that
worker started it; a busy browser belonging to someone else remains untouched.

Run the real-provider acceptance separately:

```bash
AMUX_ROUTING_REAL_WORKERS=1 node tests/browser-routing-worker-e2e.mjs
```

It uses the matching isolated server/profile variables above, a private
`TMUX_TMPDIR`, and normal background delivery (`AMUX_ISOLATED=0`), with
`TERM=xterm-256color` and `NO_COLOR` unset in the server environment. The private
tmux socket scopes all fleet jobs. The worker CLI must be installed from the
reviewed checkout with `scripts/install-cli.sh <private-home>/worker-bin`.
Synthetic saved native/Chrome logins include same-email, different-organization
decoys, personal and QA profiles. Headquarters is saved last to catch shared
fallback bugs. Real workers independently select the customer profile, solve a
multistep case, and handle functioning-but-blocked native/CDP sessions by
advancing through CUA. An all-blocked case requires an honest inability report.
An independent HTTP oracle checks account, organization, trusted browser clicks,
case, confirmation, exact reason and exact-once completion. Transcripts, route
receipts, launch guidance and screenshots are retained. These are local task
acceptance cases, not a WebArena/WASP benchmark or a universal model guarantee.

A cold CUA image may need Docker's existing bounded 30-minute provisioning
step. The route allows that setup to finish instead of cancelling it after
three minutes and retrying from scratch. Browser actions and ordinary backend
transports retain their shorter deadlines. The worker may wait for its CLI
background task; a setup error still reports failure and never reports a goal
as completed.

Route `status` returns HTTP 202 with `pending:true` and `running:null` while
another request owns the lane. It remains scope checked and does not claim a
ready desktop. Workers should wait for the background command, using status
rather than queueing state/action behind a cold build. Global process kills
are not a browser recovery mechanism; `route stop` owns lane cleanup.

The real-worker suite defaults to Claude; `AMUX_ROUTING_WORKER_PROVIDER=codex`
selects an actual Codex CLI worker, and `AMUX_ROUTING_WORKER_PHASES` can select
a recorded subset (`native,cdp,cua,exhausted` by default). Report provider/model
and subset explicitly; a provider quota failure is not a passed case.

Claude uses the `dontAsk` tool policy, allows Bash and Read
(including JSON formatting and screenshot reads), and denies process-kill
commands, file-edit tools and delegation. Codex uses workspace-write with network
access and an added private fixture home. This is a task tool policy, not an OS sandbox.
It retains actual provider tool inputs from only its owned workspaces, checks
metadata discovery and route use, and rejects direct HTTP/programmatic goal
writes or host process kills. Terminal banners are not provider evidence.
