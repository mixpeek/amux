# Cloud Mac deployment plan

Source: MSG-70893 (card AMUX-5363), plus Ethan's follow-up on 2026-09-30.
Written 2026-09-30 by amux-cloud. Prices checked the same day.

## Goal

Get a customer live on amux in one day with no hardware purchase:

1. Rent a macOS machine in the cloud that stays up 24/7.
2. Put it on a Tailscale tailnet that both our side and the customer's side join.
3. Run amux on it, with every action attributed to the person who took it.
4. Give both sides remote screen control (see the screen, click, type).
5. Back up the whole system so it can move later, for example onto a dedicated
   Mac Studio the customer buys, without rebuilding by hand.

## Deployment model

In Ethan's words (2026-09-30): we dogfood the experience ourselves first, then
deploy it for the customer the same way.

1. **We go first.** We rent the Mac, set up amux and the workflows, and invite
   ourselves as members, exactly as a customer would be invited.
2. **Full service before handover.** Email, credentials and connectors are
   working before the customer signs in, so they start with a working system
   rather than a setup task.
3. **We work on it remotely, like our own Mac.** amux in the browser plus
   Remote Desktop over Tailscale. Nothing about it differs from how we work
   today.
4. **The customer joins from their laptop.** Tailscale on the laptop, amux in
   the browser, screen sharing when they want the desktop.
5. **Backed up and portable from day one.** The whole system can move to a Mac
   Studio or any other Mac later.

### Isolation tiers

The same code sets up all three. Only where the Mac lives changes.

| Tier | Where the Mac is | When |
|---|---|---|
| 1. Cloud Mac (default) | Rented from Scaleway, on a tailnet we own | Every customer at the start. Live in a day. |
| 2. Customer's own Mac | Their office or data center; we install over Tailscale | Their security team wants the hardware in their control. |
| 3. Mac we ship | We set it up here, then courier it | They want dedicated hardware without doing the setup. |

Moving from tier 1 to tier 2 or 3 is the Time Machine restore described under
Backup and portability. The Ansible playbook runs over SSH on any Mac, so tiers
2 and 3 need no new code.

## Provider choice

All of these are real Apple hardware. Apple's macOS license requires a 24-hour
minimum lease, so every provider bills by the day or month, never by the minute.

| Provider | Machine | Price | Region | Notes |
|---|---|---|---|---|
| Scaleway | Mac mini M4, 16 GB, 256 GB | €149/mo (€0.22/h) | Paris only | Cheapest current M4. VNC built in. |
| Scaleway | Mac mini M4, 32 GB, 1 TB | €199/mo (€0.29/h) | Paris only | Best fit for amux plus a few agents. |
| Scaleway | Mac mini M4 Pro, 64 GB, 2 TB | €335/mo (€0.49/h) | Paris only | Heavy agent load. |
| MacStadium | Mac mini M4, 16 GB, 256 GB | $119 to $149/mo | US (Atlanta, Las Vegas) and EU | 24x7 support is 10 to 20% extra. |
| MacStadium | Mac mini M4, 24 GB, 512 GB | $199 to $249/mo | US and EU | |
| AWS EC2 | mac-m4.metal, 24 GB | about $1.23/h, about $900/mo | Most regions | Fits if the customer must stay inside AWS. Six times the price. |

**Recommendation: Scaleway M4-M (32 GB, €199/month), because everything has to
be infrastructure as code.** Scaleway's Terraform provider manages its Macs
(`scaleway_apple_silicon_server`). MacStadium's dedicated Macs are ordered
through a web portal and have no Terraform provider, so they fail that
requirement. AWS is fully in Terraform too, at about 4.5 times the price. Use
it when the customer's security review requires AWS, or when a US customer
finds screen sharing to Paris too laggy (roughly 80 to 100 ms round trip from
the US East Coast; worth testing in the first hour).

16 GB is too tight once amux, a browser and two or three Claude Code panes run
together. Start at 24 or 32 GB.

