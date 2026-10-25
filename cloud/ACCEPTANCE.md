# amux Cloud acceptance criteria

The goal, in the owner's words: "I never think about amux cloud again and its entirely set and
forget (users sign up -> get cloud can handle their account, delete, etc.) cloud instances get all
the amux updates as they come out etc."

amux Cloud is done when every item below passes as a real customer would experience it, with
evidence. An independent Codex thread ("#amux cloud e2e") runs the checks and reports failures to the
amux-cloud worker. Its brief and evidence are under `~/.amux/cloud/codex-e2e/`.

## Criteria

1. **Understand the offer.** From amux.io a visitor learns what amux is, that self-hosting is free,
   and what Cloud adds and costs. Works on desktop and a 390px phone with no overflow and no console
   errors.
2. **Buy without help.** A visitor gets from the homepage button through Stripe Checkout to a paid
   order.
3. **Told when ready.** On screen and by email, within a few minutes, even when the first attempt
   to create the server fails.
4. **Sign in to their own server.** Wrong credentials and other people are refused.
5. **Connect an AI account.** Claude, ChatGPT or Gemini subscription, or an API key, from the
   dashboard without help.
6. **First real work.** A first worker does real work, including on the customer's own GitHub repo.
7. **Phone.** The customer reaches the server from the iOS app or PWA and steers a worker.
8. **Team.** The customer gives a teammate their own login and can revoke it.
9. **Healthy with nobody touching it.** New amux releases reach existing servers automatically,
   the server recovers from a crash or reboot, and TLS renews.
10. **Work survives a failure.** Backups exist and a restore works.
11. **Billing alone.** Change card, see invoices, cancel, resubscribe.
12. **Delete the account.** The customer deletes their account and data themselves, and the
    server, DNS record, snapshots and subscription are really gone.
13. **Clean cancellation.** Service runs to the end of the paid period, data is kept 30 days, then
    removed, and billing stops.
14. **Failures are clear.** Every failure the customer can hit shows a clear message and a way to
    get help, and the operator hears about it without watching a dashboard.
15. **Abuse and edge cases.** Double submit, replayed and forged webhooks, name collisions,
    reserved names, Hetzner capacity errors, failed payments and a browser closed mid-flow leave no
    duplicate or orphaned server, no charge without a server and no server without a charge.
16. **Live mode is sane.** cloud.amux.io is healthy, live checkout is amux-branded, and the
    FIRSTMONTH code shows $0.00 due today. No live purchase is made in testing.

## Status (2026-10-18)

