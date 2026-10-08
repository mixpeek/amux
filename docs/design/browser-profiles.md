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
primary). The profile list is sorted by role, so its first rows are the answer.

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
AMUX_ROUTING_UI_CHROME_PROFILE='<chrome_profile from api-result.json>' AMUX_ROUTING_UI_WORK_IDENTITY=api-proof@example.test node tests/browser-routing-ui-e2e.mjs
```

The API test uses actual native profile contention and a temporarily unavailable
fixture Chrome directory to force CDP and CUA through the shipped endpoint. The
driver test independently injects transport failures, verifies no mutation
replay, kills the isolated CDP Chrome with SIGKILL and verifies persistence after
reopening. Both require an available Docker computer sandbox. For a Docker VM,
set `AMUX_ROUTING_FIXTURE_HOST` to a host address reachable from that VM. Receipts
include measured check counts, HTTP submissions and real browser screenshots.