Can it stay up 24/7? Yes. These are dedicated physical machines with no idle
shutdown. The risk is a macOS update rebooting it, so turn off automatic OS
updates and let it boot straight into an unlocked account (step 2 below).

## Infrastructure as code

Everything below the provider order lives in the repo, one directory per
customer, and a single `make up CUSTOMER=<name>` brings a machine from nothing
to a working amux.

| Layer | Tool | What it covers |
|---|---|---|
| Machine | Terraform, `scaleway` provider (`scaleway_apple_silicon_server`) or `aws` provider (`aws_ec2_host` plus `aws_instance` with a macOS AMI) | Rent, size, region, firewall, teardown |
| Network | Terraform, `tailscale` provider (`tailscale_acl`, `tailscale_tailnet_key`) | ACL policy as a reviewed file, a one-use auth key for the Mac, device tags |
| macOS | Ansible over SSH (`community.general.homebrew`, `osx_defaults`, plain `command` tasks) | Users (`sysadminctl -addUser`), energy settings (`pmset`), update policy, Screen Sharing service, Homebrew packages from a `Brewfile`, Tailscale join |
| amux | Same Ansible playbook | Runs `install.sh` and `make install-cli` from a pinned commit, writes `server.env` from vault values, creates workers and board |
| Backup | Same Ansible playbook | restic launchd job and B2 bucket key, `tmutil setdestination` for Time Machine |
| Secrets | amux vault, read at apply time | Admin password, Tailscale key, B2 key. Never in the repo |

Terraform state goes in a remote backend (a B2 or GCS bucket), with one state
file per customer, so any lane can run `plan` against the real machine.

**Steps that cannot be scripted, and the honest workaround for each:**

- **Granting remote screen control.** Since macOS 12.1 Apple blocks scripting
  the permission that lets a remote user control the screen; only MDM or a
  person clicking in System Settings can grant it. Viewing can be scripted.
  Workaround: one click through the provider's web console on day one, or enroll
  the Mac in an MDM (Jamf, Kandji, or the free MicroMDM) and push it as a
  profile. MDM is the fully coded path and worth it from the second customer on.
- **The same restriction covers Full Disk Access** for restic and the terminal
  apps amux drives. Same fix: one click on day one, or an MDM profile.
- **FileVault unlock after a reboot** needs a person at the provider console,
  which is why the runbook leaves FileVault off unless the customer requires it.

## Day-one runbook

The steps below are what the Terraform and Ansible code does. Run them by hand
only for the first machine, while the code is being written.

Target: about 4 hours of our time, most of it waiting on the provider.

### 1. Provision (30 min)

- Order the machine. Scaleway is instant through its console or API. MacStadium
  can take up to a business day, so order the day before.
- Note the admin user, the password and the public IP.

### 2. Harden and set up macOS (45 min)

- Change the admin password and store it in the amux vault.
- Turn on FileVault only if the provider gives console access for the unlock
  prompt after a reboot. Otherwise a reboot hangs at the unlock screen with
  nobody able to reach it. Scaleway and MacStadium both provide that access.
- Disable automatic macOS updates. Updates get scheduled and applied by us.
- Set energy settings: never sleep, restart after power failure.
- Create one macOS user per person (`ethan`, `customer-name`). Separate
  accounts are what make attribution work at the OS level (see below).

### 3. Tailscale (20 min)

- Install Tailscale and log it in to a tailnet we own.
- Invite the customer's people as tailnet users. Tailscale's free plan covers
  3 users, and the paid plan is about $6 per user per month.
- ACLs: both sides may reach this machine on screen sharing (5900), SSH (22)
  and amux (8824). Nothing else on our tailnet is reachable for them.
- Turn on Tailscale SSH so shell access is also tied to a tailnet identity.
- Once Tailscale works, close public screen sharing (5900) and amux (8824) with
  the macOS firewall (`pf`). Leave public SSH open for break-glass access, locked
  down as described below.

### Break-glass access: SSH outside amux and Tailscale

We need a way in that does not depend on amux or Tailscale, for when either
one is the thing that is broken.

