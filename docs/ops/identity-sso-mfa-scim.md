# Single sign-on, a second factor and SCIM

Governance PR 20. Code: `crates/amux-server/src/api/sso.rs`, `mfa.rs`, `scim.rs`; the sign-in
door's own factor is in `cloud/desktop/door.py`. All three feed the one identity model amux
already had: a person is an `org_members` row, and a signed-in browser is a used `org_invites`
row whose token is the `amux_member` cookie. Scopes, the ledger actor and offboarding read it
the same way, however the person signed in.

## Single sign-on (OIDC)

Set in `server.env` (owner-only keys; a worker cannot set them):

| key | meaning |
|---|---|
| `AMUX_OIDC_ISSUER` | the provider's issuer URL, e.g. `https://accounts.google.com`, `https://<org>.okta.com`, `https://login.microsoftonline.com/<tenant>/v2.0` |
| `AMUX_OIDC_CLIENT_ID`, `AMUX_OIDC_CLIENT_SECRET` | the app you registered with the provider |
| `AMUX_OIDC_REDIRECT_URL` | what you registered as the redirect; defaults to `https://<host>/sso/callback` |
| `AMUX_OIDC_SCOPES` | defaults to `openid email profile` |
| `AMUX_OIDC_JIT_DOMAINS` | comma list; a person from one of these domains who is not a member yet becomes one on first sign-in |
| `AMUX_OIDC_JIT_TEAM` | the team (id or name) such a person starts in; without it they start with no access |
| `AMUX_OIDC_ALLOW_UNVERIFIED_EMAIL` | `1` for a provider that never sends `email_verified` (some Entra setups); reported in `/api/sso/status` |
| `AMUX_SSO_SESSION_HOURS` | how long an SSO session lasts (12) |
| `AMUX_SSO_REQUIRED` | `report` (default), `enforce` or `off` |

Members sign in at `/sso/login`. What is checked: the authorization code flow with PKCE S256,
a single-use `state` (ten minutes), a `nonce`, the ID token's signature against the provider's
published keys (RS256 or ES256 only), its issuer, audience and expiry, and `email_verified`.
The email must be an existing member's; anyone else is refused unless their domain allows
just-in-time provisioning. An offboarded member is refused. If the token's `amr` names a second
factor (`mfa`, `otp`, `hwk`, ...), the session counts as having proved one.

`AMUX_SSO_REQUIRED=enforce` sends a member whose session came from an invite link to
`/sso/login` (API calls get 403 `sso_required`). It never touches the owner: the owner's bearer,
or the sign-in door's owner proof, is the break-glass path and always works.

## A second factor (TOTP)

Anyone signed in (a member, or the owner) opens `/mfa` or uses `amux mfa enroll` / `amux mfa
confirm <code>`: the key goes into any authenticator app, the first code turns it on, and ten
recovery codes are shown once. The secret is sealed with the at-rest key (PR 18) and bound to
its person; recovery codes are kept as SHA-256 only; a code is good once.

| key | values |
|---|---|
| `AMUX_MFA_POLICY` | `off` (no one enrolls), `optional` (default: a member who enrolled proves it once per session), `required` (every member session proves one) |
| `AMUX_MFA_ENFORCE` | `report` (default: counted, let through), `enforce` (a page goes to `/mfa`, an API call gets 403 `mfa_required`), `off` |
| `AMUX_MFA_STEP_UP` | `report` (default), `enforce`, `off` |

Step-up covers the owner's governance powers: the kill switch, placing and lifting a legal
hold, erasing a person, changing a worker capability (`AMUX_CAP_*`), issuing or revoking a SCIM
token, and resetting a member's factor. `amux mfa step-up <code>` (or `POST /api/mfa/step-up`)
returns a five-minute token; send it as `X-Amux-Step-Up`, or `export AMUX_STEP_UP=<token>` and
the CLI sends it. With `enforce` and no factor enrolled, those powers are refused until the
owner enrolls; turning the switch to `report` is the way back.

A lost phone: a recovery code, or the owner runs `amux mfa reset member:<email>`.

### At the sign-in door

The door (`cloud/desktop/door.py`) has its own factor, since it signs people in before amux sees
them. `sudo python3 /opt/amux-door/door.py totp <user>` prints an `otpauth://` link to add to an
authenticator; the sign-in form then needs the current code. Basic credentials for that account
carry it after the password (`user:password:123456`). `DOOR_MFA`: `off`, `optional` (default),
`required` (an account with no factor cannot sign in). Secrets live in `/etc/amux-door/totp`
(0600).

## SCIM 2.0

The owner issues a token (`amux identity scim-token okta`, shown once; only its hash is kept) and
gives the provider `https://<host>/scim/v2` with that bearer.

| SCIM | amux |
|---|---|
| `Users` (create, get, list with `filter=userName eq "..."`, PATCH, PUT, DELETE) | members; `userName` is the email and cannot change |
| `active: false`, or DELETE | the offboarding of PR 13: sign-ins and invites revoked, schedules paused, sponsored workers back to the owner, standing approvals revoked, approved apps back to pending |
| `active: true` | the person may sign in again |
| `Groups` (create, get, list, PATCH members, DELETE) | teams; a person is in one team, so adding them to a group moves them |

A person or group made through SCIM starts with no access (`AMUX_SCIM_DEFAULT_TEAM` names a team
to start people in instead). The provider decides who is in; the owner decides what each team
sees, in Team settings. The Everyone team cannot be deleted through SCIM.

## What is recorded

Every sign-in (and refusal), enrolment, verification, step-up, reset, token change and SCIM
change is an `identity` ledger row: sign-ins as `member:<email>` with authority `oidc`, SCIM as
`policy:scim:<token id>` on behalf of the owner. Counters are under `sso`, `mfa` and `scim` in
`GET /api/contract/counters`.

## Not done

- SSO behind the sign-in door: the door does not yet pass `/sso/` and `/scim/` through, so on a
  door host the provider's redirect and SCIM calls stop at the door.
- A tailnet sign-in has no cookie, so its session cannot be marked as having proved a factor;
  with `AMUX_MFA_ENFORCE=enforce` and a factor owed, such a member is refused and has to sign in
  by link or SSO instead.
- A dashboard screen for the SSO and SCIM settings (they are `server.env` keys and the CLI).
- Single logout (the provider's session ending does not end amux's before its 12 hours).
- WebAuthn / passkeys.
