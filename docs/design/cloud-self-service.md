# amux Cloud: a boring, profitable hosting business that runs itself

Status: plan, revision 10. Owner: amux-cloud. Written 2026-10-14. Supersedes the
multi-tenant trial model in [cloud-poc-provisioning-plan.md](../cloud-poc-provisioning-plan.md).

**Purpose.** A customer pays, gets their own amux server, brings their own model
key, and keeps paying because it keeps working. amux runs the business: it
sells, provisions, backs up, updates, supports and repairs without the founder.
The target is the first 10 paying customers on a safe base, then 100 customers
with under two hours of founder work a month.

**Decision rule.** Prefer the simpler, more reliable system that earns slightly
less over a complex one that grows faster but needs active management. No new
subsystem is added unless it makes the business safer, simpler, more profitable
or more autonomous. Founder work is flagged **[owner]**.

**Evidence labels.** **[exists]** is verified in the repository or a live API
(file and line given). **[build]** is new work. **[H]** is a hypothesis to be
measured.

**Names used throughout.** Payments run on Stripe, which is already set up
(D11). "The move bundle" is the allowlisted export in section 7.1; the plan
never copies a raw database. "The YAML" is the declarative server file in
section 7.1.

## 1. Three parts, three failure domains

| Part | What it is | Runs on | Depends on |
|---|---|---|---|
| **amux OSS** | The product. Free, MIT plus Commons Clause [exists] | The user's machine | Nothing of ours |
| **amux Cloud** | Hosting: a control plane plus one server per customer | cloud.amux.io and `<slug>.amux.cloud` | The payment provider, Clerk, Hetzner, Cloudflare, Cloudflare R2. **No agents.** |
| **amux Operator** | One worker that watches the business and improves it | Its own amux server | Read access to Cloud data; a narrow admin API |

Billing, sign-in, provisioning, backups, updates, recovery, lifecycle actions
and kill switches are plain code in the control plane and the fleet agent. They
keep working with the Operator stopped or deleted.

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | **Single tenant: one Hetzner server per paying customer.** | Each server pays for itself from day one; isolation is physical. The multi-tenant host is already offline (`.github/workflows/deploy-cloud.yml:229`, `CLOUD_HOST` unset) [exists]. |
| D2 | **Floor 2x server cost; launch price $39 Solo.** **[owner]** approves once. | $39 contributes $24.30 a customer a month after VAT, fees and backups, against $10.64 at $26 (4.2). $39 wins unless it converts under 44% as many buyers as $26 [H]. Hosted agents sell at $20 to $25 plus usage (Devin Core, Cursor background agents, Replit); generic open-source hosting at $1 to $12 per app (PikaPods, Elestio). |
| D3 | **Paid first, no free trial.** The free tier is the open-source install; moving a local install to Cloud is the main way in. | A trial spends servers on non-buyers and invites abuse. OSS users are the warmest buyers (4.5). |
| D4 | **Bring your own model key or subscription.** | No model spend on our books. The gateway's `/anthropic/` proxy (`cloud/gateway/gateway.py:1264`) is removed. |
| D5 | **One account, at cloud.amux.io.** Clerk accounts with email code, Google, GitHub and Sign in with Apple. | Today Clerk lives on the gateway and passwords in `cloud/desktop/door.py`, so a customer would have two logins [exists]. Apple sign-in is required in the iOS app whenever other social sign-ins are offered (App Store guideline 4.8). |
| D6 | **Checkout first, account second.** | One form; the success URL creates the account idempotently (J5). |
| D7 | **EU servers only at launch.** | The cx line exists only in fsn1, nbg1 and hel1. In the US the cheapest 8 GB server is ccx13 at $50.99, about 5x (Hetzner pricing API, 2026-10-14). |
| D8 | **Cloudflare proxies every customer host.** | Its edge certificate covers `*.amux.cloud` and Origin CA certificates have no rate limit, so Let's Encrypt limits never apply. It also hides server IPs. Free-plan limits: 100 MB per request (amux uploads chunk, `upq`), 100 s response timeout (SSE pings every 10 s). |
| D9 | **Releases ship as signed binaries through a fleet agent, with canary and automatic rollback, before the first customer.** | `AMUX_UPDATES=source` compiles Rust on the box for 10 to 15 minutes [exists]; wrong on a customer machine. A broken update with no rollback is the likeliest way to need the founder. |
| D10 | **The objective is owner cash flow after owner time and risk.** | Judged on retained contribution at 30 and 90 days and on measured owner minutes (section 9). |
| D11 | **Stripe for payments, with Stripe Tax, Stripe Radar and automatic dispute evidence.** **[owner]** decided 2026-10-14. | Stripe is already set up and has true time travel for tests (test clocks). A US seller to EU consumers must register once for the EU's non-Union OSS scheme and file a quarterly return; Stripe Tax calculates and collects VAT, and returns are filed through Stripe's tax-filing integration ($50 a month assumed [H]). Disputes are answered automatically with evidence through the API (M20). All billing code sits in one `billing` module. |
| D12 | **A server is running or deleted, never stopped.** | Hetzner bills stopped servers. A lapsed server is snapshotted, the snapshot is verified, then the server is deleted; restoring is a re-provision from the snapshot. |
| D13 | **No automatic deletion acts on a single signal.** | Section 3.2. Every destructive step needs agreeing evidence from two sources, waits out a hold period, stays under a daily cap, and stops when any provider is unhealthy. |
| D14 | **Every plan includes teammates at no extra charge.** | Invites already exist in amux (`/api/org/invites`, teams with scopes, revocation) [exists]. Seat billing would be a new subsystem; plans differ only by server size. |

## 3. Architecture

```
amux.io   GitHub README   App Store   amux OSS ("Move to amux Cloud")
     \          |             |              /
      v         v             v             v
cloud.amux.io  CONTROL PLANE (plain code)
  Clerk | billing module (Stripe) | fleet DB | provisioner | lifecycle | redirect | kill switches
  hcloud API, Cloudflare API, R2
        |                                          ^ read-only data, narrow admin API
        v                                          |
<slug>.amux.cloud (Cloudflare proxy)         amux OPERATOR (its own server)
  Hetzner server per customer:               one worker, shell schedules, MDAI
  Caddy (Origin CA) -> door (control-plane token) -> amux-server
  fleet agent: heartbeat, signed releases, litestream to R2, restore drill
```

| Component | Exists | Build |
|---|---|---|
| Accounts | Clerk JWT check and org creation (`/api/cloud-auth`, `gateway.py:2522`); gateway org invites (`:2611-2633`) | Account from checkout (J5); Clerk social connections for Google, GitHub and Apple; a control-plane `email_verified` flag (Clerk marks Backend-API users verified, so the restriction is ours); remove `TRIAL_DAYS` (`:44`), trial rows (`:2539`, `:2683`), the trial gate (`:3696-3707`) |
| Billing | Stripe checkout (`:3140`), webhook (`:2382`), portal (`:3203`), status (`:3223`); `invoice.payment_failed` only logged (`:2483`) | The `billing` module over the existing Stripe code: checkout, webhooks, portal, Stripe Tax, Radar; handle `invoice.payment_failed`; prices from `plans.json`; remove the $5,000 pages (`:496`, `:586`) and budget gate (`:3727-3739`) |
| Provisioning | `cloud/desktop/bootstrap.sh` builds a complete host; `remote.py` loads and verifies | hcloud and Cloudflare clients; `servers` and `provision_jobs` tables; a golden image rebuilt from `bootstrap.sh` on each release; every server labelled `amux-cloud=1, org=<id>` |
| Sign-in on the server | `door.py` (cookie, Basic, `X-Door-User`); member identity from Caddy (`bootstrap.sh:317-358`) | Token mode: door verifies a 60-second Ed25519 JWT (aud = slug, sub = member) and sets a host-only `__Host-` cookie |
| Workspace access | amux org members, invites, teams and scopes (`crates/amux-server/src/api/org.rs`, routes `/api/org/members`, `/api/org/invites`, `/api/org/teams`), Team settings UI, revocation; tested locally by `e2e/local-multiplayer.spec.ts` [exists] | Invite links resolve through cloud.amux.io so the invitee signs in with Clerk and arrives as the invited member (section 8.2) |
| Updates | `self_update.rs`, the auto-builder | Fleet agent: signed bundles, canary, rollback, SIGKILL-safe swap |
| Backups | litestream to R2 in the compose template | litestream on every host plus Hetzner's backup option; a weekly restore drill per server |
| Model keys | `PATCH /api/settings/env` (`crates/amux-server/src/api/settings.rs:51`); `has_api_key` (`api/mod.rs:1095-1125`); key modal (`app.js:1518-1530`) | A first-screen key step that validates with one provider call before saving |
| iOS | App listed (id6760410435); the cloud button now opens amux.io "to schedule onboarding" and the old Sign in with Apple was removed (`ios/AmuxApp/Sources/ServerPickerView.swift:61`) [exists] | Native Sign in with Apple plus "Sign in with amux Cloud" (section 8.3) |
| Analytics | PostHog on the site (`site/site.js`) and gateway (`gateway.py:21`) | Funnel events (section 6) mirrored into a control-plane table |

### 3.1 Backups, restores and updates (before the first customer)

| Layer | What | Verified by |
|---|---|---|
| Continuous | litestream replicates `amux.db` to R2 every 60 seconds (`sync-interval: 60s`, a daily full snapshot), so at most one minute of data can be lost; the 1-second default would cost up to $11.70 a month per server in R2 writes; a nightly `restic` snapshot of the home directory (files, `.mdai`, skills, checkouts) to R2 | Fleet agent reports the age of the last replicated write; over 15 minutes raises M4 |
| Daily | Hetzner backups (7 kept, 20% of the server price) | Hetzner API lists today's backup |
| Weekly | Restore drill: a throwaway server restores from R2, starts amux, compares row counts and a file checksum list with the live server, then is deleted | A failed or skipped drill raises M4; T-OPS-3 |
| Before every destructive step | A snapshot whose existence and size are confirmed through the API | Section 3.2 |