**Confirmed available, 2026-09-30:**
- **Scaleway** gives SSH to every Mac mini. The console's overview page shows the
  exact SSH command and default user, and login uses an SSH key uploaded to the
  Scaleway project. Its docs also note a login-failure block (fail2ban) that a
  reboot clears. The console and API can also reboot the machine, open remote
  desktop, and reinstall macOS.
- **AWS EC2 Mac** gives SSH with an EC2 key pair, plus Systems Manager Session
  Manager as a second shell path that needs no open port.

**How we lock it down** (Scaleway documents no network firewall in front of the
Mac, so this is done in macOS by the Ansible playbook):
- Key-only login: `PasswordAuthentication no` and `KbdInteractiveAuthentication
  no` in `sshd_config`.
- Only one break-glass account (`breakglass`, an admin) may log in over public
  SSH (`AllowUsers breakglass`). Its key lives in the amux vault, and a copy
  goes somewhere that does not depend on amux (a password manager), so we can
  still reach it when amux is down.
- Optionally, `pf` allows port 22 only from our fixed addresses, if we have any.
  Without fixed addresses, key-only login plus fail2ban is the protection.
- Everything else, including day-to-day shell access, goes over Tailscale SSH,
  so it is tied to a person.

**If SSH itself is broken,** we fall back in this order: Scaleway console
reboot (clears fail2ban and restarts sshd), then Scaleway remote desktop, then
restoring from backup onto a fresh Mac.

### 4. Remote screen (15 min)

- Turn on Screen Sharing (System Settings, General, Sharing) and limit it to
  the named users.
- Connect over the tailnet with the built-in Screen Sharing app
  (`vnc://<tailnet-name>`). On macOS 14 and later, "High Performance" mode gives
  smooth video on a good connection.
- Two people can watch and control one screen at once. Each person can also log
  in to their own separate desktop session with their own macOS account, so
  they can work in parallel without taking the mouse from each other.

### 5. amux (60 min)

- Install from a reviewed checkout: `install.sh`, then `make install-cli`.
- Set `~/.amux/server.env` with only the credentials this customer needs. Never
  copy our server.env over.
- Serve the dashboard on 8824 to the tailnet only.
- Create the customer's workers and board.

### 6. Backup (30 min, see next section)

### 7. Walkthrough with the customer (60 min)

- Each person joins the tailnet, opens the dashboard and the screen, and takes
  one action. Then we read the board log and confirm each action shows the
  right name.

## Attribution: who did what

Three layers, in the order to rely on them:

1. **amux**: every board mutation, message and send is stamped server-side.
   A worker is stamped by its session name. A person is stamped as
   `member:<email>` if they signed in as an invited org member (see
   Multiplayer below for what exists and what is missing).
2. **Tailscale**: every connection is tied to a tailnet user, and the admin
   console logs it. This tells us who connected, not what they did.
3. **macOS**: separate user accounts mean files, shell history and the unified
   log each carry the account name.

## Multiplayer: several people on the same workers

What amux has today, read from origin/main on 2026-09-30:

- **People already have an identity.** Invited org members sign in once and
  carry an `amux_member` cookie. The `local_member_identity` middleware
  (`api/org.rs:400`) turns it into `member:<email>`, strips any forged copy of
  its headers, and stamps it wherever no worker header is set. Board writes pick
  it up first (`actor_from_headers`, `api/board.rs:2312`), and sends record it
  as their origin (`send_post`, `api/session_verbs.rs:25805`).
- **amux can already ask Tailscale who a connection belongs to.**
  `static_files/tailnet_auth.rs` calls tailscaled's `whois` for the owner
  bootstrap. That lookup is made by tailscaled, not read from a request header,
  so a user on the tailnet cannot fake it.
- **The server listens on all interfaces** (`lib.rs:1075`) with the Tailscale
  certificate, so no proxy is needed in front of it.
- **Stale board writes are refused** with a 409 (`patch_item`, `board.rs:~10512`),
  but the reply does not say who made the newer change.