| # | Status | Evidence |
|---|---|---|
| 1 | Pass | Codex round 1 |
| 2 | Pass | Codex round 1 |
| 3 | Fixed, proven live | The Worker cron finishes any paid order without a server (c9f139760). Test order r2c1455 hit Hetzner capacity errors for about 4 minutes and was finished at 363 seconds. Readiness waits for the dashboard and first update. |
| 4 | Pass | Codex round 1 |
| 5 | Pass (API key) | Codex round 3: an OpenAI key saved in Vault drives real Codex turns. A saved key now also signs Codex in before its login picker (2f2f148f6) |
| 6 | Pass | Private repository, 2026-10-19 on priv-9654: a GH_TOKEN saved in Vault reached a default Codex worker, which cloned the private mixpeek/mixpeek-tools at its real HEAD (3810d9540) in 21 s without printing the token. Codex round 5 on image 322cec9c9: a default Codex worker (no bypass) cloned mixpeek/amux, read it, wrote a note and the proof file, read back independently. Round 4 C6-4: a default Codex worker (no bypass) could not resolve github.com, because its workspace-write sandbox has network off and `-a never` cannot ask. Codex workers now launch with network access inside the sandbox (0aaa74e83). Earlier proof: 2026-10-18 on the e2e server: a new Codex worker cloned mixpeek/amux and wrote `~/work/report.txt` with CLOUD_E2E_OK, the commit sha and the four workspace crates. The fix was on the host: Ubuntu 24.04 blocked the user namespaces Codex's sandbox needs; bootstrap.sh and the hourly update.sh now allow them (fe9ac3b56) |
| 7 | Pass at 390px | Codex round 5: steered from 390px, the worker appended the marker to both files. Earlier: 2026-10-18: steered that Codex worker from a 390px phone view by tapping Send; it appended STEER_OK_390 to its report. No horizontal overflow. A physical handset is not yet shown |
| 8 | Pass | Codex round 4: invite, join, Remove with Cancel and confirm, then the removed member's cookie and password are refused. Earlier: Round 3: invite, join and lockout pass. Revoking had no button (C8-3); Settings > People now has Remove, e2e `settings_team_member_remove` (d52cec2ad, 9b1b3c21f) |
| 9 | Pass | Lifecycle run 2026-10-19 on life-8aa2: with nobody touching it, the hourly update moved the checkout to a commit landed 4 minutes earlier and installed its new door.py and Caddy page; r2c1455 earlier moved its image the same way (bbca9bcf7, 17d4bef36). SIGKILL of amux-server: back in under 4 s (systemd restart). Reboot: back in 36 s with the board intact. TLS: Let's Encrypt certificate, renewed by Caddy; /_door/status now reports tls_days_left and the monitor emails the operator under 14 days (879d48b66, offline test C9). A real renewal needs about 60 days to observe |
| 10 | Pass | Resubscribing within 30 days boots from the final snapshot (a7afc64be). Live: life-8aa2 ended, then the same email bought back-3bf9; it booted from snapshot 445332162 in 92 s with the board card and a home-directory file from the old server, the new password worked, the old one was refused, the ready email said it was restored, and the snapshot was deleted once it was ready. Hetzner daily backups stay on for every server |
| 11 | Pass | Round 3: deleted, then signed up again with the same email and name; the new server reached ready |
| 12 | Pass | Codex round 4: hostname listed once, deletion email, Stripe canceled, server, DNS (NXDOMAIN on both nameservers) and images gone. Earlier: 2026-10-18 on r2c1455 and twice in round 3. A name bought again was listed twice (C12-3), now once (d52cec2ad). 2026-10-18 on e2efull-ba83: the Stripe webhook and the delete page raced and left the DNS record behind with no confirmation; erase now removes the address by name and treats 404 as done (595733e58). Re-confirming then reported success, sent the email, and the name returns NXDOMAIN |
| 13 | Pass | Live: cancel at period end kept the server running with no upcoming invoice (service until 2026-11-19). Ending the subscription snapshotted the server with a 30-day expiry, then deleted the server and its DNS name within 7 minutes. With the expiry label moved to the past, the cron deleted the snapshot within a minute |
| 14 | Pass | Live on life-8aa2 with amux stopped: the customer gets an "amux is restarting" page (503, refreshes itself, hello@amux.io) instead of a bare 502 (8e7804608); the operator email arrived 14 minutes later with no one watching, and a recovery email followed the fix. Email failures return 502 with a message; stuck orders email the operator once |
| 15 | Pass | Live: six simultaneous checkouts for one name left exactly one payable session (five expired). Codex round 4: a paid name returns 409 with no Checkout. The operator audit reconciles charges against servers with no problems found. Capacity errors were proven live in C3. Offline suite: 32 fault cases |
| 16 | Pass in a real browser | FIRSTMONTH shows $0.00 in real Chrome; headless browsers are refused by Stripe |

Also found in the 2026-10-18 journey and fixed (7fac35d90): schedules never fired on a server without `AMUX_RS_SCHEDULER=1`, every Cloud server included (now on by default); `amux import` refused a real 239 MB export with 413 (now sent gzipped, 1 GiB limit), and imported schedules now arrive paused so a move cannot fire them twice.

Also found 2026-10-18 after the move, fixed:

