# amux Cloud execution ledger

Task state for executing [cloud-self-service.md](cloud-self-service.md). The
`amux-cloud` worker is isolated and has no board, so this file is the record.
Statuses: done (committed, with evidence), blocked (with the owner question),
todo.

## Path to 10 (plan 13.2)

| # | Step | Status | Evidence or blocker |
|---|---|---|---|
| 0 | Demand check: waitlist form on /cloud | done | `site/cloud/index.html`; browser test with PostHog loaded and blocked at 1280 and 375 px; control-plane `/api/notify` for when it is live |
| 1a | `plans.json` and the price check (M13, T-SITE-2) | done | `cloud/control/price_check.py`; 8 tests; live Hetzner margin check passes |
| 1b | Pricing page, cloud page and homepage CTA without trial or Calendly | blocked | Owner: launch prices (O1) and the offer to existing $20 subscribers (O3) |
| 2 | Control plane: checkout-first accounts, Stripe, servers, provision jobs, redirect, emails, kill switches | done | `cloud/control/`; fake-mode Chromium journey PASS 11 checks |
| 3 | Golden image, door token mode, model sign-in screen, templates | done except the image build run | `build_image.py --dry-run`; door tests against a real amux-server; CLI login fixtures for Claude Code 2.1.299, Codex 0.162.1, Gemini CLI 0.64.0; first real image build waits on the spend ask |
| 4 | Fleet agent: backups, restore drill, signed releases, heartbeat | done except the restore drill job | release install and rollback tested with real ssh-keygen signatures; per-server R2 bucket and scoped token |
| 5 | Safe lifecycle in shadow mode, dunning, cancel, deletion, abuse, disputes | done except abuse isolation (M17) | 17 lifecycle tests, every one of 11 safety-rule mutations reddens a named test; account deletion with retried steps; dispute evidence |
| 6 | Invites through cloud.amux.io; add to tailnet | done | door redeems invites against a real amux-server; `/api/tailnet/*`; tailnet-only switch refused without a tailnet |
| 7 | iOS: Sign in with Apple and web sign-in | done, not yet on a device | app builds for the simulator; control-plane Apple verification tested with generated keys; no-purchase string scan; device test needs the Apple capability (P3) |
| 8 | Journey every 6 hours, nightly capability suite, weekly report | weekly report done | `report.py` says "not measured" instead of zero; the scheduled journey needs a deployed control plane |
| - | Portability: amux.yaml, export/import, `amux cloud move/pull/apply/login` | done | export from one real amux-server and import into another: every kind, idempotent re-import, schedules disabled, workers not started, secret-shaped values refused (5 tests) |

## Where the build departed from the plan, and why

| Plan said | Built | Why |
|---|---|---|
| Door verifies an Ed25519 token | HMAC-SHA256 with a per-server secret | Same isolation (a token for one server cannot open another, T-SSO-1) while the door stays on the Python standard library |
| iOS Sign in with Apple through Clerk's iOS SDK | The control plane verifies Apple's identity token itself and keeps the refresh token for revocation | No SDK in the app, works with the control plane's own session, and revocation on deletion does not depend on what Clerk keeps |
| The move bundle merges an allowlist of database tables through a staging copy | Export and import through amux's public API, matched on natural keys | No raw database ever leaves a machine, it works across server versions without migrations, and every write passes the target's own checks |
| Payments through Paddle | Stripe (owner decision, 2026-10-14) | Stripe Tax and automatic dispute evidence cover what Paddle would have |

## Owner asks

| Ask | Why it blocks | Unblocks |
|---|---|---|
| Approve launch prices $39 / $69 / $129 (O1) | Publishing prices and creating live Stripe products | A yes, or other prices |
| Approve Hetzner spend: one cx23 for the control plane ($6.49/month), golden image builds (about $0.02 each) and up to $25/month of short-lived test servers | Running the control plane, building the image and the real provisioning and restore tests | A yes with a cap |
| A Stripe test-mode secret key in `~/.amux/server.env` as `STRIPE_TEST_SECRET_KEY` | Checkout, webhook and dunning tests against real Stripe test mode | The key present |
| Sign in with Apple enabled on the App ID, plus a Services ID and a Sign in with Apple key (P3) | The iOS sign-in on a device, and token revocation on deletion | The capability on, and the key in `server.env` as `APPLE_TEAM_ID`, `APPLE_KEY_ID`, `APPLE_PRIVATE_KEY` |
| Offer to existing $20 and $200 tunnel subscribers (O3) | Rewriting /cloud and /pricing for the new product | A decision; the email itself goes out only after the owner approves its text |