- **Worker messages are queued one at a time** (`steering_queue`, with a
  `sender` column). A worker sender is shown to the receiving agent
  (`origin_stamped`, `session_verbs.rs:12831`); a person's sends skip that
  label, so the agent cannot tell Alice's instruction from Bob's.
- **Missing entirely:** a typing lock on a pane, presence, and a "by person"
  view of the history.

So the gaps are joining a tailnet login to a member, carrying the person
through to the worker, and making conflicts and presence visible. None of it
needs a new primitive.

### Should we use Yjs (or another CRDT)?

No, for the board and for worker input. A CRDT merges concurrent edits
silently, which is right for co-editing a text document and wrong here. When
two people change the same card, or give the same worker opposite
instructions, the useful outcome is that the second person SEES the conflict
and who caused it, then decides. amux's ethos already records this as settled
("No CRDT for the board": `rev` is a concurrency check whose failure is the
product). Yjs only earns a place if we later add a live co-edited document
surface, where merging keystrokes is the point.

### Design rules

1. **One identity: the org member.** A tailnet login is a way to sign in as a
   member, not a second identity system.
2. **Worker input is serialized, never merged.** Every message reaches the agent
   labeled with the person who sent it.
3. **Conflicts are shown, with a name.** A refused write says who got there
   first and when, and offers to re-apply.
4. **Everyone can see who else is here.**

### Work plan

Sizes: S is under a day, M is one to three days, for one lane. Every item ships
with its log signal (a verdict field or WARN line), per the amux two-fix rule.

**MP-1. Sign in by tailnet login (M). Blocks everything below.**
- In `local_member_identity` (`api/org.rs:400`), when a request carries no
  member cookie and comes from a tailnet address, ask tailscaled `whois` (reuse
  `tailnet_auth.rs`). If the login matches an invited, accepted member's email,
  treat the request as that member and set the cookie.
- Opt in per machine with a scoped env key, `AMUX_TAILNET_MEMBER_AUTH=1`, off by
  default. An unmatched login gets the normal sign-in page, not the owner.
- Log `verdict=tailnet_member_auth` with `matched`/`unmatched`/`whois_failed`.
- Done when: a test with a stubbed `whois` gets `member:<email>` on a board
  write, and an unmatched login is refused.

**MP-2. Every write path carries the person (M).**
- Board and sends already read the member. Audit the rest: steering enqueue
  (`sender` must be `member:<email>`, not empty), email and alert sends (their
  `hdr_worker` copies in `api/email.rs:191` and `api/alerts.rs:536`), schedule
  and note writes.
- Write one test that walks every mutating route in `/api/debug/routes` with a
  member request and fails on any row stored without the member. That test is
  what makes the "full provenance" claim checkable.
- Done when: that test passes, and deleting the member stamp from any one route
  makes it fail.

**MP-3. The worker sees who is talking (S).**
- Extend `origin_stamped` (`session_verbs.rs:12831`) so a member send is labeled
  `[from alice@customer.com, server-verified]`, the same way worker sends are
  labeled today. Slash commands stay unlabeled.
- Done when: two members' messages to one worker arrive in order, each with its
  label, and the agent's reply names each person.

**MP-4. A board conflict names the other person (S).**
- In the stale-rev 409 in `patch_item`, add `changed_by` and `changed_at` from
  the newest revision event.
- In the dashboard, show "Alice changed this 12 seconds ago" with Re-apply and
  Discard buttons.
- Done when: two browser sessions edit one card and the loser sees the winner's
  name and re-applies cleanly.

**MP-5. Typing lock on a pane (M).**
- `keys_verb` (`session_verbs.rs:26625`) and typed sends through `send_post` take
  a lock on the pane for the member who typed, released after 30 seconds of
  quiet. `keys_verb` needs the request headers passed in for this.
- While another member holds it, keys are refused with `pane_held_by`, and
  typed sends go to the steering queue instead, so nothing is lost.
- The dashboard shows "alice is typing" and why your input was queued.
- Screen sharing bypasses this: two people with the mouse on one screen is
  handled by macOS, not amux. Say so in the customer walkthrough.