Updates: each release is built once in CI, signed, and published. The fleet
agent downloads it, verifies the signature, keeps the previous binary, swaps by
rename, and health-checks for 5 minutes; a failed check rolls back alone.
Rollout is the synthetic customer first, then 10% of servers, then all, with a
24-hour soak between stages and an automatic halt if any stage rolls back twice.
A customer can pin "stay on this version" for 14 days from Settings.

### 3.2 Safe lifecycle: nothing is deleted on one signal

Lifecycle actions run from one table, `lifecycle_actions`, with explicit
states: proposed, held, executing, done, cancelled. Rules:

1. **Provider state is read, not inferred.** A webhook only proposes. Before any
   step that stops service or deletes, the control plane reads the subscription
   directly from the payment provider's API, and the read must agree with the
   proposal.
2. **Two sources must agree.** Deleting a lapsed customer's server needs the
   provider API showing the subscription ended or unpaid past grace, and our own
   ledger showing no successful payment since. Disagreement cancels the action
   and opens a card.
3. **Hold periods.** Payment failure: 14 days of normal service, then snapshot
   and delete, then the snapshot is kept 30 more days. Cancellation: at period
   end, snapshot and delete, snapshot kept 30 days. Each customer is emailed
   before each step.
4. **Provider health gate.** If the payment provider's API or status page, the
   webhook queue (no webhook for 6 hours while checkouts happen), Hetzner or R2
   is unhealthy, every destructive action pauses. Pausing costs a few dollars of
   server time; it never costs a customer.
5. **Caps and a breaker.** At most 3 deletions a day, and none if one run would
   delete more than the greater of 3 servers or 5% of the fleet. Breaching either freezes all
   lifecycle actions and alerts the owner, because that pattern means our data
   or a provider is wrong.
6. **Snapshot first, verified.** No server is deleted until its snapshot is
   listed by the Hetzner API with a plausible size and the latest R2 backup is
   under 1 hour old.
7. **Orphans are reported, never deleted.** A Hetzner server without the
   `amux-cloud=1` label is never touched. A labelled server with no matching row
   is reported in the weekly report and left running.
8. **Shadow mode first.** For the first 30 days after launch, lifecycle actions
   are recorded but not executed (lapsed servers keep running at about $12.59
   a month each). Each shadow decision is checked automatically 14 days later
   against a fresh read of the provider: a decision to delete must still show
   the subscription ended, and no deleted-in-shadow customer may have paid since.
   Execution turns on after at least 30 days **and** at least 5 replayed
   decisions with no disagreement; any disagreement keeps shadow mode on and
   opens a card. With too few lapses to judge, shadow mode simply continues.

Provisioning in the other direction (paid but no server) is safe and runs
immediately.

## 4. Economics

All figures are computed by `docs/design/cloud-self-service-econ.py`; rerun it
when an input changes.

### 4.1 Cost per server (Hetzner API, USD/month excl. VAT, 2026-10-14)

| Plan | Server | Specs | Server | IPv4 | Backups | Cost | 2x floor | Launch price |
|---|---|---|---|---|---|---|---|---|
| Solo | cx33 | 4 vCPU, 8 GB, 80 GB | $9.99 | $0.60 | $2.00 | $12.59 | $26 | **$39** |
| Team | cx43 | 8 vCPU, 16 GB, 160 GB | $18.49 | $0.60 | $3.70 | $22.79 | $46 | **$69** |
| Max | cx53 | 16 vCPU, 32 GB, 320 GB | $34.99 | $0.60 | $7.00 | $42.59 | $86 | **$129** |

Prices include VAT for EU consumers and exclude it for VAT-registered businesses
(Stripe Tax, tax-inclusive pricing for EU consumers); tables use the amount
after VAT.

### 4.2 Profit at scale (Stripe)

Variable per customer: server; Stripe 2.9% + $0.30, Billing 0.7%, Tax 0.5%,
plus 1.5% international-card and 1% currency fees on the 75% of cards issued
outside the US [H]; disputes 0.3% at $15 [H]; refunds 1% [H]; abuse 0.5%
[H]; support inference $0.40 [H]; lapsed snapshots about $0.02 [H]; R2 backups $0.24 (litestream syncing every 60 seconds plus a nightly file snapshot and about 3 GB stored) [H, measured per server in the synthetic run]. VAT: 35% of
customers are EU consumers at 21% [H]. Fixed: control plane $7.09 per 1,500
servers [H]; EU VAT return filing $50 [H]; the synthetic customer every 6 hours and on each deploy, $7.65; the
Operator's server $12.59 and its inference capped at $50 plus 5% of
contribution, at most $250. Plan mix 80% Solo, 16% Team, 4% Max [H]. Owner time
here is the target; section 4.8 stresses it.

| Ladder | Customers | Gross | After VAT | Variable | Fixed | Contribution | Margin |
|---|---|---|---|---|---|---|---|
| $26 (2x cost) | 100 | $3,160 | $2,968 | $1,879 | $178 | $911 | 31% |
| $26 (2x cost) | 1,000 | $31,600 | $29,680 | $18,793 | $327 | $10,560 | 36% |
| **$39** | 10 (no Operator) | $474 | $445 | $199 | $65 | $181 | 41% |
| **$39** | 100 | $4,740 | $4,452 | $1,997 | $246 | $2,208 | 50% |
| **$39** | 500 | $23,700 | $22,260 | $9,987 | $327 | $11,946 | 54% |
| **$39** | 1,000 | $47,400 | $44,521 | $19,974 | $327 | $24,219 | 54% |
| **$39** | 5,000 | $237,000 | $222,604 | $99,870 | $349 | $122,385 | 55% |
| $49 | 100 | $5,980 | $5,617 | $2,090 | $300 | $3,227 | 57% |
| $49 | 1,000 | $59,800 | $56,168 | $20,901 | $327 | $34,939 | 62% |

### 4.3 Price against churn

Per 1,000 buyers at the $26 conversion rate, if $39 converts 0.80x and $49 0.65x
as many [H]:

| Ladder | Monthly churn | Per customer | Customers at day 90 | Months 1 to 3 | Lifetime |
|---|---|---|---|---|---|
| $26 | 5% | $10.64 | 857 | $30,343 | $212,750 |
| $26 | 12% | $10.64 | 681 | $28,236 | $88,646 |
| $39 | 5% | $24.30 | 686 | $55,445 | $388,747 |
| $39 | 12% | $24.30 | 545 | $51,595 | $161,978 |
| $49 | 5% | $35.02 | 557 | $64,925 | $455,215 |
| $49 | 12% | $35.02 | 443 | $60,416 | $189,673 |

A higher price wins on conversion unless conversion falls under 44% ($39) or 30%
($49) of the $26 rate. Churn decides it: $26 at 5% churn ($213K lifetime) beats
$39 at 12% ($162K). Price is judged on 90-day retained contribution (M13).

### 4.4 Single against multi-tenant

