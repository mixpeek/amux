# amux

Single-file project: everything lives in `amux-server.py` (Python server + inline HTML/CSS/JS dashboard).

## The ethos — read before building anything

**The harness gets better as the models get better. Get out of the model's way.**

Every new feature or enhancement gets gut-checked against
[`.claude/rules/ethos.md`](.claude/rules/ethos.md) before you build it and again before
you call it done. Eight questions, each one there because it was violated in this repo
and cost something real: capability that never reached a session, model calls spent on
string manipulation, gates that could not be satisfied honestly, instruments that could
not express the discriminator, automation that accumulated instead of deciding, an audit
trail that was claimed but not implemented, checks that could not fail, and an agent
deciding something that was the user's to decide.

The question underneath all of them: when the next model is meaningfully better than
this one, does this feature get better with it, or does it become the ceiling?

The bottom of that file is a **Known deviations** ledger — live places where amux
still fights the ethos, each with a status and an exit condition. Check it before
touching state detection, auto-answers, helper-model calls, observation caps, or
auto-compact; move the deviation toward its exit rather than deepening it.

## You are dogfooding — fix it at the root

You run *inside* amux. Every rough edge you hit while working is a rough edge a user
hits, and you are the one person positioned to see it from the inside. So when
something in amux gets in your way — a peek that hides output, a board mutation that
bounces silently, a browser profile that saves to one place and loads from another, an
error message that sends you chasing the wrong cause — **treat it as a product defect
and fix it at its root**, not as an obstacle to route around in your own task.

- Do not paper over it with a workaround in your script, a manual step, or a note to
  yourself. If you needed the workaround, so will a user who has no idea it exists.
- Fix the source, not the symptom: the generator rather than the generated file, the
  API rather than the one caller, the instrument that could not express the failure
  rather than the one conclusion it misled you into.
- Instrumentation counts as product. If a failure was hard to diagnose because output
  was truncated, logs went to `/dev/null`, or a check could not fail, that is itself the
  bug worth fixing — the next person debugging it is a user.
- Then leave the comment explaining *why*, so the fix does not get undone by someone who
  only sees the shape of the code.

The bar: after you are done, could someone hit the same problem again? If yes, you fixed
your task, not the platform.

## Structure

- `amux-server.py` — the server + dashboard (single file)
- `mcp.json` — centralized MCP server config (shared by local and cloud)
- `cloud/` — GCP VM provisioning (Terraform + setup script)

## Workflow

- **Commit after every completed task.** When you finish a piece of work (bug fix, feature, refactor), immediately `git add amux-server.py && git commit` with a concise message. Don't batch multiple tasks into one commit.
- The server auto-restarts on file save (watches its own mtime) — but **it watches the
  file it is RUNNING, which is `~/.local/bin/amux-server.py`, not your repo checkout.**
  Editing `~/amux/amux-server.py` alone changes nothing that is live: `ps` the process
  and you will see it running out of `~/.local/bin`. To see a change on
  `https://localhost:8822`, install it, then confirm it actually took:
  ```bash
  cp amux-server.py ~/.local/bin/amux-server.py
  launchctl kickstart -k gui/$(id -u)/com.amux.serve   # label is com.amux.serve
  curl -sk https://localhost:8822/ | grep -c '<a line unique to your change>'
  ```
  Verify with a string your edit INTRODUCED, not one that already existed — grepping a
  common idiom returns a happy non-zero count against the old build and tells you
  nothing (this cost a wrong "it's live" call on AMUX-4).
- Always verify Python syntax after edits: `python3 -c "import ast; ast.parse(open('amux-server.py').read())"`
- **Client JS changes need `APP_VER` and the sw.js `CACHE` bumped together**, or a
  browser holding the cached script never receives the fix.
- Always verify Python syntax after edits: `python3 -c "import ast; ast.parse(open('amux-server.py').read())"`

## Deploy

⚠ **BEFORE `git push origin main`: check what you are shipping that is not yours.** This is a
SHARED checkout — other sessions commit here — and `.github/workflows/deploy-cloud.yml` triggers on
`push: branches:[main], paths:['amux-server.py']` (as does `cloud-image.yml`). **So any push of
main deploys EVERY unpushed commit touching that file, including other sessions' work that nobody
reviewed at that moment.** Always run first:

```bash
git rev-list --count origin/main..main          # how many commits am I about to ship?
git log --oneline origin/main..main             # whose are they?
```

If commits you did not write are listed, ask their author before pushing. "My change is small" is
not the question; the question is what rides along with it.