- Done when: two members type at once and one pane gets one person's input,
  with the other person's text delivered afterward from the queue.

**MP-6. Presence (M).**
- The SSE stream (`api/sse.rs:34`) only carries saved state changes. Add a second,
  in-memory channel for short-lived events and merge it into the stream.
- The dashboard reports which worker you are looking at when that changes; the
  server broadcasts `presence` with everyone's current view and drops anyone
  silent for 30 seconds.
- Include presence in the polling fallback too, per the SSE rules in
  `crates/amux-dashboard/CLAUDE.md (Realtime)`.
- Done when: two browsers on one worker each show the other's avatar within two
  seconds, and it clears within 30 seconds of one closing.

**MP-7. History by person (S).**
- Add `?actor=member:<email>` to the board log and message history APIs, and a
  person filter in the dashboard.
- Done when: filtering by Alice shows exactly the actions from the demo that
  she took.

**MP-8. The demo as an end-to-end test (M).**
- A Playwright test with two browser contexts signed in as two members, running
  the demo script below. It runs in CI so the claim stays true after later
  changes.

**MP-9. Switching servers shows who you are on each one (S).**
- The switcher in the dashboard (`amux_connections`, `_switchServerUrl` in
  `app.js`) lists servers but not whether you are signed in on them. Ask each
  listed server's `/api/identity` and show "you: alice@customer.com" or "not
  signed in" beside it, so a switch never lands on a sign-in wall by surprise.
- After a switch, the header shows the server name and your identity on it.
- Done when: from one browser, switching across three servers (ours, a
  customer's, a second customer's) lands signed in on each, with the right name
  shown, and no token typed anywhere.

**MP-10. Every dashboard action is attributed from the browser (M).**
- MP-2 proves attribution at the API. This proves it at the CLIENT: a
  Playwright test signed in as a member clicks through every dashboard action
  that writes (send, card create/edit/move, schedule, note, file save, settings)
  and then reads the board log and message history.
- It fails on any write recorded as `api-anonymous`, as the owner, or with no
  person.
- It also runs after a server switch, so attribution holds on the second server
  too.
- Done when: the test passes, and removing the member stamp from any one route
  makes it fail.

Order: MP-1, then MP-2, then MP-3 to MP-7 and MP-9 in parallel, then MP-8 and
MP-10. About 15 lane-days in total, which fits in three weeks for one lane or
under two weeks for two.

MP-1 shipped 2026-09-30 (`46c9b4cb`, AC-439).

For a first customer who cannot wait, MP-1 to MP-4 are the minimum: people sign
in, every action names them, the worker knows who is talking, and conflicts
name the other person. That is about five lane-days.

### The demo that proves it (acceptance for "multiplayer done")

Two people, two laptops, both on the tailnet, one worker:

- Both open the same worker. Each sees the other's avatar.
- Both send the worker a message within the same second. Both messages are
  delivered in order, each labeled with its sender, and the worker's reply
  addresses each person.
- Alice types in the pane. Bob's typing is queued and he sees why.
- Both edit the same card. One write lands; the other gets a conflict naming
  the first person, and re-applies.
- The board log and message history show the right person on every one of the
  above, not the session name alone.

## Switching between servers

A person will use several amux servers: ours, and one per customer. Switching
has to be one click, land signed in, and keep attribution.

**What already works.** The dashboard keeps a list of servers in the browser
and switches by opening the other server's own address. The list it carries
holds only names and addresses, never a credential, so each server signs you
in itself.

**Signing in on each server** is MP-1: if you are an invited member there, your
Tailscale login signs you in with nothing to paste. For the owner on their own
servers, `AMUX_TRUST_TAILNET_OWNER=1` does the same.

**One Tailscale login reaching every customer.** A device is signed in to one
tailnet at a time, and the plan gives each customer their own tailnet. So each
customer Mac is SHARED into our tailnet with Tailscale's device sharing:

- The Mac lives in the customer's tailnet, where their people reach it.
- It is shared into ours, so our team reaches every customer Mac without
  switching Tailscale accounts.