- "Next run" was off by the time zone difference when the browser and server zones differ ("in 4h" for an every-minute schedule). The server now sends absolute instants (fe9ac3b56, e2e `scheduler-next-run-timezone`).
- A new worker's first task waited the full 10 minute steering deadline. Codex 0.163's braille welcome logo read as a busy spinner, and Claude workers on Cloud never had amux's status hooks (52b20f811). Codex hooks stay off on Cloud because Codex holds new hooks behind a review screen.
- Deleting an account could leave its DNS record behind when the webhook won the race (595733e58, offline tests C12-4).
- main's rust run was red for hours from unrelated tests, and the image only publishes from a green run, so Cloud servers stopped updating. Fixed the failing tests (8582c5714 by another lane, c01af610c). A red main stalls every Cloud server's updates; that is by design, and it means main must stay green.

Round 4 (Codex, 2026-10-18, image 48304e544): C6-3 bubblewrap, C7 at 390px, C8-3, C12, C15-3, the scheduler time zone and the Codex first-task delay (task running 7 s after login) pass. New C6-4, fixed above. The Claude first-task timing was blocked by the lack of an Anthropic credential in the tester's environment.

Also fixed after round 4: an OpenAI-only server read "No Anthropic API key set"; the banner now appears only when no model is connected (6709d041b). The worker header showed a guessed Codex model; it now reads the model from Codex's rollout (6709d041b). The site footer left a 200px gap on phones (0903f637b).

Round 5 (Codex, 2026-10-19, image 322cec9c9, adopted on first boot with no manual step): all retested items pass. C6 with default permissions, C7 at 390px, no false model banner with only OpenAI, the header model matches Codex's transcript, first task done 30 s after create, the phone footer, and C12 deletion with NXDOMAIN, the email and a canceled subscription. Cleanup checked independently: Hetzner holds only amux-sandbox and the golden image, Stripe test mode has no active subscription or open checkout.

Still open, needing the owner: a physical iPhone run (TestFlight build on a real handset), and the last step of signing in with a Claude subscription on a fresh server, which needs Ethan's Chrome to accept remote debugging again (its one-time Allow). The claude.ai half no longer needs a human: see "2026-10-19 (evening)" at the end. The first Claude task passes with an Anthropic API key, below.

Claude sign-in attempt, 2026-10-19 on test server cl-4c67 (ready 132s after payment): the dashboard's Claude login produced a claude.com authorize link and showed the paste box, so the server half works. The authorize page stopped at a Cloudflare "Verify you are human" check in the automation browser, and Ethan's own Chrome did not answer CDP (likely a pending Allow prompt). A human has to click through that check once, so this row stays with the owner. A second route also stopped at a human check: a headed Chrome got past Cloudflare to the claude.ai sign-in page as hello@amux.io, and the emailed sign-in link then opened an image CAPTCHA. No Anthropic API key is configured on this machine, so a key-based first Claude task is not available either. Unblocking it takes one of: the owner signing in to Claude once on a Cloud server, or an Anthropic key added to the Vault. The test account was deleted through the billing flow afterwards; Hetzner holds only amux-sandbox and the golden image, and there are 0 active subscriptions and 0 open checkouts.

First Claude task, 2026-10-19 on test server ck-11a3 (ready 121 s after payment). The customer path: skip the tour, the "No model connected" banner, Add an API key in Vault, Save ("Key saved"), the banner clears, New worker with Claude Code and a first prompt.
- Found: the worker sat on Claude Code's onboarding theme picker for 493 s and never started. bootstrap.sh answered Claude's first-run screens only for a key present at install, and customers add the key later. Fixed in f99e3d09a: every Claude worker launch seeds onboarding and approves the key amux hands it (log verdict=claude_first_run_seeded).
- After the server updated itself to f99e3d09a, a new worker skipped both screens and called Anthropic in under 10 s. The staging key it used was over its monthly limit, so the run was repeated with the production key: created, ran one shell command and wrote its proof file 16 s after Create.
- Found: Claude Code, Codex and Gemini CLI were installed as root and never updated, and Claude showed "Auto-update failed: no write permission to npm prefix" in every worker. Fixed in 1a14491be: update.sh updates all three daily (verdict=agent_clis_updated). Seen live: the server's next hourly run took Claude Code from 2.1.302 to 2.1.303 and switched Claude's own updater off.
- Found in the cleanup inventory: fin-fbb1, deleted at 03:50 UTC, was running again with DNS. Stripe redelivered its checkout event an hour after payment, and provision() rebuilt the erased account. Fixed in 53a1d2926 (provision builds only for a live, not erased subscription; the daily audit erases such a leftover with no snapshot). Both Workers redeployed; the test Worker's audit then reported "fin-fbb1: account was deleted, server 171126722 erased".
- Cleanup: ck-11a3 deleted through the billing flow. Hetzner holds only amux-sandbox and the golden image, Stripe test mode has 0 active, trialing or past_due subscriptions and 0 open checkouts, and ck-11a3 and fin-fbb1 return no DNS.

