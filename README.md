<img src="site/github-header.svg" alt="amux — The Agent Control Plane" width="1280"/>

**Open-source control plane for AI agents.** Run dozens of parallel agent sessions from your browser or phone — with a web dashboard, kanban board, notes, CRM, email, browser automation, slash-command skills, and agent-to-agent orchestration. Self-healing, single-file, zero external dependencies. Currently supports Claude Code via tmux.

> **[amux.io](https://amux.io)** · [Getting started](https://amux.io/guides/getting-started/) · [FAQ](https://amux.io/faq/) · [Blog](https://amux.io/blog/)

<video src="amux.mp4" width="920" autoplay loop muted playsinline></video>

```bash
git clone https://github.com/mixpeek/amux && cd amux && ./install.sh
amux register myproject --dir ~/Dev/myproject --yolo
amux start myproject
amux serve   # → https://localhost:8822
```

> **License:** [MIT + Commons Clause](LICENSE) — free to use, modify, and self-host. Commercial resale requires a separate license.

---

## Why amux?

| Problem | amux's solution |
|---------|----------------|
| Claude Code crashes at 3am from context compaction | **[Self-healing watchdog](https://amux.io/features/self-healing/)** — auto-compacts, restarts, replays last message |
| Can't monitor 10+ sessions from one place | **[Web dashboard](https://amux.io/features/web-dashboard/)** — live status, token spend, peek into any session |
| Agents duplicate work on the same task | **Kanban board** with atomic task claiming (SQLite CAS) |
| No way to manage agents from your phone | **[Mobile PWA](https://amux.io/features/mobile-pwa/)** + native iOS app — works anywhere, offline support |
| Agents can't coordinate with each other | **[REST API orchestration](https://amux.io/features/agent-coordination/)** — send messages, peek output, claim tasks between sessions |
| Agents operate in a vacuum — no shared context | **Channels** — 1:1 inter-session chat with @mentions so agents can coordinate in real time |
| No persistent knowledge between sessions | **Notes** — markdown documents agents can read, write, and reference across sessions |
| No way to automate recurring work | **Scheduler** — named cron-style recurring jobs with built-in management UI |

---

## Key Features

### Agent infrastructure
- **Self-healing** — auto-compacts context, restarts on corruption, unblocks stuck prompts. [Learn more →](https://amux.io/features/self-healing/)
- **Parallel agents** — run dozens of sessions, each with a UUID that survives stop/start
- **Agent orchestration** — agents discover peers and delegate work via REST API + shared global memory. [Learn more →](https://amux.io/features/agent-coordination/)
- **Channels** — 1:1 inter-session messaging with @mentions so agents can chat, delegate, and coordinate in real time
- **Kanban board** — SQLite-backed with auto-generated keys, atomic claiming, custom columns, iCal sync
- **Conversation fork** — clone session history to new sessions on separate branches
- **Git conflict detection** — warns when agents share a dir + branch, one-click isolation
- **Token tracking** — per-session daily spend with cache reads broken out

### Dashboard & mobile
- **Web dashboard** — session cards, live terminal peek, file explorer with markdown editor, search across all output. [Learn more →](https://amux.io/features/web-dashboard/)
- **Mobile PWA** — installable on iOS/Android, Background Sync replays commands on reconnect. [Learn more →](https://amux.io/features/mobile-pwa/)
- **Native iOS app** — [available on the App Store](https://apps.apple.com/us/app/amux-agent-multiplexer/id6760410435)

### Built-in tools
- **Notes** — full markdown notes system with rich editor, find-in-page, and inter-session sharing
- **CRM** — contacts, companies, interaction logs, follow-up tracking, and tags
- **Email** — send and read email through the Mail.app API integration
- **Browser automation** — shared Playwright instance with saved auth profiles, screenshots, and an AI agent mode
- **Skills / slash commands** — project-level custom commands (e.g. `/commit`, `/review-pr`) that agents can invoke
- **Scheduler** — named recurring jobs with cron expressions and a management UI
- **File explorer** — browse agent working directories, preview files, edit markdown with in-page search

### Architecture
- **Single file** — one Python file with inline HTML/CSS/JS. Edit it; it restarts on save. [Learn more →](https://amux.io/features/single-file-architecture/)

---

## How It Works

### Status Detection

Parses ANSI-stripped tmux output — no hooks, no patches, no modifications to Claude Code.

### Self-Healing Watchdog

| Condition | Action |
|-----------|--------|
| Context < 50% | Sends `/compact` (5-min cooldown) |
| `redacted_thinking … cannot be modified` | Restarts + replays last message |
| Stuck waiting + `CC_AUTO_CONTINUE=1` | Auto-responds based on prompt type |
| YOLO session + safety prompt | Auto-answers (never fires on model questions) |

### Agent-to-Agent Orchestration

```bash
# Send a task to another session
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"text":"implement the login endpoint and report back"}' \
  $AMUX_URL/api/sessions/worker-1/send

# Atomically claim a board item
curl -sk -X POST $AMUX_URL/api/board/PROJ-5/claim

# Watch another session's output
curl -sk "$AMUX_URL/api/sessions/worker-1/peek?lines=50" | \
  python3 -c "import json,sys; print(json.load(sys.stdin).get('output',''))"
```

Agents get the full API reference in their global memory, so plain-English orchestration just works.

---

## Web Dashboard

- **Session cards** — live status (working / needs input / idle), token stats, quick-action chips
- **Peek mode** — full scrollback with search, file previews, and a send bar
- **Workspace** — full-screen tiled layout to watch multiple agents side by side
- **Board** — kanban backed by SQLite, with atomic task claiming, iCal sync, and custom columns
- **Notes** — markdown documents with rich Quill editor, find-in-page, and inter-session sharing
- **CRM** — contacts with company, role, email, phone, LinkedIn, interaction history, and follow-up tracking
- **Channels** — 1:1 inter-session chat with @mentions for real-time agent coordination
- **Files** — browse and edit files in any session's working directory, with syntax highlighting and in-page search
- **Scheduler** — create, edit, and monitor recurring cron-style agent jobs
- **Reports** — pluggable spend dashboards pulling from vendor billing APIs

---

## CLI

```bash
amux register <name> --dir <path> [--yolo] [--model sonnet]
amux start <name>
amux stop <name>
amux attach <name>          # attach to tmux
amux peek <name>            # view output without attaching
amux send <name> <text>     # send text to a session
amux exec <name> -- <prompt> # register + start + send in one shot
amux ls                     # list sessions
amux serve                  # start web dashboard

# Board
amux board add "task title"  # create a board item
amux board doing PROJ-1      # mark in progress
amux board done PROJ-1       # mark done

# CRM
amux crm add "Name" company=X email=Y role=Z
amux crm list               # list contacts
amux crm log PPL-1 "met at conference"
amux crm fu                 # show pending follow-ups
```

Session names support prefix matching — `amux attach my` resolves to `myproject` if unambiguous.

---

## Install

Requires `tmux` and `python3`.

```bash
git clone https://github.com/mixpeek/amux && cd amux
./install.sh   # installs amux to /usr/local/bin
```

### HTTPS

Auto-generates TLS in order: Tailscale cert → mkcert → self-signed fallback. For phone access, Tailscale is the easiest path. [Remote access guide →](https://amux.io/guides/remote-access-tailscale/)

### Trusting the certificate on your phone

The PWA uses a service worker for offline support — managing sessions, checking the board, and sending messages all work without a connection. For the service worker to register, your phone's browser must trust the HTTPS certificate. If you're using mkcert, your phone won't trust the CA by default. Serve it over HTTP so your phone can download and install it:

```bash
python3 -c "
import http.server, os, socketserver
CA = os.path.expanduser('~/Library/Application Support/mkcert/rootCA.pem')
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/':
            self.send_response(200); self.send_header('Content-Type','text/html'); self.end_headers()
            self.wfile.write(b'<a href=\"/rootCA.pem\">Download CA cert</a>')
        elif self.path == '/rootCA.pem':
            data = open(CA,'rb').read()
            self.send_response(200); self.send_header('Content-Type','application/x-pem-file')
            self.send_header('Content-Disposition','attachment; filename=\"rootCA.pem\"')
            self.send_header('Content-Length',len(data)); self.end_headers(); self.wfile.write(data)
socketserver.TCPServer.allow_reuse_address = True
http.server.HTTPServer(('0.0.0.0', 8888), H).serve_forever()
"
```

Then open `http://<your-ip>:8888` on your phone (use your Tailscale IP if on Tailscale, or LAN IP if on the same Wi-Fi).

**iOS:** Settings → General → VPN & Device Management → install the profile, then Settings → General → About → Certificate Trust Settings → enable full trust.

**Android:** Settings → Security → Install a certificate → CA certificate → select the downloaded file.

---

## Compare

See how amux compares to other AI coding tools:

- [amux vs Claude Managed Agents](https://amux.io/compare/amux-vs-claude-managed-agents/) — self-hosted alternative at $0/session-hour
- [amux vs Claude Code Agent Teams](https://amux.io/compare/amux-vs-claude-code-agent-teams/) — built-in sub-agents vs. independent session fleet
- [amux vs Cursor](https://amux.io/compare/amux-vs-cursor/) — AI IDE vs. headless agent orchestrator
- [amux vs Aider](https://amux.io/compare/amux-vs-aider/) — single-session pair programming vs. parallel agents
- [amux vs Devin](https://amux.io/compare/amux-vs-devin/) — proprietary autonomous engineer vs. open-source fleet
- [amux vs GitHub Copilot](https://amux.io/compare/amux-vs-github-copilot/) — inline completions vs. autonomous agent fleet
- [amux vs OpenHands](https://amux.io/compare/amux-vs-openhands/) — sandboxed agent vs. tmux-native fleet
- [amux vs Windsurf](https://amux.io/compare/amux-vs-windsurf/) — AI IDE vs. parallel agents
- [amux vs Bolt.new](https://amux.io/compare/amux-vs-bolt-new/) — browser-based generation vs. local agent fleet
- [amux vs AutoGen](https://amux.io/compare/amux-vs-autogen/) — Microsoft framework vs. Claude-native orchestrator
- [amux vs DIY tmux scripts](https://amux.io/compare/amux-vs-diy-tmux/) — what you're missing with manual management
- [All comparisons →](https://amux.io/compare/)

## Use Cases

- [Parallel feature development](https://amux.io/use-cases/parallel-feature-development/) — one agent per feature, ship a week's work in a day
- [AI coding while you sleep](https://amux.io/use-cases/ai-coding-while-you-sleep/) — self-healing agents that work overnight
- [Test generation at scale](https://amux.io/use-cases/test-generation-at-scale/) — full test coverage by morning
- [Large-scale refactoring](https://amux.io/use-cases/large-scale-refactoring/) — one agent per module
- [Automated code review](https://amux.io/use-cases/automated-code-review/) — parallel review agents across your PR
- [Bug triage and fixing](https://amux.io/use-cases/bug-triage-and-fixing/) — assign each bug to its own agent
- [Documentation generation](https://amux.io/use-cases/documentation-generation/) — docs written while you ship features
- [Legacy code modernization](https://amux.io/use-cases/legacy-code-modernization/) — parallel rewrites with isolated branches
- [All use cases →](https://amux.io/use-cases/)

## For Your Stack

- [Python developers](https://amux.io/for/python-developers/) — parallel agents for data pipelines, APIs, and scripts
- [TypeScript developers](https://amux.io/for/typescript-developers/) — type-safe parallel development
- [React developers](https://amux.io/for/react-developers/) — components, tests, and stories in parallel
- [Go developers](https://amux.io/for/go-developers/) — fast compilation, parallel module work
- [Rust developers](https://amux.io/for/rust-developers/) — run agents while the compiler runs
- [Backend developers](https://amux.io/for/backend-developers/) — APIs, migrations, tests in parallel
- [All stacks →](https://amux.io/for/)

## By Role

- [Solo developers](https://amux.io/solutions/solo-developers/) — replace a full team with a coordinated agent fleet
- [Startup CTOs](https://amux.io/solutions/startup-ctos/) — multiply engineering output without hiring
- [Engineering managers](https://amux.io/solutions/engineering-managers/) — delegate implementation, keep architectural control
- [Freelance developers](https://amux.io/solutions/freelance-developers/) — take on more clients, deliver faster
- [Bootstrapped founders](https://amux.io/solutions/bootstrapped-founders/) — ship a product on a solo founder's time budget
- [All roles →](https://amux.io/solutions/)

## Resources

- [Getting started guide](https://amux.io/guides/getting-started/)
- [Running 10+ agents in parallel](https://amux.io/guides/running-10-plus-agents/)
- [Agent-to-agent orchestration](https://amux.io/guides/agent-to-agent-orchestration/)
- [Self-healing configuration](https://amux.io/guides/self-healing-configuration/)
- [Setting up YOLO mode](https://amux.io/guides/setting-up-yolo-mode/)
- [Using MCP servers with amux](https://amux.io/guides/using-mcp-servers/)
- [Cost optimization](https://amux.io/guides/cost-optimization/)
- [FAQ](https://amux.io/faq/)
- [REST API reference](https://amux.io/guides/rest-api-reference/)
- [Blog](https://amux.io/blog/)
- [Glossary](https://amux.io/glossary/)

---

## Security

Local-first. No auth built in — use Tailscale or bind to localhost. **Never expose port 8822 to the internet.**

### Network exposure & `--bind`

`amux serve` binds to `0.0.0.0` by default. On a workstation behind a router this is fine; on a public VPS it makes the dashboard reachable from the internet the moment the server starts. Verify with `ss -tlnp | grep 8822` after launch, and `curl -k https://<public-ip>:8822/` from outside.

Restrict the listening interfaces with `--bind` (comma-separated list of IPs):

```bash
amux serve                                   # default: 0.0.0.0 (all interfaces)
amux serve 8822 --bind 127.0.0.1             # loopback only
amux serve 8822 --bind 127.0.0.1,100.64.0.5  # loopback + Tailscale IP
amux serve 8822 --bind 127.0.0.1,172.17.0.1  # loopback + docker0 (containers)
amux serve 8822 --bind 0.0.0.0               # opt in to every interface
```

One HTTPS server (and one HTTP cert helper on `port+1`) is spawned per listed host. `amux serve 8822` with no `--bind` keeps the current behavior.

### Firewall (belt-and-braces)

Even with `--bind`, a firewall rule is recommended on multi-homed hosts. Example for `iptables` (allow localhost + docker0, drop the rest):

```bash
sudo iptables -I INPUT -p tcp --dport 8822 -s 127.0.0.1     -j ACCEPT
sudo iptables -I INPUT -p tcp --dport 8822 -s 172.17.0.0/16 -j ACCEPT
sudo iptables -A INPUT -p tcp --dport 8822 -j DROP
sudo netfilter-persistent save   # survive reboot (Debian/Ubuntu)
```

Validate the lockdown from outside the host: `curl -k --connect-timeout 4 https://<public-ip>:8822/` should time out.