- The customer's people see only their own Mac; nothing of ours is shared back.
- To confirm in the dogfood run: `whois` on the customer Mac reports our
  person's own login for a shared-in connection, so MP-1 signs us in as the
  right member there too.

**Tests.** MP-9 and MP-10 above, plus invariant CM-16.

## Tailscale acceptance

- The Mac shows as connected in the tailnet admin console, tagged
  `tag:customer-<name>`.
- Adding a person is one Terraform change (or one invite link), with no step on
  the Mac itself. Removing them cuts off the dashboard, screen and SSH within a
  minute.
- A test person can reach only this Mac, not anything else on the tailnet.
- The Mac rejoins the tailnet on its own after a reboot.

## Other requirements worth deciding now

- **Whose Claude account runs the workers.** A personal Claude subscription is
  licensed to one person, so a shared machine used by several people should run
  on an Anthropic API key from an organization account (ours or the
  customer's). This also settles who pays for tokens.
- **Spend per person.** With human identity in place, token cost can be totaled
  per person as well as per worker. Useful for the customer, and for us if we
  bill usage.
- **Offboarding.** One command removes a person from the tailnet, their macOS
  account and their amux access, and keeps their history for the audit trail.
- **Health alerts.** Our own amux pings the customer machine every few minutes
  and pages us if it stops answering or reboots unexpectedly.
- **Data boundary.** None of our credentials ever go onto the customer's
  machine, and none of theirs come back to ours. Separate tailnet, separate
  `server.env`, separate backup bucket.
- **Data location.** Scaleway's Macs are in Paris. Check this is acceptable to a
  US customer's compliance people before renting.

## Backup and portability

The goal is to lift the whole setup onto other hardware later, such as a Mac
Studio at the customer's office.

A disk-image clone does not work for this. Apple silicon Macs cannot reliably
boot a cloned system volume on a different machine. The supported path is
Migration Assistant, which reads a Time Machine backup and rebuilds apps,
users, settings and data on the new Mac.

So run two backups:

1. **Time Machine, for moving machines.** Point it at a network share on the
   tailnet (an SMB share on a Mac or NAS we control), hourly. To move: set up
   the new Mac, run Migration Assistant, choose that backup. This restores every
   user account, app and setting.
2. **restic to Backblaze B2, for disaster recovery.** The same setup this box
   already uses (see `~/Dev/CLAUDE.md`): `~/`, `~/.amux`, `~/.claude`, every 6
   hours, 7 daily, 4 weekly and 6 monthly snapshots. Use a separate bucket and
   key per customer. Cost is well under $1/month at these sizes.

Also keep a **bootstrap script in the repo** (`Brewfile` plus the steps above as
a script), so a clean machine can be rebuilt without either backup. It also
documents exactly what the machine contains.

Test the move before a customer needs it: restore the Time Machine backup onto
a second rented Mac for a day (about $10) and check that amux, the workers and
Tailscale come back up.

## Acceptance: the dogfood run

We run this whole list on our own rented Mac, as the "customer", before any
real customer. Every line has a check that can fail. The run is not complete
until every line passes, and the results go on the card as evidence.

**Live in a day**

- [ ] From an empty account, `make up` finishes and the dashboard loads over
  Tailscale in under 4 hours of wall-clock time. Record the actual time.
- [ ] `make down`, then `make up` again from nothing, passes every check below
  a second time.

**Access**

- [ ] A second person (one of us, acting as the customer) accepts a tailnet
  invite on their own laptop, opens amux in the browser and is signed in as
  their member, with no step done on the Mac. (Needs MP-1.)
- [ ] The same person opens Screen Sharing over Tailscale and can click and
  type on the desktop.
- [ ] That person cannot reach anything else on our tailnet (connection test
  to one of our other machines fails).
- [ ] Removing the person from the tailnet cuts off the dashboard, screen and
  SSH within a minute.

**Full service**

Each connector is proven by using it for real, not by the credential being
present.

- [ ] Email: a worker sends a message through `/api/email/send` and reads the
  reply through `/api/email/inbox`, from the customer's own account.
- [ ] Calendar: a worker creates an event, and it shows up in the customer's
  calendar through the iCal feed.
- [ ] Every other connector the customer uses (Google Drive, Slack, Granola and
  so on): one real read and one real write each.
- [ ] Credentials: `GET /api/connectors` shows no connector with missing keys
  among the ones this customer needs.
- [ ] Browser: a saved sign-in profile opens a page that requires the
  customer's login.
- [ ] A schedule fires on time and its run is recorded.
- [ ] None of our credentials are on the machine: search `server.env` and the
  vault for our keys by name and find none.

**Working remotely**

- [ ] Intervention drill: we break something on purpose (stop a worker, fill a
  queue), and fix it using only amux in the browser and Screen Sharing, with
  nobody at the machine.
- [ ] Reboot drill: reboot the Mac. It comes back on Tailscale, amux is up and
  workers resume, with nobody touching it. Record how long it takes.
- [ ] Health alert: our own amux notices the reboot and alerts us.

**Attribution**

- [ ] The two-person demo under Multiplayer passes (MP-8), and the history shows
  the right person on every action.

**Portability**

- [ ] Restore the Time Machine backup onto a second rented Mac with Migration
  Assistant. amux, the workers, the connectors and Tailscale all work there.
- [ ] Restore one file from the restic backup in B2.

When this list passes on our own Mac, the first customer gets the same run with
their accounts in place of ours.

## Invariants: continuous validation on the cloud Mac

The checklist above is run once. These checks run forever, on our dogfood Mac
first and then on every customer Mac, so each promise in this plan is
something that goes red the moment it stops being true.

### How they run

- **Outside-in checks** run on OUR amux as a `kind: shell` schedule every 5
  minutes, probing the customer Mac over Tailscale. They see what a customer
  sees, and they still report when the Mac is down.
- **On-box checks** run on the customer Mac as a `kind: shell` schedule every
  15 minutes, reading its own amux API and system state.
- **Output contract.** Every check prints one JSON line:
  `{"check":"CM-1","ok":true,"measured":true,"n_considered":1,"detail":"..."}`.
  A check that could not run reports `measured:false`, and **unmeasured counts
  as failing**, never as passing (amux ethos rule 4).
- **Failure path.** A failing or unmeasured check opens or updates ONE card per
  check on the amux-cloud board (deduped by check id and customer) and sends an
  amux alert. It closes the card itself when the check passes again, with the
  passing line as evidence.
- **The checks are code in the IaC directory** (`checks/cloud-mac.sh` plus a
  per-customer config naming the expected members, connectors and backup
  targets), so a new customer gets the full set from `make up`.
- **Each check must be able to fail.** When a check is added, break the thing
  it watches once on the dogfood Mac and confirm it goes red, then restore.
  Record that in the check's comment.

### The invariants

| ID | Promise | Where | How it is checked | Passes when |
|---|---|---|---|---|
| CM-1 | amux is up | Outside-in | `GET https://<mac>:8824/health` over Tailscale | 200, and `commit` equals the pinned commit for this customer |
| CM-2 | amux and screen sharing only over Tailscale | Outside-in | Connect to ports 5900 and 8824 on the Mac's PUBLIC address | Both refused or time out |
| CM-2b | Break-glass SSH works, and only with a key | Outside-in | Over the PUBLIC address: `ssh -o BatchMode=yes breakglass@<ip> true` with the vault key, then the same with a password only (`PreferredAuthentications=password`) | Key login succeeds; password login is refused |
| CM-3 | Remote screen works | Outside-in | Open port 5900 over Tailscale and read the greeting | Greeting starts with `RFB` |
| CM-4 | Stays up 24/7 | Outside-in | Compare `/health` start time with the previous run | No restart outside an announced maintenance window |
| CM-5 | People sign in by Tailscale login | Outside-in | Our probe device is an invited member; `GET /api/identity` from it | `is_local_member` true, with our probe's email (needs MP-1) |
| CM-6 | A customer can reach only their Mac | Tailscale | `tests` block in the Tailscale policy file, checked by Tailscale on every policy change | Customer users are allowed to the Mac's ports and denied everything else |
| CM-7 | Full service: credentials | On-box | `GET /api/connectors` against the customer's required list | No required connector reports missing keys |
| CM-8 | Full service: email works | On-box, daily | Send to a canary address with a unique subject, then find it with `/api/email/search` | Found within 10 minutes |
| CM-9 | Schedules fire | On-box | `GET /api/schedules/runs` | Every enabled schedule ran within its expected window |
| CM-10 | None of our credentials on their Mac | On-box | Look up our key NAMES (a denylist in the config) in `server.env` and the vault | Zero matches |
| CM-11 | Every action names a person or worker | On-box | Board and message history for the last 24 hours | No row with an empty or `api-anonymous` actor |
| CM-12 | Backups are current | On-box | `restic snapshots --latest 1` and `tmutil latestbackup` | restic under 7 hours old, Time Machine under 2 hours |
| CM-13 | Backups restore | On-box, weekly | Restore one known file from restic to a temp directory | Its hash matches the live file |
| CM-14 | The machine stays ours to schedule | On-box | `softwareupdate --schedule` and `pmset -g` | Automatic updates off, sleep never, restart after power failure on |
| CM-15 | Room to work | On-box | Free disk and memory pressure | Disk over 20% free, memory pressure normal |
| CM-16 | Switching lands signed in | Outside-in | From our probe device, `GET /api/identity` on EVERY server in our switcher list | Each answers with our probe's member identity, not a sign-in wall and not the owner |

CM-6 is the only check Tailscale runs rather than us: a policy change that
breaks isolation is rejected before it applies, which is stronger than a probe.

### What "live" means for a customer

A customer Mac counts as live when every check above has been green for 24
hours in a row and the one-off drills in the dogfood checklist have passed.
Before that the customer can use it, and the board shows which promise is not
yet kept.

## Monthly cost, one customer

| Item | Cost |
|---|---|
| Mac mini M4, 24 to 32 GB | about $200 |
| Tailscale, if more than 3 users | about $6 per user |
| Backblaze B2 | under $1 |
| **Total** | **about $200 to $230** |

## Decisions needed from Ethan

- **Spending:** approve the first rental (about $200/month) and which provider,
  once the customer's region is known.
- **Tailnet:** one tailnet per customer, or a shared tailnet with strict ACLs.
  Recommendation: one per customer, so nothing on ours is ever reachable by them.

## Next steps

1. One card per multiplayer work item, MP-1 to MP-10 (MP-1 is shipped). MP-2
   blocks any promise of "full provenance".
2. Card: the IaC directory (Terraform for Scaleway and Tailscale, the Ansible
   playbook, the Brewfile, `make up` and `make down`).
3. After spending approval: rent one Mac and do the dogfood run above on it,
   with ourselves as the customer. The second clean `make up` is the proof that
   nothing depends on a manual step we forgot to write down.
4. Decide on MDM before the second customer.

## Sources

- Scaleway Apple silicon pricing: https://www.scaleway.com/en/pricing/apple-silicon/
- AWS EC2 Mac instances: https://aws.amazon.com/ec2/instance-types/mac/
- mac-m4.metal on-demand price: https://instances.vantage.sh/aws/ec2/mac-m4.metal?currency=USD
- MacStadium pricing: https://macstadium.com/pricing
- Scaleway Terraform provider: https://registry.terraform.io/providers/scaleway/scaleway/latest/docs
- MacStadium bare metal (portal ordering): https://docs.macstadium.com/docs/bare-metal-hosts
- Scaleway: connect to a Mac mini using SSH: https://www.scaleway.com/en/docs/apple-silicon/how-to/connect-to-mac-mini-ssh/
- Scaleway: SSH troubleshooting (fail2ban): https://www.scaleway.com/en/docs/apple-silicon/troubleshooting/cant-connect-using-ssh/
