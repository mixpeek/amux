# Moving amux Cloud to its own Stripe account

Owner ask, 2026-11-21 (relayed by mixpeek-finances): amux Cloud bills from its own Stripe
account under Mixpeek Inc., so its revenue is easy to tell apart from Mixpeek's. Today it sells
from Mixpeek's live account (about $3,039 a month of subscriptions). Nothing on the Mixpeek side
changes: its subscriptions, prices, webhooks and the `STRIPE_LIVE_SECRET_KEY` in server.env stay
as they are.

Every live step below is parked until Ethan says go. The tools refuse a write without
`--approved <his go>`.

## What moves, and how nobody pays twice

`cloud/control/stripe_account_move.py` does the Stripe and Hetzner work. Each amux Cloud
subscription in the shared account gets a copy in the new account:

- **Same customer and card.** Stripe support copies customers and their cards between accounts
  under one owner, keeping the customer ids. Nobody re-enters a card.
- **Billing starts where the old paid period ends.** The new subscription is created with
  `billing_cycle_anchor` at the old period end and `proration_behavior=none`, so it costs nothing
  until that day and then bills on the same date as before. The old subscription is set to
  `cancel_at_period_end`, so it never renews. One period, one charge.
- **A customer who already cancelled stays cancelled.** Their copy gets `trial_end` at the old
  period end and `cancel_at_period_end`: it is never charged and ends the same day, and the new
  account's end event deprovisions the server as usual.
- **Servers follow.** `relabel` points each server's `amux-sub` label at its new subscription
  (the old id is kept in `amux-moved-from`). Final snapshots of past customers get an `amux-cust`
  label, and the Worker restores a returning customer through it (`restore.by_customer` in the
  Worker log).

A subscription counts as amux Cloud's only when it has `metadata.slug` and every price on it
carries an amux Cloud lookup key from `cloud/control/plans.json`. Everything else in the shared
account is Mixpeek's: `plan` counts it and never lists or writes it.

`plan` refuses to let `apply` run while any subscription is blocked, and says why for each:
signup unfinished, past due, a lasting discount, a missing price or customer in the new account,
no default card, no ready server, or a period ending within the hour. Nothing is written until
every blocker is cleared.

## What Ethan does himself

1. **Create the account.** Stripe Dashboard, account menu, New account, named "amux Cloud", under
   Mixpeek Inc. Set the statement descriptor (AMUX CLOUD), public details (amux.io, support
   hello@amux.io), payouts bank, and tax settings if amux Cloud collects tax.
2. **Store its live secret key** as `AMUX_CLOUD_STRIPE_LIVE_SECRET_KEY` with
   `POST /api/connectors/<id>/credentials` (or the Connectors page). Leave
   `AMUX_CLOUD_STRIPE_ACCOUNT` unset: that is the switch, and it moves at cutover.
3. **File the card copy request** with Stripe support, from the shared account: copy the
   customers and payment methods of the amux Cloud customers (the ids `plan` lists under
   `customer`) to the new account. Stripe takes a few days. `plan` shows when it has landed
   (no "not in the new account" blockers left).
4. **Approve the cutover**, by message, once `plan` reads `"blocked": 0`. That message id is the
   `--approved` value.

## Steps

Commands run from the repo root on this Mac. `--test` on any `stripe_account_move.py` command
rehearses on two test-mode accounts (`STRIPE_TEST_SECRET_KEY`,
`AMUX_CLOUD_STRIPE_TEST_SECRET_KEY`).

### Before the cutover (no customer sees anything)

```bash
cloud/start/deploy.sh live-prepare      # products, prices, FIRSTMONTH, portal, webhook in the NEW account
python3 cloud/control/stripe_account_move.py plan > ~/.amux/cloud/stripe-move-plan.json
```

`live-prepare` runs `stripe_setup.py --live` against the new account only and stores its webhook
secret in `~/.amux/cloud/stripe-live-amux-webhook-secret-worker`. The live Worker is not touched.
Check `plan`: `amux_mrr_usd` near the $3,039 figure, `left_alone_not_amux` equal to Mixpeek's
count, `blocked` 0.

### Cutover (one sitting, about ten minutes; not near 13:00 UTC, when the daily audit runs)

```bash
GO=MSG-<Ethan's go>
python3 cloud/control/stripe_account_move.py plan                        # still blocked: 0
python3 cloud/control/stripe_account_move.py apply --approved $GO        # new subscriptions; old ones end at period end
# set AMUX_CLOUD_STRIPE_ACCOUNT=amux in ~/.amux/server.env
cloud/start/deploy.sh live                                               # log says: stripe account: amux
python3 cloud/control/stripe_account_move.py relabel --approved $GO      # servers and snapshots follow
python3 cloud/control/stripe_account_move.py retire-old-webhook --approved $GO
```

Between `apply` and the deploy, the new account's events reach a Worker still holding the old
secret and are refused. Stripe retries them, and the Worker only acts on checkout completions and
subscription ends, neither of which the move creates. After `retire-old-webhook` the shared
account no longer sends amux anything, so the old subscriptions ending at period end cannot
deprovision a server.

### Check

- `curl -s -H "x-ops-key: $(cat ~/.amux/cloud/ops-key-live)" https://cloud.amux.io/api/ops`:
  `customers` equals the moved count, `problems` is empty (no "no subscription Stripe knows").
- `plan` again: every row has `new` set.
- One fresh signup through amux.io/start lands in the new account.
- At the first renewal date, the new account shows the invoices and the shared account shows
  none for amux.

### Rollback (any time before the first old period ends)

```bash
python3 cloud/control/stripe_account_move.py rollback --approved $GO    # old subscriptions renew again, new ones cancelled, labels back
# set AMUX_CLOUD_STRIPE_ACCOUNT=shared
cloud/start/deploy.sh live      # recreates the shared account's webhook if it was retired, and stores its secret
```

`rollback` refuses once any old subscription has ended, because by then that customer is already
billed from the new account.

## Not covered

- The old gateway (`cloud/gateway/gateway.py`, `STRIPE_SECRET_KEY` in GitHub secrets, the
  platform plan) is not part of this move. If `plan` counts gateway-era subscriptions among
  `left_alone_not_amux`, they need their own decision.
- Past invoices and payouts stay in the shared account's history. The split starts at cutover.

Tests: `python3 cloud/tests/test_stripe_account_move.py` (the CLI against fake Stripe and
Hetzner) and `node cloud/start/test-worker.mjs` (restore through `amux-cust`).