**The mirror case is just as real: "my commits are not pushed" does NOT mean they are inert.** On a
shared checkout they are staged to ship under someone else's push, at a time you do not choose. A
session's unpushed commit was replayed onto origin by a peer's `git pull --rebase` on 2026-08-03 —
a third party's push shipped a commit its author never pushed. You control neither *when* your work
ships nor *whether* it does.

When the user says **"deploy"**, run the full pipeline:
1. `git add` changed files (typically `amux-server.py`)
2. `git commit` with a concise message
3. Run the two checks above
4. `git push origin main`

## Single-codebase rule (CRITICAL)

**`amux-server.py` is identical for both local (OSS) and cloud deployments — no exceptions.**

- Never add cloud-only or OSS-only code branches (no `if IS_CLOUD`, no `if os.environ.get('CLOUD')`).
- Features that differ between environments must be driven by headers/env vars injected by the gateway (e.g., `X-Amux-User-Email`) or by presence/absence of configuration, not by build-time flags.
- `cloud/docker/amux-server.py` must never be committed — it is auto-generated during deploy. It is in `.gitignore`.

## Server config — `~/.amux/server.env`

Persistent env vars for the server. Loaded at startup via `os.environ.setdefault` so process-level env always wins. Survives `os.execv` auto-restarts.

Example `~/.amux/server.env`:
```
AMUX_S3_BUCKET=ethan-personal
AMUX_S3_KEY=amux/cal-<random-token>.ics
AMUX_S3_REGION=us-east-2
```

After creating/editing server.env, `touch amux-server.py` to trigger a reload.

## iCal / Google Calendar sync

The iCal feed exports amux **calendar events** only (`cal_events` table) — NOT
schedules or board issues. Those are in-app-only calendar layers. Timed events are
emitted as UTC (`DTSTART:...Z`) so Google shows the correct local time; all-day use
`VALUE=DATE`.
- Local: `GET /api/calendar.ics`
- Public S3 (for Google/Apple Calendar subscriptions): set `AMUX_S3_BUCKET` in `server.env`

S3 bucket config (on `ethan-personal`):
- Public access block: `BlockPublicAcls=true, IgnorePublicAcls=true, BlockPublicPolicy=false, RestrictPublicBuckets=false`
- Bucket policy grants public `s3:GetObject` on `arn:aws:s3:::ethan-personal/amux/*.ics` (widened from the single `calendar.ics` key so cache-busting keys work). Bucket LISTING is denied (403), so a random key is not discoverable.
- Current key: a **random token** (`amux/cal-<32hex>.ics`) that lives ONLY in `~/.amux/server.env` — **NEVER commit the actual key/URL to this repo: the repo is public, and a committed feed URL is how the old guessable key leaked.** Read it with `grep AMUX_S3_KEY ~/.amux/server.env`; the dashboard's Subscribe button shows the full URL.

**Google caches ICS feeds by URL, hard.** There is no reliable way to force a refresh — Google refetches on its own cadence (hours). If you edit the feed's *content shape* and need Google to see it now, publish to a NEW random key and re-subscribe; the old URL keeps serving Google's stale cache. `AMUX_S3_KEY` is read at startup via `os.environ.setdefault`, and execv reloads inherit the env, so changing it needs a real restart: `launchctl kickstart -k gui/$(id -u)/com.amux.serve`.

The feed auto-uploads to S3 on every event write. The dashboard's calendar subscription button shows the S3 URL directly when configured.

## Browser Automation

Use `/chrome-cdp` for browser tasks. It connects directly to the user's live Chrome via CDP — real tabs, real cookies, no fresh browser.

```bash
node skills/chrome-cdp/scripts/cdp.mjs list           # list open tabs
node skills/chrome-cdp/scripts/cdp.mjs snap <target>   # accessibility tree
node skills/chrome-cdp/scripts/cdp.mjs shot <target>   # screenshot
node skills/chrome-cdp/scripts/cdp.mjs click <target> <selector>
node skills/chrome-cdp/scripts/cdp.mjs type <target> <text>
node skills/chrome-cdp/scripts/cdp.mjs eval <target> <js>
node skills/chrome-cdp/scripts/cdp.mjs nav <target> <url>
```

Requires Chrome remote debugging enabled (`chrome://inspect/#remote-debugging`) and Node.js 22+.

Claude Code, the amux server, and Chrome all run on the same desktop machine. Use `https://localhost:8822` for amux dashboard URLs.