| | Multi-tenant (today's gateway) | Single tenant |
|---|---|---|
| Idle capacity | Ours | The customer's, priced per server |
| Noisy neighbour | Containers capped at 3 GB on a shared host | None |
| Bad release or escape | Every customer on the host | One customer, limited further by canary |
| Ops | One delicate host | Identical hosts, one fleet agent |

### 4.5 No free trial

| Option | Verdict |
|---|---|
| Paid first; OSS is the free tier | **Chosen.** Every server is paid for; no abuse surface |
| OSS user moves to Cloud (section 7.1) | **Chosen as the main bridge.** They already use amux and keep their data |
| Hosted preview | Rejected: a second hosting mode, free compute for abusers, unknown conversion |

### 4.6 Volume (measured 2026-10-14)

amux.io had 3,596 unique visitors in 30 days; 178 viewed `/pricing`, 19 viewed
`/cloud` (PostHog). GitHub had 459 unique visitors in 14 days (top referrers
amux.io 177 and Google 64) and 534 stars since 2026-02-18.

| Share of pricing viewers who buy [H] | Viewers flat | Viewers +10%/month | Flat-traffic ceiling at 5% churn |
|---|---|---|---|
| 1% (planning case) | 100 never reached | 23 months | 36 customers, $716/month |
| 2% | 100 never reached | 17 months | 71 customers, $1,532/month |
| 5% | 17 months | 9 months | 178 customers, $4,042/month |
| 8% | 9 months | 7 months | 285 customers, $6,668/month |

Qualified traffic is the constraint. The 500 to 5,000 rows of 4.2 describe the
business's value at scale and arrive only if organic traffic grows severalfold.

### 4.7 Operating budgets

| Budget | Cap | Enforced by | At the cap |
|---|---|---|---|
| Paid acquisition | $0 | No ad accounts; the Operator holds no card | **[owner]** opens one deliberately |
| Operator inference | $50 + 5% of last month's contribution, max $250 | An Anthropic Console workspace spend limit, reset monthly by the control plane | The worker stops; the report shows its cost against the contribution credited to its changes, and the cap is not raised below a 3x return |
| Support inference | $0.40 per customer, pooled | Its own Console workspace | Escalations go to the queue |
| Infrastructure | Servers = paid subscriptions + synthetic + held lifecycle servers + 1 | Hourly count in the control plane | Provisioning freezes; owner alerted |
| Refunds | Automatic up to the lesser of 2% of last month's revenue or $500 | Control-plane counter | Queued for the owner; paid by default after 7 days |
| Experiments | One at a time | M14 | Others wait |

### 4.8 Stress test: owner interventions and outsourcing

Interventions per 100 customers per month, by source [H]. Fixed owner time
covers reading the report, provider changes (API deprecations, price changes),
renewals and App Store reviews.

| Source | Optimistic | Realistic | Pessimistic |
|---|---|---|---|
| Support the agent could not resolve | 1.0 x 15 min | 2.0 x 20 min | 5.0 x 30 min |
| Billing exceptions | 0.2 x 15 | 0.5 x 20 | 1.0 x 30 |
| Infrastructure beyond automatic recovery | 0.2 x 20 | 0.5 x 30 | 1.5 x 45 |
| Abuse decisions (owner only) | 0.1 x 20 | 0.2 x 30 | 0.5 x 45 |
| Third-party data or legal requests (owner only) | 0.05 x 30 | 0.1 x 30 | 0.3 x 45 |
| Fixed owner time per month | 40 min | 75 min | 150 min |

Results at $39 on Stripe. Owner time at $150/hour. Outsourcing hands support,
billing and infrastructure escalations to a contractor at $60/hour with a
15-minute minimum per item and a $300/month retainer [H]; abuse and legal stay
with the owner.

| Scenario | Customers | Contribution | Owner hours, all in-house | After owner time | Owner hours if outsourced | Contractor | After outsourcing and owner time |
|---|---|---|---|---|---|---|---|
| Optimistic | 100 | $2,208 | 1.1 | $2,045 | 0.7 | $322 | $1,778 |
| Realistic | 10 | $181 | 1.4 | -$25 | 1.3 | $306 | -$316 |
| Realistic | 100 | $2,208 | 2.5 | $1,836 | 1.4 | $365 | $1,633 |
| Realistic | 500 | $11,946 | 7.4 | $10,834 | 2.0 | $625 | $11,021 |
| Realistic | 1,000 | $24,219 | 13.6 | $22,182 | 2.8 | $950 | $22,857 |
| Pessimistic | 100 | $2,208 | 7.2 | $1,125 | 3.1 | $548 | $1,196 |
| Pessimistic | 1,000 | $24,219 | 49.8 | $16,757 | 8.5 | $2,775 | $20,169 |

What this means:

- At 10 customers the business earns about $181 a month and the realistic owner
  load (1.4 hours) consumes all of it at $150/hour. That is acceptable
  for a launch phase and is why the path to 10 is kept small.
- At 100 customers the realistic case is 2.5 hours, over the 2-hour target.
  Getting under it needs either the support agent resolving 85% of contacts
  (optimistic column) or a contractor for escalations (1.4 owner hours, $365).
  Hire the contractor only once measured owner time exceeds 2 hours for two
  consecutive months; below that the retainer costs more than it saves.
- The pessimistic case misses the target even with a contractor (3.1 hours)
  because fixed owner time alone is 2.5 hours. If measured fixed time exceeds
  90 minutes, the Operator's next work is removing that source (section 9).
- Profit stays positive in every scenario from 100 customers up; the business
  is robust to owner time, and owner time is the measure that needs watching.

## 5. The Operator

One worker, `amux-business`, on its own server. Shell schedules (no model
cost) snapshot every source into files every 15 minutes; the worker reads MDAI
digests of those files every 4 hours and on an anomaly, then acts within its
level and records what it did on its board. It starts in the path to 100
(section 13). A second worker is added only if the first one's queue stays older
than 7 days for two consecutive weeks.

| Source | Reads | May change on its own | Reports |
|---|---|---|---|
| Payments (Stripe restricted read-only key) | New, renewed, failed, refunded; MRR; plan mix | Wording of our own reminder emails | Cash line; refunds over the cap |
| Control plane | Provision jobs, lifecycle actions, redirects | Re-runs a failed provisioning or restore step | Repeated failures as cards |
| Fleet | Value events, CPU, memory, disk (counts only) | Lifecycle messages (M6, M7, M10) | Activation and retention |
| Hetzner (read-only token) | Servers, prices, quota | Nothing | Price guard (M13), quota at 80% |
| PostHog | Funnel, CTA clicks, sources | Site copy and docs through PRs (L2 scope below) | Funnel line |
| Cloudflare (analytics read) | 5xx rate, latency per host | Nothing | Cards on sustained errors |
| GitHub | Stars, traffic, referrers, Cloud issues | README and release-note wording via PR | Acquisition line |
| App Store Connect | Installs, ratings, reviews | Nothing; promotional text is set through a control-plane endpoint that allows only that field | Ratings below 4.0 |
| Provider status pages | Anthropic, OpenAI, Google, Hetzner, Stripe, Cloudflare | A dashboard banner during a model-provider incident | None |
| Its own spend | Inference against its cap | Slows itself at 80% | Budget line |

**Permissions.** Read-only keys everywhere, except: pull requests to the repo;
the narrow admin API (re-run a provisioning step, restart an amux service,
send a lifecycle email); and the abuse@, support@ and privacy@ mailboxes, never
the owner's. **Merge scope:** the Operator may merge at L2 only under `site/`
and `docs/` when CI passes. Everything under `cloud/` and the amux product
reaches customers only through the release train (3.1). Files on the
destructive path (billing, lifecycle, deletion, door, fleet agent) are listed in
`CODEOWNERS` and need owner review, expected under once a month. T-OP-1 tries
every write outside this list and expects refusal.

| Level | Allowed | Examples |
|---|---|---|
| L0 Observe | Read and report | Weekly report |
| L1 Reversible | Act alone when one step undoes it | Restart a service, re-run a step, send a lifecycle email, publish a doc fix |
| L2 Bounded | Act alone inside a cap or test gate | Refunds within the cap (issued by the control plane), merges under `site/` and `docs/` |
| L3 Approval | Owner says yes first, with a default after 7 days (section 9) | Prices, deleting data outside the lifecycle, abuse deletions, third-party requests, budget increases, destructive-path code |

## 6. Funnel and measurement

| Stage | Event | Source of truth |
|---|---|---|
| Visit | `acq.visit` with first-touch source | PostHog |
| Pricing view | `acq.pricing_view` | PostHog |
| Install (opt-in) | `oss.install` | `install.sh` ping [build] |
| Checkout started | `cloud.checkout_started` with source | Control plane |
| Paid | `cloud.paid` | Payment provider API, confirmed |
| Ready | `cloud.ready`, minutes from payment | Control plane |
| First reply | `cloud.first_reply` | Customer server |
| Activated | `cloud.activated` (V1 to V4) | Fleet agent counts |
| Active week | `cloud.active_week` | Fleet agent counts |
| Teammate joined | `cloud.member_joined` | Control plane |
| Cancelled | `cloud.cancelled` with optional reason | Control plane |

Activation candidates [H], ranked by correlation with 90-day retention once 100
customers exist: V1 a worker produced an artifact the customer kept (a commit,
PR or downloaded file); V2 a schedule ran three times; V3 work on two distinct
days in week one; V4 the phone app or a teammate connected. The fleet agent
sends counts and event names only, and Settings shows exactly what is sent.
Weekly cohorts by first payment week give D7, D30 and D90 retention and lifetime
contribution.

**Qualitative evidence counts.** At this traffic the most reliable signals are
cancellation reasons, support transcripts, setup steps where customers stall,
and PostHog session recordings of the pricing and checkout pages. The Operator
summarises them weekly.

## 7. Acquisition: organic and compounding

| Channel | Exists | Build |
|---|---|---|
| GitHub README and releases | README, star CTA experiments (EXP-001) | Run-it-24/7 line and badge; `amux cloud move` in release notes |
| The OSS product | Self-hosted dashboard | "Move to amux Cloud" (7.1) |
| Docs, guides, compare pages | Large set plus an AEO backlog (`site/AEO_BACKLOG.md`, `site/guides/`, `site/compare/`) | The content loop (M15) |
| App Store | Listed | Sign-in for existing customers; no purchasing in the app |
| Integration tutorials | Skills, connectors, MCP | One CI-tested tutorial per integration |

Not used: founder-led outbound, sales calls, demos, consulting, paid ads. The
Calendly "Book a setup call" on `site/cloud/index.html:50` is removed.

**Judging channels without killing them early.** Organic channels compound
slowly, so tactics and channels are judged separately:

| Unit | Leading indicator | Checked | If weak |
|---|---|---|---|
| A tactic (one page, one CTA, one README line) | Indexed in 14 days; impressions in Search Console by day 60; clicks to `/pricing` by day 120 | Per item | Revise it: retitle, merge into a stronger page, fix examples. A tactic is replaced after two revisions without movement, never deleted while it ranks |
| A channel (all guides; all GitHub CTAs; the move flow) | Cumulative impressions, pricing views and attributed paid customers, as a 3-month moving trend | Months 6 and 12 | Stop only if the trend is flat at month 12 **and** at least three distinct tactics were tried. Upkeep near zero means a slow channel keeps running |
| The business plan | Pricing viewers per month | Monthly | Under 3% monthly growth at month 6: budgets and the Operator's cap assume the flat-traffic ceiling (4.6); channels keep running |

### 7.1 Portability: YAML provisioning and import/export, local and Cloud

One format describes a whole amux server, and it moves in every direction:
local to Cloud, Cloud to local, and Cloud to Cloud.

**Exists.** `amux iac export` and `PUT /api/config/apply` turn configuration
(skins, board columns, worker declarations) into YAML and back idempotently,
reporting what changed; apply declares workers and does not start them
(`crates/amux-server/src/api/config_iac.rs`, `amux` `cmd_config`) [exists].
`cloud/desktop/remote.py load` and `verify` load workers, files and schedules
onto a host; sandbox.amux.io was loaded this way [exists]. There is no data
export today [build].

**The YAML** [build]: `amux.yaml` extends today's IaC file so it describes a
whole server, plus an optional `cloud:` block for the hosting itself.

```yaml
version: 1
cloud:                       # read only by `amux cloud apply`
  plan: solo                 # solo | team | max
  location: fsn1
  tailnet: { join: true, tailnet_only: false }   # 8.5
members:                     # invites sent on apply (8.2)
  - { email: dana@example.com, scope: team:research }
groups: [research, ops]
workers:
  - { name: researcher, model: claude, dir: ~/work/notes, groups: [research], start: true }
schedules:
  - { title: nightly digest, worker: researcher, expr: "daily at 6am", kind: tmux }
skills: [./skills/digest.md]
files: [{ from: ./mdai, to: ~/mdai }]
repos: [{ url: https://github.com/acme/app, branch: main, dir: ~/work/app }]
env:                         # names only; values never live in the file
  ANTHROPIC_API_KEY: { from: prompt }   # prompt | local-env | skip
skin: { theme: dark, tabs: [workers, board, messages, files] }
```

Rules: the file never contains a secret value; `env` entries say where a value
comes from, and `prompt` asks for it in the CLI or on the first screen.
`start: true` is the only way a worker starts from YAML, so a mistyped file
cannot start a fleet of agents. Apply is idempotent and prints, per item,
created, changed, unchanged or skipped with the reason.

**Commands, the same on local and Cloud servers** [build]:

| Command | Does |
|---|---|
| `amux iac export > amux.yaml` | Writes the YAML for the current server (exists today for configuration; extended to members, groups, schedules, skills, files, repos and env names) |
| `amux iac apply amux.yaml` | Applies it to the current server, local or Cloud, after a dry-run diff |
| `amux cloud apply amux.yaml` | Signs in (`amux cloud login`, 8.4). If no server matches the file, creates one: a new account goes through Stripe Checkout; an existing paid account adds a server only with `--confirm-billing`, which shows the monthly price and needs a typed yes. Waits for ready, then applies the rest of the file. Run again, it updates the same server |
| `amux export out.amuxmove` | Writes the move bundle (below) from any server |
| `amux import out.amuxmove` | Imports a bundle into the current server, local or Cloud, after a dry-run diff |
| `amux cloud move` | Export, sign in or pay, provision, upload in chunks while provisioning, import: one command from local to Cloud |
| `amux cloud pull <server>` | Downloads a Cloud server's bundle and imports it into the local install: the way back from Cloud, offered before cancelling |

**The move bundle** [build], one file, `<name>.amuxmove`: the YAML plus data.

| Part | Contents | On import |
|---|---|---|
| Configuration | The YAML above | `amux iac apply` |
| Data | An allowlisted, per-table export with the source's schema version: board cards and history, schedules, memories, CRM, messages, MDAI runs, skills. Never a raw copy of `amux.db`, which also holds secrets (`chatgpt_oauth_tokens`, migration 0098; share, member and invite tokens; connector state) | Loaded into a staging copy, migrated from the source schema (older or newer within one major version), refused if any table is off the allowlist, then merged by stable ids |
| Workers | Each worker's declaration | Declared, not started; started with one click |
| Files | `.mdai` nodes, skills, scratchpad, and folders the user ticks | Restored at the same paths relative to home |
| Repositories | Remote URL and branch per worker | Re-cloned; uncommitted local changes are listed as a warning before export |
| Secrets | Excluded unless the user ticks "bring my API keys" (CLI sign-ins are redone, 8.4) | Written with `PATCH /api/settings/env` |

**Ways in.** The CLI as above; "Move to amux Cloud" and "Download everything"
(a bundle) in Settings on any server; "Import from your local amux" on a new
Cloud server's first screen, which accepts a bundle or plain YAML. The importer
always shows a dry-run diff and lists every item imported or skipped with the
reason. Nothing at the source is deleted. Cancelling (J17) offers `amux cloud
pull` and the bundle download first, so leaving Cloud is as easy as joining.

**Calls to action** [build]: the README under the install command; the GitHub
About link; a release-notes footer; the closing message of `install.sh`; a
homepage secondary CTA ("Already running amux? Move it in one command"); a
pricing FAQ; a docs page; OSS Settings, plus one dismissible banner when the
local server shows lost work (the machine slept with workers mid-task, or
schedules were missed); first in `amux cloud` CLI help. Never in the iOS app.

## 8. The customer journey

### 8.1 Stages

| # | Stage | Customer sees | System does | Target | Automatic recovery | Test |
|---|---|---|---|---|---|---|
| J1 | Find us | amux.io, a guide, README, App Store, OSS dashboard | Cloud CTAs point to `cloud.amux.io` with a source | Page LCP < 1.5 s | A nightly link check fails the build on a dead CTA | T-SITE-1 |
| J2 | Compare plans | Free (open source), Solo, Team, Max; "Sign in with your Claude, ChatGPT or Google subscription, or bring an API key"; "teammates included" | Prices from `plans.json` | < 1 s | A price disagreeing with the provider fails the deploy | T-SITE-2 |
| J3 | Choose | "Get your server" | `cloud.amux.io/start?plan=solo` | 1 click | Unknown plan falls back to Solo, logged | T-SITE-3 |
| J4 | Pay | Stripe Checkout (cards, Apple Pay, Google Pay, Link; tax shown) | Checkout with a pending signup id and source | < 60 s | Abandoned: one reminder after 1 hour if consented | T-PAY-1 |
| J5 | Account | Nothing to fill in | Success URL reads the transaction from the provider API and, idempotently on its id, creates user, org and provision job (the webhook does the same; first wins); one-time sign-in token for the paying browser; `email_verified` false until email 1's link is clicked | < 5 s | Existing account: payment attaches. Late webhook: the success URL already did the work | T-ACC-1, T-ACC-2, T-ACC-4 |
| J6 | Being built | Email 1 | Control plane sends it | < 10 s | Bounce shown on the progress page | T-MAIL-1 |
| J7 | Provision | Progress page with real steps | Golden image, DNS, Origin CA certificate, backups configured, first backup written, health | < 3 min [H] | Capacity error: next EU location. Over 15 min: refund within the cap, email 2, card | T-PROV-1, T-PROV-3, T-PROV-4 |
| J8 | Enter | `https://<slug>.amux.cloud`, signed in | 60-second token, `__Host-` cookie | < 2 s | Expired token: one bounce through the control plane | T-SSO-1 |
| J9 | Connect a model | First screen: "Sign in with Claude", "Sign in with ChatGPT", "Sign in with Google (Gemini)", paste an API key, or import from local amux | Section 8.4: the CLI's own login runs on the customer's server and the dashboard relays its link and code; or the key is saved on the customer's own origin after one validating call | < 1 min | A failed or expired login shows the CLI's message and a retry; an invalid key shows the provider's error and nothing is saved | T-KEY-1..3, T-CLI-1..6 |
| J10 | First value | "Start with a template": code on a repo, inbox triage, research brief, nightly schedule | Worker created, first task sent | Paid to first reply < 5 min [H] | Failure: the cause from the worker log and a retry | T-FIRST-1 |
| J11 | Invite a teammate | Team settings: email and scope | Section 8.2 | < 1 min to send | Section 8.2 | T-TEAM-1..6 |
| J12 | Move local work, or provision from YAML | Section 7.1: `amux cloud move`, `amux cloud apply amux.yaml`, or the import screen | Bundle or YAML, upload, dry run, apply | < 10 min for 1 GB | A failed apply leaves the server unchanged and lists failures | T-MIG-1..7, T-YAML-1..5 |
| J13 | Phone | Section 8.3 | Section 8.3 | < 1 min | Section 8.3 | T-IOS-1..5, T-TEAM-7 |
| J21 | Tailnet | "Add to my tailnet" in Settings | Section 8.5 | < 2 min | A failed join leaves the public URL unchanged and shows Tailscale's message | T-TS-1..5 |
| J14 | Day 2 and after | Release notes after an update | Fleet agent updates and backs up (3.1) | Update downtime < 10 s | Rollback; backup retries, then M4 | T-OPS-1..4 |
| J15 | Change plan | "Upgrade" | hcloud `change_type` with the larger disk; about 1 minute down, announced | < 5 min | Failure restores the old type | T-PLAN-1 |
| J16 | Payment fails | Banner with the Stripe portal link; emails 3 and 4 | Stripe Smart Retries; 14 days of service; then snapshot and delete under 3.2 | Stripe's retry schedule | A fixed card restores from the snapshot in < 10 min | T-DUN-1 |
| J17 | Cancel | "Download everything" (a bundle) and `amux cloud pull` first, one optional reason | Export; at period end snapshot and delete under 3.2; snapshot kept 30 days | Export < 10 min | A failed export blocks deletion | T-CAN-1, T-CAN-2 |
| J18 | Delete my account | Settings or the iOS app | Server, snapshots, R2 data, Clerk user and Apple token revoked; invoices and tax records are kept as the law requires; receipt lists each item | < 24 h | Failed steps retried daily | T-DEL-1 |
| J19 | Help | "Something wrong?" or support@ | Diagnostics bundle the customer sees first; the support agent answers (M8) | First answer < 5 min | Unresolved: queued with the bundle | T-HELP-1 |
| J20 | Can't sign in | Email code, Google, GitHub, Apple, or a passkey | Clerk; a sign-in link to the billing email on the Stripe customer | < 2 min | Section 11.1 for the case where every factor is lost | T-ACC-3 |

Downgrades, a second server, region moves and the US region wait for demand
(section 13). Downgrading needs export and import onto a smaller disk, which the
move bundle will provide once it exists.

**`cloud.amux.io` sends each visitor to one place:** not signed in to pricing;
`?plan=` to checkout; signed out with an account to sign-in then this list;
signed in, never paid to pricing; provisioning to progress; ready to their
server with a fresh token; several servers to a chooser; payment failed in grace
to their server with a banner; after deletion to "Restore from your snapshot"
while it exists, then pricing; an invited teammate to sign-in then the inviting
server; `?from=ios` the same and back to the app (T-RED-1..12).

**Emails** (control plane; the provider sends receipts): 1 being built; 2
refunded because provisioning failed; 3 payment failed; 4 server will be
snapshotted on <date>; 5 cancelled, with export link and snapshot expiry;
6 deletion receipt; 7 you were invited to <server>.

### 8.2 Workspace access and invites

The amux server already has members, invites, teams with scopes and revocation,
and the local Playwright test proves an invitee can join, work, appear in logs
and be revoked (`e2e/local-multiplayer.spec.ts`) [exists]. Cloud adds only the
sign-in hop.

1. The owner opens Team settings on their server, enters an email and a scope
   (whole server, or a team), and sends. amux creates the invite as today; on
   Cloud the link is `https://cloud.amux.io/invite/<token>` and email 7 is sent.
2. The invitee signs in or signs up at cloud.amux.io with any method (email
   code, Google, GitHub, Apple). The account's verified email must equal the
   invited address; Apple private-relay addresses are accepted when the invite
   was sent to that relay address, otherwise the invitee is asked to sign in
   with the invited email instead.
   Email 7 is sent by the control plane: the customer server calls
   `POST cloud.amux.io/api/fleet/invite-email` with its per-server fleet token
   and the invite id; the control plane sends only to the address bound to that
   invite, at most 20 a day per server.
3. The control plane redeems the invite on the customer server and issues a
   door token whose subject is the new member. The member lands in the server
   with their scope; presence and Messages show them.
4. Revocation from Team settings ends access within 60 seconds. The door cookie
   lasts 12 hours, but the door, which runs on the same host, re-checks the
   member's status with amux-server at most every 60 seconds and rejects a
   revoked member; amux-server closes that member's open streams when the
   member is revoked.
5. Invitees never need a subscription. Teammates count toward nothing.

Failure handling: an expired or used invite shows who to ask; a wrong email is
refused without revealing the server; a deleted member's cookie stops working at
the next refresh; the owner cannot remove themselves.

### 8.3 iOS and Sign in with Apple

The app sells nothing and shows no calls to buy (App Store guideline 3.1.3(f)),
so the current "schedule onboarding" button that opens amux.io is replaced.

| Step | Behaviour |
|---|---|
| Sign in | Native Sign in with Apple button first, then "Sign in with email, Google or GitHub" through `ASWebAuthenticationSession` to cloud.amux.io |
| Apple sign-in | The app uses Clerk's iOS SDK, whose native Sign in with Apple completes the Clerk session on the device; the app then exchanges that Clerk session at the control plane for its server list and door tokens |
| Same person, same account | Apple sign-in is also offered on the web, at checkout and in account settings, so the identity used to pay can be linked. If an Apple sign-in in the app finds no account, the app says "No amux Cloud server for this Apple ID" and offers email sign-in, after which Apple is linked to that account. The app never creates an account |
| Hide My Email | Our sending domain is registered with Apple's private relay service in A0, so emails to relay addresses arrive |
| Servers | The account's servers are listed; tapping one opens it with a fresh door token |
| Teammates | An invitee who signs in with Apple sees the servers they were invited to |
| Delete account | Settings, "Delete account", which runs J18. Apple requires the Apple token to be revoked through its REST API; P3 confirms whether deleting a Clerk user does this, and if not the control plane fetches the user's Apple token from Clerk's Backend API and calls Apple's revoke endpoint before deleting the Clerk user |
| Apple notifications | The control plane receives Apple's server-to-server notifications; consent revoked signs the app out, account deleted runs J18 |

### 8.4 Model sign-in through the coding CLIs

Customers already pay for Claude, ChatGPT or Google AI plans and already sign in
to Claude Code, Codex and Gemini CLI. The server runs those CLIs, so the easiest
sign-in is the CLI's own login, started from the dashboard. Our code never sees
the provider password, and the credential stays in the CLI's own store on the
customer's server, which our control plane cannot read (D4).

| CLI | Headless login it supports | What the dashboard shows | Credential lives |
|---|---|---|---|
| Claude Code | `claude setup-token` (a subscription token for headless use) and the `/login` URL-and-code flow | "Sign in with Claude": Anthropic's page in a new tab, then a box for the code it shows | `~/.claude` on the customer server |
| Codex | `codex login --device-auth` (device code) | "Sign in with ChatGPT": the device code and a button to OpenAI's device page; completes by itself | `~/.codex` on the customer server |
| Gemini CLI | "Login with Google" with `NO_BROWSER=true`, which prints a URL and asks for the authorization code | "Sign in with Google": Google's page, then a box for the code | `~/.gemini` on the customer server |

How it runs [build]: a login endpoint on amux-server starts the CLI's login
command in a private pseudo-terminal, reads the URL or device code from its
output, shows it as a button, writes a pasted code back to the CLI, and reports
success only when the CLI itself confirms it (`claude auth status`, `codex login
status`, a one-line Gemini prompt). Login output is never logged. A small
adapter per CLI matches its prompts, tested against the CLI version pinned in
the golden image, so an update that changes a prompt fails the release
(T-CLI-5) before it reaches a customer. An expired login shows "Sign in again"
on the worker card and sends one email.

`amux cloud move` does not carry these sign-ins: local CLIs keep them in the
macOS keychain or device-bound files, so the customer signs in once on the
server.

The same pattern gives the amux CLI an easy sign-in to amux Cloud: `amux cloud
login` shows a device code and opens `cloud.amux.io/device`; the signed-in
browser approves it and the CLI receives a scoped token. `amux cloud move`,
`amux cloud pull` and `amux cloud apply` (7.1) use it.

Risk: a provider may change whether a subscription may run on a remote server,
or change its login flow. API keys always work, and T-CLI-5 catches a changed
flow before release.

### 8.5 Add the server to your tailnet

`bootstrap.sh` already installs Tailscale, runs `tailscale up --authkey ...
--ssh` and allows inbound traffic on `tailscale0`
(`cloud/desktop/bootstrap.sh:143-156`); amux already treats Tailscale
identities as members with scopes (migration `0059_local_member_scope.sql`),
tested by `e2e/multi-network-multiplayer.spec.ts` [exists].

| Step | Behaviour |
|---|---|
| Join | Settings, "Add to my tailnet". Either a login link (amux-server runs `tailscale up --hostname amux-<slug>` and shows the URL Tailscale prints; the customer approves it in their own tailnet) or a pasted auth key (for tagged or pre-approved devices). Nothing about the customer's tailnet passes through our control plane |
| Reach | `https://amux-<slug>.<tailnet>.ts.net` through `tailscale serve` with the tailnet's own certificate; the public `amux.cloud` URL keeps working |
| Who gets in | Tailscale identities map to amux members through the existing member scopes; the owner chooses whether tailnet users join automatically or must be invited (8.2) |
| Tailnet only | An optional switch turns the public `amux.cloud` URL off. Updates, backups, heartbeats and lifecycle are outbound, so they keep working; the redirect shows the tailnet URL; support asks the customer to run diagnostics from their side |
| SSH | Tailscale SSH stays off unless the customer turns it on |
| Leave | "Remove from tailnet" runs `tailscale logout`; deleting the server (J18, 3.2) does the same first, so no stale node remains in the customer's tailnet |


## 9. The owner's interface

**Weekly report** (Monday email, one screen): cash and contribution; new,
churned and net customers; activation; owner minutes and interventions per 100
customers by source; synthetic pass rate, backup age, restore drills, rollbacks;
lifecycle actions taken, held and paused; what the Operator shipped; decisions
waiting, each as one question with what a yes does. Owner minutes are measured
from every approval, alert opened and card touched.

**Every escalation has a default after 7 days:**

| Escalation | Default |
|---|---|
| Refund over the cap | Refund it |
| Lifecycle breaker tripped (3.2) | Stay frozen; servers keep running |
| Restore drill failed | Keep retrying daily and take an extra Hetzner snapshot of every server weekly |
| Synthetic journey failing for 6 hours | Roll back the last site, control-plane or release deploy |
| Price proposal | No change |
| Abuse appeal | Keep the server isolated and the data intact; refund the unused period |
| Third-party data request | Standard reply asking for verification through the account; disclose nothing |
| Destructive-path pull request | Not merged |

**What interrupts the owner** (email immediately): a security incident;
customer data at risk; billing or provisioning down for 30 minutes after
automatic recovery; the lifecycle breaker; a provider account problem (payment
method failing, account review, payout hold). Everything else waits for Monday.

**Autonomy release criterion:** 30 consecutive days without a routine owner
intervention, then 90. Every routine intervention is filed as a defect naming
the mechanism that should have handled it. If measured owner time is over 2
hours a month at 100 customers for two months, the Operator's only work is the
largest intervention source until it is under.

## 10. Mechanisms

Each runs Trigger, Data, Decision, Action, Verification, Rollback, Escalation.
"Runs in" says what keeps it alive when the Operator is stopped. Stage is when
it ships (section 13).

| # | Mechanism | Runs in | Trigger | Data | Decision | Action | Verification | Rollback | Escalation | Level | Stage |
|---|---|---|---|---|---|---|---|---|---|---|---|
| M1 | Provisioning recovery | Control plane | Step over its time or errors | `provision_jobs`, Hetzner and Cloudflare errors | Retryable or not | Retry in the next EU location; recreate from the image | `cloud.ready` within 15 min | Delete the half-built server | Over 15 min: refund within cap, email 2, card | L2 | 10 |
| M2 | Self-healing | Control plane, fleet agent | Heartbeat missing 3 min or health failing | Heartbeats | Reachable or not | Restart amux; reboot via API; restore to a new server from backup | Health green 5 min | Old server snapshotted and kept 7 days | Third failure in a week: card | L1, restore L2 | 10 |
| M3 | Disk pressure | Fleet agent | Disk over 85% | Fleet agent | Logs and caches or customer data | Prune logs and build caches; tell the customer if it is their data | Under 75% | Pruned items regenerate | Over 95%: offer the upgrade | L1 | 100 |
| M4 | Backup and restore check | Fleet agent | Replication over 15 min old, or drill failed or skipped | Fleet agent, drill result | Transient or broken | Retry; rebuild the litestream config; re-run the drill | Next drill passes | Read-only | Two failures: card; drill failing 7 days: default in section 9 | L1 | 10 |
| M5 | Lifecycle and reconcile | Control plane | Hourly, and on provider events | Provider API, our ledger, `servers`, Hetzner labels | The rules in 3.2 | Provision paid customers missing a server; propose, hold and execute snapshot-and-delete for lapsed ones; report orphans | Counts match; each action has its snapshot id | Restore from snapshot for 30 days | Breaker: freeze and alert | L2 (shadow for the first 30 days) | 10 |
| M6 | Incomplete setup | Operator | 30 min after ready without a key, or 24 h without a first reply | Funnel events | Which step stalled | One email naming the step and its fix | Step completes | None needed | Counted in the report | L1 | 100 |
| M7 | Inactivity | Operator | Paid, no value event for 7 days | `cloud.active_week` | Ever activated | One email with the template retained customers use most; at most one per 14 days | Value event within 7 days | None needed | None | L1 | 100 |
| M8 | Support | Operator with the support workspace | "Something wrong?" or support@ | Diagnostics bundle, telemetry, docs | Known fix, safe fix or unknown | Answer; apply a customer-approved restart or config fix | Resolved, no repeat in 7 days | Fixes keep a backup | Unknown: queue (owner, or contractor at scale) | L2 | 10 (owner answers), 100 (agent) |
| M10 | Upgrade suggestion | Fleet agent signal, control plane | CPU or memory over 80% for 3 days | Fleet agent | Would an upgrade relieve it | One in-app suggestion with the price difference | Upgraded or pressure gone | None needed | None | L1 | 100 |
| M11 | Synthetic failure | Control plane schedule; Operator for fixes | Journey fails | Step, screenshots, logs | Infrastructure (M1, M2) or code | Card; Operator opens a PR | Test passes against the fix | Revert | Failing 6 hours: default in section 9 | L2 | 10 (detection), 100 (fixes) |
| M12 | Release rollout | Control plane, fleet agent | New release | Fleet health | Stage healthy | Synthetic, 10%, all; 24-hour soak between | Health at each stage | Automatic rollback | Two rollbacks: halt and card | L2 | 10 |
| M13 | Price guard | Control plane | Daily | Hetzner pricing API, cohorts | Any plan under 1.95x cost | Card with a proposal | Includes the 4.3 churn test | None needed | Always owner; default no change | L3 | 100 |
| M14 | Site improvements | Operator via PRs | Weekly | Funnel, recordings, cancel reasons, support transcripts | Section 10.1 rule | Ship deterministic fixes; run a test only when the sample-size rule allows | Guardrails unchanged | Revert the PR | Guardrail breach: revert and card | L2 | 100 |
| M15 | Content loop | Operator | Weekly | Search Console, existing pages, attribution | Query relevant and uncovered, or an existing page losing position | Write or refresh; every code example runs in CI; claims cite sources | Indexed; examples pass | Revert the PR | None | L2 | 100 |
| M17 | Abuse | Control plane | Hetzner abuse mail to abuse@, sustained 100% CPU over 6 h, outbound spam, known miners, card testing | Fleet agent, provider fraud signals, the report | Clear, likely or noise | Clear: isolate (Hetzner firewall denies outbound, workers stopped), notify the customer, answer Hetzner before its deadline. Likely: throttle and notify. Card testing: block | Behaviour stops; report closed | Lift on a successful appeal | Deletion: owner only, default in section 9 | L2; deletion L3 | 10 |
| M20 | Disputes | Control plane | Stripe `charge.dispute.created` | Provisioning log, usage counts, terms acceptance, invoices | Contest, or accept under $10 | Submit evidence through the Stripe API before the deadline | Dispute closed | None needed | None; counted in the report | L2 | 10 |
| M18 | Budget guard | Control plane, provider spend limits | A budget reaches 80% | Spend counters | Projected to exceed | Slow at 80%, stop at 100% | Under cap | Resume next period | Stop that hurts customers: alert | L1 | 100 |
| M19 | Data requests | Control plane | Delete or download in the app, or privacy@ | Account and resources | From the verified holder or not | Run J17 export or J18 deletion; receipt | Every resource exported or gone | Exports are additive | Not the holder: default in section 9 | L2 | 10 |

M9 (cancellation offers) and M16 (referral rewards) from earlier revisions are
cut: they add code
without being needed for safety or margin. Referrals return if attribution shows
customers already recommend amux.

### 10.1 Experiments at low traffic

A test is run only if it can reach its sample size within 8 weeks at a 50/50
split. Sample size per arm for a two-sided two-proportion test at alpha 0.05 and
power 0.80 is n = (1.96 + 0.84)^2 x (p1(1 - p1) + p2(1 - p2)) / (p2 - p1)^2,
computed by the econ script at measured traffic:

| Metric | Baseline | Lift to detect | Per arm | Months of current traffic |
|---|---|---|---|---|
| Visit to pricing view | 4.9% | +20% | 8,242 | 4.6 |
| Visit to pricing view | 4.9% | +50% | 1,484 | 0.8 |
| Pricing view to paid | 2% [H] | +50% | 3,822 | 42.9 |
| Pricing view to paid | 2% [H] | +100% | 1,138 | 12.8 |

So conversion-to-paid cannot be A/B tested at current traffic, and only large
changes to the visit-to-pricing step can. The horizon is fixed in advance and
results are read once at the end; no peeking. Below the threshold the Operator
ships **deterministic improvements**: broken links, slow pages, unclear prices,
steps customers stall on, copy contradicted by support transcripts. Each is
judged by whether it removed the problem it targeted. They are logged as
"shipped, not measured", never as wins. The 19 rows in
`docs/homepage-experiments.md` still marked running are closed as inconclusive.

### 10.2 Choosing work

Each opportunity gets: expected change in 90-day retained contribution per
month (dollars), confidence (0 to 1), effort (agent-days), upkeep (owner minutes
per month it adds) and risk (0 to 1). Score = dollars x confidence / (effort +
upkeep / 60 + 10 x risk). The Operator works the top item; anything adding owner
upkeep must say why. Content is valued on its long-term attributed
contribution, so a page is refreshed or merged rather than removed.

### 10.3 MDAI nodes

Three nodes, all over files the shell schedules write: `business.mdai` (funnel,
cohorts, cash, owner minutes), `failures.mdai` (synthetic results, support
themes, incidents, lifecycle actions) and `decisions.mdai` (`amux:messages`
from the owner and this plan). The weekly report and the Operator's choices read
from them.

## 11. Failure audit, kill switches and decisions

### 11.1 What could still need the founder, and the fix

| Area | Failure mode | Fix | Residual owner work |
|---|---|---|---|
| Destructive actions | Webhook lost or delayed, provider outage, inconsistent subscription state, bug in our ledger | Section 3.2: API reads, two sources, holds, health gate, caps, breaker, verified snapshots, shadow mode | Breaker alerts only |
| Destructive actions | Hetzner deletes or locks the project (abuse, unpaid invoice) | M17 answers abuse before the deadline; customers split across two projects from 50 customers; R2 backups live outside Hetzner, so any server can be rebuilt elsewhere | Rare; alert |
| Destructive actions | A bad release corrupts data | Canary with soak, automatic rollback, litestream point-in-time restore | None expected |
| Customer credentials | Customer loses email access | Sign-in with any linked method (Google, GitHub, Apple, passkey), or a link to the billing email on the Stripe customer | None |
| Customer credentials | Customer loses every factor | We cannot verify them, so the honest answer is automated: their data stays until the subscription lapses and follows the normal lifecycle; they can start a new account. Support says this without escalation | None |
| Customer credentials | Apple Hide My Email relay | Domain registered with Apple's relay; linking flow (8.3) | None |
| Our credentials | Owner locked out of Hetzner, Cloudflare, Stripe, Clerk, Apple, Tailscale, the registrar | Two factors on each, recovery codes in an offline vault, a second admin user where the provider allows it, set up in A0 | One-time setup |
| Our credentials | A provider's payment method expires (Hetzner suspends unpaid accounts) | Two payment methods on Hetzner, Cloudflare, the registrar and Apple; the control plane warns 30 days before a card expires | Replacing a card, rare |
| Our credentials | Domain or Apple developer membership lapses | Auto-renew on, with the warning above | None |
| Our credentials | Control-plane signing key or fleet token leaks | Keys rotate by schedule; the fleet agent accepts the current and previous key; per-server fleet tokens | None |
| Payments | Webhook delayed | The success URL reads the transaction directly (J5) | None |
| Payments | Duplicate payment | Idempotency on the transaction id; automatic refund of the duplicate within the cap | None |
| Payments | Stripe reviews the account or holds payouts | Accurate business description and terms in A0; Radar rules against card testing; dispute rate watched (M20) | Answer Stripe; rare |
| Payments | EU VAT return due | Stripe Tax collects; returns filed through Stripe's filing integration; the report shows each filing | None after setup |
| Payments | Refunds beyond the cap | Default after 7 days (section 9) | Optional |
| Model sign-in | A provider changes its CLI login flow or expires a token | T-CLI-5 blocks a release with a changed flow; expired tokens show "Sign in again" and an email; API keys always work | None |
| Tailnet | A customer's tailnet ACLs lock them out while tailnet only is on | The account owner turns tailnet only off at cloud.amux.io, which reaches the server through the fleet agent's outbound channel | None |
| Provisioning from YAML | A file creates servers or charges by mistake | A new server on a paid account needs `--confirm-billing` and a typed yes; workers start only with `start: true` | None |
| Agent permissions | Operator merges a harmful change | Merge only under `site/` and `docs/`; destructive paths need owner review; the release train for everything else | Reviewing rare destructive-path PRs |
| Agent permissions | Operator overspends | Console spend limit enforced by the provider | None |
| Agent permissions | A credential grants more than intended | Read-only keys; T-OP-1 attempts each forbidden write | None |
| Agent permissions | The support agent damages a customer server | Only customer-approved restarts and config edits, each after a backup | None |

### 11.2 Kill switches

Control-plane flags; each leaves customer servers and data untouched.

| Freeze | Effect |
|---|---|
| `lifecycle_freeze` | No deletions or snapshots-for-deletion; provisioning continues |
| `deploy_freeze` | CI refuses site and control-plane deploys; the fleet agent ignores new releases |
| `provisioning_freeze` | Checkout shows "back soon"; existing customers unaffected |
| `spend_freeze` | Operator and support workspaces set to $0; refunds queue |
| Operator off | `amux-business` schedules disabled and the worker stopped; all of Cloud keeps running |

### 11.3 Open decisions [owner]

| ID | Question | Recommendation |
|---|---|---|
| O1 | Launch price | $39 Solo, $69 Team, $129 Max; reviewed on 90-day cohorts |
| O2 | Payment provider | Decided: Stripe (D11) |
| O3 | Existing $20 monthly and $200 annual Stripe subscribers and open "no credit card" trials (`site/pricing/index.html:239-275`, `:257`) | Count them in A0. Offer each a link to the new plan at their old price for 12 months; Stripe subscriptions end at period end. The owner approves that email (it goes to customers) |
| O4 | Customer domain | `amux.cloud` behind Cloudflare |

## 12. Tests

One Playwright journey, `cloud/tests/journey.spec.ts`, drives a synthetic
customer from amux.io to a working server, every 6 hours and on every deploy;
each run's server is deleted when the run ends. Once a night the journey server
is kept for the capability suite and deleted after it (about two server-hours). Lifecycle timers are our
own code and are tested with an injected clock; provider events are replayed
through Stripe test mode with test clocks. Each test checks the API and the
UI. A test that cannot run reports "not run" and fails the suite.

| ID | Asserts |
|---|---|
| T-SITE-1 | Every Cloud CTA (site, README, release notes, `install.sh`, OSS dashboard, CLI help) resolves to `cloud.amux.io` with a source or to `amux cloud move`; no Calendly or "free trial" anywhere |
| T-SITE-2 | Site prices equal `plans.json` and the provider's prices |
| T-SITE-3 | Each plan button opens checkout preselected |
| T-RED-1..12 | Each redirect case, with an account placed in that state |
| T-PAY-1 | Test-mode payment completes, with tax shown for an EU address; one provision job; a replayed webhook creates none |
| T-ACC-1 | A new email is signed in after payment with no form |
| T-ACC-2 | An existing account's payment attaches to it |
| T-ACC-3 | Sign-in by email code, Google, GitHub and Apple each reach the server; billing-email link works |
| T-ACC-4 | Success URL before webhook: one user, one job; another device refused until the email link |
| T-MAIL-1..7 | Each email arrives in the test inbox with working links, including at an Apple relay address |
| T-PROV-1 | Paid to ready under 3 minutes, with the first backup written |
| T-PROV-3 | Injected capacity error moves to the next EU location |
| T-PROV-4 | Injected 15-minute failure refunds, emails and files a card |
| T-CLI-1 | Claude sign-in: the dashboard shows Anthropic's link, a pasted code completes it, `claude auth status` reports signed in, and a Claude worker answers (nightly, with a test subscription token from `claude setup-token`) |
| T-CLI-2 | ChatGPT sign-in: the device code is shown, the flow completes against a test account, `codex login status` reports signed in, and a Codex worker answers |
| T-CLI-3 | Google sign-in: Gemini CLI's URL is shown, a pasted code completes it, and a Gemini worker answers |
| T-CLI-4 | No login output, code or token appears in any log on the server or in the control plane |
| T-CLI-5 | Against each CLI version in the golden image, the login adapter recognises the URL and code prompts (recorded fixtures plus a live start of each login); a changed prompt fails the release |
| T-CLI-6 | `amux cloud login`: the CLI receives a token after browser approval; a denied or expired code fails cleanly |
| T-TS-1 | With a test tailnet's auth key, "Add to my tailnet" joins; a CI runner joined to the same tailnet as an ephemeral node opens `amux-<slug>.<tailnet>.ts.net` |
| T-TS-2 | The login-link path shows Tailscale's URL; approving the device through the test tailnet's API joins it |
| T-TS-3 | A tailnet user mapped to a team scope sees only that team's workers |
| T-TS-4 | Tailnet only: the public URL refuses, the tailnet URL serves, and a release, a backup and a heartbeat still complete |
| T-TS-5 | Removing from the tailnet and deleting the server both remove the node from the tailnet's device list |
| T-SSO-1 | Token and cookie are refused on another customer's server |
| T-KEY-1..3 | Key validated and saved on the server origin, absent from every control-plane log and table; invalid key refused; OpenAI path works |
| T-FIRST-1 | Template worker replies; homepage to first reply under 5 minutes |
| T-TEAM-1 | Owner invites by email from Team settings in browser A; invitee in browser B signs up at cloud.amux.io and lands in the server as a member with the chosen scope |
| T-TEAM-2 | The member sends a message to a worker in scope and sees the reply; presence and Messages show both people to each other |
| T-TEAM-3 | A team-scoped member cannot see or message workers outside the team, in the UI or the API |
| T-TEAM-4 | Signing in with a different email than invited is refused without revealing the server |
| T-TEAM-5 | A used or expired invite is refused with who to ask |
| T-TEAM-6 | Revoking the member ends their access within 60 seconds, including an open stream; their messages remain attributed |
| T-TEAM-7 | An invitee who signs in with Apple sees the server in the iOS app |
| T-IOS-1 | XCUITest: Sign in with Apple (sandbox Apple ID) lists the server and opens it |
| T-IOS-2 | XCUITest: email sign-in through the web sheet lists the server; message a worker and see the reply |
| T-IOS-3 | Apple sign-in with no account shows the message, offers email sign-in, then links Apple; no account is created by the app |
| T-IOS-4 | In-app "Delete account" completes J18; Apple's token is revoked (a later sign-in with the same Apple ID asks for consent again) |
| T-IOS-5 | The app contains no price, purchase link or call to buy (string and link scan of the build) |
| T-MIG-1 | `amux cloud move` from a seeded instance: dry run lists every worker, card, schedule, memory and file; apply makes them present |
| T-MIG-2 | A second import of the same bundle changes nothing and says so |
| T-MIG-3 | The bundle is scanned for token-shaped values (`sk-`, `ghp_`, OAuth and session tokens) and any match fails; with the key box ticked, the key works |
| T-MIG-4 | Uncommitted local changes are flagged; the repository is re-cloned |
| T-MIG-6 | `amux cloud pull` from a Cloud server into a fresh local install: the dry run and the result match the source |
| T-MIG-7 | Round trip local to Cloud to local: workers, cards, schedules, memories, files and MDAI runs compare equal by id and checksum |
| T-YAML-1 | `amux cloud apply` on the sample file with a test account creates a server through test-mode checkout; every member, group, worker, schedule, skill, file, repo and skin in the file is present, and only `start: true` workers run |
| T-YAML-2 | Applying the same file again reports every item unchanged and changes nothing |
| T-YAML-3 | Changing one schedule in the file changes only that schedule |
| T-YAML-4 | A file containing a secret-shaped value is refused; `env: from: prompt` asks for the value and stores it on the server only |
| T-YAML-5 | `amux iac export` from a configured local server, then `amux cloud apply` of that file, reproduces it on Cloud; adding a server to a paid account is refused without `--confirm-billing` |
| T-MIG-5 | Plain `amux iac export` YAML imports; a corrupt bundle, unknown schema version or off-list table is refused and changes nothing |
| T-OPS-1 | A release reaches the synthetic server first, then the canary set; `/health` shows it |
| T-OPS-2 | A broken release rolls back on every server it reached, and the rollout halts |
| T-OPS-3 | The restore drill restores row counts and file checksums equal to the source |
| T-OPS-4 | Fleet agent SIGKILLed mid-swap: old or new version, never neither, and it says which |
| T-LIFE-1 | Webhook says cancelled while the provider API says active: nothing happens and a card opens |
| T-LIFE-2 | Provider API unreachable or webhooks silent for 6 hours: every destructive action pauses, provisioning continues |
| T-LIFE-3 | At 10 customers one deletion proceeds; a run that would delete more than the greater of 3 servers or 5% of the fleet trips the breaker and deletes none |
| T-LIFE-4 | No deletion without a listed snapshot and a backup under 1 hour old |
| T-LIFE-5 | An unlabelled server and a labelled orphan are both left running and reported |
| T-LIFE-6 | Shadow mode records decisions and executes none; the 14-day replay flags a decision the provider no longer supports |
| T-DUN-1 | Stripe test clock and injected clock: payment failure keeps service 14 days, then snapshot and delete; a fixed card restores from the snapshot |
| T-CAN-1 | Export downloads and opens |
| T-CAN-2 | Injected clock: 30 days after deletion the snapshot is gone |
| T-DEL-1 | Every listed resource gone; receipt lists each |
| T-DIS-1 | A test-mode dispute receives evidence through the API within an hour |
| T-DEL-2 | A sandbox Apple account's token is revoked by the deletion path actually used (Clerk's or ours); Apple's revoke endpoint returns success and a new sign-in asks for consent |
| T-PLAN-1 | Upgrade keeps data |
| T-HELP-1 | Bundle produced; agent answers |
| T-OP-1 | Each Operator credential attempts a write outside its scope and is refused; a merge outside `site/` and `docs/` is refused |
| T-KILL-1 | Each kill switch freezes its target and leaves a customer server serving; with the Operator stopped, M1, M2, M4, M5, M12, M17 still act |
| T-BUD-1 | A budget at 100% stops its spender and nothing else |
| T-M* | Each mechanism in section 10 fires on an injected trigger and produces its action, verification and escalation |

