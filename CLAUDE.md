# amux

Single-file project: everything lives in `amux-server.py` (Python server + inline HTML/CSS/JS dashboard).

## Structure

- `amux-server.py` — the server + dashboard (single file)
- `mcp.json` — centralized MCP server config (shared by local and cloud)
- `cloud/` — GCP VM provisioning (Terraform + setup script)

## Workflow

- **Commit after every completed task.** When you finish a piece of work (bug fix, feature, refactor), immediately `git add amux-server.py && git commit` with a concise message. Don't batch multiple tasks into one commit.
- The server auto-restarts on file save (watches its own mtime), so changes are live immediately.
- Always verify Python syntax after edits: `python3 -c "import ast; ast.parse(open('amux-server.py').read())"`

## Deploy

When the user says **"deploy"**, run the full pipeline:
1. `git add` changed files (typically `amux-server.py`)
2. `git commit` with a concise message
3. `git push origin main`

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

**Google caches ICS feeds by URL, hard.** There is no reliable way to force a refresh — Google refetches on its own cadence (hours). If you edit the feed's *content shape* and need Google to see it now, publish to a NEW random key and re-subscribe; the old URL keeps serving Google's stale cache. `AMUX_S3_KEY` is read at startup via `os.environ.setdefault`, and execv reloads inherit the env, so changing it needs a real restart: `launchctl kickstart -k gui/$(id -u)/com.amux.server`.

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
