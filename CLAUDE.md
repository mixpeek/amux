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

## Server config — `~/.amux/server.env`

Persistent env vars for the server. Loaded at startup via `os.environ.setdefault` so process-level env always wins. Survives `os.execv` auto-restarts.

Example `~/.amux/server.env`:
```
AMUX_S3_BUCKET=ethan-personal
AMUX_S3_KEY=amux/calendar.ics
AMUX_S3_REGION=us-east-2
```

After creating/editing server.env, `touch amux-server.py` to trigger a reload.

## iCal / Google Calendar sync

Board items with `due` dates are exported as an iCal feed:
- Local: `GET /api/calendar.ics`
- Public S3 (for Google/Apple Calendar subscriptions): set `AMUX_S3_BUCKET` in `server.env`

S3 bucket config (one-time, already done on `ethan-personal`):
- Public access block: `BlockPublicAcls=true, IgnorePublicAcls=true, BlockPublicPolicy=false, RestrictPublicBuckets=false`
- Bucket policy grants `s3:GetObject` on `arn:aws:s3:::ethan-personal/amux/calendar.ics` only
- Public URL: `https://ethan-personal.s3.us-east-2.amazonaws.com/amux/calendar.ics`

The feed auto-uploads to S3 on every board write (POST/PATCH/DELETE). The dashboard's calendar subscription button shows the S3 URL directly when configured.

## Browser Automation

Always use the `claude-in-chrome` MCP server for browser tasks (tools: `mcp__claude-in-chrome__*`). Do not use Playwright or any other browser MCP unless explicitly asked. claude-in-chrome connects directly to the user's Chrome browser and preserves session state across navigations.

Claude Code, the amux server, and Chrome all run on the same desktop machine. Use `https://localhost:8822` for all `mcp__claude-in-chrome__navigate` calls to the amux dashboard.