**Capability suite (nightly):** a test per dashboard tab and capability:
Workers (a Claude and a Codex worker each answer), Board, Groups, Projects,
Scheduler, Files, Scratchpad, MDAI, Messages (@mention and reply), Skills,
Browser, Email, Calendar, Connectors, Draw, Record, Map, Terminal, Workspace
grid, Metrics, Cost, Logs, Database, Disk, Proxies, Torrents, Converse and
dictation (fake audio capture), MCP, Computer use, Vault, Grants, Telegram,
Memories, journal, habits, CRM, Settings, Team presence, Self-update.

| Run | Cost |
|---|---|
| Journey every 6 hours and on each deploy | $7.65/month (4.2) |
| Capability suite nightly | Included in the same server-hour |
| Lifecycle, billing and kill-switch tests on each control-plane PR | Sandbox, injected clock |
| Restore drill weekly per server | One server-hour each |
| iOS tests on each app build | CI minutes |

## 13. Paths

### 13.1 Launch prerequisites (all must hold before the first paid checkout)

| # | Prerequisite | Evidence |
|---|---|---|
| P1 | Stripe products from `plans.json`; Stripe Tax on with the EU non-Union OSS registration; tax-filing integration connected; Radar rules; dispute webhook | Test and live products match `plans.json`; registration number recorded |
| P2 | Terms, privacy policy, data processing agreement, subprocessor list (Hetzner, Cloudflare, Clerk, Stripe, PostHog, Apple, Tailscale when used) | Pages live; linked from checkout |
| P3 | Clerk production with email code, Google, GitHub and Apple; Apple Services ID and key; relay email domain registered; Clerk production configured with our own Apple Services ID and key (so our key can sign Apple's client secret for revocation); whether Clerk revokes Apple tokens on user deletion, recorded | T-ACC-3, T-IOS-4, T-DEL-2 |
| P4 | `amux.cloud` on Cloudflare; Origin CA; Hetzner project with quota raised and abuse@ as contact; two payment methods on every provider | T-PROV-1; provider pages |
| P5 | Backups, restore drill and signed updates with rollback | T-OPS-1..4 green 7 days running |
| P6 | Safe lifecycle in shadow mode | T-LIFE-1..6 |
| P7 | Workspace invites through cloud.amux.io | T-TEAM-1..6 |
| P8 | Journey every 6 hours green for 7 days | Journey history |
| P9 | Existing Stripe subscribers and trials handled (O3) | Count and the approved email |
| P10 | Owner recovery codes and second admins stored | Checklist signed |
| P11 | Test credentials for the nightly suite: a Claude subscription token from `claude setup-token`, a ChatGPT test account for Codex, a Google test account for Gemini CLI, a test tailnet with an auth key | Stored in the control plane's secret store; the nightly suite reports "not run" if one is missing |

### 13.2 Critical path to 10 paying customers

The smallest system that sells, provisions, protects and supports. Founder
handles the rare exception directly (about 1.4 hours a month at 10 customers,
4.8).

Cost, stated plainly: about 32 agent-days to build [H], roughly $3,200 of
inference at $100 an agent-day [H], against $181 a month of contribution at 10
customers. That pays back in about 18 months at 10 customers before owner time,
and never at 10 once realistic owner time is counted (-$25 a month, 4.8); at 100 customers it pays back in under 2 months.
The build is justified by the path to 100, and every step below
is one that path needs anyway. The iOS step runs in parallel and launch is not
gated on App Review; web checkout and web Sign in with Apple cover payment.

**Step 0, a demand check before the 32 agent-days.** Only 19 people viewed
`/cloud` last month. Replace the Calendly button there and the OSS "Move to amux
Cloud" entry with "Get notified when your server is ready" (email only, stored
in the control plane). Build steps 2 to 8 once 10 people have signed up, and
email them at launch. If fewer than 10 sign up in 60 days, the plan stays at
step 1 and the decision goes to the weekly report as an L3 item whose default
is to wait.

