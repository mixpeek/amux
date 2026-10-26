# Connector uptime: every connector stays live

Owner: the `amux` lane (Ethan, 2026-10-26: "take ownership of keeping every
connector live permanently: detect expiry before it happens, re-auth or send
me a one-click link, and fix the root cause"). This file is the task ledger;
the amux lane's board is off.

## State measured 2026-10-26 10:57

| Connector | Live test | Cause |
|---|---|---|
| google-gmail | connected, all 5 accounts | the `error` was stale: the list showed the last live test, and nothing re-ran it after the accounts were re-authorized |
| google-calendar | connected | same |
| google-drive | connected | same |
| google-admin | needs_auth | service-account delegation: `unauthorized_client`; the SA client (115154724552579385731) is not granted the admin.directory scopes in Workspace Admin |
| google-youtube | needs_auth | same error for the youtube scopes. The YouTube Data API does not accept domain-wide delegation for a channel, so this needs a user OAuth grant |
| slack, telegram, mattermost | needs_credentials | never configured |

Two amux components disagree about the same accounts: the maintenance canary
(`/api/connectors/maintenance`) reports hello@amux.io, beefinethan@gmail.com and
esteininger21@gmail.com's Google grant as `needs_reauth`, while a live provider
call for each succeeds.

## Plan

1. **No stale verdicts.** A connector's status comes from a live test that the
   server re-runs on a schedule and right after any re-authorization, so `error`
   means "failing now". The canary and the live test read one result, so they
   cannot disagree.
2. **See expiry coming.** For a grant from an OAuth client in Testing status,
   the refresh token dies 7 days after it was issued. Record the issue time on
   every grant, and at day 6 send one alert carrying a one-click re-auth link
   (built from `AMUX_PUBLIC_URL`). Do the same for any token whose refresh
   fails, the moment it fails.
3. **Re-auth without the owner where possible.** Workspace accounts (mixpeek.com,
   and amux.io if it is a Workspace domain) move to service-account delegation,
   which never expires. Only personal Gmail accounts keep user OAuth.
4. **Root cause for personal accounts (AMUX-3747).** The shared OAuth client is in
   Testing, so its tokens expire weekly. The options, cheapest first:
   - Publish the consent screen to Production without verification. Google
     then shows an "unverified app" screen at sign-in and caps the app at 100
     users, but refresh tokens stop expiring every 7 days. For a handful of the
     owner's own accounts this is enough. Needs Ethan: one click in Google
     Cloud console.
   - Full verification for the restricted Gmail scopes (a CASA security
     assessment, paid). Only needed if the app serves outside users.
5. **Admin and YouTube.** Admin: Ethan adds the two admin.directory scopes to
   the SA client in Workspace Admin > Security > API controls > Domain-wide
   delegation. YouTube: switch the connector from delegation to a user OAuth
   grant on the channel's account.

## Needs Ethan

- Publish the OAuth consent screen to Production (item 4).
- Add the admin.directory scopes to the SA client's delegation (item 5).
- Grant YouTube once through its OAuth link after item 5 lands.
