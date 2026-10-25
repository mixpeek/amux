# amux Cloud launch posts (drafts for the owner)

2026-10-21. Drafts only: the owner posts them. Why now: Reddit sent 55 visitors and
Hacker News 7 in the last 30 days, while one roundup guide alone sent 282. A launch
post is the cheapest way to put Cloud in front of people who already run coding agents.

Facts checked against the code on 2026-10-21: plans from `cloud/control/plans.json`,
the move from `amux export` / `amux import --to`, deletion from `cloud/start/worker.mjs`.

## 1. Show HN

**Title:** Show HN: amux Cloud, your own always-on server for Claude Code and Codex agents

**Text:**

I built amux to run a dozen Claude Code and Codex sessions at once from one dashboard,
including from my phone. It is open source (Rust binary plus SQLite) and most people
self-host it.

The recurring complaint was that agents stop when the laptop sleeps. amux Cloud gives
you a dedicated server at yourname.amux.io with amux already running:

- One server per customer (Hetzner, single tenant). Your sessions, files and keys stay
  on your box.
- Bring your own Anthropic or OpenAI key, or sign in with a Claude subscription.
- Steer it from the browser, the PWA or the iOS app.
- Move a local setup over with `amux export` and `amux import --to`.
- Daily backups. Deleting the account erases the server, its DNS name, backups and
  snapshots right away.

Plans are $39, $69 and $129 a month (4, 8 and 16 vCPU). Signup takes about two
minutes and the server is usually ready two minutes after payment.

Self-hosting stays free and gets the same code: there is one codebase for local and
cloud.

Site: https://amux.io/start/?source=hn
Code: https://github.com/mixpeek/amux

Happy to answer questions about running long agent jobs unattended, and about what
broke on the way.

## 2. Reddit, r/ClaudeAI (and r/ClaudeCode)

**Title:** I run my Claude Code agents on a $39 server so they keep going when my laptop sleeps

**Text:**

I have been running 10+ Claude Code sessions in parallel for months with amux, an open
source dashboard I wrote (board, scheduler, phone control, self-restarting sessions).

The weak spot was always my laptop: close the lid and every agent stops. So amux now
has a hosted option: a dedicated server at yourname.amux.io with amux on it. You sign
in with your Claude subscription or paste an API key, and you steer it from your phone.

- One server per person, not shared.
- `amux export` on your machine, `amux import --to` on the server, and your workers,
  board and schedules come along.
- From $39 a month; delete it from Settings and everything is erased.

Self-hosting is still free: https://github.com/mixpeek/amux
Cloud: https://amux.io/start/?source=reddit

What do you use to keep agents running overnight?

## Notes for posting

- Post Show HN on a weekday morning US time; reply to every comment in the first two hours.
- The FIRSTMONTH code gives a first month free. Offering it in the post would lift
  signups but loses about $15 per customer in month one; owner's call.
- Check Reddit's self-promotion rules for each subreddit before posting.
- Track results with `scripts/site-funnel.py --days 7` (the `source` parameter shows as
  the first-touch source).