| Step | Ships | Done when |
|---|---|---|
| 1 | `plans.json` and the price check; site pricing, cloud page and homepage CTA without trial or Calendly | T-SITE-1..3 |
| 2 | Control plane: checkout-first accounts with Stripe, Clerk with Apple, `servers`, `provision_jobs`, redirect, emails, kill switches | T-PAY-1, T-ACC-1..4, T-RED-*, T-MAIL-*, T-KILL-1 |
| 3 | Golden image with pinned Claude Code, Codex and Gemini CLI; door token mode; the model sign-in screen (keys and CLI logins, 8.4); templates | T-PROV-1, T-PROV-3, T-SSO-1, T-KEY-*, T-CLI-1..5, T-FIRST-1 |
| 4 | Fleet agent: litestream and nightly file backup to R2, Hetzner backups, weekly restore drill, signed releases with canary and rollback, heartbeat (M2, M4, M12) | T-OPS-1..4 |
| 5 | Safe lifecycle (M5) in shadow mode, dunning, cancel with export, account deletion (M19), abuse isolation (M17), provisioning recovery (M1), dispute evidence (M20) | T-LIFE-*, T-DUN-1, T-CAN-*, T-DEL-1, T-PROV-4, T-DIS-1 |
| 6 | Invites through cloud.amux.io; "Add to my tailnet" (8.5) | T-TEAM-1..6, T-TS-1..5 |
| 7 | iOS app update: Sign in with Apple and web sign-in; the onboarding button removed (parallel; not a launch gate) | T-IOS-1..5, T-TEAM-7 |
| 8 | Journey every 6 hours, nightly capability suite, weekly report with owner minutes | 7 green days; first report sent |
| Launch | Prerequisites P1 to P10 hold | |

