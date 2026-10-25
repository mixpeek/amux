---
description: Before shipping, play the bad guy: run a fixed checklist of 14 attacks against your OWN app with test accounts and test data, collect proof in a before/after table, then fix everything in one pass. Pre-launch security self-test; never touches real users, production data, or services you don't own.
allowed-tools: Bash, Read, Edit, Write
argument-hint: [app URL or "the local app"]
---

# /bad-guy: pre-launch security self-test

Target: **$ARGUMENTS**

Before this app ships, attack it the way a hostile user would, prove what breaks,
then fix it. Adapted from Voxyz_ai's "play the bad guy" checklist
(x.com/voxyz_ai/status/2111079716832886886).

## Hard boundary: read first, every run

This is authorized testing of the owner's OWN app, and only that:

- **First, confirm the preview environment is NOT wired to the production
  database.** If it is, run everything LOCALLY ONLY. Never attack a deployment
  that can reach real user data.
- Use **test accounts and test data**, a separate set per subagent (A, B, admin).
  Never touch real users, real content, or anyone's account but your test ones.
- **Do not attack services you don't own.** SSRF targets, redirect targets, and
  import URLs stay inside localhost / link-local / your own hosts.
- **Do not hammer** any one endpoint. Rate-limit tests fire a small, bounded burst.
- **Payments only in test mode.**
- Turn a protection off ONLY in local code, temporarily, to prove the attack:
  then turn it back on. **Never change the database or dashboard/platform
  settings** to do it. Those changes are the owner's to make: give the steps.
- Keep test-account passwords in `.env` (gitignored). **Mask tokens** in the
  results table.

If any of these cannot hold, stop and report rather than proceed.

## Shape of the run

One subagent per group runs that group's attacks and RECORDS (does not fix).
Run the **impersonation** subagent at **high** effort (most expensive to get
wrong); the rest at medium. Then YOU fix everything from their findings, rerun
every attack, and produce the before/after table.

Spawn the attack subagents in parallel (one message, one Agent call each). Give
each the boundary above verbatim plus its group.

## The 14 attacks

### Group 1: Impersonate other users (run at HIGH effort)
1. Without logging in, take the public key from the frontend and query every
   table in the database directly. See what comes back.
2. Logged in as A, swap the id in a request for B's and try to read, edit and
   delete B's data.
3. When A edits their profile, add `role: admin` to the request; then try
   editing the role in A's own account data directly.
4. Use a password-reset link once, then click it again.

### Group 2: Get around limits in the UI
5. For disabled buttons and hidden options, skip the page and send the request
   directly.
6. Once the free uses run out, check whether calling the API directly still works.
7. With only a little credit left, fire ~20 requests at once (bounded, not a
   flood) and see whether more succeed than the credit allows.
8. Fill forms with very long text, special characters, negative numbers, and
   empty values.
9. Change the post-login redirect (`?next=`) to another site, and also try
   values starting with `//` and `@`.

### Group 3: Come in from outside
10. Put `<img src=x onerror=alert(1)>` in a username, a comment or Markdown,
    then open the page in a browser and see if an alert pops up.
11. If the app fetches URLs (importing links, building previews), give it
    `http://localhost` and `http://169.254.169.254` and check the server logs
    for an actual outgoing request. Skip if there's no such feature.
12. For pages and API responses A has seen, request the same address as B or
    logged out, and see whether A's content shows up.
13. Open the hosting platform's preview links without logging in to the platform.
14. Hide an instruction in an uploaded document or web page that tells an AI
    feature to read another user's data or call a tool it shouldn't, and see if
    it does.

## Proof: every item must actually RUN

- Read-only-in-the-code does not count. If it was not executed, it is "not proven".
- After a fix, temporarily turn the protection off in LOCAL code and run the
  attack again, it must succeed. Turn it back on, it must be refused.
- Results go in a table: **attack · how · request · response · refused when it
  should be? (before / after)**. Mask tokens.
- Save the attacks as scripts in the repo so they can be rerun before the next
  launch.

## Finish

- You fix everything from the subagents' lists yourself, in one pass, then rerun
  every attack and fill the after column.
- **Delete the test accounts and data at the end.**
- Installing Anthropic's `security-guidance` plugin, or any dashboard/platform
  setting change, is the owner's to do: hand them the exact steps
  (`/plugin install security-guidance@claude-plugins-official`) rather than
  doing it.