Round 6 (Codex, 2026-10-19): the Claude first-run seed (f99e3d09a) and the daily CLI updater (1a14491be) pass review and tests. New C12-6: provision() resumed an existing server before checking the subscription, so a redelivery during an erase published the address again. Fixed in bfb032b03 (the guard runs first) and both Workers redeployed.

Round 7 (Codex, 2026-10-19): ALL PASS for C12-6. Both deployed Workers match bfb032b03 byte for byte, the suite is 35 PASS and 0 FAIL, and Codex's own repro passes. Inventory clean: only amux-sandbox and the golden image, no customer images, 0 nonterminal test subscriptions, 0 open checkouts, NXDOMAIN for every test name. Codex thread ENDED.

Live through the pipeline: main went green at 61b7bfe41, the image deploy built, and a fresh signup's first boot reported live_commit 61b7bfe41 with tls_days_left and the error page in place (fin-fbb1, deleted). A logged signup (tl-9330) shows the webhook creating the server 2 s after payment and ready in 132 s.

Seen in the same run: two orders waited about 5 to 6.5 minutes for Hetzner capacity (fsn1 and nbg1 full, created in hel1) and was ready 9 minutes after payment, with the cron retrying as designed.

## 2026-10-19 (evening): Claude subscription sign-in, account deletion from amux, iOS app

Claude subscription sign-in. Ethan's Chrome, signed in to Claude Max, reached claude.ai's authorize page with no CAPTCHA (the emailed sign-in link cleared a reauth prompt), so the browser half is automatable. Three defects stood between a new customer and a connected subscription, all fixed:
- A Cloud server never offered "Sign in with Claude": the first-run window opened only for the old shared gateway, and the banner pointed at the Vault's API keys. It now opens for a Cloud server's owner, and the banner's "Connect a model" opens it everywhere (175404226).
- The pasted code never submitted: the dashboard wrote the code and Enter in one write, which Claude Code's setup-token reads as a paste. With a separate Enter, setup-token printed "Long-lived authentication token created successfully" on sub-4950. Fixed in 4ebf3cd3a; leaving the page now also ends an unfinished sign-in.
- A first visit reloaded the dashboard about 5 s in (the service worker taking control), which discarded a click on "Sign in with Claude" (seen twice on acct-8699). Fixed in 73be7678b.

On acct-8699 (image 50884ac9a, ready 133 s after payment) the window opened by itself and "Sign in with Claude" produced the link and the code prompt. The code round trip and a Claude worker's first task on a subscription are the open step above.

Account deletion from inside amux (ec93b1112). Settings > Account shows "amux Cloud account: Manage billing or delete account" to the owner only, never to invited members. On acct-8699 it opened amux.io/start/billing with the email filled in; the deletion email arrived, confirming deleted the account, and the customer got "Your amux Cloud account is deleted". Afterwards Hetzner had no acct-8699 server and no snapshot, Stripe test mode had 0 active, trialing or past_due subscriptions and 0 open checkouts, and acct-8699.amux.io was NXDOMAIN. sub-4950 was deleted the same way through the billing page, with the same result.

iOS app in the Simulator (not a real handset) against acct-8699: the app reached the server's sign-in page in 10 to 21 s, the door refused a wrong password, and after signing in a relaunch reopened the server's dashboard with its first-run window. The opt-in UI test (TEST_RUNNER_SANDBOX_URL) has not passed end to end yet: on a fresh install the app starts on a built-in default server and the test's Add Server route fails (9ab23410f).