### 13.3 Path to 100 customers under two hours of founder work a month

| Step | Ships | Done when |
|---|---|---|
| 9 | Lifecycle execution switches on by itself after 30 shadow days and 5 replayed decisions with no disagreement | T-LIFE-6 |
| 10 | The Operator (one worker), its three MDAI nodes, budgets (M18) | T-OP-1, T-BUD-1 |
| 11 | Support agent answering first (M8), setup and inactivity emails (M6, M7) | Agent resolution rate measured weekly |
| 12 | Portability: extended YAML, `amux cloud login`, `amux cloud apply`, bundle export and import in both directions, `amux cloud move` and `pull`, the dashboard buttons and every CTA in 7.1 | T-MIG-1..7, T-YAML-1..5, T-CLI-6 |
| 13 | Upgrade (J15), disk and upgrade signals (M3, M10), price guard (M13) | T-PLAN-1 |
| 14 | Content loop (M15) and deterministic site improvements (M14) | Leading indicators in the report |
| 15 | Contractor for escalations, only if measured owner time is over 2 hours for two months (4.8) | Owner minutes under 120 |
| Done | 30, then 90 days without a routine owner intervention; owner time under 2 hours a month at 100 customers | Weekly report |

**Later, only on evidence:** downgrades and a second server (requests in
support), the US region (EU customers churn citing latency), a warm pool
(a cold wait measurably hurts activation), referrals (attribution shows
word of mouth), a second Operator worker (section 5 rule).

**First concrete step:** `plans.json` and the price check, because the site, the
payment provider, the control plane and the tests all read prices from it.

## 14. Iteration history

| Revision | Score | Scored by | Changes | Remaining gaps |
|---|---|---|---|---|
| 1 | 6.5 | Self | First draft | Coverage, friction, owners |
| 2 | 9.1 | Self (inflated) | Redirects, 26 tabs, checkout first | Overturned at 6.6 by the reviewer |
| 3 | 7.65 | Independent reviewer | Certificate limits, downgrades, key handoff, email verification fixed | Duplicate-certificate limit, region moves, warm pool |
| 4 | 6.3 | Independent reviewer, owner's revised brief | Rewritten as an owner-light business with an Operator, funnel, budgets, mechanisms | VAT, fees, stopped servers, hidden owner work |
| 5 | 7.1 | Independent reviewer | Merchant of record, VAT-net economics, volume model, move to Cloud, monitoring worker | Bundle secrets, credential scopes |
| 6 | 7.7 | Independent reviewer | Allowlist bundle, scoped credentials, flat-traffic ceiling | App Store key, Operator cost, traffic stop rule |
| 7 | 7.6 | Independent reviewer | Owner's safety revision: backups, restores and updates before launch; safe lifecycle (3.2); owner-intervention stress test with outsourcing (4.8); sample-size rule for experiments; content never auto-unpublished; channel versus tactic judging; failure audit (11.1); one payment provider name; launch prerequisites; paths to 10 and 100; invites end to end (8.2); Sign in with Apple (8.3); cut M9, M16, M20, four MDAI nodes and hourly runs | R2 write cost, breaker at small fleets, Apple sign-in through Clerk, unstated build cost, test-range and invite-mail details |
| 8 | 8.0 | Independent reviewer | litestream every 60 s and R2 in the model; breaker uses the greater of 3 servers or 5%; shadow mode judged automatically by a 14-day replay; Apple sign-in through Clerk's iOS SDK with token revocation confirmed; build cost and payback stated; iOS not a launch gate; invite email and revocation mechanics; journey server lifetime | |
| 9 | not re-scored | | Payback after owner time stated; a step-0 demand check gates the build; shadow mode needs 5 replayed decisions; Apple revocation tested with our own Apple key (T-DEL-2) | Final reviewer score 8.0 on revision 8, below the 9 target |
| 10 | pending | | Stripe only (owner decision); model sign-in through Claude Code, Codex and Gemini CLI logins and `amux cloud login`; add to your tailnet; full YAML provisioning with `amux cloud apply` and import/export in both directions | |
