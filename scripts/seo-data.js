// SEO page data for amux.io — all 100 pages
// Each entry: { slug, title, description, keywords, content (HTML), schema? }

const INSTALL = `<pre><code>git clone https://github.com/mixpeek/amux &amp;&amp; cd amux &amp;&amp; ./install.sh
amux register myproject --dir ~/Dev/myproject --yolo
amux start myproject
amux serve  # → https://localhost:8822</code></pre>`;

const CTA = `<div class="cta-box"><h3>Get started with amux</h3><p>Run dozens of Claude Code agents in parallel. Python 3 + tmux. Open source.</p>${INSTALL}<a class="btn btn-primary" href="https://github.com/mixpeek/amux">View on GitHub</a></div>`;

function comparisonTable(rows) {
  return `<table><thead><tr><th>Feature</th><th>amux</th><th>${rows[0][0]}</th></tr></thead><tbody>${rows.map(r => `<tr><td>${r[1]}</td><td>${r[2]}</td><td>${r[3]}</td></tr>`).join('')}</tbody></table>`;
}

// ═══════════════════════════════════════════════════════════════════
// COMPARISONS (15 pages)
// ═══════════════════════════════════════════════════════════════════
const comparisons = [
{
  slug: 'amux-vs-claude-code-agent-teams',
  title: 'amux vs Claude Code Agent Teams',
  description: 'Compare amux with Anthropic\'s built-in Agent Teams feature for running parallel Claude Code sessions.',
  keywords: 'amux vs agent teams, claude code agent teams, parallel claude code, multi-agent claude',
  content: `<h1>amux vs Claude Code Agent Teams</h1>
<p class="subtitle">Both run multiple Claude Code sessions in parallel. Here's how they differ.</p>
<p>Anthropic's <strong>Agent Teams</strong> is an experimental built-in feature that lets Claude Code spawn sub-agents. amux takes a different approach: it manages independent tmux sessions with a web dashboard, self-healing watchdog, and shared kanban board.</p>
<h2>Key differences</h2>
${comparisonTable([
['Agent Teams','Parallel sessions','Unlimited independent sessions','Sub-agents spawned by parent'],
['Agent Teams','Self-healing','Auto-compact, restart, unstick','Relies on parent session'],
['Agent Teams','Web dashboard','Full UI with peek, board, calendar','CLI only'],
['Agent Teams','Mobile access','PWA with offline support','No mobile support'],
['Agent Teams','Task coordination','SQLite kanban with atomic claiming','Parent delegates to children'],
['Agent Teams','Session persistence','Survives restarts with UUID tracking','Tied to parent lifecycle'],
['Agent Teams','Git isolation','Conflict detection + branch helpers','Git worktree per agent'],
['Agent Teams','Token tracking','Per-session daily spend dashboards','Aggregated in parent'],
])}
<h2>When to use Agent Teams</h2>
<p>Agent Teams works well for <strong>short-lived, tightly-coupled tasks</strong> where a parent agent needs to fan out work and collect results. It requires no setup beyond Claude Code itself.</p>
<h2>When to use amux</h2>
<p>amux is built for <strong>long-running, independent agents</strong> that work on separate concerns. The web dashboard, self-healing, and mobile access make it ideal for running agents overnight or managing a fleet of workers across multiple projects.</p>
<ul>
<li>You want agents that survive context compaction crashes</li>
<li>You need a visual dashboard to monitor 10+ sessions</li>
<li>You want to manage agents from your phone</li>
<li>You need a shared task board with atomic claiming</li>
</ul>
${CTA}`,
  schema: { '@context':'https://schema.org', '@type':'Article', headline:'amux vs Claude Code Agent Teams', description:'Compare amux with Anthropic Agent Teams for parallel Claude Code sessions' }
},
{
  slug: 'amux-vs-cursor',
  title: 'amux vs Cursor',
  description: 'Compare amux\'s parallel agent orchestration with Cursor\'s AI-powered IDE for coding productivity.',
  keywords: 'amux vs cursor, cursor alternative, claude code vs cursor, AI coding IDE comparison',
  content: `<h1>amux vs Cursor</h1>
<p class="subtitle">Different tools for different workflows. One is an IDE, the other is an agent multiplexer.</p>
<p><strong>Cursor</strong> is an AI-powered IDE (VS Code fork) with inline completions, chat, and an agent mode. <strong>amux</strong> is a headless agent orchestrator that runs dozens of Claude Code sessions in parallel from a web dashboard.</p>
<h2>Key differences</h2>
${comparisonTable([
['Cursor','Architecture','Headless CLI agents + web dashboard','GUI IDE with AI features'],
['Cursor','Parallel agents','Dozens of independent sessions','Single agent per window'],
['Cursor','Unattended operation','Self-healing watchdog, runs overnight','Requires IDE to be open'],
['Cursor','Mobile management','PWA on iOS/Android','Desktop only'],
['Cursor','Task coordination','Shared kanban board + REST API','No built-in coordination'],
['Cursor','Agent model','Claude Code (any model)','Proprietary + Claude/GPT'],
['Cursor','Cost model','Pay per API token','$20-40/mo subscription'],
['Cursor','Open source','Yes (MIT)','No'],
])}
<h2>They complement each other</h2>
<p>Many developers use Cursor for <strong>interactive editing</strong> (inline completions, quick fixes) and amux for <strong>batch operations</strong> (running 10 agents overnight to generate tests, refactor modules, or fix bugs across a monorepo). You don't have to choose one.</p>
${CTA}`
},
{
  slug: 'amux-vs-aider',
  title: 'amux vs Aider',
  description: 'Compare amux with Aider, the popular open-source AI pair programming tool, for parallel development.',
  keywords: 'amux vs aider, aider alternative, AI pair programming, claude code multiplexer',
  content: `<h1>amux vs Aider</h1>
<p class="subtitle">Both are CLI-first. amux adds orchestration, self-healing, and a web dashboard.</p>
<p><strong>Aider</strong> is an excellent open-source AI pair programming tool that works with many LLM providers. <strong>amux</strong> orchestrates multiple Claude Code sessions with monitoring, self-healing, and a shared task board.</p>
<h2>Key differences</h2>
${comparisonTable([
['Aider','Focus','Multi-agent orchestration','Single-session pair programming'],
['Aider','Parallel sessions','Dozens with shared coordination','One per terminal'],
['Aider','Self-healing','Auto-compact, restart, unstick','Manual restart on errors'],
['Aider','Web dashboard','Full UI with live peek','CLI only'],
['Aider','LLM support','Claude Code (Opus, Sonnet, Haiku)','Many providers (OpenAI, Claude, etc.)'],
['Aider','Task board','SQLite kanban with iCal sync','No built-in task management'],
['Aider','Git integration','Conflict detection across agents','Auto-commit per change'],
])}
<h2>When to use Aider</h2>
<p>Aider excels at <strong>interactive pair programming</strong> with a single agent. Its multi-provider support is great if you switch between models frequently.</p>
<h2>When to use amux</h2>
<p>amux is the right choice when you need to <strong>scale to many parallel agents</strong>, monitor them from a dashboard, and coordinate work across sessions.</p>
${CTA}`
},
{
  slug: 'amux-vs-devin',
  title: 'amux vs Devin',
  description: 'Compare amux\'s open-source agent multiplexer with Devin, the autonomous AI software engineer.',
  keywords: 'amux vs devin, devin alternative, autonomous AI developer, open source devin',
  content: `<h1>amux vs Devin</h1>
<p class="subtitle">Open-source agent fleet vs. proprietary autonomous engineer.</p>
<p><strong>Devin</strong> by Cognition is a proprietary autonomous AI software engineer with its own sandboxed environment. <strong>amux</strong> runs Claude Code agents on your own machine with full access to your codebase, tools, and environment.</p>
<h2>Key differences</h2>
${comparisonTable([
['Devin','Hosting','Your machine (local-first)','Cloud sandbox'],
['Devin','Agent count','Unlimited parallel sessions','One agent per task'],
['Devin','Environment','Full access to your tools/env','Isolated sandbox'],
['Devin','Cost','Pay per Claude API token','$500/mo subscription'],
['Devin','Open source','Yes (MIT)','No'],
['Devin','Self-healing','Auto-compact, restart, unstick','Proprietary recovery'],
['Devin','Customization','Full control over session config','Limited configuration'],
])}
<h2>The trade-off</h2>
<p>Devin offers a polished, fully-managed experience but at a high price point with limited customization. amux gives you <strong>full control</strong> over your agents, runs on your own infrastructure, and costs only what you pay for Claude API tokens.</p>
${CTA}`
},
{
  slug: 'amux-vs-github-copilot',
  title: 'amux vs GitHub Copilot',
  description: 'Compare amux parallel agent orchestration with GitHub Copilot for AI-assisted development.',
  keywords: 'amux vs github copilot, copilot alternative, parallel AI coding, claude code vs copilot',
  content: `<h1>amux vs GitHub Copilot</h1>
<p class="subtitle">Copilot assists one developer. amux runs a fleet of agents.</p>
<p><strong>GitHub Copilot</strong> provides AI completions and chat inside your IDE. <strong>amux</strong> runs independent Claude Code agents that work on entire tasks autonomously, in parallel, while you sleep.</p>
<h2>Key differences</h2>
${comparisonTable([
['Copilot','Paradigm','Autonomous agents working in parallel','IDE assistant (completions + chat)'],
['Copilot','Autonomy','Fully autonomous with self-healing','Requires developer in the loop'],
['Copilot','Parallel tasks','Dozens of simultaneous agents','One suggestion at a time'],
['Copilot','Unattended','Runs overnight, recovers from crashes','Requires IDE open'],
['Copilot','Task coordination','Kanban board with atomic claiming','No coordination features'],
['Copilot','Price','Pay per API token','$10-39/mo subscription'],
])}
<h2>Use them together</h2>
<p>Copilot handles <strong>inline completions</strong> while you code. amux handles <strong>batch autonomous work</strong> — running 20 agents to generate tests, fix lint errors, or refactor modules. They solve different problems.</p>
${CTA}`
},
{
  slug: 'amux-vs-windsurf',
  title: 'amux vs Windsurf',
  description: 'Compare amux with Windsurf (formerly Codeium) for AI coding productivity.',
  keywords: 'amux vs windsurf, windsurf alternative, codeium vs claude code, AI coding tools',
  content: `<h1>amux vs Windsurf</h1>
<p class="subtitle">Windsurf is an AI IDE. amux is a headless agent fleet manager.</p>
<p><strong>Windsurf</strong> (formerly Codeium) is an AI-powered IDE with Cascade, its agentic coding flow. <strong>amux</strong> manages dozens of Claude Code sessions with a web dashboard and self-healing.</p>
<h2>Key differences</h2>
${comparisonTable([
['Windsurf','Interface','Web dashboard + CLI','Desktop IDE'],
['Windsurf','Parallel agents','Unlimited headless sessions','Single Cascade flow'],
['Windsurf','Unattended mode','Self-healing, runs overnight','Requires IDE open'],
['Windsurf','Mobile','PWA on iOS/Android','Desktop only'],
['Windsurf','Model','Claude Code (any Anthropic model)','Multiple providers'],
['Windsurf','Price','Pay per API token','$15/mo subscription'],
['Windsurf','Open source','Yes (MIT)','No'],
])}
${CTA}`
},
{
  slug: 'amux-vs-openhands',
  title: 'amux vs OpenHands',
  description: 'Compare amux with OpenHands (formerly OpenDevin), the open-source autonomous AI developer.',
  keywords: 'amux vs openhands, opendevin alternative, open source AI developer, autonomous coding',
  content: `<h1>amux vs OpenHands</h1>
<p class="subtitle">Both are open source. Different architectures for different needs.</p>
<p><strong>OpenHands</strong> (formerly OpenDevin) is an open-source autonomous AI developer with a sandboxed Docker environment. <strong>amux</strong> orchestrates Claude Code agents directly on your machine.</p>
<h2>Key differences</h2>
${comparisonTable([
['OpenHands','Environment','Direct machine access','Docker sandbox'],
['OpenHands','Agent count','Unlimited parallel tmux sessions','Typically single agent'],
['OpenHands','Self-healing','Auto-compact, restart, unstick','Relies on sandbox isolation'],
['OpenHands','Dashboard','Built-in web UI with peek/board','Web UI for single agent'],
['OpenHands','Dependencies','Python 3 + tmux only','Docker required'],
['OpenHands','Model lock-in','Claude Code specific','Multi-provider'],
])}
<h2>When to choose amux</h2>
<p>If you're already using Claude Code and want to <strong>scale to many parallel agents</strong> with self-healing and coordination, amux is purpose-built for that. OpenHands is better if you need provider flexibility or sandboxed execution.</p>
${CTA}`
},
{
  slug: 'amux-vs-diy-tmux',
  title: 'amux vs DIY tmux Scripts',
  description: 'Why use amux instead of rolling your own tmux-based Claude Code management scripts?',
  keywords: 'amux vs tmux scripts, claude code tmux, manage claude code sessions, tmux automation',
  content: `<h1>amux vs DIY tmux Scripts</h1>
<p class="subtitle">You can manage Claude Code in tmux manually. Here's why amux exists anyway.</p>
<p>Many developers start by writing bash scripts to manage Claude Code in tmux. It works — until you need self-healing, a dashboard, or coordination between agents.</p>
<h2>What you get with DIY scripts</h2>
<ul>
<li><code>tmux new-session -d -s agent1 "claude"</code> — starts a session</li>
<li><code>tmux send-keys -t agent1 "fix the login bug" Enter</code> — sends a prompt</li>
<li><code>tmux capture-pane -t agent1 -p</code> — reads output</li>
</ul>
<h2>What you're missing</h2>
${comparisonTable([
['DIY tmux','Self-healing','Auto-compact + restart + unstick','Manual monitoring'],
['DIY tmux','Dashboard','Web UI with live peek, search, files','Terminal only'],
['DIY tmux','Mobile access','PWA on any device','SSH + tmux attach'],
['DIY tmux','Task board','SQLite kanban with atomic claiming','External tool needed'],
['DIY tmux','Token tracking','Per-session spend dashboards','Manual calculation'],
['DIY tmux','Status detection','ANSI-parsed working/waiting/idle','Custom parsing needed'],
['DIY tmux','Conversation fork','Clone JSONL history to new branch','Manual copy'],
['DIY tmux','Agent coordination','REST API + global memory','Custom implementation'],
])}
<h2>Start simple, then upgrade</h2>
<p>If you're running 1-2 agents, DIY tmux works fine. When you hit 5+ agents and need to monitor them from your phone at 2am, amux pays for itself immediately.</p>
${CTA}`
},
{
  slug: 'amux-vs-crewai',
  title: 'amux vs CrewAI',
  description: 'Compare amux with CrewAI for multi-agent AI orchestration and task coordination.',
  keywords: 'amux vs crewai, crewai alternative, multi-agent framework, AI agent orchestration',
  content: `<h1>amux vs CrewAI</h1>
<p class="subtitle">CrewAI orchestrates custom AI agents. amux orchestrates Claude Code sessions.</p>
<p><strong>CrewAI</strong> is a Python framework for building multi-agent systems with custom tools and roles. <strong>amux</strong> specifically orchestrates Claude Code CLI sessions — real coding agents with full filesystem and terminal access.</p>
<h2>Key differences</h2>
${comparisonTable([
['CrewAI','Agent type','Claude Code (full dev environment)','Custom agents with defined tools'],
['CrewAI','Setup','3 commands to start','Python code to define agents/tasks'],
['CrewAI','Coding capability','Full Claude Code (edit, test, git)','Depends on custom tools'],
['CrewAI','Self-healing','Built-in watchdog','Custom error handling'],
['CrewAI','Dashboard','Web UI included','Separate monitoring needed'],
['CrewAI','Flexibility','Claude Code only','Any LLM provider'],
])}
<h2>Different tools for different jobs</h2>
<p>CrewAI is a <strong>general-purpose agent framework</strong> — great for building custom workflows (research, analysis, content). amux is specifically for <strong>running Claude Code agents that write, test, and ship code</strong>.</p>
${CTA}`
},
{
  slug: 'amux-vs-cline',
  title: 'amux vs Cline (Roo Code)',
  description: 'Compare amux headless agent orchestration with Cline\'s VS Code-based AI coding assistant.',
  keywords: 'amux vs cline, roo code alternative, VS Code AI coding, claude code orchestration',
  content: `<h1>amux vs Cline (Roo Code)</h1>
<p class="subtitle">Cline lives in VS Code. amux runs headless with a web dashboard.</p>
<p><strong>Cline</strong> (now Roo Code) is an AI coding assistant that runs inside VS Code. <strong>amux</strong> orchestrates headless Claude Code agents with no IDE dependency.</p>
<h2>Key differences</h2>
${comparisonTable([
['Cline','Interface','Web dashboard + CLI','VS Code extension'],
['Cline','Parallel agents','Unlimited headless sessions','One per VS Code window'],
['Cline','Unattended','Self-healing, runs overnight','Requires VS Code open'],
['Cline','Agent model','Claude Code CLI','Multi-provider API'],
['Cline','Task board','Built-in kanban','No built-in coordination'],
['Cline','Mobile','PWA access','Desktop only'],
])}
${CTA}`
},
{
  slug: 'amux-vs-goose',
  title: 'amux vs Goose',
  description: 'Compare amux with Block\'s open-source Goose AI coding agent framework.',
  keywords: 'amux vs goose, goose AI alternative, block AI agent, open source coding agent',
  content: `<h1>amux vs Goose</h1>
<p class="subtitle">Both open source. Goose is a single agent; amux manages a fleet.</p>
<p><strong>Goose</strong> (by Block) is an open-source AI coding agent with MCP tool support. <strong>amux</strong> manages many Claude Code sessions in parallel with orchestration.</p>
<h2>Key differences</h2>
${comparisonTable([
['Goose','Agent count','Dozens in parallel','Single agent'],
['Goose','Self-healing','Auto-compact + restart','Manual recovery'],
['Goose','Dashboard','Web UI with live monitoring','CLI only'],
['Goose','Coordination','Kanban board + REST API','No multi-agent coordination'],
['Goose','Tool system','Claude Code built-in tools','MCP protocol'],
['Goose','LLM support','Claude (via Claude Code)','Multi-provider'],
])}
${CTA}`
},
{
  slug: 'amux-vs-codex',
  title: 'amux vs OpenAI Codex',
  description: 'Compare amux\'s local Claude Code orchestration with OpenAI\'s cloud-based Codex agent.',
  keywords: 'amux vs codex, openai codex alternative, claude vs codex, AI coding agent comparison',
  content: `<h1>amux vs OpenAI Codex</h1>
<p class="subtitle">Local-first fleet vs. cloud-based agent. Different ecosystems, similar goals.</p>
<p><strong>OpenAI Codex</strong> runs cloud-based coding agents in sandboxed containers. <strong>amux</strong> runs Claude Code agents locally on your machine with full environment access.</p>
<h2>Key differences</h2>
${comparisonTable([
['Codex','Hosting','Local machine','Cloud sandbox'],
['Codex','Environment','Your tools, configs, credentials','Isolated container'],
['Codex','Parallel agents','Unlimited local sessions','Cloud-based parallelism'],
['Codex','Self-healing','Auto-compact, restart, unstick','Cloud infrastructure'],
['Codex','LLM','Claude (Opus, Sonnet, Haiku)','GPT-4o, o3'],
['Codex','Open source','Yes (MIT)','CLI open source, cloud proprietary'],
['Codex','Cost','API tokens only','OpenAI pricing'],
])}
${CTA}`
},
{
  slug: 'amux-vs-sandboxed-sh',
  title: 'amux vs sandboxed.sh (Open Agent)',
  description: 'Compare amux with sandboxed.sh for self-hosted AI agent orchestration.',
  keywords: 'amux vs sandboxed, open agent alternative, self-hosted AI agents, agent orchestration',
  content: `<h1>amux vs sandboxed.sh</h1>
<p class="subtitle">Both self-hosted. Different isolation models.</p>
<p><strong>sandboxed.sh</strong> (Open Agent) runs AI agents in isolated Linux workspaces with a web dashboard and iOS app. <strong>amux</strong> runs Claude Code in tmux sessions directly on your machine.</p>
<h2>Key differences</h2>
${comparisonTable([
['sandboxed.sh','Isolation','Direct machine access (tmux)','Linux workspace containers'],
['sandboxed.sh','Setup','Python 3 + tmux','Docker + infrastructure'],
['sandboxed.sh','Agent type','Claude Code CLI','Multi-provider agents'],
['sandboxed.sh','Self-healing','Built-in watchdog','Container restart'],
['sandboxed.sh','Mobile','PWA (works offline)','iOS native app'],
['sandboxed.sh','Task board','SQLite kanban built-in','Separate task management'],
])}
${CTA}`
},
{
  slug: 'amux-vs-claude-code-web-ui',
  title: 'amux vs Claude Code Web UIs',
  description: 'Compare amux with other web-based interfaces for Claude Code like CloudCLI and claude-code-web.',
  keywords: 'claude code web ui, claude code dashboard, claude code gui, remote claude code',
  content: `<h1>amux vs Claude Code Web UIs</h1>
<p class="subtitle">Several projects add web interfaces to Claude Code. Here's how amux differs.</p>
<p>Projects like <strong>CloudCLI</strong>, <strong>claude-code-web</strong>, and <strong>Remote Code</strong> add web frontends to Claude Code. amux goes further with self-healing, agent orchestration, and a full productivity suite.</p>
<h2>What other web UIs provide</h2>
<ul><li>Basic web terminal for Claude Code</li><li>Session listing and switching</li><li>Some offer multi-session support</li></ul>
<h2>What amux adds</h2>
<ul><li><strong>Self-healing watchdog</strong> — auto-compact, restart, unstick</li><li><strong>Kanban board</strong> with atomic task claiming</li><li><strong>Agent-to-agent orchestration</strong> via REST API</li><li><strong>Mobile PWA</strong> with offline support</li><li><strong>Conversation forking</strong> — clone history to new branches</li><li><strong>Token spend tracking</strong> per session</li><li><strong>Git conflict detection</strong> across agents</li><li><strong>Built-in cron scheduler</strong></li><li><strong>iCal sync</strong> for board due dates</li></ul>
${CTA}`
},
{
  slug: 'amux-vs-langraph',
  title: 'amux vs LangGraph',
  description: 'Compare amux with LangGraph for building and orchestrating AI agent workflows.',
  keywords: 'amux vs langgraph, langgraph alternative, AI agent graph, coding agent orchestration',
  content: `<h1>amux vs LangGraph</h1>
<p class="subtitle">LangGraph builds agent graphs. amux orchestrates real coding sessions.</p>
<p><strong>LangGraph</strong> is a framework for building stateful, graph-based AI agent workflows. <strong>amux</strong> orchestrates actual Claude Code sessions that write, test, and deploy code.</p>
<h2>Key differences</h2>
${comparisonTable([
['LangGraph','Purpose','Orchestrate Claude Code sessions','Build custom agent graphs'],
['LangGraph','Agent capability','Full dev environment (files, git, shell)','Custom tools per node'],
['LangGraph','Setup','3 shell commands','Python code + LangChain ecosystem'],
['LangGraph','Self-healing','Built-in watchdog','Custom error edges'],
['LangGraph','State management','SQLite + tmux sessions','Checkpointed graph state'],
['LangGraph','Flexibility','Claude Code specific','Any LLM/tool combination'],
])}
<h2>Choose based on your goal</h2>
<p>Building a <strong>custom AI pipeline</strong> with specific tools? Use LangGraph. Need to <strong>run many Claude Code agents in parallel</strong> to ship code? Use amux.</p>
${CTA}`
},
];

// ═══════════════════════════════════════════════════════════════════
// USE CASES (15 pages)
// ═══════════════════════════════════════════════════════════════════
const useCases = [
{
  slug: 'parallel-feature-development',
  title: 'Parallel Feature Development',
  description: 'Run multiple Claude Code agents simultaneously to develop different features in parallel.',
  keywords: 'parallel feature development, multiple AI agents, concurrent coding, parallel AI coding',
  content: `<h1>Parallel Feature Development</h1>
<p class="subtitle">Assign each feature to its own agent. Ship a week's work in a day.</p>
<p>Instead of working on features sequentially, spin up a Claude Code agent for each one. amux manages them all from a single dashboard, prevents git conflicts, and tracks progress on a shared board.</p>
<h2>How it works</h2>
<ol>
<li>Register one session per feature: <code>amux register auth --dir ~/Dev/app --yolo</code></li>
<li>Start them all: <code>amux start auth && amux start search && amux start payments</code></li>
<li>Send each agent its task from the dashboard or CLI</li>
<li>Monitor progress from the web UI or your phone</li>
</ol>
<h2>Git conflict avoidance</h2>
<p>amux detects when two sessions share the same directory and branch, warning you before conflicts happen. One click isolates each agent onto its own branch.</p>
<h2>Coordination via the board</h2>
<p>Post tasks to the kanban board. Agents claim work atomically — no duplicated effort, no lock broker needed.</p>
${CTA}`
},
{
  slug: 'automated-code-review',
  title: 'Automated Code Review with AI Agents',
  description: 'Use parallel Claude Code agents to review pull requests, find bugs, and suggest improvements automatically.',
  keywords: 'automated code review, AI code review, parallel code review, claude code review',
  content: `<h1>Automated Code Review</h1>
<p class="subtitle">Review every PR with an AI agent. Catch bugs before they ship.</p>
<p>Spin up a dedicated Claude Code agent for each pull request. It reads the diff, checks for bugs, security issues, and style violations, then reports back through the amux board.</p>
<h2>Setup</h2>
<pre><code>amux register reviewer --dir ~/Dev/app --yolo
amux start reviewer
amux send reviewer "Review PR #42: check for security issues, race conditions, and missing error handling"</code></pre>
<h2>Scale it up</h2>
<p>With amux, you can run a reviewer agent per PR. The self-healing watchdog ensures they keep working even if Claude Code hits context limits.</p>
${CTA}`
},
{
  slug: 'large-scale-refactoring',
  title: 'Large-Scale Refactoring with AI Agents',
  description: 'Refactor entire codebases in parallel by assigning different modules to different Claude Code agents.',
  keywords: 'large scale refactoring, AI refactoring, parallel code refactoring, codebase modernization',
  content: `<h1>Large-Scale Refactoring</h1>
<p class="subtitle">Assign each module to its own agent. Refactor a monolith in a day.</p>
<p>Breaking a monolith into services? Migrating from one framework to another? Split the work across agents — each one handles a module, and amux coordinates the effort.</p>
<h2>Example: migrating from Express to Fastify</h2>
<ol>
<li>Create an agent per route group: <code>auth-routes</code>, <code>user-routes</code>, <code>billing-routes</code></li>
<li>Each agent migrates its routes independently</li>
<li>Use the kanban board to track which modules are done</li>
<li>Git conflict detection prevents agents from stepping on each other</li>
</ol>
${CTA}`
},
{
  slug: 'test-generation-at-scale',
  title: 'Test Generation at Scale',
  description: 'Generate comprehensive test suites in parallel with multiple Claude Code agents.',
  keywords: 'AI test generation, parallel test generation, automated testing AI, claude code testing',
  content: `<h1>Test Generation at Scale</h1>
<p class="subtitle">One agent per module. Full test coverage by morning.</p>
<p>Low test coverage? Assign one Claude Code agent per module or file. Each agent writes unit tests, integration tests, and edge cases. amux's self-healing ensures they keep working through context resets.</p>
<h2>Workflow</h2>
<ol>
<li>List your untested modules</li>
<li>Post each as a task on the amux board</li>
<li>Start agents: they claim tasks atomically from the board</li>
<li>Monitor from the dashboard — check coverage as agents complete work</li>
</ol>
<h2>Why self-healing matters here</h2>
<p>Test generation is token-intensive. Agents frequently hit context limits. amux's watchdog automatically runs <code>/compact</code> and restarts agents that crash — so your test generation runs don't stall at 3am.</p>
${CTA}`
},
{
  slug: 'bug-triage-and-fixing',
  title: 'Bug Triage and Fixing',
  description: 'Assign bugs to parallel AI agents for automated triage, reproduction, and fixing.',
  keywords: 'automated bug fixing, AI bug triage, parallel bug fixing, claude code bug fix',
  content: `<h1>Bug Triage and Fixing</h1>
<p class="subtitle">File bugs on the board. Agents claim and fix them autonomously.</p>
<p>Post bugs to the amux kanban board. Start a pool of agents. Each agent claims a bug atomically, reproduces it, writes a fix, and adds tests. You review the PRs.</p>
<h2>Atomic task claiming</h2>
<p><code>POST /api/board/:id/claim</code> is a SQLite compare-and-swap. Even with 20 agents racing for the same queue, each bug gets claimed exactly once. No Redis required.</p>
${CTA}`
},
{
  slug: 'documentation-generation',
  title: 'Documentation Generation',
  description: 'Generate API docs, README files, and developer guides with parallel AI agents.',
  keywords: 'AI documentation generation, automated docs, parallel documentation, claude code docs',
  content: `<h1>Documentation Generation</h1>
<p class="subtitle">One agent per module's docs. Comprehensive documentation overnight.</p>
<p>Assign each module, API endpoint group, or library to a dedicated agent. They read the source, generate documentation, and commit it. The board tracks completion.</p>
${CTA}`
},
{
  slug: 'monorepo-management',
  title: 'Monorepo Management with AI Agents',
  description: 'Manage monorepo packages in parallel with dedicated Claude Code agents per package.',
  keywords: 'monorepo AI agents, parallel monorepo development, AI monorepo management, claude code monorepo',
  content: `<h1>Monorepo Management</h1>
<p class="subtitle">One agent per package. Keep your monorepo in sync.</p>
<p>In a monorepo, each package has its own concerns. Assign a Claude Code agent per package — one updates dependencies, another fixes types, a third adds tests. amux's git conflict detection prevents collisions.</p>
<h2>Git isolation</h2>
<p>amux detects when multiple agents work on the same repo and offers one-click branch isolation. Each agent works on its own branch, and you merge when ready.</p>
${CTA}`
},
{
  slug: 'dependency-updates',
  title: 'Automated Dependency Updates',
  description: 'Run AI agents in parallel to update dependencies, fix breaking changes, and run tests.',
  keywords: 'automated dependency updates, AI dependency management, parallel dependency upgrade, renovate alternative',
  content: `<h1>Automated Dependency Updates</h1>
<p class="subtitle">Upgrade every dependency in parallel. Fix breaking changes automatically.</p>
<p>Major version bumps break things. Assign each dependency update to its own agent. It upgrades, fixes breaking changes, runs tests, and reports results. Self-healing keeps agents running through long update cycles.</p>
${CTA}`
},
{
  slug: 'api-endpoint-generation',
  title: 'API Endpoint Generation',
  description: 'Generate REST API endpoints in parallel with multiple Claude Code agents.',
  keywords: 'AI API generation, automated API development, parallel API coding, claude code API',
  content: `<h1>API Endpoint Generation</h1>
<p class="subtitle">One agent per endpoint group. Ship your entire API in a day.</p>
<p>Define your API spec, then assign each endpoint group (auth, users, billing, etc.) to a dedicated agent. They implement routes, validation, tests, and docs in parallel.</p>
${CTA}`
},
{
  slug: 'security-audit',
  title: 'Security Audit with AI Agents',
  description: 'Run parallel AI agents to audit your codebase for security vulnerabilities and compliance issues.',
  keywords: 'AI security audit, automated security review, parallel security scanning, claude code security',
  content: `<h1>Security Audit</h1>
<p class="subtitle">Assign each attack surface to a dedicated agent. Find vulnerabilities faster.</p>
<p>Split your codebase by attack surface — authentication, input validation, SQL queries, API boundaries. Each agent audits its area, reports findings to the board, and suggests fixes.</p>
${CTA}`
},
{
  slug: 'code-migration',
  title: 'Code Migration (Language/Framework)',
  description: 'Migrate codebases between languages or frameworks using parallel AI agents.',
  keywords: 'AI code migration, automated code translation, framework migration AI, language migration',
  content: `<h1>Code Migration</h1>
<p class="subtitle">Python 2 to 3. JavaScript to TypeScript. Express to Fastify. In parallel.</p>
<p>Assign each file or module to its own agent. They migrate the code, update imports, fix type errors, and run tests. amux's board tracks which modules are migrated.</p>
${CTA}`
},
{
  slug: 'research-and-prototyping',
  title: 'Research and Prototyping',
  description: 'Run multiple research agents in parallel to explore different approaches and prototype solutions.',
  keywords: 'AI research agents, parallel prototyping, automated research, claude code prototyping',
  content: `<h1>Research and Prototyping</h1>
<p class="subtitle">Try five approaches at once. Pick the best one.</p>
<p>Evaluating different databases? Comparing auth libraries? Run a separate agent for each approach. They each build a prototype, benchmark it, and report results. You compare and decide.</p>
<h2>Conversation forking</h2>
<p>Use amux's fork feature to clone a session's context into multiple new sessions — each one explores a different direction from the same starting point.</p>
${CTA}`
},
{
  slug: 'ci-cd-pipeline-creation',
  title: 'CI/CD Pipeline Creation',
  description: 'Generate CI/CD pipelines, GitHub Actions, and deployment configs with AI agents.',
  keywords: 'AI CI/CD generation, automated pipeline creation, github actions AI, deployment automation',
  content: `<h1>CI/CD Pipeline Creation</h1>
<p class="subtitle">Agent-generated pipelines for every service in your stack.</p>
<p>Need GitHub Actions, Docker configs, Terraform modules, and deployment scripts? Assign each to a dedicated agent. They work in parallel and the board tracks completion.</p>
${CTA}`
},
{
  slug: 'legacy-code-modernization',
  title: 'Legacy Code Modernization',
  description: 'Modernize legacy codebases module-by-module with parallel AI coding agents.',
  keywords: 'legacy code modernization, AI code modernization, technical debt AI, codebase upgrade',
  content: `<h1>Legacy Code Modernization</h1>
<p class="subtitle">Tackle technical debt with a fleet of agents.</p>
<p>Legacy code is intimidating. Break it into modules, assign each to an agent, and let them modernize in parallel — updating patterns, adding types, replacing deprecated APIs, and writing tests.</p>
<h2>Self-healing for long-running tasks</h2>
<p>Modernizing a large module can exhaust Claude Code's context window. amux's watchdog automatically compacts context and restarts on corruption, so agents keep making progress through days-long refactoring work.</p>
${CTA}`
},
{
  slug: 'ai-coding-while-you-sleep',
  title: 'AI Coding While You Sleep',
  description: 'Run autonomous AI coding agents overnight with self-healing. Wake up to completed features.',
  keywords: 'AI coding while you sleep, unattended AI coding, autonomous coding overnight, AI agents 24/7',
  content: `<h1>AI Coding While You Sleep</h1>
<p class="subtitle">Queue up work before bed. Wake up to pull requests.</p>
<p>The dream of autonomous coding requires one thing DIY setups can't deliver: <strong>reliability</strong>. Claude Code sessions crash from context compaction, get stuck on prompts, and silently hang. amux's self-healing watchdog handles all of this automatically.</p>
<h2>What goes wrong at 3am (and how amux fixes it)</h2>
<table>
<thead><tr><th>Problem</th><th>amux's response</th></tr></thead>
<tbody>
<tr><td>Context window fills up</td><td>Auto-sends <code>/compact</code> (5-min cooldown)</td></tr>
<tr><td>Thinking-block corruption</td><td>Restarts session + replays last message</td></tr>
<tr><td>Stuck on tool-approval prompt</td><td>Auto-responds in YOLO mode</td></tr>
<tr><td>Silent hang / no output</td><td>Detects idle + sends continue signal</td></tr>
</tbody>
</table>
<h2>Morning routine</h2>
<ol>
<li>Open amux on your phone (PWA)</li>
<li>Check the board — see what agents completed</li>
<li>Peek into each session's output</li>
<li>Review and merge the PRs</li>
</ol>
${CTA}`
},
];

// ═══════════════════════════════════════════════════════════════════
// GUIDES (15 pages)
// ═══════════════════════════════════════════════════════════════════
const guides = [
{
  slug: 'getting-started',
  title: 'Getting Started with amux',
  description: 'Install amux and run your first parallel Claude Code agents in under 5 minutes.',
  keywords: 'amux getting started, install amux, amux tutorial, claude code multiplexer setup',
  content: `<h1>Getting Started with amux</h1>
<p class="subtitle">From zero to running agents in under 5 minutes.</p>
<h2>Prerequisites</h2>
<ul><li><code>python3</code> (3.8+)</li><li><code>tmux</code></li><li><code>claude</code> CLI (Claude Code) installed and authenticated</li></ul>
<h2>Install</h2>
<pre><code>git clone https://github.com/mixpeek/amux && cd amux
./install.sh</code></pre>
<h2>Register your first session</h2>
<pre><code>amux register myproject --dir ~/Dev/myproject --yolo
amux start myproject</code></pre>
<h2>Open the dashboard</h2>
<pre><code>amux serve  # → https://localhost:8822</code></pre>
<p>Open the URL in your browser. You'll see your session with live status, token tracking, and a send bar.</p>
<h2>Send your first task</h2>
<p>Click the session card, type a prompt in the send bar, and hit Enter. Or from the CLI:</p>
<pre><code>amux send myproject "add unit tests for the auth module"</code></pre>
<h2>Scale up</h2>
<p>Register more sessions and run them in parallel. The dashboard shows all of them at a glance.</p>
${CTA}`
},
{
  slug: 'running-10-plus-agents',
  title: 'Running 10+ Agents in Parallel',
  description: 'How to effectively run 10 or more Claude Code agents simultaneously with amux.',
  keywords: 'run multiple AI agents, parallel claude code, 10 agents parallel, scale AI coding',
  content: `<h1>Running 10+ Agents in Parallel</h1>
<p class="subtitle">Practical tips for managing a fleet of Claude Code agents.</p>
<h2>Machine requirements</h2>
<p>Each Claude Code session uses ~200-400MB RAM. For 10+ agents, you'll want at least 8GB free. CPU usage is minimal since agents spend most time waiting for API responses.</p>
<h2>Organization strategies</h2>
<ul>
<li><strong>By feature:</strong> one agent per feature branch</li>
<li><strong>By module:</strong> one agent per package in a monorepo</li>
<li><strong>By task type:</strong> reviewers, testers, documenters, implementers</li>
</ul>
<h2>Use the board for coordination</h2>
<p>Post all tasks to the kanban board. Agents claim work atomically with <code>POST /api/board/:id/claim</code>. No duplicated effort.</p>
<h2>Monitor from your phone</h2>
<p>Install the PWA on your phone. Check in periodically — the dashboard shows which agents are working, which need input, and which are idle.</p>
${CTA}`
},
{
  slug: 'self-healing-configuration',
  title: 'Self-Healing Configuration',
  description: 'Configure amux\'s self-healing watchdog for auto-compaction, restart, and prompt recovery.',
  keywords: 'self-healing AI agent, auto recovery claude code, context compaction, amux watchdog',
  content: `<h1>Self-Healing Configuration</h1>
<p class="subtitle">How amux keeps your agents running through crashes, hangs, and context exhaustion.</p>
<h2>What the watchdog does</h2>
<table>
<thead><tr><th>Condition</th><th>Action</th></tr></thead>
<tbody>
<tr><td>Context below 20%</td><td>Sends <code>/compact</code> (5-min cooldown)</td></tr>
<tr><td>Thinking-block corruption</td><td>Restarts + replays last message</td></tr>
<tr><td>Stuck on tool-approval prompt</td><td>Auto-responds in YOLO mode</td></tr>
<tr><td>Idle too long with pending work</td><td>Sends continue signal</td></tr>
</tbody>
</table>
<h2>YOLO mode</h2>
<p>Register sessions with <code>--yolo</code> to enable automatic tool-approval responses. The watchdog detects the "Esc to cancel" marker (which only appears on tool-approval prompts, never on open-ended questions) and auto-answers.</p>
<pre><code>amux register myproject --dir ~/Dev/app --yolo</code></pre>
${CTA}`
},
{
  slug: 'agent-to-agent-orchestration',
  title: 'Agent-to-Agent Orchestration',
  description: 'How to make Claude Code agents discover peers, delegate tasks, and coordinate work via REST API.',
  keywords: 'agent orchestration, AI agent coordination, multi-agent REST API, claude code agents talk',
  content: `<h1>Agent-to-Agent Orchestration</h1>
<p class="subtitle">Agents discover peers and delegate work without being told how.</p>
<h2>How it works</h2>
<p>Every session gets <code>$AMUX_SESSION</code> (its own name) and <code>$AMUX_URL</code> injected at startup. A global memory file gives every agent the full REST API reference.</p>
<h2>Key API calls</h2>
<pre><code># Send a task to another session
curl -sk -X POST -H 'Content-Type: application/json' \\
  -d '{"text":"implement the login endpoint"}' \\
  $AMUX_URL/api/sessions/worker-1/send

# Claim a board task atomically
curl -sk -X POST $AMUX_URL/api/board/PROJ-5/claim

# Peek at another session's output
curl -sk "$AMUX_URL/api/sessions/worker-1/peek?lines=50"</code></pre>
<h2>Orchestration patterns</h2>
<ul>
<li><strong>Manager + workers:</strong> One agent posts tasks, workers claim and complete them</li>
<li><strong>Peer review:</strong> Agents review each other's output via peek</li>
<li><strong>Pipeline:</strong> Agent A generates code, Agent B writes tests, Agent C reviews</li>
</ul>
${CTA}`
},
{
  slug: 'mobile-management-pwa',
  title: 'Mobile Management via PWA',
  description: 'Install amux as a PWA on your phone to manage AI agents from anywhere.',
  keywords: 'amux mobile, PWA AI dashboard, manage AI agents phone, mobile coding agents',
  content: `<h1>Mobile Management via PWA</h1>
<p class="subtitle">Install amux on your phone. Manage agents from anywhere.</p>
<h2>Install the PWA</h2>
<ol>
<li>Open <code>https://your-server:8822</code> in Safari (iOS) or Chrome (Android)</li>
<li>Tap "Add to Home Screen"</li>
<li>The app works offline — queued commands replay when you reconnect</li>
</ol>
<h2>What you can do from your phone</h2>
<ul>
<li>View all session statuses (working/waiting/idle)</li>
<li>Peek into any session's live output</li>
<li>Send commands and prompts</li>
<li>Manage the kanban board</li>
<li>Check token spend</li>
</ul>
${CTA}`
},
{
  slug: 'token-spend-tracking',
  title: 'Token Spend Tracking',
  description: 'Monitor per-session AI token spend and daily costs with amux\'s built-in dashboards.',
  keywords: 'AI token tracking, claude code cost, token spend monitoring, AI agent cost management',
  content: `<h1>Token Spend Tracking</h1>
<p class="subtitle">Know exactly what each agent costs. Per session, per day.</p>
<p>amux parses Claude Code's JSONL conversation logs to extract token usage. Daily spend per session with cache reads broken out. Deduplicates by signature so restarts don't double-count.</p>
<h2>What you see</h2>
<ul>
<li>Input tokens, output tokens, cache reads per session</li>
<li>Daily spend breakdown</li>
<li>Model used (Opus, Sonnet, Haiku)</li>
<li>Cost estimates</li>
</ul>
${CTA}`
},
{
  slug: 'kanban-board-for-agents',
  title: 'Kanban Board for AI Agents',
  description: 'Use amux\'s SQLite-backed kanban board to coordinate work across parallel AI agents.',
  keywords: 'AI agent kanban board, agent task board, coordinate AI agents, parallel task management',
  content: `<h1>Kanban Board for AI Agents</h1>
<p class="subtitle">Post tasks. Agents claim them. No duplicated work.</p>
<p>amux includes a SQLite-backed kanban board with auto-generated issue keys, custom columns, per-agent ownership, and iCal sync for due dates.</p>
<h2>Atomic task claiming</h2>
<p><code>POST /api/board/:id/claim</code> is a compare-and-swap operation. Even with 20 agents racing for the same queue, each task gets assigned exactly once.</p>
<h2>iCal sync</h2>
<p>Board items with due dates export as an iCal feed. Subscribe from Google Calendar or Apple Calendar to see agent deadlines.</p>
${CTA}`
},
{
  slug: 'conversation-forking',
  title: 'Conversation Forking',
  description: 'Clone a Claude Code session\'s context into multiple new agents to explore different approaches.',
  keywords: 'conversation fork, clone AI session, branch AI agent, parallel exploration AI',
  content: `<h1>Conversation Forking</h1>
<p class="subtitle">Clone one session into many. Explore branches in parallel.</p>
<p>Fork a session's JSONL history into new sessions on separate branches. Each fork inherits the full conversation context and can take it in a different direction.</p>
<h2>Use cases</h2>
<ul>
<li>Try three different implementation approaches from the same starting point</li>
<li>A/B test different architectures</li>
<li>Branch a working prototype into feature-specific development sessions</li>
</ul>
${CTA}`
},
{
  slug: 'git-conflict-avoidance',
  title: 'Git Conflict Avoidance',
  description: 'How amux detects and prevents git conflicts when multiple AI agents work on the same repo.',
  keywords: 'git conflict AI agents, parallel coding git, branch isolation AI, multi-agent git',
  content: `<h1>Git Conflict Avoidance</h1>
<p class="subtitle">amux warns before agents collide. One click to isolate.</p>
<p>When two sessions share the same directory and branch, amux warns you in the dashboard. Click to isolate each agent onto its own branch.</p>
<h2>How it works</h2>
<ul>
<li>Dashboard shows a warning icon when sessions share a dir + branch</li>
<li>One-click branch creation isolates agents</li>
<li>Agents work independently, you merge when ready</li>
</ul>
${CTA}`
},
{
  slug: 'rest-api-reference',
  title: 'REST API Reference',
  description: 'Complete reference for amux\'s REST API — sessions, board, memory, sync, and orchestration.',
  keywords: 'amux API reference, amux REST API, agent orchestration API, session management API',
  content: `<h1>REST API Reference</h1>
<p class="subtitle">Everything agents need to discover peers and coordinate work.</p>
<h2>Sessions</h2>
<pre><code>GET  /api/sessions                    # List all sessions
GET  /api/sessions/:name/meta         # Session metadata
GET  /api/sessions/:name/peek?lines=N # Read terminal output
POST /api/sessions/:name/send         # Send text to session
GET  /api/sessions/self?session=:name # Session self-lookup</code></pre>
<h2>Board</h2>
<pre><code>GET    /api/board                     # List all items
POST   /api/board                     # Create item
PATCH  /api/board/:id                 # Update item
DELETE /api/board/:id                 # Delete item
POST   /api/board/:id/claim           # Atomic claim</code></pre>
<h2>Memory</h2>
<pre><code>GET  /api/sessions/:name/memory       # Session memory
POST /api/sessions/:name/memory       # Update session memory
GET  /api/memory/global               # Global shared memory
POST /api/memory/global               # Update global memory</code></pre>
<h2>Sync</h2>
<pre><code>GET /api/sync?since=:unix_ts          # Delta sync (issues + statuses)</code></pre>
${CTA}`
},
{
  slug: 'scaling-to-50-agents',
  title: 'Scaling to 50+ Agents',
  description: 'Tips for running 50 or more Claude Code agents in parallel with amux.',
  keywords: 'scale AI agents, 50 agents parallel, large scale AI coding, agent farm',
  content: `<h1>Scaling to 50+ Agents</h1>
<p class="subtitle">Run a full agent farm. Here's how to keep it stable.</p>
<h2>Hardware</h2>
<p>Each agent uses ~200-400MB RAM. For 50 agents, plan for 16-32GB RAM. Consider running on a cloud VM or a dedicated development server.</p>
<h2>API rate limits</h2>
<p>Anthropic rate limits are per-organization. With 50 concurrent agents, you'll hit rate limits. Stagger agent start times and use the board to batch work naturally.</p>
<h2>Monitoring</h2>
<p>The workspace view lets you tile multiple agent outputs side by side. Use the board's status columns to track progress across all agents.</p>
${CTA}`
},
{
  slug: 'remote-access-tailscale',
  title: 'Remote Access with Tailscale',
  description: 'Access your amux dashboard securely from anywhere using Tailscale.',
  keywords: 'amux remote access, tailscale amux, remote AI agents, secure agent dashboard',
  content: `<h1>Remote Access with Tailscale</h1>
<p class="subtitle">Access your agent dashboard from anywhere. No port forwarding needed.</p>
<p>amux generates TLS certificates automatically. With Tailscale, your dashboard is accessible from any device on your tailnet — phone, laptop, or another server.</p>
<h2>Setup</h2>
<ol>
<li>Install Tailscale on your amux server and devices</li>
<li>amux auto-detects Tailscale and generates a valid TLS cert</li>
<li>Open <code>https://your-tailscale-ip:8822</code> from any device</li>
<li>Install as PWA on your phone for quick access</li>
</ol>
${CTA}`
},
{
  slug: 'custom-automation-cron',
  title: 'Custom Automation with Cron',
  description: 'Schedule recurring AI agent tasks with amux\'s built-in cron scheduler.',
  keywords: 'amux cron, scheduled AI tasks, recurring agent commands, automated AI scheduling',
  content: `<h1>Custom Automation with Cron</h1>
<p class="subtitle">Schedule agent tasks without crontab or systemd.</p>
<p>amux includes a built-in cron scheduler. Schedule one-time or recurring commands in any session. Next-run is computed atomically in SQLite with missed-fire recovery.</p>
<h2>Examples</h2>
<ul>
<li>Run daily dependency checks every morning</li>
<li>Weekly security audits on Friday evenings</li>
<li>Periodic test suite runs</li>
</ul>
${CTA}`
},
{
  slug: 'workspace-tiled-layout',
  title: 'Multi-Pane Workspace Layout',
  description: 'Watch multiple AI agents side by side with amux\'s tiled workspace view.',
  keywords: 'amux workspace, tiled agent view, multi-pane dashboard, parallel agent monitoring',
  content: `<h1>Multi-Pane Workspace</h1>
<p class="subtitle">Full-screen tiled layout to watch multiple agents at once.</p>
<p>The workspace view tiles multiple agent outputs side by side. Each pane has its own send bar. Drag to rearrange. Layout and named profiles persist across sessions.</p>
${CTA}`
},
{
  slug: 'session-memory-and-context',
  title: 'Session Memory and Context',
  description: 'How amux manages per-session memory and global shared context for AI agents.',
  keywords: 'session memory, AI agent context, shared memory, claude code memory management',
  content: `<h1>Session Memory and Context</h1>
<p class="subtitle">Per-session markdown context that persists across restarts. Global memory shared across all agents.</p>
<h2>Per-session memory</h2>
<p>Each session has a markdown file that persists across restarts. Agents can read and write their own memory via the API.</p>
<h2>Global memory</h2>
<p>The global memory file is shared across all agents. It contains the full REST API reference and orchestration patterns. Every agent reads it at startup.</p>
${CTA}`
},
];

// ═══════════════════════════════════════════════════════════════════
// GLOSSARY (20 pages)
// ═══════════════════════════════════════════════════════════════════
function glossaryEntry(slug, term, definition, details) {
  return {
    slug, title: term,
    description: `${term}: ${definition}`,
    keywords: `${term.toLowerCase()}, AI agent glossary, amux, claude code`,
    content: `<h1>${term}</h1><p class="subtitle">${definition}</p>${details}<p><a href="/glossary/">← Back to glossary</a></p>`,
  };
}

const glossary = [
  glossaryEntry('agent-multiplexer', 'Agent Multiplexer', 'A tool that runs and manages multiple AI coding agents simultaneously.',
    `<p>An agent multiplexer manages the lifecycle of multiple AI coding sessions — starting, stopping, monitoring, and coordinating them from a single interface. amux is an agent multiplexer for Claude Code, built on tmux.</p><h2>Why multiplex?</h2><p>Running one agent at a time is a bottleneck. With a multiplexer, you assign different tasks to different agents and they work in parallel, dramatically increasing throughput.</p>${CTA}`),
  glossaryEntry('self-healing-agent', 'Self-Healing AI Agent', 'An AI agent that automatically recovers from crashes, context exhaustion, and stuck states.',
    `<p>Self-healing agents detect when something goes wrong and fix it without human intervention. In amux, the watchdog monitors for context exhaustion, thinking-block corruption, and stuck prompts — automatically recovering from each.</p><h2>Common failure modes</h2><ul><li>Context window fills up → auto-compact</li><li>Thinking-block corruption → restart + replay</li><li>Tool-approval stuck → auto-respond (YOLO mode)</li></ul>${CTA}`),
  glossaryEntry('context-compaction', 'Context Compaction', 'The process of summarizing conversation history to free up context window space in an AI agent.',
    `<p>When a Claude Code session's context window fills up, the <code>/compact</code> command summarizes the conversation to free space. amux's watchdog triggers this automatically when context drops below 20%.</p><h2>Why it matters</h2><p>Without compaction, agents stop working when they run out of context. With self-healing compaction, agents can work indefinitely on long tasks.</p>`),
  glossaryEntry('yolo-mode', 'YOLO Mode', 'A configuration that auto-approves tool-use prompts in Claude Code for fully unattended operation.',
    `<p>YOLO mode detects blocking tool-approval prompts (identified by the "Esc to cancel" marker) and auto-answers them. This enables truly headless operation — agents never block waiting for human approval.</p><h2>Safety</h2><p>The detection specifically targets tool-approval prompts. It never fires on open-ended model questions, ensuring agents don't auto-answer when human judgment is needed.</p>`),
  glossaryEntry('agent-orchestration', 'Agent Orchestration', 'Coordinating multiple AI agents to work together on complex tasks.',
    `<p>Agent orchestration involves managing task distribution, communication, and coordination across multiple AI agents. amux provides orchestration through a shared kanban board, REST API, and global memory.</p><h2>Orchestration patterns</h2><ul><li><strong>Manager + workers</strong> — one agent delegates, others execute</li><li><strong>Peer review</strong> — agents review each other's work</li><li><strong>Pipeline</strong> — sequential handoff between specialized agents</li><li><strong>Swarm</strong> — agents claim tasks from a shared queue</li></ul>${CTA}`),
  glossaryEntry('parallel-coding-agents', 'Parallel Coding Agents', 'Multiple AI agents working on different coding tasks at the same time.',
    `<p>Instead of one agent working sequentially, parallel coding agents tackle different tasks simultaneously. This can multiply development throughput by 10x or more.</p><h2>Requirements for parallel agents</h2><ul><li>Session management (start, stop, monitor)</li><li>Git isolation (prevent conflicts)</li><li>Task coordination (prevent duplicate work)</li><li>Self-healing (recover from crashes)</li></ul><p>amux provides all four out of the box.</p>${CTA}`),
  glossaryEntry('token-accounting', 'Token Accounting', 'Tracking token usage and costs across multiple AI coding sessions.',
    `<p>Token accounting measures how many input/output tokens each agent consumes. amux parses JSONL conversation logs and deduplicates by signature to provide accurate per-session daily spend.</p>`),
  glossaryEntry('atomic-task-claiming', 'Atomic Task Claiming', 'A concurrent-safe mechanism for agents to claim tasks without duplicating work.',
    `<p>amux's <code>POST /api/board/:id/claim</code> is a SQLite compare-and-swap operation. Even with 20+ agents racing for the same queue, each task gets assigned exactly once. No Redis or external lock broker needed.</p>`),
  glossaryEntry('session-persistence', 'Session Persistence', 'The ability for AI agent sessions to survive restarts and maintain their identity.',
    `<p>Each amux session has a UUID that persists across stop/start cycles. Conversation history, memory, and metadata survive restarts. Stopped sessions can still be peeked to review their last output.</p>`),
  glossaryEntry('prompt-replay', 'Prompt Replay', 'Automatically re-sending the last message after an AI agent crash to recover progress.',
    `<p>When amux detects thinking-block corruption (a known Claude Code failure mode), it restarts the session and replays the last user message. The agent picks up roughly where it left off.</p>`),
  glossaryEntry('global-memory', 'Global Memory', 'A shared knowledge base accessible to all AI agents in a multiplexer.',
    `<p>amux's global memory is a markdown file shared across all sessions. It contains the REST API reference and orchestration patterns. Every agent reads it at startup, giving them the knowledge to coordinate with peers.</p>`),
  glossaryEntry('background-watchdog', 'Background Watchdog', 'A monitoring process that detects and recovers from AI agent failures automatically.',
    `<p>amux's watchdog runs on every status check cycle. It monitors terminal output for known failure patterns and takes corrective action — compacting context, restarting sessions, or auto-responding to stuck prompts.</p>`),
  glossaryEntry('conversation-fork', 'Conversation Fork', 'Cloning an AI session\'s history into multiple new sessions for parallel exploration.',
    `<p>Forking creates new sessions that inherit the full conversation context of the parent. Each fork can explore a different approach from the same starting point, enabling parallel experimentation.</p>`),
  glossaryEntry('terminal-peek', 'Terminal Peek', 'Viewing an AI agent\'s live terminal output without attaching to the session.',
    `<p>Peek mode shows full scrollback with search, file previews, and a send bar. You can monitor any agent's progress and send commands without attaching to the tmux session. Output snapshots persist across restarts.</p>`),
  glossaryEntry('pwa-progressive-web-app', 'PWA (Progressive Web App)', 'A web application installable on mobile devices with offline support.',
    `<p>amux's dashboard is a PWA — install it on your phone for native-like access. Background Sync replays queued commands when you reconnect. Manage your agent fleet from anywhere.</p>`),
  glossaryEntry('tmux-session-management', 'tmux Session Management', 'Using tmux terminal multiplexer to run persistent background processes.',
    `<p>amux uses tmux to run Claude Code sessions in persistent background terminals. tmux sessions survive SSH disconnects and system restarts. amux adds monitoring, self-healing, and a web dashboard on top.</p>`),
  glossaryEntry('rest-api-orchestration', 'REST API Orchestration', 'Using HTTP endpoints to coordinate AI agent communication and task management.',
    `<p>amux exposes REST endpoints for session management, board operations, memory access, and inter-agent communication. Agents use <code>curl</code> calls to discover peers, send messages, claim tasks, and report progress.</p>`),
  glossaryEntry('ical-sync', 'iCal Sync', 'Exporting task deadlines as calendar events via the iCal format.',
    `<p>Board items with due dates are exported as an iCal feed. Subscribe from Google Calendar or Apple Calendar to see agent task deadlines alongside your regular schedule.</p>`),
  glossaryEntry('sqlite-backed-board', 'SQLite-Backed Board', 'A kanban board using SQLite for concurrent-safe task management.',
    `<p>amux's board uses SQLite in WAL mode for concurrent reads and atomic writes. Auto-generated issue keys, per-agent ownership, custom columns, and soft deletes. No external database required.</p>`),
  glossaryEntry('git-worktree-isolation', 'Git Worktree Isolation', 'Using git worktrees to give each AI agent its own working copy of a repository.',
    `<p>When multiple agents need to work on the same repo without conflicts, git worktrees provide isolated working copies that share the same git history. amux detects shared directories and offers one-click branch isolation.</p>`),
];

// ═══════════════════════════════════════════════════════════════════
// SOLUTIONS (10 pages)
// ═══════════════════════════════════════════════════════════════════
const solutions = [
{
  slug: 'solo-developers',
  title: 'amux for Solo Developers',
  description: 'How solo developers use amux to multiply their output with parallel AI coding agents.',
  keywords: 'solo developer AI, one person startup, 10x developer, solo dev productivity',
  content: `<h1>amux for Solo Developers</h1>
<p class="subtitle">The output of a small team. The overhead of one person.</p>
<p>As a solo developer, you're the bottleneck. amux lets you run 10+ Claude Code agents in parallel — each working on a different feature, test suite, or module — while you review and merge.</p>
<h2>A typical day</h2>
<ol>
<li><strong>Morning:</strong> Queue up the day's tasks on the board</li>
<li><strong>Start agents:</strong> Each claims a task and gets to work</li>
<li><strong>Your job:</strong> Review PRs, make architectural decisions, handle the parts only you can do</li>
<li><strong>Evening:</strong> Queue overnight tasks. Self-healing keeps agents working while you sleep</li>
</ol>
${CTA}`
},
{
  slug: 'engineering-managers',
  title: 'amux for Engineering Managers',
  description: 'How engineering managers use amux to augment their team\'s velocity with AI agents.',
  keywords: 'AI tools engineering managers, team velocity AI, developer productivity management',
  content: `<h1>amux for Engineering Managers</h1>
<p class="subtitle">Augment your team's velocity without hiring.</p>
<p>Use amux to assign repetitive tasks to AI agents — test generation, dependency updates, code review, documentation. Your engineers focus on architecture and complex problems.</p>
<h2>Track everything</h2>
<ul>
<li>Per-session token spend — know what each agent costs</li>
<li>Board-based task tracking with due dates</li>
<li>iCal sync so deadlines appear in your calendar</li>
</ul>
${CTA}`
},
{
  slug: 'startup-ctos',
  title: 'amux for Startup CTOs',
  description: 'How startup CTOs use amux to ship faster with parallel AI coding agents.',
  keywords: 'startup AI tools, CTO AI productivity, ship faster AI, startup development velocity',
  content: `<h1>amux for Startup CTOs</h1>
<p class="subtitle">Ship like a team of 10 with a team of 3.</p>
<p>Early-stage startups need to move fast. amux lets your small team multiply output by running AI agents for the tasks that don't need human creativity — tests, boilerplate, migrations, docs.</p>
${CTA}`
},
{
  slug: 'open-source-maintainers',
  title: 'amux for Open Source Maintainers',
  description: 'How open source maintainers use amux to manage contributions, triage issues, and ship releases.',
  keywords: 'open source AI tools, maintainer productivity, automated issue triage, AI contributions',
  content: `<h1>amux for Open Source Maintainers</h1>
<p class="subtitle">Triage issues, review PRs, and ship releases with AI agents.</p>
<p>Open source maintenance is overwhelming. Use amux agents to triage issues, review contributor PRs, update dependencies, and generate release notes — all in parallel.</p>
${CTA}`
},
{
  slug: 'devops-engineers',
  title: 'amux for DevOps Engineers',
  description: 'How DevOps engineers use amux for infrastructure automation, pipeline creation, and monitoring.',
  keywords: 'DevOps AI automation, infrastructure AI, parallel DevOps, automated pipeline creation',
  content: `<h1>amux for DevOps Engineers</h1>
<p class="subtitle">Automate infrastructure work with parallel agents.</p>
<p>Use agents to generate Terraform modules, write GitHub Actions, create Docker configs, and audit infrastructure — all in parallel. The board tracks which components are complete.</p>
${CTA}`
},
{
  slug: 'remote-teams',
  title: 'amux for Remote Teams',
  description: 'How distributed teams use amux to run shared AI agents accessible from anywhere.',
  keywords: 'remote team AI tools, distributed development AI, shared AI agents, team coding agents',
  content: `<h1>amux for Remote Teams</h1>
<p class="subtitle">Shared agent dashboard accessible from anywhere.</p>
<p>Run amux on a shared server. Every team member accesses the dashboard via Tailscale or VPN. Agents work 24/7 across time zones — someone's always awake to check in.</p>
${CTA}`
},
{
  slug: 'monorepo-teams',
  title: 'amux for Monorepo Teams',
  description: 'How teams with monorepos use amux to assign one AI agent per package.',
  keywords: 'monorepo AI development, package-level agents, monorepo automation, parallel package development',
  content: `<h1>amux for Monorepo Teams</h1>
<p class="subtitle">One agent per package. Parallel development across your monorepo.</p>
<p>In a monorepo, each package has its own concerns. Assign agents per package — updates, tests, migrations — and coordinate via the shared board.</p>
${CTA}`
},
{
  slug: 'enterprise-engineering',
  title: 'amux for Enterprise Engineering',
  description: 'How enterprise teams use amux for large-scale automated coding and compliance.',
  keywords: 'enterprise AI coding, large scale automated development, corporate AI tools, enterprise agents',
  content: `<h1>amux for Enterprise Engineering</h1>
<p class="subtitle">Scale AI coding across dozens of services and teams.</p>
<p>Enterprise codebases have hundreds of services. amux lets you assign agents across services for consistent upgrades, security patches, and compliance fixes — tracked on a shared board.</p>
<h2>Local-first security</h2>
<p>amux runs on your infrastructure. No code leaves your network. No external services. Just Python 3 and tmux.</p>
${CTA}`
},
{
  slug: 'freelance-developers',
  title: 'amux for Freelance Developers',
  description: 'How freelancers use amux to handle multiple client projects with parallel AI agents.',
  keywords: 'freelance AI tools, freelancer productivity, multiple projects AI, parallel client work',
  content: `<h1>amux for Freelancers</h1>
<p class="subtitle">Work on multiple client projects simultaneously.</p>
<p>Register one session per client project. Agents work in parallel while you handle client communication and code review. Token tracking shows per-project costs.</p>
${CTA}`
},
{
  slug: 'ai-ml-engineers',
  title: 'amux for AI/ML Engineers',
  description: 'How AI/ML engineers use amux for parallel experiment running and model evaluation.',
  keywords: 'AI ML development tools, parallel ML experiments, model evaluation agents, ML automation',
  content: `<h1>amux for AI/ML Engineers</h1>
<p class="subtitle">Run experiments in parallel. Evaluate multiple approaches at once.</p>
<p>Use conversation forking to try different model architectures from the same base. Each agent evaluates a different approach, runs benchmarks, and reports results to the board.</p>
${CTA}`
},
];

// ═══════════════════════════════════════════════════════════════════
// BLOG (15 pages)
// ═══════════════════════════════════════════════════════════════════
const blog = [
{
  slug: 'ship-features-while-you-sleep',
  title: 'Ship Features While You Sleep',
  description: 'How to run AI coding agents overnight with self-healing to wake up to completed features.',
  keywords: 'AI coding overnight, ship while sleeping, unattended AI development, autonomous coding',
  content: `<h1>Ship Features While You Sleep</h1>
<p class="subtitle">The economics of autonomous overnight development.</p>
<p>A developer reported waking up to a fully tested REST API. Total cost: $3.47 in API tokens. The catch? It only works if your agents don't crash at 3am.</p>
<h2>The reliability problem</h2>
<p>Claude Code sessions fail in predictable ways: context compaction crashes, thinking-block corruption, and stuck tool-approval prompts. Without recovery, your overnight agent is dead by midnight.</p>
<h2>Self-healing changes the equation</h2>
<p>amux's watchdog handles every known failure mode automatically. Context low? Auto-compact. Corruption? Restart and replay. Stuck prompt? Auto-respond. Your agents work through the night.</p>
<h2>The morning routine</h2>
<p>Check the board from your phone. See what shipped. Review PRs. Merge. Your overnight agents just saved you a day of work.</p>
${CTA}`
},
{
  slug: 'one-developer-five-person-team',
  title: 'How One Developer Replaces a 5-Person Team',
  description: 'The economics and workflow of using parallel AI agents to multiply individual output.',
  keywords: 'solo developer productivity, 10x developer AI, one person team, AI multiply output',
  content: `<h1>How One Developer Replaces a 5-Person Team</h1>
<p class="subtitle">It's not about replacing people. It's about removing bottlenecks.</p>
<p>A solo developer with 10 parallel agents can tackle 10 tasks simultaneously. The developer's job shifts from writing code to reviewing, directing, and making architectural decisions.</p>
<h2>The new workflow</h2>
<ol>
<li><strong>Architect:</strong> Design the system and break it into tasks</li>
<li><strong>Delegate:</strong> Post tasks to the board, start agents</li>
<li><strong>Review:</strong> Check agent output, provide feedback</li>
<li><strong>Merge:</strong> Integrate completed work</li>
</ol>
<h2>What agents handle well</h2>
<ul>
<li>Implementation from clear specs</li>
<li>Test generation</li>
<li>Refactoring and migrations</li>
<li>Documentation</li>
<li>Dependency updates</li>
</ul>
<h2>What still needs a human</h2>
<ul>
<li>System architecture decisions</li>
<li>User experience design</li>
<li>Business logic nuance</li>
<li>Final code review</li>
</ul>
${CTA}`
},
{
  slug: 'self-healing-pattern',
  title: 'The Self-Healing Agent Pattern',
  description: 'Design patterns for building AI agents that automatically recover from failures.',
  keywords: 'self-healing AI pattern, agent recovery, resilient AI agents, fault tolerant agents',
  content: `<h1>The Self-Healing Agent Pattern</h1>
<p class="subtitle">Why reliable agents need automatic recovery, not just error handling.</p>
<p>Traditional error handling assumes you can predict failure modes. Self-healing agents monitor for symptoms and apply corrective actions — even for failures you didn't anticipate.</p>
<h2>amux's approach</h2>
<p>The watchdog runs on every status check cycle, parsing terminal output for known patterns. When it detects a problem, it applies the least-disruptive fix.</p>
${CTA}`
},
{
  slug: 'why-claude-code-sessions-crash',
  title: 'Why Claude Code Sessions Crash (and How to Fix It)',
  description: 'Common Claude Code failure modes and how amux\'s self-healing handles each one automatically.',
  keywords: 'claude code crashes, claude code context error, claude code stuck, claude code recovery',
  content: `<h1>Why Claude Code Sessions Crash</h1>
<p class="subtitle">And how amux fixes each one automatically.</p>
<h2>Context compaction failures</h2>
<p>When Claude Code's context window fills up, it attempts to compact. This can hang or crash. amux proactively triggers <code>/compact</code> before the window fills, with a 5-minute cooldown to prevent loops.</p>
<h2>Thinking-block corruption</h2>
<p>A known failure where Claude Code produces corrupted thinking blocks. amux detects the error message, restarts the session, and replays the last user message.</p>
<h2>Stuck prompts</h2>
<p>Tool-approval prompts block the session waiting for human input. In YOLO mode, amux auto-responds to these (detected via the "Esc to cancel" marker).</p>
${CTA}`
},
{
  slug: 'scaling-parallel-development',
  title: 'From 1 Agent to 30: Scaling Parallel Development',
  description: 'Lessons learned from scaling AI agent fleets from a single session to dozens.',
  keywords: 'scale parallel AI agents, grow agent fleet, AI development scaling, many agents parallel',
  content: `<h1>From 1 Agent to 30</h1>
<p class="subtitle">Lessons from scaling an AI agent fleet.</p>
<h2>1-3 agents: Manual management works</h2>
<p>You can manage a few sessions with tmux directly. But you'll lose track of what each one is doing.</p>
<h2>5-10 agents: You need a dashboard</h2>
<p>At this point, you can't keep everything in your head. amux's dashboard shows live status, token spend, and current task for every agent.</p>
<h2>10-30 agents: You need coordination</h2>
<p>Without a task board and atomic claiming, agents duplicate work. amux's kanban board with SQLite CAS ensures no wasted effort.</p>
${CTA}`
},
{
  slug: 'agent-to-agent-communication',
  title: 'The Case for Agent-to-Agent Communication',
  description: 'Why AI coding agents need to communicate with each other and how amux enables it.',
  keywords: 'agent communication, multi-agent communication, AI agent collaboration, inter-agent API',
  content: `<h1>Agent-to-Agent Communication</h1>
<p class="subtitle">Agents that talk to each other accomplish more than isolated agents.</p>
<p>amux gives every agent the REST API reference in global memory. Agents can send messages to peers, peek at each other's output, and claim work from a shared board — all via HTTP.</p>
${CTA}`
},
{
  slug: 'mobile-first-agent-management',
  title: 'Mobile-First AI Agent Management',
  description: 'Why managing AI agents from your phone matters and how amux\'s PWA enables it.',
  keywords: 'mobile AI management, PWA AI dashboard, manage agents phone, mobile agent monitoring',
  content: `<h1>Mobile-First Agent Management</h1>
<p class="subtitle">Your agents work 24/7. You should be able to check on them from anywhere.</p>
<p>amux's PWA works on iOS and Android. Check agent status, peek at output, send commands, and manage the board — all from your phone. Offline support means queued commands replay when you reconnect.</p>
${CTA}`
},
{
  slug: 'token-economics-parallel-coding',
  title: 'Token Economics of Parallel AI Coding',
  description: 'Understanding the cost structure of running multiple AI coding agents in parallel.',
  keywords: 'AI coding cost, token economics, parallel agent cost, claude code pricing',
  content: `<h1>Token Economics of Parallel AI Coding</h1>
<p class="subtitle">What it costs to run 10 agents for a day.</p>
<p>At current Claude API prices, a typical coding agent uses $2-10 worth of tokens per day depending on task complexity and model choice. Running 10 agents costs $20-100/day — far less than developer salaries for equivalent work.</p>
<h2>Cost optimization</h2>
<ul>
<li>Use Sonnet for routine tasks (tests, docs, migrations)</li>
<li>Reserve Opus for complex architectural work</li>
<li>amux's per-session tracking shows where your tokens go</li>
</ul>
${CTA}`
},
{
  slug: 'end-of-single-threaded-development',
  title: 'The End of Single-Threaded Development',
  description: 'Why sequential development is a bottleneck and how parallel AI agents change the equation.',
  keywords: 'parallel development, sequential vs parallel coding, AI developer productivity, future of coding',
  content: `<h1>The End of Single-Threaded Development</h1>
<p class="subtitle">You wouldn't run one thread on a 16-core machine. Why run one coding agent?</p>
<p>Developers have always worked sequentially — one task at a time. AI agents change this. Like going from single-threaded to multi-threaded execution, parallel agents dramatically increase throughput.</p>
<h2>The mental model shift</h2>
<p>Your job stops being "write code" and becomes "direct agents and review output." It's a different skill set — more like project management than programming.</p>
${CTA}`
},
{
  slug: 'context-compaction-silent-killer',
  title: 'Context Compaction: The Silent Killer',
  description: 'Why context compaction is the #1 reason AI coding agents fail and how to prevent it.',
  keywords: 'context compaction failure, claude code context, AI agent crashes, context window management',
  content: `<h1>Context Compaction: The Silent Killer</h1>
<p class="subtitle">The most common way AI agents die — and how to prevent it.</p>
<p>When an AI agent's context window fills up, it must compact (summarize) its history to continue. This process frequently crashes or hangs, silently killing your agent.</p>
<h2>How amux prevents this</h2>
<p>The watchdog monitors context usage and proactively triggers compaction before the window fills. A 5-minute cooldown prevents compaction loops. If compaction crashes, the session restarts with the last message replayed.</p>
${CTA}`
},
{
  slug: 'running-ai-agents-24-7',
  title: 'Running AI Agents 24/7',
  description: 'Architecture and best practices for running Claude Code agents continuously.',
  keywords: 'AI agents 24/7, continuous AI coding, always-on agents, persistent AI development',
  content: `<h1>Running AI Agents 24/7</h1>
<p class="subtitle">Continuous operation requires self-healing, not just auto-start.</p>
<p>Running agents continuously isn't hard — keeping them productive is. amux's self-healing addresses every failure mode that would otherwise require manual intervention.</p>
${CTA}`
},
{
  slug: 'developers-guide-unattended-coding',
  title: 'A Developer\'s Guide to Unattended Coding',
  description: 'Practical guide to running AI coding agents without constant supervision.',
  keywords: 'unattended AI coding, autonomous development, headless coding agents, no supervision coding',
  content: `<h1>A Developer's Guide to Unattended Coding</h1>
<p class="subtitle">Set it up right, and you only check in once a day.</p>
<h2>Prerequisites for unattended operation</h2>
<ol>
<li><strong>YOLO mode:</strong> Auto-approve tool-use prompts</li>
<li><strong>Self-healing:</strong> Recover from crashes automatically</li>
<li><strong>Task board:</strong> Agents know what to work on</li>
<li><strong>Git isolation:</strong> Agents don't conflict with each other</li>
</ol>
<p>amux provides all four. Register sessions with <code>--yolo</code>, and the watchdog handles the rest.</p>
${CTA}`
},
{
  slug: 'kanban-board-ai-agents-use',
  title: 'The Kanban Board Your AI Agents Actually Use',
  description: 'How amux\'s shared board enables real task coordination between AI coding agents.',
  keywords: 'AI agent kanban, agent task management, automated kanban, agent coordination board',
  content: `<h1>The Kanban Board Your AI Agents Actually Use</h1>
<p class="subtitle">Not a human board with AI features. A board designed for agents.</p>
<p>Most kanban tools are built for humans. amux's board is built for agents — with atomic task claiming via SQLite CAS, auto-generated issue keys, and a REST API that agents call directly.</p>
${CTA}`
},
{
  slug: 'open-source-agent-orchestration',
  title: 'Why Open Source Agent Orchestration Matters',
  description: 'The case for open-source, self-hosted AI agent orchestration vs. proprietary platforms.',
  keywords: 'open source AI agents, self-hosted agents, agent orchestration open source, local AI tools',
  content: `<h1>Why Open Source Agent Orchestration Matters</h1>
<p class="subtitle">Your code stays on your machine. Your workflow stays under your control.</p>
<p>Proprietary agent platforms lock you in. amux is MIT-licensed, runs on your machine, and has zero external dependencies beyond Python and tmux. You own your setup completely.</p>
${CTA}`
},
{
  slug: 'building-agent-fleet',
  title: 'Building Your First Agent Fleet',
  description: 'Step-by-step guide to setting up your first fleet of parallel AI coding agents with amux.',
  keywords: 'build agent fleet, first parallel agents, agent fleet setup, amux fleet guide',
  content: `<h1>Building Your First Agent Fleet</h1>
<p class="subtitle">From zero to a fleet of 10 agents in 15 minutes.</p>
<h2>Step 1: Install</h2>
<pre><code>git clone https://github.com/mixpeek/amux && cd amux && ./install.sh</code></pre>
<h2>Step 2: Register sessions</h2>
<pre><code>amux register auth --dir ~/Dev/app --yolo
amux register api --dir ~/Dev/app --yolo
amux register tests --dir ~/Dev/app --yolo</code></pre>
<h2>Step 3: Start the fleet</h2>
<pre><code>amux start auth && amux start api && amux start tests
amux serve</code></pre>
<h2>Step 4: Assign work</h2>
<p>Open the dashboard, post tasks to the board, and let agents claim them. Monitor progress from the web UI or your phone.</p>
${CTA}`
},
];

// ═══════════════════════════════════════════════════════════════════
// FEATURES (5 pages)
// ═══════════════════════════════════════════════════════════════════
const features = [
{
  slug: 'self-healing',
  title: 'Self-Healing AI Agents',
  description: 'amux\'s self-healing watchdog automatically recovers from crashes, context exhaustion, and stuck states.',
  keywords: 'self-healing AI agent, auto recovery, context compaction, agent watchdog, claude code crash fix',
  content: `<h1>Self-Healing AI Agents</h1>
<p class="subtitle">The #1 reason to use amux: your agents never silently die.</p>
<p>Claude Code sessions fail in predictable ways. amux's background watchdog detects every known failure mode and recovers automatically — no human intervention needed.</p>
<h2>Failure modes and responses</h2>
<table>
<thead><tr><th>Failure</th><th>Detection</th><th>Recovery</th></tr></thead>
<tbody>
<tr><td>Context exhaustion</td><td>Context below 20%</td><td>Auto-send <code>/compact</code></td></tr>
<tr><td>Thinking-block corruption</td><td>Error message pattern</td><td>Restart + replay last message</td></tr>
<tr><td>Tool-approval stuck</td><td>"Esc to cancel" marker</td><td>Auto-respond (YOLO mode)</td></tr>
<tr><td>Silent hang</td><td>No output for extended period</td><td>Continue signal</td></tr>
</tbody>
</table>
${CTA}`
},
{
  slug: 'web-dashboard',
  title: 'Web Dashboard',
  description: 'amux\'s web dashboard for monitoring AI agents — session cards, peek mode, workspace, board, and reports.',
  keywords: 'AI agent dashboard, agent monitoring UI, web dashboard agents, session management UI',
  content: `<h1>Web Dashboard</h1>
<p class="subtitle">Everything you need to manage a fleet of AI agents from your browser.</p>
<ul>
<li><strong>Session cards</strong> — live status, token spend, model, current task</li>
<li><strong>Peek mode</strong> — full scrollback, search, file previews, send bar</li>
<li><strong>Workspace</strong> — tiled layout for watching multiple agents</li>
<li><strong>Board</strong> — kanban with atomic claiming and iCal sync</li>
<li><strong>Calendar</strong> — due dates and scheduled commands</li>
<li><strong>Reports</strong> — spend dashboards from vendor billing APIs</li>
</ul>
${CTA}`
},
{
  slug: 'agent-coordination',
  title: 'Agent Coordination',
  description: 'How amux agents discover peers, delegate tasks, and coordinate work via REST API and shared board.',
  keywords: 'agent coordination, multi-agent orchestration, agent REST API, task delegation AI',
  content: `<h1>Agent Coordination</h1>
<p class="subtitle">Agents discover peers and delegate work automatically.</p>
<p>Every agent gets <code>$AMUX_URL</code> and <code>$AMUX_SESSION</code> at startup. Global memory contains the full API reference. Agents coordinate via HTTP — no special framework needed.</p>
<h2>Coordination primitives</h2>
<ul>
<li><strong>Send message:</strong> <code>POST /api/sessions/:name/send</code></li>
<li><strong>Peek output:</strong> <code>GET /api/sessions/:name/peek</code></li>
<li><strong>Claim task:</strong> <code>POST /api/board/:id/claim</code></li>
<li><strong>Read memory:</strong> <code>GET /api/memory/global</code></li>
</ul>
${CTA}`
},
{
  slug: 'single-file-architecture',
  title: 'Single-File Architecture',
  description: 'Why amux is a single Python file with inline HTML/CSS/JS — and why that matters.',
  keywords: 'single file architecture, minimal dependencies, simple deployment, python server',
  content: `<h1>Single-File Architecture</h1>
<p class="subtitle">~12,000 lines. One file. Zero build step.</p>
<p>amux is a single Python file (<code>amux-server.py</code>) with the full web dashboard embedded as inline HTML/CSS/JS. Edit it, and it restarts automatically via <code>os.execv</code>.</p>
<h2>Why single-file?</h2>
<ul>
<li><strong>Zero build step:</strong> No npm, no webpack, no compilation</li>
<li><strong>Easy to deploy:</strong> Copy one file, run it</li>
<li><strong>Easy to modify:</strong> Everything is in one place</li>
<li><strong>Minimal dependencies:</strong> Just Python 3 and tmux</li>
</ul>
${CTA}`
},
{
  slug: 'mobile-pwa',
  title: 'Mobile PWA',
  description: 'Install amux as a Progressive Web App on your phone for mobile agent management.',
  keywords: 'amux PWA, mobile AI management, progressive web app, iOS Android agents',
  content: `<h1>Mobile PWA</h1>
<p class="subtitle">Install amux on your phone. Manage agents from anywhere.</p>
<p>amux's dashboard is a full Progressive Web App. Install it on iOS or Android for native-like access to your agent fleet. Background Sync replays queued commands when you reconnect.</p>
<h2>Capabilities</h2>
<ul>
<li>View live session statuses</li>
<li>Peek into agent terminal output</li>
<li>Send commands and prompts</li>
<li>Manage the kanban board</li>
<li>Works offline — commands queue and replay</li>
</ul>
${CTA}`
},
];

// ═══════════════════════════════════════════════════════════════════
// FAQ (1 page)
// ═══════════════════════════════════════════════════════════════════
const faqItems = [
  ['What is amux?', 'amux is an open-source agent multiplexer that runs dozens of Claude Code sessions in parallel from a web dashboard. It adds self-healing, task coordination, and mobile access on top of Claude Code + tmux.'],
  ['What do I need to run amux?', 'Python 3, tmux, and Claude Code (the <code>claude</code> CLI) installed and authenticated. That\'s it.'],
  ['How many agents can I run?', 'As many as your machine can handle. Each agent uses ~200-400MB RAM. On a 32GB machine, 50+ agents is practical.'],
  ['Does amux modify Claude Code?', 'No. amux manages Claude Code sessions externally via tmux. It parses terminal output to detect status — no hooks, patches, or modifications.'],
  ['What happens when an agent crashes?', 'amux\'s self-healing watchdog detects crashes and recovers automatically. Context exhaustion triggers auto-compaction. Thinking-block corruption triggers restart with message replay. Stuck prompts get auto-answered in YOLO mode.'],
  ['Can I use amux from my phone?', 'Yes. The dashboard is a PWA — install it on iOS or Android. Works offline with Background Sync.'],
  ['How do agents coordinate?', 'Via REST API. Every agent gets the API reference in global memory. They can send messages to peers, claim tasks from the board, and peek at each other\'s output.'],
  ['What does it cost?', 'amux is free and open source (MIT). You pay only for Claude API tokens. Running 10 agents typically costs $20-100/day depending on task complexity.'],
  ['Is it secure?', 'amux is local-first with no auth built in. Use Tailscale or bind to localhost. Never expose port 8822 to the internet.'],
  ['Can I use it with models other than Claude?', 'amux is specifically built for Claude Code (the <code>claude</code> CLI). It relies on Claude Code\'s specific terminal output patterns for status detection and self-healing.'],
  ['How is amux different from Claude Code Agent Teams?', 'Agent Teams spawns short-lived sub-agents from a parent. amux manages long-running independent sessions with a web dashboard, self-healing, and shared task board. <a href="/compare/amux-vs-claude-code-agent-teams/">See detailed comparison</a>.'],
  ['Can I self-host it?', 'Yes — that\'s the only way to run it. amux runs on your own machine or server. No cloud service, no account needed.'],
];

const faqPage = {
  slug: 'faq',
  title: 'Frequently Asked Questions',
  description: 'Common questions about amux — the open-source Claude Code agent multiplexer.',
  keywords: 'amux FAQ, amux questions, claude code multiplexer FAQ, parallel AI agents FAQ',
  content: `<h1>Frequently Asked Questions</h1>
<p class="subtitle">Everything you need to know about amux.</p>
${faqItems.map(([q, a]) => `<h2>${q}</h2><p>${a}</p>`).join('\n')}
${CTA}`,
  schema: {
    '@context': 'https://schema.org',
    '@type': 'FAQPage',
    mainEntity: faqItems.map(([q, a]) => ({
      '@type': 'Question', name: q,
      acceptedAnswer: { '@type': 'Answer', text: a.replace(/<[^>]+>/g, '') }
    }))
  }
};


// ═══════════════════════════════════════════════════════════════════
// GEO PAGES — CITY PAGES (15)
// ═══════════════════════════════════════════════════════════════════
const geoPages = [
{
  slug: 'san-francisco',
  title: 'AI Coding Agents for San Francisco Developers',
  description: 'Run parallel Claude Code agents tailored for the SF Bay Area engineering culture — fast iteration, high autonomy, overnight runs.',
  keywords: 'san francisco AI coding, bay area claude code, parallel agents san francisco',
  content: `<h1>AI Coding Agents for San Francisco Developers</h1>
<p class="subtitle">The Bay Area moves fast. amux lets your codebase keep up by running dozens of Claude Code agents in parallel, unattended.</p>
<h2>The SF Engineering Context</h2>
<p>San Francisco is home to some of the densest concentrations of software engineers in the world — and some of the most aggressive shipping cultures. Whether you're at a Series A startup in SoMa, a growth-stage company in Mission Bay, or building solo from a Castro co-working space, the pressure to ship is constant. amux is built for exactly this environment.</p>
<p>With companies like Anthropic, OpenAI, Stripe, Figma, Notion, and hundreds of startups all within a few miles, SF developers are among the earliest adopters of AI coding tools. amux is the next step: not just one AI assistant, but a fleet of them working in parallel while you sleep.</p>
<h2>Workflows That Make Sense Here</h2>
<ul>
<li><strong>End-of-day agent launches:</strong> Queue up 10–15 tasks before heading to dinner in the Mission. By the time you're back, agents have drafted implementations, written tests, and opened PRs.</li>
<li><strong>Sprint compression:</strong> SF startups often have 2-week sprints. Run parallel agents on every ticket simultaneously — finish in 3 days.</li>
<li><strong>Monorepo coverage:</strong> Many SF companies run large monorepos (Turborepo, Nx). Assign one agent per package, coordinate via the shared kanban board.</li>
<li><strong>On-call automation:</strong> Assign an agent to triage incoming GitHub issues overnight and draft fix PRs by morning standup.</li>
</ul>
<h2>What amux Gives You</h2>
<table>
<thead><tr><th>Need</th><th>amux solution</th></tr></thead>
<tbody>
<tr><td>Run agents overnight</td><td>Self-healing watchdog restarts crashes automatically</td></tr>
<tr><td>Monitor from anywhere</td><td>PWA dashboard works on iPhone from BART</td></tr>
<tr><td>Coordinate across sessions</td><td>Shared SQLite kanban with atomic task claiming</td></tr>
<tr><td>Stay in flow</td><td>Delegate entire feature branches to agent fleets</td></tr>
</tbody>
</table>
<h2>Setup in 2 Minutes</h2>
<p>No cloud account, no subscription. Runs on your MacBook or any Linux box. Just Python 3 + tmux.</p>
${CTA}`,
},
{
  slug: 'new-york',
  title: 'AI Coding Agents for New York Developers',
  description: 'NYC developers ship across fintech, media, and adtech. amux runs parallel Claude Code agents so you can scale output without scaling headcount.',
  keywords: 'new york AI coding, NYC claude code agents, parallel development new york',
  content: `<h1>AI Coding Agents for New York Developers</h1>
<p class="subtitle">New York never sleeps. Neither do your amux agents.</p>
<h2>NYC's Engineering Landscape</h2>
<p>New York has one of the most diverse tech ecosystems in the world: fintech in Midtown, adtech in Flatiron, media-tech in Hudson Square, healthcare-tech in Murray Hill, and a dense startup scene spread across Manhattan, Brooklyn, and Queens. The common thread: shipping pressure, high standards, and small-to-mid-sized engineering teams doing the work of teams twice their size.</p>
<p>amux is designed for exactly this kind of environment — where one senior engineer might be responsible for an entire product surface, and the margin for wasted hours is thin.</p>
<h2>Workflows That Work in NYC</h2>
<ul>
<li><strong>Finance and compliance code:</strong> Run one agent per regulatory requirement — SOC2 audit prep, GDPR compliance checks, data retention policies — all in parallel.</li>
<li><strong>Agency-style delivery:</strong> NYC has a huge consulting and agency scene. amux lets a 2-person team deliver like a 10-person team — parallel agents per client workstream.</li>
<li><strong>Overnight batch runs:</strong> Launch agents at midnight EST, coordinate with SF teammates who are still at their desks 3 hours behind you.</li>
<li><strong>Test coverage blitzes:</strong> Assign 10 agents to write unit tests across a legacy codebase. Done before morning standup.</li>
</ul>
<h2>Timezone Advantage</h2>
<p>Being on EST means you're 3 hours ahead of SF. Launch agent fleets at 6pm your time and your West Coast teammates see the output in their morning. amux's self-healing watchdog keeps agents running overnight without babysitting.</p>
${CTA}`,
},
{
  slug: 'seattle',
  title: 'AI Coding Agents for Seattle Developers',
  description: 'Seattle engineers at Amazon, Microsoft, and startups use parallel agent fleets to move faster. Here is how amux fits the Seattle dev workflow.',
  keywords: 'seattle AI coding, seattle developer tools, claude code seattle',
  content: `<h1>AI Coding Agents for Seattle Developers</h1>
<p class="subtitle">Home to Amazon and Microsoft — Seattle developers know how to build at scale. amux brings that mindset to AI coding.</p>
<h2>Seattle's Engineering Culture</h2>
<p>Seattle is defined by systems-scale thinking. Amazon's leadership principles, Microsoft's engineering culture, and a thriving independent startup scene (Stripe, Tableau, Redfin, Outreach) all share a focus on operational excellence. Engineers here aren't afraid of complexity — they want tools that match their ambition.</p>
<p>amux fits naturally: it's a systems-level tool for AI coding. Instead of one chatbot, you get a coordinated fleet of agents with a REST API, shared task board, and self-healing infrastructure.</p>
<h2>Workflows for Seattle Developers</h2>
<ul>
<li><strong>Microservices coverage:</strong> Seattle companies often run sprawling microservice architectures. Assign one amux agent per service for parallel refactoring, migration, or test generation.</li>
<li><strong>AWS integration work:</strong> Let agents handle the boilerplate — IAM policies, CloudFormation templates, Lambda handlers — while you focus on architecture.</li>
<li><strong>On-call automation:</strong> Use amux to maintain an always-on agent that watches error logs and generates fix PRs for known error patterns.</li>
<li><strong>Documentation blitzes:</strong> Run 5 agents across 5 services to generate API docs, README files, and runbooks simultaneously.</li>
</ul>
<h2>Rainy Day Productivity</h2>
<p>Seattle averages 150 rainy days a year. Put them to work: launch an agent fleet on a Friday afternoon, let it run through the weekend, and review the PRs Monday morning with coffee.</p>
${CTA}`,
},
{
  slug: 'austin',
  title: 'AI Coding Agents for Austin Developers',
  description: 'Austin is the fastest-growing tech hub in the US. amux helps Austin developers move at startup speed with parallel Claude Code agents.',
  keywords: 'austin AI coding, texas developer tools, claude code austin',
  content: `<h1>AI Coding Agents for Austin Developers</h1>
<p class="subtitle">Keep Austin Weird. Ship Fast. Use parallel agents.</p>
<h2>Austin's Tech Scene</h2>
<p>Austin has become one of the most dynamic tech cities in the US. Tesla, Apple, Google, Oracle, and Dell all have major Austin presences, alongside a thriving startup ecosystem around East 6th, the Domain, and South Congress. The city has attracted waves of engineers from SF, NYC, and Seattle — people who want to build seriously but live better.</p>
<p>The engineering culture in Austin values autonomy, ownership, and moving fast without bureaucracy. amux is a natural fit: it's a zero-cloud, local-first tool that gives you full control.</p>
<h2>Austin Developer Workflows</h2>
<ul>
<li><strong>Startup velocity:</strong> Austin startups punch above their weight. A 4-person engineering team using amux can execute like a 15-person team — parallel agents per feature, per test suite, per customer integration.</li>
<li><strong>Hardware/embedded adjacent work:</strong> Austin has strong hardware and defense-tech communities. Use agents for firmware documentation, protocol parsers, test harnesses, and simulation code.</li>
<li><strong>CST timezone advantage:</strong> Launch agent fleets at 5pm CST and they run 2 hours while your SF counterparts are still at their desks, then overnight as the whole country sleeps.</li>
<li><strong>Remote team coordination:</strong> Many Austin companies are remote-first. Use amux's REST API to coordinate agents across timezones — task board, session peeks, and message passing via HTTP.</li>
</ul>
<h2>No Cloud Required</h2>
<p>amux is local-first: runs on your own machine, no SaaS subscription, no data leaving your network. Perfect for Austin companies working on proprietary or regulated code.</p>
${CTA}`,
},
{
  slug: 'london',
  title: 'AI Coding Agents for London Developers',
  description: 'London developers in fintech, SaaS, and deep tech use amux to run parallel Claude Code agents — overnight and across timezones.',
  keywords: 'london AI coding, UK claude code, parallel agents london',
  content: `<h1>AI Coding Agents for London Developers</h1>
<p class="subtitle">GMT is a superpower. Launch agents in the evening and your US teammates wake up to completed work.</p>
<h2>London's Tech Ecosystem</h2>
<p>London is Europe's leading tech hub: Monzo, Revolut, DeepMind, Babylon Health, Checkout.com, and hundreds of enterprise SaaS companies call it home. The City's fintech scene is world-class, and the broader startup ecosystem in Shoreditch, King's Cross, and Canary Wharf is among the most sophisticated in the world.</p>
<p>London engineers often work across US and EU timezones — collaborating with SF, NYC, and Berlin simultaneously. amux turns this timezone overlap into an advantage: launch agents at 6pm GMT and they run while your US colleagues are still at lunch.</p>
<h2>Workflows That Work in London</h2>
<ul>
<li><strong>Fintech compliance:</strong> FCA regulations, PSD2, GDPR — use parallel agents to generate compliance documentation, audit trails, and policy implementations simultaneously.</li>
<li><strong>Cross-timezone delivery:</strong> London teams often act as the bridge between EU and US. Run agents overnight (GMT) so both sides wake up to completed work.</li>
<li><strong>Legacy modernisation:</strong> UK enterprises carry a lot of legacy Java and C# code. Use agent fleets to migrate, document, and test-cover it in parallel.</li>
<li><strong>API integration sprints:</strong> Open Banking requires connecting to dozens of bank APIs. One agent per integration, running simultaneously.</li>
</ul>
<h2>GDPR-Safe by Design</h2>
<p>amux is local-first — your code never leaves your infrastructure. No third-party SaaS, no cloud storage of source code. Compliant by architecture.</p>
${CTA}`,
},
{
  slug: 'berlin',
  title: 'AI Coding Agents for Berlin Developers',
  description: 'Berlin developers building deep tech, open source, and SaaS use amux to run unattended Claude Code agent fleets.',
  keywords: 'berlin AI coding, german developer tools, claude code berlin',
  content: `<h1>AI Coding Agents for Berlin Developers</h1>
<p class="subtitle">Berlin ships open source, SaaS, and deep tech. amux fits the Berlin ethos: self-hosted, open, powerful.</p>
<h2>Berlin's Developer Culture</h2>
<p>Berlin has a distinctive engineering culture: pragmatic, open-source-friendly, skeptical of lock-in, and deeply serious about privacy. Companies like Zalando, SoundCloud, N26, HelloFresh, and GetYourGuide have shaped a scene that values technical depth over hype. Berlin developers tend to want tools they can run themselves, inspect, and trust.</p>
<p>amux is MIT-licensed, single-file Python, and entirely self-hosted. There's no SaaS layer, no telemetry, no cloud dependency. Just clone it, run it, own it.</p>
<h2>Berlin Developer Workflows</h2>
<ul>
<li><strong>Open source contribution sprints:</strong> Use agent fleets to generate PRs, write documentation, and add test coverage to open source projects simultaneously.</li>
<li><strong>GDPR-native development:</strong> Berlin teams build privacy-by-design from day one. Run agents to audit codebase for GDPR issues, generate data-flow documentation, and implement consent flows.</li>
<li><strong>Deep tech and research:</strong> Berlin has a strong ML/AI research community. Use amux to run parallel implementation agents for research prototypes.</li>
<li><strong>Event-driven architecture:</strong> Many Berlin companies run Kafka-based event systems. Use agents to generate consumers, producers, schema definitions, and integration tests in parallel.</li>
</ul>
<h2>CET Timezone</h2>
<p>CET gives Berlin developers a 6-9 hour head start on US timezones. Launch agents at end-of-day and your US collaborators find completed work in their morning Slack.</p>
${CTA}`,
},
{
  slug: 'toronto',
  title: 'AI Coding Agents for Toronto Developers',
  description: 'Toronto has one of the strongest AI research scenes in the world. amux helps Toronto developers build with parallel Claude Code agents.',
  keywords: 'toronto AI coding, canada claude code, parallel agents toronto',
  content: `<h1>AI Coding Agents for Toronto Developers</h1>
<p class="subtitle">Toronto is a global AI research hub. amux brings parallel agent orchestration to your local workflow.</p>
<h2>Toronto's Tech Scene</h2>
<p>Toronto is home to the Vector Institute, the University of Toronto's deep learning lab (where Geoffrey Hinton did foundational work), and a world-class AI research community. Companies like Shopify, Wattpad, Hootsuite, and Cohere call Toronto home. The city's developer scene is technically rigorous and internationally connected.</p>
<p>Toronto developers often work on complex systems: e-commerce platforms, ML infrastructure, financial services. amux is designed for exactly this kind of multi-faceted, long-running development work.</p>
<h2>Toronto Developer Workflows</h2>
<ul>
<li><strong>E-commerce scale:</strong> Toronto is home to Shopify. Use agent fleets to build and test Shopify apps, themes, and custom storefronts — one agent per integration point.</li>
<li><strong>ML pipeline automation:</strong> Use agents to generate data loaders, model training scripts, evaluation harnesses, and documentation for ML projects.</li>
<li><strong>Bilingual codebases:</strong> Many Canadian companies serve both English and French markets. Use agents to generate i18n implementations and translated content simultaneously.</li>
<li><strong>EST/EDT coordination:</strong> Toronto runs on the same timezone as NYC — use that alignment for coordinated overnight agent runs with US-based teams.</li>
</ul>
<h2>University Pipeline</h2>
<p>U of T, Waterloo, and McGill produce exceptional engineering talent. Many Toronto developers are recent grads comfortable with AI tooling. amux is designed to be approachable — single Python file, no build step, a clean web dashboard.</p>
${CTA}`,
},
{
  slug: 'singapore',
  title: 'AI Coding Agents for Singapore Developers',
  description: 'Singapore is Southeast Asia\'s tech hub. amux lets Singapore developers run parallel Claude Code agents overnight to serve global markets.',
  keywords: 'singapore AI coding, southeast asia developer tools, claude code singapore',
  content: `<h1>AI Coding Agents for Singapore Developers</h1>
<p class="subtitle">SGT puts you 8–13 hours ahead of European and US markets. Run agents while the world sleeps.</p>
<h2>Singapore's Tech Ecosystem</h2>
<p>Singapore is the financial and tech gateway to Southeast Asia. Grab, Sea Group, Lazada, Razer, and hundreds of global companies have major engineering presence here. The government's Smart Nation initiative and a robust startup funding scene have made Singapore one of the most active tech environments in Asia.</p>
<p>Singapore developers often build for multiple Southeast Asian markets simultaneously — Indonesia, Thailand, Vietnam, the Philippines — each with different regulatory requirements, payment systems, and user behaviors. Parallel agents are a natural fit.</p>
<h2>Singapore Developer Workflows</h2>
<ul>
<li><strong>Multi-market localization:</strong> Run one agent per market to generate localized payment flows, legal text, and UI copy simultaneously across your SEA markets.</li>
<li><strong>Fintech compliance:</strong> MAS regulations, cross-border payments, digital banking licenses — use agents to generate compliance documentation and audit-ready code in parallel.</li>
<li><strong>Global overnight runs:</strong> Launch agent fleets at midnight SGT. By the time US markets open, the work is done. Your Singapore team reviews it over morning kopi.</li>
<li><strong>Super-app architecture:</strong> SEA companies often build super-apps with dozens of mini-apps. One agent per mini-app for parallel development.</li>
</ul>
<h2>Infrastructure-Friendly</h2>
<p>Singapore has excellent cloud infrastructure (AWS ap-southeast-1, GCP). Run amux on a Singapore VM for low-latency Claude API calls, then manage your fleet from anywhere via the PWA.</p>
${CTA}`,
},
{
  slug: 'amsterdam',
  title: 'AI Coding Agents for Amsterdam Developers',
  description: 'Amsterdam developers building SaaS, adtech, and open source use amux for parallel Claude Code agent workflows.',
  keywords: 'amsterdam AI coding, netherlands developer tools, claude code amsterdam',
  content: `<h1>AI Coding Agents for Amsterdam Developers</h1>
<p class="subtitle">Amsterdam: global reach, pragmatic engineering, strong open-source culture. amux fits right in.</p>
<h2>Amsterdam's Tech Scene</h2>
<p>Amsterdam is home to Booking.com, TomTom, ASML (nearby), Adyen, and a dense cluster of SaaS and data companies. The Dutch engineering culture is famously pragmatic — practical solutions, minimal over-engineering, strong emphasis on code quality and testing.</p>
<p>Amsterdam also hosts one of the strongest open-source communities in Europe, with regular meetups, conferences (like Fosdem contributions), and companies that actively contribute upstream. amux is open-source, and that matters here.</p>
<h2>Amsterdam Developer Workflows</h2>
<ul>
<li><strong>A/B test infrastructure:</strong> Companies like Booking.com run thousands of A/B tests. Use agents to generate test implementations, analysis code, and documentation in parallel.</li>
<li><strong>Data engineering:</strong> Amsterdam has a strong data engineering scene. Use agent fleets to generate dbt models, Airflow DAGs, data quality checks, and documentation.</li>
<li><strong>Payment systems:</strong> Adyen's presence has made Amsterdam a payments engineering hub. Use agents to implement payment gateway integrations, generate test cases, and document flows.</li>
<li><strong>CET overnight runs:</strong> Same timezone advantage as Berlin — launch at end-of-day, review in the morning, stay a full working day ahead of your US counterparts.</li>
</ul>
<h2>Privacy by Default</h2>
<p>Dutch developers take GDPR seriously — it's not an afterthought, it's architecture. amux is local-first: source code stays on your infrastructure, never passes through a third-party cloud.</p>
${CTA}`,
},
{
  slug: 'sydney',
  title: 'AI Coding Agents for Sydney Developers',
  description: 'Sydney developers have a massive timezone advantage for overnight agent runs. amux keeps your Claude Code agents running while the world is asleep.',
  keywords: 'sydney AI coding, australia claude code, parallel agents sydney',
  content: `<h1>AI Coding Agents for Sydney Developers</h1>
<p class="subtitle">AEDT puts you 11–16 hours ahead of US markets. The overnight run isn't overnight — it's your workday.</p>
<h2>Sydney's Tech Scene</h2>
<p>Sydney is Australia's largest tech hub, home to Atlassian, Canva, Afterpay, SafetyCulture, and a growing fintech and SaaS scene. Australian developers are building globally competitive products from a timezone that gives them a natural async advantage: when Sydney is working, most of the world is asleep.</p>
<p>amux turns this into a productivity superpower. Run agent fleets during your normal working hours and ship completed work to your US and EU collaborators while they sleep.</p>
<h2>Sydney Developer Workflows</h2>
<ul>
<li><strong>Async-first global teams:</strong> Use amux's shared kanban board to coordinate work with US and EU teammates across timezones — agents claim tasks, complete them, and update the board without any synchronous handoff.</li>
<li><strong>Legacy modernization:</strong> Australian enterprises carry significant legacy infrastructure. Use agent fleets to migrate, refactor, and test-cover legacy systems in parallel.</li>
<li><strong>Atlassian ecosystem work:</strong> Sydney is Atlassian's home. Use agents to build Jira/Confluence plugins, Forge apps, and integrations in parallel.</li>
<li><strong>Canva-style design-system work:</strong> Use agents to generate component variants, accessibility fixes, and Storybook stories across a large design system simultaneously.</li>
</ul>
<h2>The Timezone Superpower</h2>
<p>A Sydney developer launching agents at 9am AEDT completes work by 5pm that a US developer would call "overnight." You're not waiting for agents — your agents are working your normal day, delivering to global teams around the clock.</p>
${CTA}`,
},
{
  slug: 'boston',
  title: 'AI Coding Agents for Boston Developers',
  description: 'Boston\'s deep academic and biotech engineering scene benefits from parallel agent workflows. amux for Boston developers.',
  keywords: 'boston AI coding, boston developer tools, claude code boston',
  content: `<h1>AI Coding Agents for Boston Developers</h1>
<p class="subtitle">Boston has MIT, Harvard, and one of the strongest biotech/healthtech engineering scenes in the world. amux helps you ship at research speed.</p>
<h2>Boston's Tech Ecosystem</h2>
<p>Boston is unusual: it combines world-class academic research (MIT, Harvard, Northeastern, BU) with a serious commercial engineering scene. Kendall Square is one of the densest biotech clusters in the world. Companies like HubSpot, DraftKings, Wayfair, Chewy, and hundreds of life-sciences companies employ engineers who move between academia and industry.</p>
<p>This means Boston developers often work on technically demanding, research-adjacent codebases: computational biology, clinical trial software, healthcare data pipelines, ML research tools. Parallel agents shine in exactly these contexts.</p>
<h2>Boston Developer Workflows</h2>
<ul>
<li><strong>Research prototype to production:</strong> Academic researchers spin up prototype implementations. Use agent fleets to refactor, test, document, and harden them for production simultaneously.</li>
<li><strong>Biotech data pipelines:</strong> Run one agent per data source — EHR integration, genomics pipeline, clinical trial ingestion — all in parallel.</li>
<li><strong>Compliance-heavy codebases:</strong> HIPAA, 21 CFR Part 11, FDA validation. Use agents to generate compliance documentation, audit logs, and validation test suites.</li>
<li><strong>EST overnight runs:</strong> Boston EST aligns well with EU morning. Launch agents at 8pm and your London or Berlin collaborators review the work in their morning.</li>
</ul>
<h2>Academic Open-Source Ethos</h2>
<p>Boston's engineering culture values rigor and transparency. amux is MIT-licensed, single-file Python, fully inspectable. No black boxes.</p>
${CTA}`,
},
{
  slug: 'chicago',
  title: 'AI Coding Agents for Chicago Developers',
  description: 'Chicago developers in fintech, logistics, and enterprise software use amux for parallel Claude Code agent workflows.',
  keywords: 'chicago AI coding, chicago developer tools, claude code chicago',
  content: `<h1>AI Coding Agents for Chicago Developers</h1>
<p class="subtitle">Chicago is the center of the US: enterprise software, fintech, logistics tech, and a strong indie dev scene. amux scales with all of it.</p>
<h2>Chicago's Tech Scene</h2>
<p>Chicago is the third-largest tech hub in the US, with strengths in fintech (Morningstar, Cboe, CME Group), logistics tech (Echo Global Logistics, project44), enterprise SaaS (Salesforce Chicago, Accenture, Braintree), and a growing startup ecosystem around Fulton Market and the Loop.</p>
<p>Chicago engineers often work on high-stakes, high-reliability systems: trading platforms, supply chain management, healthcare records. These codebases are large, complex, and require careful, parallel work.</p>
<h2>Chicago Developer Workflows</h2>
<ul>
<li><strong>Financial systems testing:</strong> Trading and fintech code requires exhaustive testing. Use agent fleets to generate unit tests, integration tests, and stress tests in parallel across a trading system.</li>
<li><strong>Logistics API integrations:</strong> Chicago logistics companies connect to dozens of carrier APIs. One agent per carrier integration, running simultaneously.</li>
<li><strong>Enterprise refactoring:</strong> Chicago has a lot of legacy Java, .NET, and Oracle code in large enterprises. Use agents to generate migration plans, refactored code, and test coverage in parallel.</li>
<li><strong>CST overnight advantage:</strong> CST puts Chicago 1-2 hours ahead of SF. Launch agent fleets at 5pm CST and they complete work while your West Coast teammates are still in afternoon meetings.</li>
</ul>
<h2>Midwest Reliability</h2>
<p>Chicago engineering culture values reliability over hype. amux's self-healing watchdog, atomic SQLite task claiming, and local-first architecture reflect the same values: it just works, without drama.</p>
${CTA}`,
},
{
  slug: 'los-angeles',
  title: 'AI Coding Agents for Los Angeles Developers',
  description: 'LA developers in media tech, gaming, and consumer apps use amux to run parallel Claude Code agents unattended.',
  keywords: 'los angeles AI coding, LA developer tools, claude code los angeles',
  content: `<h1>AI Coding Agents for Los Angeles Developers</h1>
<p class="subtitle">LA is where entertainment meets tech. amux helps developers in media, gaming, and consumer apps ship faster.</p>
<h2>LA's Tech Ecosystem</h2>
<p>Los Angeles has a unique tech identity shaped by entertainment (Snap, TikTok US, Netflix, Riot Games, Activision), consumer apps (SpaceX, Hims, Dollar Shave Club), and a growing deep-tech scene in Silicon Beach (Venice, Santa Monica, Playa Vista). LA developers often work on consumer-facing products at massive scale, with strict performance and reliability requirements.</p>
<p>The creative industry connection also means LA developers often collaborate with designers, product managers, and content teams — contexts where parallel agent execution (one agent per feature, one per component, one for tests) maps naturally to how creative teams work.</p>
<h2>LA Developer Workflows</h2>
<ul>
<li><strong>Game development:</strong> Games require massive parallel work: AI behavior, rendering, networking, physics, audio, UI. Assign agents to each subsystem.</li>
<li><strong>Streaming platform features:</strong> Media companies ship many features simultaneously. Use agents to build recommendation system integrations, player components, and analytics pipelines in parallel.</li>
<li><strong>Consumer app polish:</strong> LA consumer apps have high design standards. Use agents to generate component variants, implement design specs, and write accessibility tests simultaneously.</li>
<li><strong>PST late-night runs:</strong> LA's culture is known for late nights. Launch agent fleets at 11pm PST — they run while you're asleep and the results are ready for your East Coast stakeholders at 9am EST.</li>
</ul>
<h2>Scale Without Headcount</h2>
<p>LA startup culture is scrappy. A 3-person team using amux agent fleets can execute product roadmaps that would take a 15-person team. No VC required for headcount — just API tokens.</p>
${CTA}`,
},
{
  slug: 'remote-developers',
  title: 'AI Coding Agents for Remote Developers',
  description: 'Remote developers use amux to run parallel Claude Code agents across timezones — async-first, self-healing, mobile-accessible.',
  keywords: 'remote AI coding, remote developer tools, distributed team claude code',
  content: `<h1>AI Coding Agents for Remote Developers</h1>
<p class="subtitle">Remote developers already work async. amux makes your AI agents async too.</p>
<h2>The Remote Developer's Challenge</h2>
<p>Remote developers face a unique challenge: they often work across timezones, collaborate asynchronously, and can't always be at their desk when an agent needs attention. Traditional AI coding tools require constant supervision. amux is designed for the opposite: <strong>set it and forget it</strong>.</p>
<p>Whether you're a solo freelancer, part of a distributed startup, or a remote employee at a large company, amux gives you an agent fleet that works while you live your life.</p>
<h2>Features Built for Remote Workflows</h2>
<ul>
<li><strong>Mobile PWA:</strong> Monitor your agents from your phone — on a walk, at a coffee shop, from a different continent. The dashboard works on iOS and Android with offline support.</li>
<li><strong>Self-healing watchdog:</strong> Agents auto-recover from crashes, context exhaustion, and stuck prompts. No need to babysit.</li>
<li><strong>Shared kanban board:</strong> Coordinate work across timezones via a shared task board with atomic claiming. No Slack required for task handoff.</li>
<li><strong>REST API:</strong> Integrate amux into your existing async workflows — Zapier webhooks, GitHub Actions, custom scripts.</li>
<li><strong>iCal sync:</strong> Board items with due dates sync to Google Calendar or Apple Calendar — stay on top of deadlines from any device.</li>
</ul>
<h2>The Async Advantage</h2>
<p>Effective remote work is async work. amux extends that to AI coding: describe the task, assign it to an agent, and check back when it's done. No synchronous pair-programming sessions, no waiting for AI responses. Your agent fleet works independently, reports back via the board, and you review at your own pace.</p>
<table>
<thead><tr><th>Remote challenge</th><th>amux solution</th></tr></thead>
<tbody>
<tr><td>Agent crashes while you're asleep</td><td>Self-healing watchdog auto-restarts</td></tr>
<tr><td>Can't check desktop from phone</td><td>PWA dashboard with full mobile support</td></tr>
<tr><td>Task handoff across timezones</td><td>Shared SQLite kanban board</td></tr>
<tr><td>Need to peek at agent output</td><td>Live peek via web dashboard</td></tr>
</tbody>
</table>
${CTA}`,
},
{
  slug: 'india',
  title: 'AI Coding Agents for Developers in India',
  description: 'India\'s massive developer community — from Bengaluru to Hyderabad to Pune — uses amux to run parallel Claude Code agents and ship faster.',
  keywords: 'india AI coding, indian developer tools, claude code india, Bengaluru AI coding',
  content: `<h1>AI Coding Agents for Developers in India</h1>
<p class="subtitle">India is the world's largest developer population. amux helps Indian developers ship at a pace that matches that scale.</p>
<h2>India's Developer Ecosystem</h2>
<p>India has over 5 million software developers — more than any other country. Bengaluru (Bangalore), Hyderabad, Pune, Chennai, and Mumbai each have major tech ecosystems serving both global companies (Amazon, Google, Microsoft, Oracle all have large India centers) and a rapidly growing domestic startup scene (Zepto, Razorpay, CRED, Meesho, Swiggy).</p>
<p>Indian developers face a unique opportunity: the timezone (IST, UTC+5:30) means you're working while the US sleeps. An agent fleet launched at 10pm IST completes work before the US East Coast has finished breakfast. That's a structural advantage.</p>
<h2>Workflows for Indian Developers</h2>
<ul>
<li><strong>Overnight US delivery:</strong> Launch agent fleets in your evening, deliver completed work to US product managers at their 9am. Your agents bridge the timezone gap automatically.</li>
<li><strong>Service company scale:</strong> India's IT services sector handles massive, parallel client workloads. amux's fleet model maps directly to this: one agent per client workstream, coordinated via the kanban board.</li>
<li><strong>Startup velocity:</strong> Bengaluru's startup scene is one of the fastest-growing in the world. Use agent fleets to ship at a pace that competes globally.</li>
<li><strong>Engineering education:</strong> India produces hundreds of thousands of CS graduates annually. amux is approachable — single Python file, a web UI, no cloud dependencies — ideal for developers at any experience level.</li>
</ul>
<h2>Cost-Effective at Scale</h2>
<p>Claude API tokens are priced globally. Indian developers using amux get the same AI coding capability as developers anywhere — with no additional platform fees, no SaaS subscription, and full ownership of their workflow.</p>
<table>
<thead><tr><th>City</th><th>Tech strength</th><th>amux use case</th></tr></thead>
<tbody>
<tr><td>Bengaluru</td><td>SaaS, ML, fintech</td><td>Parallel feature development, ML pipeline agents</td></tr>
<tr><td>Hyderabad</td><td>Microsoft, Amazon, pharma-tech</td><td>Enterprise integrations, compliance code</td></tr>
<tr><td>Pune</td><td>Automotive, embedded, services</td><td>Firmware tooling, test automation</td></tr>
<tr><td>Mumbai</td><td>Fintech, BFSI, media</td><td>Payment integrations, regulatory compliance</td></tr>
</tbody>
</table>
${CTA}`,
},


// ── STACK/LANGUAGE GEO PAGES (15) ────────────────────────────────
{
  slug: 'python-developers',
  title: 'amux for Python Developers',
  description: 'Run parallel Claude Code agents for Python: one per package, one per test module, one per migration. amux for Python developers.',
  keywords: 'python AI coding agent, claude code python, parallel python development',
  content: `<h1>amux for Python Developers</h1>
<p class="subtitle">Python's ecosystem is vast. amux lets you work on multiple packages, modules, and migrations simultaneously.</p>
<h2>Why Python + amux Works</h2>
<p>Python projects often sprawl: multiple packages in a monorepo, dozens of test modules, long migration chains, data pipelines with many stages. The natural way to speed this up is parallelism — and amux gives you that at the agent level.</p>
<ul>
<li><strong>One agent per package:</strong> Migrating a monorepo from Python 3.9 to 3.12? Assign one agent per package. They run simultaneously and the work is done in hours instead of days.</li>
<li><strong>One agent per test module:</strong> Run 10 agents writing unit tests across 10 test files. Cover an entire module in a single afternoon.</li>
<li><strong>Dependency upgrade agents:</strong> When a major dependency (Django, SQLAlchemy, Pydantic) releases a breaking version, use agents to handle the migration per-module, coordinated via the board.</li>
<li><strong>Type annotation blitzes:</strong> Adding <code>mypy</code> or <code>pyright</code> to a large untyped codebase? One agent per file or module, all running in parallel.</li>
</ul>
<h2>Python-Specific Workflows</h2>
<pre><code># Register separate agents for different parts of your Python project
amux register auth-agent --dir ~/myproject --yolo
amux register api-agent --dir ~/myproject --yolo
amux register tests-agent --dir ~/myproject --yolo

# Start them all
amux start auth-agent api-agent tests-agent

# Assign tasks via the board
curl -sk -X POST https://localhost:8822/api/board \\
  -d '{"title":"Migrate auth/ to Pydantic v2","session":"auth-agent"}'</code></pre>
<h2>Data Science and ML</h2>
<p>Python is the lingua franca of ML. Use amux to run parallel agents for:</p>
<ul>
<li>Generating model training scripts across multiple architectures</li>
<li>Writing data validation and cleaning pipelines</li>
<li>Implementing paper reproductions in parallel</li>
<li>Generating experiment notebooks and evaluation harnesses</li>
</ul>
<h2>Async by Nature</h2>
<p>Python developers are already familiar with async thinking (asyncio, Celery, multiprocessing). amux applies the same pattern to AI agents: fire off tasks, check back when they're done, aggregate results.</p>
${CTA}`,
},
{
  slug: 'javascript-developers',
  title: 'amux for JavaScript Developers',
  description: 'JavaScript developers use amux to run parallel Claude Code agents for components, APIs, tests, and tooling simultaneously.',
  keywords: 'javascript AI agent, claude code javascript, parallel JS development',
  content: `<h1>amux for JavaScript Developers</h1>
<p class="subtitle">JS is everywhere — frontend, backend, tooling, edge functions. Run agents for all of it simultaneously.</p>
<h2>JavaScript's Breadth = More Parallelism</h2>
<p>A modern JavaScript project touches many domains: React components, Node.js API routes, Express middleware, bundler configs, test suites, CI pipelines, and more. Each of these can be worked on independently — which makes them perfect candidates for parallel agents.</p>
<ul>
<li><strong>Component library coverage:</strong> Assign one agent per component to add tests, stories (Storybook), and accessibility attributes simultaneously.</li>
<li><strong>API route generation:</strong> Building a REST or GraphQL API? One agent per resource/type, generating routes, resolvers, and tests in parallel.</li>
<li><strong>Bundle optimization:</strong> Use an agent to analyze and implement bundle-splitting while other agents work on features.</li>
<li><strong>Lint/prettier cleanup:</strong> Run a dedicated agent to fix all ESLint warnings across a codebase while you work on new features.</li>
</ul>
<h2>Node.js Backend Workflows</h2>
<p>Node.js services often have repetitive patterns: controllers, services, repositories, DTOs. Use agents to generate these layers in parallel:</p>
<pre><code># One agent per domain entity
amux register user-agent --dir ~/api --yolo
amux register product-agent --dir ~/api --yolo
amux register order-agent --dir ~/api --yolo</code></pre>
<h2>Ecosystem Size = More Tasks</h2>
<p>The npm ecosystem has over 2 million packages. Keeping dependencies updated, migrating between major versions (Webpack 4 → 5, Jest 27 → 29, React 17 → 18), and adapting to breaking changes is a constant maintenance burden. amux agent fleets handle these migrations in parallel, reducing a week of work to a day.</p>
${CTA}`,
},
{
  slug: 'typescript-developers',
  title: 'amux for TypeScript Developers',
  description: 'TypeScript developers use amux to run parallel agents for type migrations, strict mode adoption, and large-scale refactoring.',
  keywords: 'typescript AI agent, claude code typescript, parallel TS development',
  content: `<h1>amux for TypeScript Developers</h1>
<p class="subtitle">TypeScript's type system is powerful but verbose. Parallel agents handle the boilerplate while you focus on design.</p>
<h2>TypeScript-Specific Parallelism</h2>
<p>TypeScript projects have unique opportunities for parallel agent work:</p>
<ul>
<li><strong>Strict mode migration:</strong> Enabling <code>strict: true</code> on a large codebase can surface hundreds of type errors. Run one agent per module to fix them simultaneously.</li>
<li><strong>JavaScript to TypeScript conversion:</strong> Migrating a JS codebase? One agent per file or directory, converting and typing in parallel.</li>
<li><strong>Interface and type generation:</strong> Generate TypeScript types from OpenAPI specs, database schemas, or GraphQL schemas — one agent per API surface.</li>
<li><strong>Generic type utilities:</strong> Use agents to write and test complex generic types, discriminated unions, and conditional types for a shared type library.</li>
</ul>
<h2>Monorepo TypeScript Patterns</h2>
<p>Large TypeScript monorepos (Turborepo, Nx) often have dozens of packages that need coordinated type updates. When you update a shared types package, downstream packages need updates too. Use amux to:</p>
<pre><code># One agent per downstream package after a breaking types change
amux register pkg-auth --dir ~/monorepo --yolo
amux register pkg-api --dir ~/monorepo --yolo
amux register pkg-ui --dir ~/monorepo --yolo</code></pre>
<h2>tRPC and Type-Safe APIs</h2>
<p>TypeScript-first frameworks like tRPC, Zod, and Prisma generate a lot of boilerplate code. Use agents to generate router definitions, Zod schemas, and Prisma models in parallel — then review and merge.</p>
${CTA}`,
},
{
  slug: 'rust-developers',
  title: 'amux for Rust Developers',
  description: 'Rust compile times are long. Use amux to run agents while you wait — and parallelize work across crates, modules, and lifetimes.',
  keywords: 'rust AI coding agent, claude code rust, parallel rust development',
  content: `<h1>amux for Rust Developers</h1>
<p class="subtitle">Rust compile times are notorious. amux turns waiting time into productive time.</p>
<h2>The Rust Developer's Parallel Opportunity</h2>
<p>Rust is powerful but demanding. Long compile times, strict borrow checker, verbose error handling, and complex lifetime annotations mean that a lot of development time is spent waiting or fighting the compiler. amux agents can work on different crates or modules while you handle the one that needs your attention.</p>
<ul>
<li><strong>Compile-time multitasking:</strong> While your main crate compiles, agents are writing tests for a different module, generating docs for a third, and fixing clippy warnings in a fourth.</li>
<li><strong>Error handling boilerplate:</strong> Rust's error handling is thorough but repetitive. Use agents to implement <code>thiserror</code> or <code>anyhow</code> error types across all modules in parallel.</li>
<li><strong>Unsafe code auditing:</strong> Use a dedicated agent to audit unsafe blocks, generate safety comments, and suggest safe alternatives across a codebase.</li>
<li><strong>FFI bindings:</strong> Generating bindgen FFI wrappers is tedious. Run agents to generate bindings for multiple C libraries simultaneously.</li>
</ul>
<h2>Workspace Parallelism</h2>
<p>Rust workspaces with multiple crates are a natural fit for amux. Each crate can have its own agent:</p>
<pre><code># One agent per crate in your workspace
amux register core-agent --dir ~/myproject --yolo
amux register cli-agent --dir ~/myproject --yolo
amux register server-agent --dir ~/myproject --yolo</code></pre>
<h2>Documentation and Tests</h2>
<p>Rust's documentation system (<code>rustdoc</code>) and test infrastructure are excellent but time-consuming to write. Use agents to generate <code>///</code> doc comments, doctests, and integration tests across your workspace while you focus on the complex algorithms that actually need your brain.</p>
${CTA}`,
},
{
  slug: 'go-developers',
  title: 'amux for Go Developers',
  description: 'Go developers use amux to run parallel Claude Code agents for microservices, gRPC handlers, test coverage, and tooling.',
  keywords: 'golang AI coding agent, claude code go, parallel go development',
  content: `<h1>amux for Go Developers</h1>
<p class="subtitle">Go's simplicity makes it fast to write. Parallel agents make it faster still.</p>
<h2>Go + Parallel Agents</h2>
<p>Go's philosophy is simplicity and explicitness — which also makes it highly parallelizable at the agent level. There's less magic, clearer patterns, and more code to write by hand. Agents excel at this kind of structured, pattern-following work.</p>
<ul>
<li><strong>gRPC service handlers:</strong> A gRPC service with 20 methods needs 20 handlers, with tests. Run 4-5 agents in parallel, each handling a group of methods.</li>
<li><strong>HTTP middleware chains:</strong> Use agents to implement logging, auth, rate-limiting, and tracing middleware simultaneously.</li>
<li><strong>Interface mocks:</strong> Generating mock implementations for testing is tedious in Go. Use agents to generate all your mocks across the entire codebase in a single run.</li>
<li><strong>Error wrapping:</strong> Go's explicit error handling requires a lot of <code>if err != nil</code> boilerplate. Agents handle this pattern perfectly.</li>
</ul>
<h2>Microservice Architecture</h2>
<p>Go is the dominant language for microservices (Kubernetes itself is Go). A microservice fleet maps naturally to an agent fleet:</p>
<pre><code># One agent per microservice
amux register auth-svc --dir ~/services/auth --yolo
amux register billing-svc --dir ~/services/billing --yolo
amux register notifications-svc --dir ~/services/notifications --yolo</code></pre>
<h2>Fast Compilation = Fast Feedback</h2>
<p>Go's near-instant compile times mean agents can run, test, fix, and iterate rapidly. Unlike Rust or Java, there's minimal waiting. An agent can make dozens of compile-test cycles per hour, making Go one of the best languages for autonomous agent work.</p>
${CTA}`,
},
{
  slug: 'java-developers',
  title: 'amux for Java Developers',
  description: 'Java developers use amux to handle migrations, boilerplate generation, and test coverage across large enterprise codebases in parallel.',
  keywords: 'java AI coding agent, claude code java, parallel java development',
  content: `<h1>amux for Java Developers</h1>
<p class="subtitle">Java enterprise codebases are large by nature. Parallel agents tackle the scale.</p>
<h2>Java's Parallel Opportunities</h2>
<p>Java projects — especially enterprise ones — are characterized by large codebases, lots of boilerplate (getters/setters, builders, DTOs), complex dependency injection, and significant test coverage requirements. These are exactly the kinds of tasks where parallel agents add the most value.</p>
<ul>
<li><strong>Spring Boot migrations:</strong> Migrating from Spring Boot 2 to 3 involves hundreds of deprecation changes. Run agents in parallel across service layers.</li>
<li><strong>JUnit 5 migration:</strong> Migrating from JUnit 4 to 5? One agent per test class, running simultaneously. A 1000-test codebase done in hours.</li>
<li><strong>Lombok adoption:</strong> Adding <code>@Data</code>, <code>@Builder</code>, <code>@Slf4j</code> annotations across a codebase and removing manual boilerplate — one agent per package.</li>
<li><strong>Java 17/21 modernization:</strong> Switch expressions, records, sealed classes, text blocks — use agents to modernize legacy Java code to modern idioms.</li>
</ul>
<h2>Maven/Gradle Multi-Module Projects</h2>
<p>Java enterprise projects often have 10-50 Maven/Gradle modules. Each module can have its own agent:</p>
<pre><code># One agent per module
amux register core-module --dir ~/enterprise-app --yolo
amux register api-module --dir ~/enterprise-app --yolo
amux register persistence-module --dir ~/enterprise-app --yolo</code></pre>
<h2>Test Coverage at Enterprise Scale</h2>
<p>Enterprise Java codebases often have low test coverage — a legacy of deadlines and boilerplate costs. A fleet of 10 agents writing JUnit tests simultaneously can bring a codebase from 20% to 80% coverage in a single working day.</p>
${CTA}`,
},
{
  slug: 'react-developers',
  title: 'amux for React Developers',
  description: 'React developers use amux to run parallel agents for components, hooks, tests, stories, and accessibility — all at once.',
  keywords: 'react AI coding agent, claude code react, parallel react development',
  content: `<h1>amux for React Developers</h1>
<p class="subtitle">A React app has hundreds of moving parts. Run agents for components, hooks, tests, and stories simultaneously.</p>
<h2>React's Natural Parallelism</h2>
<p>React applications are built from composable pieces — components, hooks, contexts, routes, stores. Each of these can be developed independently, which makes them perfect candidates for parallel agent work.</p>
<ul>
<li><strong>Component agents:</strong> Assign one agent per component to implement it, write its tests, and add Storybook stories simultaneously.</li>
<li><strong>Custom hook library:</strong> Building a shared hooks package? Run agents for <code>useFetch</code>, <code>useForm</code>, <code>useAuth</code>, <code>useDebounce</code> all in parallel.</li>
<li><strong>React 18 concurrent features:</strong> Migrating to Suspense, Server Components, and transitions? One agent per page or feature area.</li>
<li><strong>Accessibility (a11y) blitz:</strong> Run 5 agents across 5 component groups to add ARIA labels, keyboard navigation, and focus management simultaneously.</li>
</ul>
<h2>Design System Development</h2>
<p>Design systems require generating many variants of each component. Use agents to implement:</p>
<ul>
<li>Size variants (sm, md, lg) across all components</li>
<li>Color scheme variants with proper dark mode</li>
<li>Loading and error states for all data-fetching components</li>
<li>Responsive breakpoint handling</li>
</ul>
<h2>Testing React at Scale</h2>
<p>React Testing Library + Jest tests are verbose but follow clear patterns. An agent can write thorough test suites for a component including: render tests, interaction tests, accessibility tests, and snapshot tests. Run 10 agents across 10 components and your test coverage doubles in an afternoon.</p>
${CTA}`,
},
{
  slug: 'nextjs-developers',
  title: 'amux for Next.js Developers',
  description: 'Next.js developers use amux for parallel agents across pages, API routes, server components, and middleware.',
  keywords: 'nextjs AI coding agent, claude code nextjs, parallel next.js development',
  content: `<h1>amux for Next.js Developers</h1>
<p class="subtitle">Next.js apps span frontend, backend, and edge. Parallel agents cover all three simultaneously.</p>
<h2>Next.js Architecture = Multiple Agent Domains</h2>
<p>A production Next.js app has distinct technical layers that can be worked on independently: App Router pages, API routes, server components, client components, middleware, edge config, and deployment infrastructure. This is a natural parallel agent workload.</p>
<ul>
<li><strong>App Router migration:</strong> Migrating from Pages Router to App Router? One agent per route, handling layout, loading, error, and page files simultaneously.</li>
<li><strong>API routes to Route Handlers:</strong> Convert legacy <code>/pages/api</code> routes to App Router Route Handlers — one agent per endpoint, running in parallel.</li>
<li><strong>Server Component conversion:</strong> Auditing components and moving data fetching to server components — one agent per page or feature module.</li>
<li><strong>Middleware implementations:</strong> Auth, rate limiting, i18n, A/B testing middleware — one agent per middleware concern.</li>
</ul>
<h2>Full-Stack Coverage</h2>
<pre><code># Agent per concern in a Next.js project
amux register pages-agent --dir ~/myapp --yolo     # App Router pages
amux register api-agent --dir ~/myapp --yolo       # Route handlers
amux register components-agent --dir ~/myapp --yolo # Client components
amux register tests-agent --dir ~/myapp --yolo     # Playwright + Jest</code></pre>
<h2>Vercel Deployment Optimization</h2>
<p>Next.js on Vercel has many optimization levers: image optimization, font optimization, bundle analysis, edge vs serverless routing decisions. Use a dedicated agent to audit and implement performance improvements while other agents work on features.</p>
${CTA}`,
},
{
  slug: 'rails-developers',
  title: 'amux for Ruby on Rails Developers',
  description: 'Rails developers use amux to run parallel agents across models, controllers, specs, and migrations — convention over repetition.',
  keywords: 'ruby rails AI agent, claude code rails, parallel rails development',
  content: `<h1>amux for Ruby on Rails Developers</h1>
<p class="subtitle">Rails is opinionated and convention-driven. Agents love conventions — they can follow them at scale.</p>
<h2>Rails + Parallel Agents</h2>
<p>Rails' strong conventions make it one of the best frameworks for agent work. The MVC pattern, RESTful routes, Active Record conventions, and RSpec patterns are predictable enough that agents can generate correct code reliably. Run many agents against a Rails app and the output fits together naturally.</p>
<ul>
<li><strong>Resource generation at scale:</strong> Building a multi-tenant SaaS? Assign one agent per resource (users, organizations, subscriptions, billing) to generate full MVC stack simultaneously.</li>
<li><strong>RSpec coverage:</strong> Rails apps often have under-tested controllers and services. Run agents to generate model specs, controller specs, request specs, and system tests in parallel.</li>
<li><strong>Active Record migrations:</strong> Use agents to write migrations for schema changes, add indices, and create join tables — one agent per migration set.</li>
<li><strong>Rails 7/8 upgrades:</strong> Hotwire, Turbo, Stimulus, import maps — one agent per upgrade concern, running in parallel.</li>
</ul>
<h2>Service Object and Query Object Patterns</h2>
<p>Modern Rails apps use service objects, query objects, form objects, and decorators. These follow strict patterns that agents implement reliably:</p>
<pre><code># Assign one agent per domain service
amux register billing-agent --dir ~/railsapp --yolo
amux register notifications-agent --dir ~/railsapp --yolo
amux register reporting-agent --dir ~/railsapp --yolo</code></pre>
<h2>Background Job Coverage</h2>
<p>Sidekiq workers are another area where Rails codebases often lack test coverage. A fleet of agents can generate job implementations, tests, and error handling across all background jobs in a codebase simultaneously.</p>
${CTA}`,
},
{
  slug: 'django-developers',
  title: 'amux for Django Developers',
  description: 'Django developers use amux to run parallel agents for views, serializers, models, migrations, and test coverage across large Django projects.',
  keywords: 'django AI coding agent, claude code django, parallel django development',
  content: `<h1>amux for Django Developers</h1>
<p class="subtitle">Django powers some of the largest sites in the world. amux helps Django developers scale their output to match.</p>
<h2>Django's Agent-Friendly Patterns</h2>
<p>Django's "batteries included" philosophy means there are strong conventions for everything: ORM models, class-based views, serializers (DRF), URL routing, admin configuration, and management commands. Agents follow these conventions reliably, making Django one of the best frameworks for parallel agent work.</p>
<ul>
<li><strong>DRF serializers and viewsets:</strong> A large REST API with 30+ endpoints needs serializers, viewsets, and URL routes for each. Run agents in parallel — one per app or resource group.</li>
<li><strong>Django admin customization:</strong> Use agents to generate <code>ModelAdmin</code> classes with filters, search, list displays, and inline admin for every model simultaneously.</li>
<li><strong>Django migrations:</strong> Complex data migrations with <code>RunPython</code> are time-consuming to write. Use agents to generate migrations for large schema changes across multiple apps.</li>
<li><strong>pytest-django test suites:</strong> Factory Boy + pytest-django + fixtures = lots of boilerplate. Agents generate thorough test suites for views, models, and signals in parallel.</li>
</ul>
<h2>Multi-App Django Projects</h2>
<p>Large Django projects typically have 5-20 apps. Each app can have its own agent:</p>
<pre><code># One agent per Django app
amux register users-app --dir ~/djangoproject --yolo
amux register products-app --dir ~/djangoproject --yolo
amux register orders-app --dir ~/djangoproject --yolo</code></pre>
<h2>Celery Task Implementation</h2>
<p>Django projects using Celery for async tasks often have many recurring and deferred tasks. Use agents to implement task functions, retry logic, error handling, and test coverage across all Celery tasks in parallel.</p>
${CTA}`,
},
{
  slug: 'fastapi-developers',
  title: 'amux for FastAPI Developers',
  description: 'FastAPI developers use amux to run parallel agents for endpoint implementation, Pydantic schemas, dependency injection, and test coverage.',
  keywords: 'fastapi AI coding agent, claude code fastapi, parallel python API development',
  content: `<h1>amux for FastAPI Developers</h1>
<p class="subtitle">FastAPI is the fastest-growing Python API framework. amux makes building large FastAPI services equally fast.</p>
<h2>FastAPI + Parallel Agents</h2>
<p>FastAPI's design — Pydantic models, dependency injection, automatic OpenAPI docs, async support — creates clear separation of concerns that maps well to parallel agent work. Each router, each set of schemas, each service layer can be worked on independently.</p>
<ul>
<li><strong>Router per domain:</strong> A FastAPI app with 10 domains (users, products, orders, billing, etc.) can have 10 agents building each router simultaneously — with Pydantic schemas, service layer, and tests.</li>
<li><strong>Pydantic v2 migration:</strong> Migrating from Pydantic v1 to v2 involves updating validators, field definitions, and model configs everywhere. One agent per module, running in parallel.</li>
<li><strong>Dependency injection chains:</strong> Use agents to implement authentication dependencies, database session handling, and permission checking — one agent per dependency concern.</li>
<li><strong>pytest + httpx test coverage:</strong> FastAPI testing with <code>TestClient</code> or async <code>AsyncClient</code> follows clear patterns. Agents generate thorough test suites for every endpoint.</li>
</ul>
<h2>OpenAPI-Driven Development</h2>
<p>FastAPI generates OpenAPI specs automatically. Use agents to:</p>
<ul>
<li>Generate client SDKs from the OpenAPI spec</li>
<li>Write contract tests against the spec</li>
<li>Generate postman collections and documentation</li>
<li>Implement mock servers for frontend development</li>
</ul>
<h2>Async All the Way</h2>
<p>FastAPI's async nature means you're already thinking in concurrent terms. amux brings that concurrency to the development process itself — async agents for your async API.</p>
${CTA}`,
},
{
  slug: 'fullstack-developers',
  title: 'amux for Fullstack Developers',
  description: 'Fullstack developers use amux to run parallel agents across frontend, backend, database, and infrastructure simultaneously.',
  keywords: 'fullstack AI coding agent, claude code fullstack, parallel fullstack development',
  content: `<h1>amux for Fullstack Developers</h1>
<p class="subtitle">Fullstack means everything is your responsibility. Parallel agents mean everything gets done simultaneously.</p>
<h2>The Fullstack Developer's Challenge</h2>
<p>Fullstack developers are context-switchers by nature: frontend in the morning, backend in the afternoon, infra when something breaks. The breadth of ownership is both the joy and the challenge of fullstack work. amux doesn't eliminate that breadth — it lets you pursue all of it at once.</p>
<ul>
<li><strong>Frontend + backend in parallel:</strong> One agent implements the API endpoint, another builds the React component that consumes it. Both done by the time you return from lunch.</li>
<li><strong>Database schema + ORM models:</strong> One agent writes the migration, another updates the ORM models, another updates the TypeScript types. Coordinated, parallel, consistent.</li>
<li><strong>E2E test coverage:</strong> While feature agents build the feature, a test agent writes the Playwright or Cypress tests against it.</li>
<li><strong>Infra-as-code:</strong> A dedicated agent handles Terraform changes, Dockerfile updates, and CI workflow modifications while you focus on application code.</li>
</ul>
<h2>The Fullstack Agent Fleet</h2>
<pre><code>amux register frontend --dir ~/myapp --yolo    # React/Next.js work
amux register backend --dir ~/myapp --yolo     # API/server work
amux register db --dir ~/myapp --yolo          # Migrations/models
amux register tests --dir ~/myapp --yolo       # E2E + unit tests
amux register infra --dir ~/myapp --yolo       # Docker/CI/Terraform</code></pre>
<h2>One Developer, Team Output</h2>
<p>Fullstack developers using amux agent fleets routinely ship what would normally require a 4-5 person team. The board coordination, self-healing, and mobile monitoring let you manage the fleet from anywhere — without anyone dropping the ball.</p>
${CTA}`,
},
{
  slug: 'backend-developers',
  title: 'amux for Backend Developers',
  description: 'Backend developers use amux to run parallel agents for APIs, database layers, authentication, and service integrations simultaneously.',
  keywords: 'backend AI coding agent, claude code backend, parallel backend development',
  content: `<h1>amux for Backend Developers</h1>
<p class="subtitle">Backend systems have many independent layers. amux lets you build all of them in parallel.</p>
<h2>Backend's Natural Parallelism</h2>
<p>Backend systems are inherently modular: authentication, authorization, data models, business logic, external API integrations, background jobs, caching, monitoring. Each layer is relatively independent, making backend the ideal domain for parallel agent work.</p>
<ul>
<li><strong>Service layer generation:</strong> A service with 20 business operations can have agents implementing each operation's service class, repository, and tests simultaneously.</li>
<li><strong>Third-party API integrations:</strong> Every SaaS integration (Stripe, SendGrid, Twilio, Salesforce) is independent. Run one agent per integration in parallel.</li>
<li><strong>Authentication and authorization:</strong> JWT, OAuth2, RBAC, ABAC — implement all auth layers simultaneously with dedicated agents.</li>
<li><strong>Database optimization:</strong> Use a dedicated agent to analyze query performance, add indices, and implement query optimizations while other agents build features.</li>
</ul>
<h2>API Contract Development</h2>
<p>Backend developers are responsible for the API contracts that frontend teams depend on. Use amux to:</p>
<ul>
<li>Generate API documentation from code (OpenAPI, GraphQL schema)</li>
<li>Implement contract tests that frontend teams can run</li>
<li>Generate mock servers for frontend development</li>
<li>Maintain changelog and migration guides for breaking changes</li>
</ul>
<h2>On-Call Automation</h2>
<p>Backend developers often carry on-call burden. Use a persistent amux agent to watch error logs, match known error patterns, and generate draft fix PRs automatically — reducing on-call toil significantly.</p>
${CTA}`,
},
{
  slug: 'devops-engineers-stack',
  title: 'amux for DevOps Engineers',
  description: 'DevOps engineers use amux to run parallel agents for infrastructure-as-code, CI/CD pipelines, monitoring configs, and runbook automation.',
  keywords: 'devops AI agent, claude code devops, parallel infrastructure automation',
  content: `<h1>amux for DevOps Engineers</h1>
<p class="subtitle">Infrastructure is code. Parallel agents write, test, and document it faster.</p>
<h2>DevOps + Parallel Agents</h2>
<p>DevOps work is inherently broad: Terraform, Kubernetes manifests, Helm charts, CI/CD pipelines, monitoring configs, runbooks, incident response playbooks. Much of it follows patterns and conventions — exactly where AI agents excel.</p>
<ul>
<li><strong>Terraform module generation:</strong> Use agents to generate Terraform modules for each infrastructure component (VPC, EKS, RDS, ElastiCache) simultaneously.</li>
<li><strong>Kubernetes manifests:</strong> One agent per service — Deployments, Services, ConfigMaps, Secrets, HPA configs — generated and tested in parallel.</li>
<li><strong>GitHub Actions workflows:</strong> Build CI/CD pipelines for each service or environment simultaneously, with proper caching, test coverage, and deployment steps.</li>
<li><strong>Runbook automation:</strong> Use agents to convert informal runbooks to executable scripts with error handling, alerting, and rollback procedures.</li>
</ul>
<h2>Multi-Environment Management</h2>
<p>DevOps teams manage dev, staging, and production environments. Use agents to:</p>
<pre><code>amux register dev-infra --dir ~/infrastructure --yolo
amux register staging-infra --dir ~/infrastructure --yolo
amux register prod-infra --dir ~/infrastructure --yolo</code></pre>
<h2>Incident Response</h2>
<p>During incidents, DevOps teams need to move fast across multiple systems simultaneously. amux's parallel agent model lets you address infrastructure issues in multiple areas at once — without losing track of any thread. The board gives you a real-time view of what each agent is working on.</p>
${CTA}`,
},
{
  slug: 'ml-engineers',
  title: 'amux for ML Engineers',
  description: 'ML engineers use amux to run parallel agents for experiment implementations, data pipelines, model evaluation, and paper reproductions.',
  keywords: 'machine learning AI agent, claude code ML, parallel ML development',
  content: `<h1>amux for ML Engineers</h1>
<p class="subtitle">ML research is inherently parallel. So is amux.</p>
<h2>ML Engineering + Parallel Agents</h2>
<p>ML engineers live in a world of parallel experiments: different architectures, different hyperparameters, different datasets, different preprocessing pipelines. The code that supports these experiments — loaders, trainers, evaluators, visualization — follows patterns that agents can implement reliably and simultaneously.</p>
<ul>
<li><strong>Paper reproductions:</strong> Reproducing ML papers often means implementing 5-10 architectural variants. Run one agent per variant, all building simultaneously.</li>
<li><strong>Data pipeline generation:</strong> Dataset loading, preprocessing, augmentation, and validation — one agent per dataset or data source, all building in parallel.</li>
<li><strong>Experiment harnesses:</strong> Run agents to generate PyTorch Lightning or Hugging Face Trainer implementations for multiple model architectures simultaneously.</li>
<li><strong>Evaluation frameworks:</strong> Generate evaluation scripts, metric implementations, and visualization code for model performance in parallel with the models themselves.</li>
</ul>
<h2>Long-Running GPU Workflows</h2>
<p>While your GPU is busy training a model, your CPU-bound agents can be:</p>
<ul>
<li>Writing the evaluation pipeline for the model currently training</li>
<li>Implementing the next architecture variant to train after this one</li>
<li>Generating documentation and README for the current experiment</li>
<li>Writing tests for the data pipeline</li>
</ul>
<h2>MLOps Integration</h2>
<p>ML engineers increasingly own the full MLOps stack: model serving, feature stores, model registries, A/B testing infrastructure. Use agent fleets to implement these components in parallel with model development.</p>
${CTA}`,
},
];


// ═══════════════════════════════════════════════════════════════════
// MORE BLOG POSTS (10)
// ═══════════════════════════════════════════════════════════════════
const moreBlog = [
{
  slug: 'vibe-coding-with-agents',
  title: 'Vibe Coding at Scale: How AI Agent Fleets Changed My Workflow',
  description: 'What "vibe coding" actually means when you have 20 AI agents running in parallel.',
  keywords: 'vibe coding agents, parallel AI coding workflow, agent fleet productivity',
  content: `<h1>Vibe Coding at Scale: How AI Agent Fleets Changed My Workflow</h1>
<p class="subtitle">What happens when you stop coding one task at a time and start thinking in fleets.</p>
<p>The first time someone called me a "vibe coder" I wasn't sure if it was a compliment. Vibe coding — the practice of describing what you want in natural language and letting an AI generate the implementation — sounded imprecise, unprofessional. Then I tried it with 20 agents running in parallel, and I understood.</p>
<h2>What Vibe Coding Actually Means</h2>
<p>Vibe coding isn't about being imprecise. It's about operating at a higher level of abstraction. Instead of thinking in functions and loops, you think in outcomes and interfaces. Instead of writing the implementation, you describe the behavior and review the result.</p>
<p>This is how senior engineers have always wanted to work: at the architectural level, with implementation as a detail. AI agents make it real. But one agent is still too slow. The real shift happens at scale.</p>
<h2>The Fleet Model</h2>
<p>Here's what changed for me: I stopped assigning one task to one agent and started decomposing problems into parallel workstreams. A feature that used to take me 2 days now looks like this:</p>
<ul>
<li>Agent 1: Data model + migrations</li>
<li>Agent 2: API endpoints + validation</li>
<li>Agent 3: Frontend components</li>
<li>Agent 4: Tests (unit + integration)</li>
<li>Agent 5: Documentation + types</li>
</ul>
<p>I describe each workstream at a high level, assign them to agents on the amux board, and step back. An hour later I have 5 branches to review, merge, and assemble into a feature. The assembly and review is where my actual judgment is spent — not the implementation.</p>
<h2>The Skill Shift</h2>
<p>Vibe coding at scale requires a different skillset than traditional coding. You need to be good at:</p>
<ul>
<li><strong>Decomposition:</strong> Breaking a problem into independent workstreams that agents can pursue in parallel without stepping on each other.</li>
<li><strong>Specification:</strong> Writing clear task descriptions that give agents enough context to succeed without constant clarification.</li>
<li><strong>Review:</strong> Reading agent-generated code critically — catching logic errors, security issues, and missed edge cases in code you didn't write.</li>
<li><strong>Integration:</strong> Assembling parallel workstreams into a coherent whole, resolving conflicts, and maintaining consistency.</li>
</ul>
<h2>What I Stopped Doing</h2>
<p>I no longer write boilerplate. I no longer look up API docs for common patterns. I no longer manually write tests for code I've already mentally verified. These tasks take time but require minimal judgment. Agents handle them. My time goes to the parts that require genuine thought.</p>
<h2>The Honest Tradeoffs</h2>
<p>Vibe coding at scale isn't free of friction. Agent output needs review. Parallel branches sometimes conflict. Agents occasionally go in the wrong direction and need course-correction. The management overhead of a fleet is real.</p>
<p>But the net result is clear: I ship more, faster, with higher test coverage than I did coding solo. The fleet is doing work I would have delegated to junior developers — except it's available at midnight, never takes a sick day, and costs $30/day in API tokens.</p>
${CTA}`,
},
{
  slug: 'claude-4-agent-workflow',
  title: 'Building with Claude 4: The Parallel Agent Workflow',
  description: 'How to structure your development workflow to take full advantage of Claude 4 capabilities in parallel.',
  keywords: 'claude 4 workflow, parallel agent workflow, claude opus 4 coding',
  content: `<h1>Building with Claude 4: The Parallel Agent Workflow</h1>
<p class="subtitle">Claude 4's extended thinking and tool use capabilities change what parallel agents can accomplish.</p>
<p>Claude 4 — specifically Claude Opus 4 — represents a meaningful capability jump for long-horizon agentic tasks. Extended thinking, better tool use reliability, stronger code generation. For parallel agent workflows, this matters because the limiting factor was always agent capability per session, not the ability to run many sessions. With Claude 4, the per-agent output quality is higher, which multiplies across a fleet.</p>
<h2>What Claude 4 Changes for Agents</h2>
<p>The most important improvements for agentic workflows:</p>
<ul>
<li><strong>Extended thinking:</strong> Claude 4 can reason through complex problems before generating code. This reduces the "plausible but wrong" outputs that required re-prompting.</li>
<li><strong>Tool use reliability:</strong> Agents use bash, read files, write code, run tests. Claude 4's tool use is more reliable — fewer hallucinated tool calls, better error recovery.</li>
<li><strong>Context utilization:</strong> Claude 4 makes better use of its context window, meaning agents can hold more of your codebase in mind when making changes.</li>
<li><strong>Instruction following:</strong> Complex, multi-step task descriptions are executed more faithfully. You can give agents richer specifications and expect better compliance.</li>
</ul>
<h2>Structuring Tasks for Claude 4</h2>
<p>With a more capable model, the bottleneck shifts from agent capability to task specification. Here's how I structure tasks for Claude 4 agents:</p>
<ul>
<li><strong>Give context:</strong> Include file paths, relevant existing code patterns, and the overall architecture. Claude 4 uses this effectively.</li>
<li><strong>Specify constraints:</strong> What should the agent NOT do? Don't modify these files, don't add new dependencies, maintain this interface. Explicit constraints prevent scope creep.</li>
<li><strong>Define done:</strong> What does success look like? "All tests passing" or "The endpoint returns 200 for these inputs" gives the agent a clear target to verify against.</li>
</ul>
<h2>Fleet Configuration for Claude 4</h2>
<p>In your amux session config, you can specify which model each agent uses. For complex architectural tasks, use Opus 4. For high-volume, pattern-following work (test generation, documentation, boilerplate), use Sonnet 4 or Haiku to keep costs down.</p>
<pre><code># Complex tasks: Opus 4
amux register architect --dir ~/project --model claude-opus-4 --yolo

# High-volume tasks: Sonnet for balance
amux register test-writer --dir ~/project --model claude-sonnet-4 --yolo</code></pre>
<h2>The Model Mix Strategy</h2>
<p>The most cost-effective fleet uses mixed models: 1-2 Opus 4 agents for complex reasoning and architecture decisions, 5-10 Sonnet 4 agents for implementation, and Haiku agents for search, triage, and documentation. This gives you frontier capability where it matters and lower cost where it doesn't.</p>
${CTA}`,
},
{
  slug: 'mcp-server-agent-orchestration',
  title: 'MCP Servers + Parallel Agents: A Force Multiplier',
  description: 'How to configure MCP servers in amux sessions to give each agent specialized tool access.',
  keywords: 'MCP server parallel agents, model context protocol amux, claude code MCP tools',
  content: `<h1>MCP Servers + Parallel Agents: A Force Multiplier</h1>
<p class="subtitle">MCP turns agents from code-writers into systems — with access to databases, APIs, browsers, and internal tools.</p>
<h2>What MCP Adds to Agent Workflows</h2>
<p>Model Context Protocol (MCP) lets Claude Code agents connect to specialized tool servers: database access, browser automation, Slack messaging, GitHub API, internal REST APIs, Figma, Linear, and more. When you combine MCP servers with parallel agents, each agent becomes a specialist — not just a code writer, but a system that can query your database, check Slack, and push to GitHub directly.</p>
<h2>Per-Session MCP Configuration</h2>
<p>amux uses a centralized <code>mcp.json</code> config that all sessions share. But you can also configure session-specific MCP servers using environment variables or session-level config overrides. This lets you give different agents different tool access:</p>
<pre><code># mcp.json — centralized for all sessions
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "\${GITHUB_TOKEN}" }
    },
    "postgres": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-postgres"],
      "env": { "DATABASE_URL": "\${DATABASE_URL}" }
    },
    "browser": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-puppeteer"]
    }
  }
}</code></pre>
<h2>Specialist Agent Patterns</h2>
<p>With MCP, you can build specialist agents that have the right tools for their domain:</p>
<ul>
<li><strong>Database agent:</strong> Postgres MCP + Filesystem → queries the live schema, reads migrations, writes new ones correctly.</li>
<li><strong>GitHub agent:</strong> GitHub MCP → creates PRs, reviews open issues, posts comments, creates labels and milestones.</li>
<li><strong>QA agent:</strong> Puppeteer MCP → runs browser automation tests, takes screenshots, reports visual regressions.</li>
<li><strong>Documentation agent:</strong> Filesystem + GitHub MCP → reads code, generates docs, commits them to a docs branch automatically.</li>
</ul>
<h2>Coordination via REST</h2>
<p>amux agents coordinate via HTTP API — they can read the shared board, post updates, and message each other. Combined with MCP tool access, this means an orchestrator agent can see what specialist agents have done (via the board) and issue follow-up tasks without any human intervention.</p>
<p>The pattern looks like: orchestrator reads the task list, assigns sub-tasks to specialist agents via the board, specialist agents complete tasks with their MCP tools, orchestrator verifies and closes the loop. Pure agent orchestration, no human in the loop.</p>
${CTA}`,
},
{
  slug: 'true-cost-ai-coding',
  title: 'The True Cost of AI Coding in 2025',
  description: 'A realistic breakdown of what it costs to run AI coding agents — tokens, time, and how to optimize.',
  keywords: 'AI coding cost 2025, claude code token cost, agent fleet pricing, AI development ROI',
  content: `<h1>The True Cost of AI Coding in 2025</h1>
<p class="subtitle">Spoiler: the tokens are cheaper than you think. The bottleneck is task design, not the API bill.</p>
<h2>The Numbers</h2>
<p>Let's be concrete. Claude Sonnet 4 at current pricing costs roughly $3/million input tokens and $15/million output tokens. A typical coding session — reading files, generating code, running tests, fixing errors — might use 50k-200k tokens per task. That's $0.15-$3 per task depending on complexity and model.</p>
<p>Running 10 agents simultaneously for 8 hours (an overnight run) on Sonnet 4 typically costs $20-80 depending on task complexity. If those agents complete 10-30 meaningful coding tasks, you've paid $1-5 per completed task. A developer's hourly rate divided by tasks-per-hour almost always exceeds this.</p>
<h2>Model Choice Is the Biggest Lever</h2>
<table>
<thead><tr><th>Model</th><th>Relative cost</th><th>Best for</th></tr></thead>
<tbody>
<tr><td>Claude Opus 4</td><td>High</td><td>Architecture decisions, complex refactors, ambiguous tasks</td></tr>
<tr><td>Claude Sonnet 4</td><td>Medium</td><td>Feature implementation, API development, most coding tasks</td></tr>
<tr><td>Claude Haiku 4</td><td>Low</td><td>Documentation, boilerplate, search and triage, simple fixes</td></tr>
</tbody>
</table>
<p>The biggest cost optimization is using the right model for each task. Don't use Opus for writing getters and setters. Don't use Haiku for complex architectural refactors. amux lets you configure the model per session, so you can match model capability to task complexity.</p>
<h2>What Actually Wastes Tokens</h2>
<ul>
<li><strong>Vague task descriptions:</strong> An agent that goes in the wrong direction and needs to backtrack uses 3-5x more tokens than one that executes correctly the first time. Write clear, specific task descriptions.</li>
<li><strong>Agents reading the whole codebase:</strong> If an agent reads 500k tokens of source code for a 10-line fix, your cost explodes. Keep task scope tight and specify relevant files explicitly.</li>
<li><strong>Long error loops:</strong> An agent stuck in a loop trying to fix a compilation error can burn thousands of tokens on unproductive iterations. amux's watchdog detects stuck agents and interrupts them.</li>
<li><strong>No context boundaries:</strong> Running agents on huge context windows when smaller ones would suffice. Use session checkpointing and compact summaries to keep context efficient.</li>
</ul>
<h2>The Real ROI Calculation</h2>
<p>The correct comparison isn't "agent tokens vs. zero." It's "agent tokens vs. developer hours." At $100-200/hr fully loaded developer cost, even a $50/day agent fleet is economical if it produces meaningful output. The question is task quality — designing tasks that agents can execute successfully is the skill that determines ROI.</p>
${CTA}`,
},
{
  slug: 'solo-dev-ai-stack-2025',
  title: "The Solo Developer's AI Stack in 2025",
  description: 'The tools, workflows, and habits that let a solo developer ship at team speed.',
  keywords: 'solo developer AI stack 2025, indie developer AI tools, single developer team speed',
  content: `<h1>The Solo Developer's AI Stack in 2025</h1>
<p class="subtitle">One developer. Team output. Here's the exact stack that makes it possible.</p>
<h2>The Core Stack</h2>
<p>Solo developers in 2025 who are shipping at team speed are using roughly the same stack. Here's what it looks like:</p>
<table>
<thead><tr><th>Layer</th><th>Tool</th><th>Why</th></tr></thead>
<tbody>
<tr><td>Agent orchestration</td><td>amux</td><td>Run 10+ Claude Code agents in parallel, unattended</td></tr>
<tr><td>AI model</td><td>Claude Sonnet/Opus 4</td><td>Best code generation quality for agentic workflows</td></tr>
<tr><td>Interactive coding</td><td>Claude Code or Cursor</td><td>Real-time pair programming for the hard problems</td></tr>
<tr><td>Version control</td><td>GitHub + gh CLI</td><td>Agent-created PRs, automated code review</td></tr>
<tr><td>CI/CD</td><td>GitHub Actions</td><td>Automated testing and deployment</td></tr>
<tr><td>Deployment</td><td>Railway / Fly.io / Vercel</td><td>Zero-config deploys from agent-created PRs</td></tr>
</tbody>
</table>
<h2>The Daily Workflow</h2>
<p>A solo developer using this stack has a fundamentally different daily rhythm than one coding solo:</p>
<ul>
<li><strong>Morning (30 min):</strong> Review output from overnight agent runs. Merge, close, or redirect tasks. Identify what needs human attention today.</li>
<li><strong>Deep work block (2-3 hrs):</strong> Work on the 1-2 problems that actually require your full cognition. Architecture decisions, complex bugs, user research.</li>
<li><strong>Afternoon (30 min):</strong> Launch the next batch of agent tasks based on morning decisions. Assign to the board, start sessions.</li>
<li><strong>Evening (15 min):</strong> Check agent status from phone. Redirect any stuck agents. Set up overnight run.</li>
</ul>
<h2>What You Still Own</h2>
<p>The solo developer's job changes but doesn't disappear. You still own:</p>
<ul>
<li>Product direction and prioritization</li>
<li>Architecture decisions and technical taste</li>
<li>Customer relationships and feedback interpretation</li>
<li>Code review and quality gates</li>
<li>Security and compliance decisions</li>
</ul>
<p>These are the highest-leverage activities. The agent fleet handles the rest — implementation, tests, documentation, boilerplate, migrations. The ratio of high-leverage work goes up dramatically.</p>
<h2>The Honest Ceiling</h2>
<p>Solo developers with agent fleets aren't infinite. Complex UX problems, ambiguous product decisions, user research, stakeholder management — these don't parallelize to agents. The ceiling is still one person's judgment and taste. But the floor rises enormously: implementation speed is no longer the bottleneck.</p>
${CTA}`,
},
{
  slug: 'sprint-in-48-hours',
  title: 'How I Completed a 2-Week Sprint in 48 Hours',
  description: 'A real walkthrough of using an agent fleet to compress a sprint: parallel features, automated tests, and overnight runs.',
  keywords: '48 hour sprint AI agents, sprint compression AI coding, parallel development sprint',
  content: `<h1>How I Completed a 2-Week Sprint in 48 Hours</h1>
<p class="subtitle">A concrete walkthrough — not a hype piece — of what parallel agents actually delivered.</p>
<h2>The Setup</h2>
<p>I had a 2-week sprint with 8 tickets: 3 new features, 2 bug fixes, 2 refactoring tasks, and a test coverage requirement (bring coverage from 45% to 70%). Normally this is a full sprint's worth of work for one developer. I ran it over a weekend with an amux agent fleet.</p>
<p>This isn't a story about AI magic. It's a story about task decomposition, good specifications, and letting agents run overnight while I slept.</p>
<h2>Friday Evening: Setup (2 hours)</h2>
<p>I spent 2 hours not coding, but writing. Task descriptions for each of the 8 tickets, with explicit file paths, interface requirements, and success criteria. I registered 6 amux sessions, assigned tasks via the board, and launched them at 10pm. Then I went to sleep.</p>
<pre><code># The board at 10pm Friday
FEAT-01: User profile API endpoints [agent: api-1]
FEAT-02: Email notification system [agent: api-2]
FEAT-03: Dashboard analytics component [agent: frontend-1]
BUG-01: Fix pagination off-by-one [agent: api-1] (queued)
BUG-02: Fix mobile menu z-index [agent: frontend-1] (queued)
REFAC-01: Extract auth middleware [agent: api-2] (queued)
REFAC-02: Consolidate CSS variables [agent: frontend-2]
TEST: Coverage blitz — models + controllers [agents: test-1, test-2]</code></pre>
<h2>Saturday Morning: Review (3 hours)</h2>
<p>I woke up to 5 completed tasks and 3 still in progress. I reviewed the completed work — merged 4 items cleanly, sent one back with corrections (the notification system had used the wrong email template structure). The test agents had brought coverage to 68%.</p>
<p>By noon Saturday all 8 tickets were done. I spent the afternoon doing integration testing and review — finding 3 issues the agents had missed (one security, two UX problems) and fixing them myself. These were genuinely complex issues that required judgment. The agents handled everything they could handle.</p>
<h2>What Actually Took Time</h2>
<p>The 48 hours included: 2 hours of task specification, 3 hours of code review, 2 hours of integration testing, and 1 hour of fixing what agents missed. Total hands-on time: 8 hours. An agent fleet ran for 36 hours of that window handling the rest.</p>
<h2>What the Agents Couldn't Do</h2>
<ul>
<li>Catch a subtle race condition in the notification queue (human caught it in review)</li>
<li>Decide that the analytics component needed a different data model than specified</li>
<li>Notice that the email notification design didn't match the company's style guide</li>
</ul>
<p>These are judgment calls. They require context, taste, and product knowledge. The agents delivered implementations — the developer delivered decisions.</p>
${CTA}`,
},
{
  slug: 'debugging-20-agents',
  title: 'Debugging When You Have 20 AI Agents',
  description: 'Strategies for monitoring, diagnosing, and fixing issues in a large agent fleet.',
  keywords: 'debug AI agent fleet, stuck claude code agent, agent fleet monitoring, amux debugging',
  content: `<h1>Debugging When You Have 20 AI Agents</h1>
<p class="subtitle">More agents means more things that can go wrong. Here's how to stay on top of it.</p>
<h2>The New Debugging Problem</h2>
<p>Solo coding: one process, one context, all in your head. Agent fleet: 20 processes, 20 contexts, one dashboard. The debugging challenge isn't technical complexity — it's information management. How do you know which of your 20 agents is stuck, looping, or going in the wrong direction?</p>
<h2>Signals That an Agent Is in Trouble</h2>
<ul>
<li><strong>No progress after 20 minutes:</strong> If a task that should take 10 minutes shows no code changes after 20, peek at the session. The agent is probably stuck on a compilation error or waiting for input.</li>
<li><strong>Repeated identical tool calls:</strong> An agent in a reasoning loop will keep calling the same tools in the same order. Visible in the peek output.</li>
<li><strong>Growing token spend with no output:</strong> Token tracking in the amux dashboard shows per-session spend. Unusually high spend with no committed code = wasted loop.</li>
<li><strong>Board task not moving:</strong> If a task has been "doing" for 3 hours on a 30-minute estimate, something's wrong.</li>
</ul>
<h2>The Peek-Then-Steer Workflow</h2>
<p>amux's live peek feature lets you see any agent's terminal in real time from the dashboard. The debugging workflow:</p>
<ol>
<li>Notice a suspicious agent (no progress, high token spend)</li>
<li>Peek at its session from the dashboard</li>
<li>Diagnose: stuck prompt? wrong direction? compilation error loop?</li>
<li>Send a correction message via the dashboard: "Stop. The file is at path/to/file.ts, not path/to/file.js."</li>
<li>Watch the agent recover and continue</li>
</ol>
<h2>Common Failure Modes and Fixes</h2>
<table>
<thead><tr><th>Failure mode</th><th>Diagnosis</th><th>Fix</th></tr></thead>
<tbody>
<tr><td>Compilation error loop</td><td>Same error in peek output, repeated attempts</td><td>Send the correct import path or dependency name</td></tr>
<tr><td>Wrong file modified</td><td>Git diff shows unexpected changes</td><td>Reset branch, re-specify file paths explicitly</td></tr>
<tr><td>Context exhausted</td><td>amux auto-compact triggered</td><td>Self-healed automatically; check output quality after</td></tr>
<tr><td>Task too ambiguous</td><td>Agent asking many clarifying questions</td><td>Send a more specific task description via dashboard</td></tr>
<tr><td>Dependency not available</td><td>Package not found errors</td><td>Run the install command, or tell agent to add to package.json first</td></tr>
</tbody>
</table>
<h2>The 5-Agent Monitoring Rule</h2>
<p>In practice, actively monitoring more than 5 agents simultaneously is difficult. With 20 agents, the approach is triage: spend the first 10 minutes of each check reviewing all sessions quickly (peek for 30 seconds each), flag the 2-3 that need attention, and intervene only on those. The other 17 run fine on their own.</p>
${CTA}`,
},
{
  slug: 'overnight-saas-mvp',
  title: 'Building a SaaS MVP Overnight with Agent Fleets',
  description: 'How to structure an agent fleet to go from spec to deployed MVP in under 24 hours.',
  keywords: 'SaaS MVP overnight AI, build SaaS with agents, 24 hour MVP AI coding',
  content: `<h1>Building a SaaS MVP Overnight with Agent Fleets</h1>
<p class="subtitle">Not a magic trick — a workflow. Here's how to structure it.</p>
<h2>What "MVP Overnight" Actually Means</h2>
<p>An overnight MVP isn't a production-ready product. It's a working prototype with the core value proposition implemented: real data, real logic, real UI, deployable and demonstrable. The kind of thing you'd show to a potential customer to validate the idea. That's achievable in 16-24 hours with a well-structured agent fleet.</p>
<h2>Phase 1: The Spec (2 hours, you)</h2>
<p>The biggest mistake in overnight MVP builds is underspecifying. Before launching any agents, write:</p>
<ul>
<li><strong>User story:</strong> "As a [user], I can [action], so that [outcome]." For every core flow.</li>
<li><strong>Data model:</strong> The 5-10 entities and their relationships. Draw an ERD.</li>
<li><strong>API contract:</strong> List every endpoint the frontend will call. Write it as an OpenAPI stub.</li>
<li><strong>Tech stack decision:</strong> Pick boring tech. Next.js + Postgres + Prisma is a reliable overnight stack.</li>
</ul>
<h2>Phase 2: Fleet Launch (30 min, you)</h2>
<p>Register and launch agents for each domain. Assign tasks to the board with explicit specs:</p>
<pre><code>amux register db-agent --dir ~/mvp --yolo       # Schema + migrations
amux register api-agent --dir ~/mvp --yolo      # Route handlers
amux register auth-agent --dir ~/mvp --yolo     # Auth flow
amux register frontend-agent --dir ~/mvp --yolo # Pages + components
amux register deploy-agent --dir ~/mvp --yolo   # Vercel config + CI</code></pre>
<h2>Phase 3: Overnight Run (8-12 hours, agents)</h2>
<p>This is the part where you sleep. amux's self-healing watchdog keeps agents running. By morning:</p>
<ul>
<li>Database schema is migrated and seeded</li>
<li>API routes are implemented and tested</li>
<li>Auth (login, register, session) is working</li>
<li>Core UI pages are functional</li>
<li>Vercel deployment is configured</li>
</ul>
<h2>Phase 4: Integration and Polish (4-6 hours, you)</h2>
<p>Morning is integration time. Agents built parts independently — now you wire them together, fix the inevitable integration issues, and add the polish that makes it feel real. This is where your product sense and taste matter most.</p>
<h2>Phase 5: Deploy and Demo</h2>
<p><code>git push</code> to main, Vercel deploys automatically. You have a working SaaS MVP by early afternoon the next day. Total time you spent: 8-10 hours. Total wall-clock time: 16-24 hours.</p>
${CTA}`,
},
{
  slug: 'context-window-strategy',
  title: 'Context Window Strategy for Long-Running Agents',
  description: 'How to design prompts and tasks to keep agents productive across context boundaries.',
  keywords: 'context window strategy agents, long running claude code, context management AI coding',
  content: `<h1>Context Window Strategy for Long-Running Agents</h1>
<p class="subtitle">Context exhaustion is the #1 failure mode for long-running agents. Here's how to design around it.</p>
<h2>The Context Problem</h2>
<p>Claude Code has a finite context window. In a long-running session — hours of coding, many files read, many outputs generated — the context window fills up. When it does, either the agent stops working or (with amux) it auto-compacts. Auto-compact helps, but poorly structured sessions lose important context during compaction and produce lower-quality output afterward.</p>
<p>The solution is to design tasks and sessions so that context usage is efficient from the start — not treating compaction as a fix for poor session hygiene.</p>
<h2>Task Scoping Rules</h2>
<ul>
<li><strong>One task per session:</strong> Don't ask one agent to implement a feature, write tests, and update docs. Give those to three agents. Each agent's context stays focused on one problem.</li>
<li><strong>Specify files explicitly:</strong> "Read src/api/users.ts and src/models/user.ts" is better than "look at the user-related code." Agents that search broadly consume more context finding relevant files.</li>
<li><strong>Set scope boundaries:</strong> Tell the agent what to ignore. "Don't read test files unless needed for understanding the interface" prevents unnecessary context loading.</li>
<li><strong>Short task horizons:</strong> Tasks that take 30 minutes are better than tasks that take 3 hours. Short tasks complete within one healthy context window. Long tasks risk degradation.</li>
</ul>
<h2>amux's Auto-Compact</h2>
<p>When amux detects context exhaustion, it:</p>
<ol>
<li>Sends <code>/compact</code> to summarize the conversation</li>
<li>Monitors for the summary confirmation</li>
<li>Resumes the agent</li>
</ol>
<p>This keeps agents running, but compacted context loses detail. After a compaction, agents are working from a summary, not the full conversation. Quality can drop. The goal is to design tasks that complete before compaction is needed.</p>
<h2>Context Checkpointing Patterns</h2>
<p>For genuinely long tasks that can't be broken up:</p>
<ul>
<li><strong>Commit frequently:</strong> Instruct agents to <code>git commit</code> after each meaningful unit of work. Commits externalize progress so context can be cleared more aggressively.</li>
<li><strong>Write progress notes:</strong> Have the agent write a <code>PROGRESS.md</code> file summarizing what it's done and what's left. This note survives compaction and re-orients the agent.</li>
<li><strong>Staged handoffs:</strong> Break a large task into sequential sub-tasks on the board. When one agent completes its sub-task, a fresh agent starts the next with a clean context window.</li>
</ul>
${CTA}`,
},
{
  slug: 'ai-coding-2025-landscape',
  title: "The 2025 AI Coding Landscape: A Developer's Field Guide",
  description: 'A practical overview of the tools, tradeoffs, and workflows shaping AI-assisted development in 2025.',
  keywords: 'AI coding landscape 2025, AI developer tools guide, best AI coding tools 2025',
  content: `<h1>The 2025 AI Coding Landscape: A Developer's Field Guide</h1>
<p class="subtitle">Which tools matter, what they're actually good for, and where parallel agents fit.</p>
<h2>The Tool Categories</h2>
<p>In 2025, AI coding tools fall into a few distinct categories. Understanding the categories helps you pick the right tool for the right job — and avoid paying for overlap.</p>
<table>
<thead><tr><th>Category</th><th>Examples</th><th>Best for</th></tr></thead>
<tbody>
<tr><td>IDE-integrated assistants</td><td>GitHub Copilot, Cursor, Windsurf</td><td>Real-time completions, inline edits, interactive pair programming</td></tr>
<tr><td>Terminal agents</td><td>Claude Code, Aider, Goose</td><td>Autonomous task execution, file editing, running commands</td></tr>
<tr><td>Agent orchestrators</td><td>amux, Claude Code Agent Teams</td><td>Running multiple terminal agents in parallel, fleet management</td></tr>
<tr><td>Cloud AI IDEs</td><td>Replit Agent, Bolt.new, v0</td><td>Prototype generation, no-setup environments</td></tr>
<tr><td>Specialized generators</td><td>v0 (UI), GitHub Copilot Workspace</td><td>Specific output types: UI components, PR descriptions</td></tr>
</tbody>
</table>
<h2>The Dominant Patterns</h2>
<p>The workflows that are actually producing results in 2025:</p>
<ul>
<li><strong>IDE + agent orchestrator:</strong> Use Cursor or Copilot for real-time interactive work. Use amux + Claude Code for batch operations and overnight runs. These don't compete — they complement.</li>
<li><strong>Spec-driven fleet:</strong> Write detailed task specs, launch parallel agents per spec, review and merge output. Compress days of work into hours.</li>
<li><strong>Continuous background agent:</strong> A persistent agent that monitors GitHub issues, generates fix PRs, and updates documentation — running continuously with no human in the loop.</li>
</ul>
<h2>What Actually Differentiates Tools</h2>
<p>Ignore the marketing. The real differentiators:</p>
<ul>
<li><strong>Model quality:</strong> Which underlying model? How well does it handle your codebase's language and patterns?</li>
<li><strong>Autonomy:</strong> How much can it do without asking for confirmation? Constant interruptions destroy the value of autonomous agents.</li>
<li><strong>Self-healing:</strong> What happens when the agent crashes or gets stuck? Does it recover automatically?</li>
<li><strong>Context management:</strong> How does the tool handle long sessions and large codebases?</li>
<li><strong>Cost transparency:</strong> Can you see what you're spending per session?</li>
</ul>
<h2>The Near-Term Direction</h2>
<p>The clearest trend: agents are getting more autonomous, tools are adding more parallelism, and the developer's role is shifting toward specification, review, and orchestration. The bottleneck in 2025 isn't "can the AI code" — it's "can the developer specify tasks clearly enough and review output fast enough to saturate the agents." That's a very different skill from traditional coding, and it's worth developing deliberately.</p>
${CTA}`,
},
];


// ═══════════════════════════════════════════════════════════════════
// MORE COMPARISONS (5)
// ═══════════════════════════════════════════════════════════════════
const moreCompare = [
{
  slug: 'amux-vs-bolt-new',
  title: 'amux vs Bolt.new',
  description: 'Compare amux with Bolt.new for AI-assisted development workflows.',
  keywords: 'amux vs bolt new, bolt.new alternative, AI coding comparison',
  content: `<h1>amux vs Bolt.new</h1>
<p class="subtitle">Bolt.new generates apps in the browser. amux runs agents against your existing codebase. Different tools for different moments.</p>
<p><strong>Bolt.new</strong> is a browser-based AI coding environment that generates full-stack applications from prompts — instant, no setup, great for prototypes. <strong>amux</strong> is a local agent orchestrator that runs Claude Code sessions against your own codebase, with a web dashboard, self-healing, and parallel execution.</p>
<h2>Key differences</h2>
${comparisonTable([
['Bolt.new','Environment','Local machine, your codebase','Browser sandbox'],
['Bolt.new','Parallel agents','Dozens of independent sessions','Single session'],
['Bolt.new','Existing codebases','Full access to your files, git, tools','Starts from scratch'],
['Bolt.new','Unattended operation','Self-healing watchdog, overnight runs','Browser tab must stay open'],
['Bolt.new','Data privacy','Local only, code never leaves your machine','Code runs in cloud sandbox'],
['Bolt.new','Cost','API tokens only (no platform fee)','Subscription + token credits'],
['Bolt.new','Open source','Yes (MIT)','No'],
['Bolt.new','Mobile management','PWA dashboard','Browser only'],
])}
<h2>When to use Bolt.new</h2>
<p>Bolt.new is ideal for <strong>generating a new project from scratch</strong> when you want instant setup and don't have an existing codebase. It's a great prototyping tool for validating ideas quickly without any local configuration.</p>
<h2>When to use amux</h2>
<p>amux is the right choice when you're working on an <strong>existing codebase</strong> and want to run multiple autonomous agents in parallel — overnight, from your phone, with a shared task board and self-healing infrastructure. If you're past the prototype stage, amux is the tool for the long run.</p>
${CTA}`,
},
{
  slug: 'amux-vs-replit-agent',
  title: 'amux vs Replit Agent',
  description: 'Compare amux with Replit Agent for automated coding tasks.',
  keywords: 'amux vs replit, replit agent alternative, cloud vs local AI coding',
  content: `<h1>amux vs Replit Agent</h1>
<p class="subtitle">Cloud-hosted Replit vs local-first amux. The tradeoffs are real — here's what matters.</p>
<p><strong>Replit Agent</strong> is an AI-powered cloud development environment that can build and deploy apps in the Replit cloud. <strong>amux</strong> runs Claude Code agents locally (or on your own server) against your existing codebase, with no vendor lock-in.</p>
<h2>Key differences</h2>
${comparisonTable([
['Replit Agent','Infrastructure','Runs locally or on your server','Runs in Replit cloud'],
['Replit Agent','Parallel agents','Dozens of independent sessions','Single agent per project'],
['Replit Agent','Existing codebase','Full local access + git integration','Import or start from scratch'],
['Replit Agent','Data ownership','Your code stays on your machine','Code stored on Replit servers'],
['Replit Agent','Self-healing','Auto-compact, restart, unstick agents','Managed by Replit'],
['Replit Agent','Open source','Yes (MIT)','No'],
['Replit Agent','Offline capability','PWA works offline','Cloud-dependent'],
['Replit Agent','Pricing','API tokens only','Subscription required'],
])}
<h2>When to use Replit Agent</h2>
<p>Replit Agent makes sense when you want a <strong>zero-setup cloud environment</strong> — no local dependencies, instant deployment, and a browser-accessible IDE. Great for learners, quick experiments, or projects that live entirely in the cloud.</p>
<h2>When to use amux</h2>
<p>amux is better when you care about <strong>data privacy</strong>, need to work with <strong>existing local codebases</strong>, want to run agents in parallel, or want the flexibility that comes with open-source, local-first tooling. If your codebase can't go to a third-party cloud, amux is your option.</p>
${CTA}`,
},
{
  slug: 'amux-vs-v0',
  title: 'amux vs v0 (Vercel)',
  description: "Compare amux with Vercel's v0 for AI-driven development.",
  keywords: 'amux vs v0, vercel v0 alternative, UI generation vs agent orchestration',
  content: `<h1>amux vs v0 (Vercel)</h1>
<p class="subtitle">v0 generates React UI components from prompts. amux orchestrates full-codebase agents. They solve different problems.</p>
<p><strong>v0 by Vercel</strong> is a specialized AI tool for generating React and Next.js UI components from natural language prompts — excellent for building interfaces quickly. <strong>amux</strong> is a general-purpose agent orchestrator for running Claude Code sessions across your entire stack in parallel.</p>
<h2>Key differences</h2>
${comparisonTable([
['v0','Scope','Full-stack: code, tests, infra, docs','UI component generation'],
['v0','Parallel agents','Dozens of independent sessions','Single generation session'],
['v0','Codebase access','Full local filesystem access','Isolated generation sandbox'],
['v0','Unattended runs','Self-healing overnight agent fleet','Interactive prompt-response'],
['v0','Technology scope','Any language or framework','React/Next.js/Tailwind focused'],
['v0','Open source','Yes (MIT)','No'],
['v0','Pricing','API tokens only','Credits system'],
])}
<h2>They Complement Each Other</h2>
<p>v0 and amux are more complementary than competitive. A common workflow: use <strong>v0 to generate initial component designs</strong>, then pull them into your codebase and use <strong>amux agents to integrate, test, and iterate</strong> on the components alongside the rest of your application.</p>
<h2>When to use v0</h2>
<p>When you need to quickly generate a UI mockup or component with good visual design — especially for Tailwind + shadcn/ui based projects. v0 is fast and produces visually polished output for common UI patterns.</p>
<h2>When to use amux</h2>
<p>When you're working on a real codebase that needs agents working on backend logic, database layers, tests, and infrastructure — not just UI components. If your scope goes beyond the UI, amux handles the full stack.</p>
${CTA}`,
},
{
  slug: 'amux-vs-autogen',
  title: 'amux vs AutoGen (Microsoft)',
  description: "Compare amux with Microsoft's AutoGen framework for multi-agent systems.",
  keywords: 'amux vs autogen, autogen alternative, multi-agent framework comparison',
  content: `<h1>amux vs AutoGen (Microsoft)</h1>
<p class="subtitle">AutoGen is a framework for building multi-agent systems. amux is an orchestrator for running Claude Code agents. Different levels of abstraction.</p>
<p><strong>AutoGen</strong> is Microsoft's open-source framework for building multi-agent conversation systems — you program agent interactions in Python. <strong>amux</strong> is an operational tool for running Claude Code (AI coding agents) in parallel with a web dashboard, shared board, and self-healing infrastructure.</p>
<h2>Key differences</h2>
${comparisonTable([
['AutoGen','Purpose','Operational agent fleet management','Agent interaction framework'],
['AutoGen','Agent type','Claude Code (coding-specialized)','General-purpose LLM agents'],
['AutoGen','Setup','Clone + run, web dashboard in minutes','Requires Python programming'],
['AutoGen','Web UI','Full dashboard with peek, board, mobile','No built-in UI'],
['AutoGen','Self-healing','Built-in watchdog for coding agents','Custom implementation needed'],
['AutoGen','Task coordination','SQLite kanban with atomic claiming','Custom implementation needed'],
['AutoGen','User type','Developers using Claude Code','Developers building agent systems'],
])}
<h2>Different Levels of Abstraction</h2>
<p>AutoGen is a <strong>framework</strong> — you use it to build custom multi-agent systems. amux is an <strong>operator</strong> — you use it to run Claude Code agents doing real coding work. They operate at different levels of abstraction.</p>
<h2>When to use AutoGen</h2>
<p>When you're building a custom multi-agent application — research tools, complex reasoning systems, automated workflows that require programming custom agent interactions. AutoGen gives you the primitives to build whatever you need.</p>
<h2>When to use amux</h2>
<p>When you want to run Claude Code agents to actually write code in your repositories — right now, with a UI, self-healing, and a task board. amux is operational where AutoGen is foundational.</p>
${CTA}`,
},
{
  slug: 'amux-vs-claude-desktop',
  title: 'amux vs Claude Desktop',
  description: 'Compare amux with the Claude Desktop app for development workflows.',
  keywords: 'amux vs claude desktop, claude app alternative, terminal vs desktop AI coding',
  content: `<h1>amux vs Claude Desktop</h1>
<p class="subtitle">Claude Desktop is for conversations. amux is for coding at scale. The comparison clarifies when to use which.</p>
<p><strong>Claude Desktop</strong> is Anthropic's native macOS and Windows application for conversational use of Claude — chat, document analysis, MCP tool connections. <strong>amux</strong> runs Claude Code (the coding-specialized CLI) in parallel terminal sessions with a web dashboard and agent orchestration.</p>
<h2>Key differences</h2>
${comparisonTable([
['Claude Desktop','Agent type','Claude Code (autonomous coding CLI)','Claude chat (conversational)'],
['Claude Desktop','Parallel sessions','Dozens of independent coding sessions','Single conversation window'],
['Claude Desktop','Codebase access','Direct filesystem, git, bash access','MCP filesystem (read-heavy)'],
['Claude Desktop','Unattended operation','Self-healing overnight runs','Interactive, requires user prompts'],
['Claude Desktop','Task coordination','Shared kanban board','No task management'],
['Claude Desktop','Open source','Yes (MIT)','No'],
['Claude Desktop','Mobile','PWA dashboard','iOS/Android app'],
])}
<h2>When to use Claude Desktop</h2>
<p>Claude Desktop is excellent for <strong>general-purpose AI assistance</strong>: drafting documents, analyzing files, researching topics, having extended conversations. If you need Claude to help you think through a problem, write a document, or analyze data, Claude Desktop is the right tool.</p>
<h2>When to use amux</h2>
<p>When you need Claude to <strong>actually execute code changes</strong> in your repository — autonomously, in parallel, with self-healing, overnight. amux runs Claude Code, which has direct filesystem and shell access and is specifically optimized for long-horizon coding tasks. It's the right tool when the output is code, not conversation.</p>
${CTA}`,
},
];

// ═══════════════════════════════════════════════════════════════════
// MORE GUIDES (5)
// ═══════════════════════════════════════════════════════════════════
const moreGuides = [
{
  slug: 'setting-up-yolo-mode',
  title: 'Setting Up YOLO Mode for Headless Agents',
  description: 'Configure amux sessions to run fully unattended — auto-approve tools, auto-respond to prompts, zero babysitting.',
  keywords: 'yolo mode setup, headless claude code, auto approve claude, unattended AI agent',
  content: `<h1>Setting Up YOLO Mode for Headless Agents</h1>
<p class="subtitle">YOLO mode lets Claude Code agents run without any confirmation prompts. Here's how to configure it safely.</p>
<h2>What YOLO Mode Does</h2>
<p>By default, Claude Code asks for confirmation before running potentially dangerous commands (deleting files, running shell scripts, making network requests). YOLO mode disables these confirmations — the agent runs autonomously without stopping to ask permission.</p>
<p>This is essential for overnight and long-running unattended agents. An agent that stops every 10 minutes waiting for your "yes" is not an unattended agent.</p>
<h2>Enabling YOLO Mode in amux</h2>
<p>Register your session with the <code>--yolo</code> flag:</p>
<pre><code>amux register myagent --dir ~/myproject --yolo</code></pre>
<p>This sets the <code>--dangerously-skip-permissions</code> flag in the underlying Claude Code invocation. You can verify it's active by checking the session config:</p>
<pre><code>amux info myagent</code></pre>
<h2>Scoping YOLO Mode Safely</h2>
<p>YOLO mode is powerful but requires deliberate scoping. Best practices:</p>
<ul>
<li><strong>Use git branches:</strong> Always run YOLO agents on a feature branch, never directly on main. Git is your undo button.</li>
<li><strong>Restrict the working directory:</strong> The <code>--dir</code> flag limits the agent's filesystem access to one directory. Don't give agents access to your entire home directory.</li>
<li><strong>Don't run YOLO agents with production credentials:</strong> Keep <code>.env</code> files with production database URLs out of agent-accessible directories. Use <code>.gitignore</code> and separate config directories.</li>
<li><strong>Review before merging:</strong> YOLO mode means the agent acts without confirmation — it does not mean you merge without review. Always review agent-generated PRs.</li>
</ul>
<h2>Claude Code's Permission Model</h2>
<p>Even in YOLO mode, Claude Code has a layered permission model:</p>
<ul>
<li>Network requests: allowed by default</li>
<li>File reads: always allowed in the working directory</li>
<li>File writes/deletes: allowed in YOLO mode</li>
<li>Shell commands: allowed in YOLO mode</li>
<li>System-level operations (outside working dir): generally restricted</li>
</ul>
<h2>YOLO Mode for Different Task Types</h2>
<table>
<thead><tr><th>Task type</th><th>YOLO mode</th><th>Reason</th></tr></thead>
<tbody>
<tr><td>Code generation</td><td>Yes</td><td>Low risk, easy to review</td></tr>
<tr><td>Test running</td><td>Yes</td><td>Tests are contained</td></tr>
<tr><td>Database migrations</td><td>Dev DB only</td><td>Never run against production</td></tr>
<tr><td>npm/pip installs</td><td>Yes</td><td>Review package.json afterward</td></tr>
<tr><td>Deployment commands</td><td>Staging only</td><td>Manual review for production</td></tr>
</tbody>
</table>
${CTA}`,
},
{
  slug: 'using-mcp-servers',
  title: 'Using MCP Servers with amux Sessions',
  description: 'Configure Model Context Protocol servers for each amux session to give agents specialized capabilities.',
  keywords: 'MCP server setup, claude code MCP, model context protocol amux',
  content: `<h1>Using MCP Servers with amux Sessions</h1>
<p class="subtitle">MCP servers give agents access to databases, APIs, browsers, and internal tools. Here's how to configure them in amux.</p>
<h2>What MCP Servers Do</h2>
<p>Model Context Protocol (MCP) is an open standard that lets Claude Code connect to external tool servers. Each MCP server exposes a set of tools that the agent can use: query a database, search the web, post to Slack, create GitHub issues, control a browser. With MCP, agents become systems — not just code writers.</p>
<h2>The amux mcp.json Config</h2>
<p>amux uses a centralized <code>mcp.json</code> in the project root for MCP server configuration. This file is shared by all sessions:</p>
<pre><code>{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/dir"]
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": {
        "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_your_token_here"
      }
    },
    "postgres": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-postgres"],
      "env": {
        "POSTGRES_CONNECTION_STRING": "postgresql://user:pass@localhost:5432/mydb"
      }
    }
  }
}</code></pre>
<h2>Useful MCP Servers for Coding Agents</h2>
<table>
<thead><tr><th>MCP Server</th><th>What it enables</th><th>Install</th></tr></thead>
<tbody>
<tr><td>server-filesystem</td><td>Controlled file access with path restrictions</td><td>@modelcontextprotocol/server-filesystem</td></tr>
<tr><td>server-github</td><td>Create PRs, issues, search code</td><td>@modelcontextprotocol/server-github</td></tr>
<tr><td>server-postgres</td><td>Query live database schema and data</td><td>@modelcontextprotocol/server-postgres</td></tr>
<tr><td>server-puppeteer</td><td>Browser automation and screenshots</td><td>@modelcontextprotocol/server-puppeteer</td></tr>
<tr><td>server-brave-search</td><td>Web search for docs and examples</td><td>@modelcontextprotocol/server-brave-search</td></tr>
<tr><td>server-slack</td><td>Post messages, read channels</td><td>@modelcontextprotocol/server-slack</td></tr>
</tbody>
</table>
<h2>Security Considerations</h2>
<ul>
<li><strong>Never put production database URLs in mcp.json if it's committed.</strong> Use environment variable references and keep secrets in <code>~/.amux/server.env</code>.</li>
<li><strong>Use path restrictions for filesystem MCP.</strong> Specify exactly which directories agents are allowed to access.</li>
<li><strong>Scope GitHub tokens.</strong> Give agents tokens with only the permissions they need (read repo, write PRs — not admin, not billing).</li>
</ul>
<h2>Testing Your MCP Setup</h2>
<pre><code># Start a session and verify MCP tools are available
amux start myagent
# Then in the dashboard, peek at the session and run:
# "List the MCP tools available to you"</code></pre>
${CTA}`,
},
{
  slug: 'team-workflow-setup',
  title: 'Setting Up amux for a Development Team',
  description: 'Share an amux instance across a team — naming conventions, board workflows, and access patterns.',
  keywords: 'amux team setup, shared agent dashboard, team AI coding workflow',
  content: `<h1>Setting Up amux for a Development Team</h1>
<p class="subtitle">amux can serve a whole team. Here's how to set up naming conventions, board workflows, and shared access.</p>
<h2>Deployment Options for Teams</h2>
<p>Teams can run amux in two configurations:</p>
<ul>
<li><strong>One amux per developer:</strong> Each developer runs their own amux instance locally. Sessions are named with developer initials (e.g., <code>eh-auth</code>, <code>jd-tests</code>). The board is individual but visible to the team via the dashboard.</li>
<li><strong>Shared amux server:</strong> amux runs on a shared server (cloud VM or on-prem machine) accessible by the whole team. All sessions and the board are shared. Use Tailscale for secure access.</li>
</ul>
<h2>Session Naming Conventions</h2>
<p>Clear naming conventions make fleet management much easier:</p>
<pre><code># Format: [owner]-[domain]-[task-type]
eh-auth-feature      # Ethan, auth domain, feature work
jd-payments-tests    # Jordan, payments domain, test writing
team-infra-deploy    # Shared, infrastructure, deployment

# Or by sprint ticket
FEAT-042-frontend
BUG-078-api
REFAC-015-db</code></pre>
<h2>Board Workflow for Teams</h2>
<p>The shared kanban board is the coordination hub. Team conventions that work well:</p>
<ul>
<li><strong>Tag issues with session names:</strong> When assigning a board task to an agent, set the <code>session</code> field. Agents claim tasks with <code>POST /api/board/TASK-ID/claim</code>.</li>
<li><strong>Status discipline:</strong> Only move to "doing" when an agent has actually claimed the task. Don't pre-move to doing as a planning artifact.</li>
<li><strong>Review column:</strong> Add a custom "review" status for tasks where agent output needs human review before merging.</li>
<li><strong>Daily board review:</strong> In standups, screen-share the amux dashboard — shows all running sessions and board status at a glance.</li>
</ul>
<h2>Access Control</h2>
<p>amux doesn't have built-in auth. For teams:</p>
<ul>
<li>Use Tailscale to expose amux only on your team's private network</li>
<li>Or bind amux to localhost and use SSH port forwarding for remote access</li>
<li>Never expose port 8822 directly to the public internet</li>
</ul>
<h2>Session Isolation</h2>
<p>If multiple developers share one amux instance, use separate working directories per developer and clear session ownership in naming. The amux board's <code>session</code> field shows which agent owns each task — use this to avoid two developers' agents working the same ticket.</p>
${CTA}`,
},
{
  slug: 'cost-optimization',
  title: 'Optimizing Token Costs with Parallel Agents',
  description: 'Strategies to get maximum output per dollar when running many Claude Code agents in parallel.',
  keywords: 'claude code token cost, AI coding cost optimization, reduce agent token spend',
  content: `<h1>Optimizing Token Costs with Parallel Agents</h1>
<p class="subtitle">Running 10 agents in parallel costs 10x per hour. Here's how to maximize what you get for that spend.</p>
<h2>The Token Cost Model</h2>
<p>Claude API pricing is per token: input tokens (what the agent reads) and output tokens (what the agent generates). For coding agents, input typically dominates: reading source files, reading error messages, reading test output. The goal of cost optimization is getting maximum useful output per token spent.</p>
<h2>Biggest Cost Wins</h2>
<h3>1. Right-size the model</h3>
<p>This is the single biggest lever. Haiku is ~10-20x cheaper than Opus per token. Most coding tasks don't require Opus-level reasoning. Use Sonnet for the bulk of your work, Opus only for complex architecture decisions.</p>
<table>
<thead><tr><th>Task type</th><th>Recommended model</th></tr></thead>
<tbody>
<tr><td>Documentation generation</td><td>Haiku</td></tr>
<tr><td>Test writing (standard patterns)</td><td>Haiku or Sonnet</td></tr>
<tr><td>Feature implementation</td><td>Sonnet</td></tr>
<tr><td>Complex refactoring</td><td>Sonnet</td></tr>
<tr><td>Architecture design + hard bugs</td><td>Opus</td></tr>
</tbody>
</table>
<h3>2. Write precise task descriptions</h3>
<p>A vague task ("fix the auth stuff") leads to an agent reading many files speculatively before finding the right one. A precise task ("Fix the JWT expiration bug in src/auth/middleware.ts line 47 — the token TTL should use seconds not milliseconds") goes straight to the problem. The precise task uses 5-10x fewer tokens to complete.</p>
<h3>3. Set explicit file scope</h3>
<p>Instruct agents to read only the files relevant to their task. "Only read files in src/api/users/ and src/models/user.ts" prevents agents from scanning the whole codebase for context they don't need.</p>
<h3>4. Use short-horizon tasks</h3>
<p>A 4-hour task consumes a large context window and may require compaction (losing context quality). Four 1-hour tasks on four agents cost the same in wall-clock time, but each agent operates at full context efficiency. Shorter tasks = better quality per token.</p>
<h3>5. Don't run agents that are stuck</h3>
<p>A stuck agent in a loop can burn thousands of tokens making no progress. Set up monitoring — check the dashboard every hour, kill sessions that haven't committed any code in 2 hours, redirect them with clearer instructions.</p>
<h2>Measuring Your Cost</h2>
<p>amux tracks per-session token spend in the dashboard. Check it daily. If one session is consuming 5x the tokens of others, it's either working on a legitimately harder task or it's stuck in a loop. Investigate.</p>
${CTA}`,
},
{
  slug: 'agent-debugging',
  title: 'Debugging Agent Fleets with amux',
  description: 'Tools and techniques for diagnosing stuck, looping, or wrong-output agents in a fleet.',
  keywords: 'debug AI agent, stuck claude code, agent troubleshooting, amux debugging',
  content: `<h1>Debugging Agent Fleets with amux</h1>
<p class="subtitle">An agent fleet has many failure modes. Here's how to diagnose and fix each one.</p>
<h2>Your Debugging Tools</h2>
<ul>
<li><strong>Live peek:</strong> See any agent's terminal output in real time from the dashboard. The most important debugging tool.</li>
<li><strong>Board status:</strong> If a task is stuck on "doing" for longer than expected, the agent is either blocked or in a loop.</li>
<li><strong>Token tracker:</strong> High token spend with no committed code = wasted loop or incorrect file reading.</li>
<li><strong>Git diff:</strong> Run <code>git diff</code> in the session's working directory to see what the agent has actually changed.</li>
<li><strong>Send message:</strong> Use the dashboard to send a correction or clarification to any running session.</li>
</ul>
<h2>Failure Mode Playbook</h2>
<h3>Agent is stuck on a compilation error</h3>
<p><strong>Symptom:</strong> Peek shows the same error repeating, agent is trying different fixes but none work.</p>
<p><strong>Diagnosis:</strong> The agent has incorrect information about the codebase — wrong import path, wrong function signature, incorrect type.</p>
<p><strong>Fix:</strong> Send the exact correct information via dashboard message. "The correct import is: <code>import { User } from '../models/user'</code>" — specific is better than general.</p>
<h3>Agent is reading the wrong files</h3>
<p><strong>Symptom:</strong> Git diff shows changes to files unrelated to the task.</p>
<p><strong>Diagnosis:</strong> The task description was ambiguous about scope. The agent found a plausible but incorrect location for the relevant code.</p>
<p><strong>Fix:</strong> Send a correction with explicit file paths. Reset the git changes, restart with a more specific task description that names exact files.</p>
<h3>Agent completed the wrong task</h3>
<p><strong>Symptom:</strong> The code runs and tests pass, but it doesn't implement what you wanted.</p>
<p><strong>Diagnosis:</strong> Task specification was ambiguous or the agent misinterpreted the intent.</p>
<p><strong>Fix:</strong> This is the hardest failure mode because the output looks correct. Prevention is the fix: write task specs that include examples, specify the interface explicitly, and include a "success looks like" description.</p>
<h3>Agent is in a reasoning loop</h3>
<p><strong>Symptom:</strong> The agent keeps reading the same files, generating the same analysis, not writing any code.</p>
<p><strong>Diagnosis:</strong> The task is too open-ended. The agent is stuck deciding between approaches.</p>
<p><strong>Fix:</strong> Break the impasse by sending a specific direction: "Use approach X. Start by editing file Y."</p>
<h3>Context compaction degraded output quality</h3>
<p><strong>Symptom:</strong> After an auto-compact, the agent's output quality drops — it seems to have forgotten important context about the codebase.</p>
<p><strong>Fix:</strong> After compaction, send a brief context summary: "You're implementing the users API in src/api/users/. The User model is in src/models/user.ts. The coding style uses async/await with try-catch error handling."</p>
${CTA}`,
},
];


// ═══════════════════════════════════════════════════════════════════
// MORE GLOSSARY (5)
// ═══════════════════════════════════════════════════════════════════
const moreGlossary = [
{
  slug: 'headless-coding-agent',
  title: 'Headless Coding Agent',
  description: 'A coding agent that runs without a GUI, controlled entirely via CLI or API.',
  keywords: 'headless AI agent, background coding agent, CLI AI coding',
  content: `<h1>Headless Coding Agent</h1>
<p class="subtitle">A coding agent that runs without a graphical interface — controlled via CLI, API, or web dashboard.</p>
<h2>Definition</h2>
<p>A <strong>headless coding agent</strong> is an AI coding agent that operates without requiring a graphical user interface (GUI) open and actively supervised. The agent runs in a background process — typically a terminal session managed by a process supervisor like tmux — and can be monitored and controlled remotely via API or a web dashboard.</p>
<p>The term "headless" comes from web browser automation (headless Chrome, headless Firefox), where the browser operates without a visible window. Headless coding agents extend this concept: Claude Code running unattended in a tmux session, managed by amux, is a headless coding agent.</p>
<h2>Key Characteristics</h2>
<ul>
<li><strong>Background execution:</strong> Runs as a persistent process, not tied to a terminal session that requires user presence.</li>
<li><strong>Remote management:</strong> Controlled via REST API or web UI rather than keyboard input.</li>
<li><strong>Self-healing:</strong> A well-designed headless agent recovers from crashes, context exhaustion, and stuck prompts automatically.</li>
<li><strong>Unattended operation:</strong> Can run overnight, over weekends, or across timezones without a human watching.</li>
</ul>
<h2>How amux Implements Headless Agents</h2>
<p>amux runs Claude Code inside tmux sessions (which persist across terminal disconnects), provides a web dashboard for monitoring and control, and implements a self-healing watchdog that handles failure modes autonomously. This combination makes Claude Code — which is normally an interactive tool — fully headless and unattended.</p>
<h2>Contrast with Interactive Agents</h2>
<p>An interactive coding agent (like Claude Code running directly in your terminal) requires the user to be present — to confirm actions, answer questions, and redirect the agent. A headless agent is designed to operate independently, with the user checking in periodically via a dashboard rather than watching in real time.</p>
<p>Most production agent workflows combine both: interactive sessions for complex decisions and exploratory work, headless sessions for long-running batch tasks and overnight execution.</p>
${CTA}`,
},
{
  slug: 'vibe-coding',
  title: 'Vibe Coding',
  description: 'A programming style where developers describe intent in natural language and AI generates the implementation.',
  keywords: 'vibe coding, AI-assisted programming, natural language coding',
  content: `<h1>Vibe Coding</h1>
<p class="subtitle">Programming by describing intent in natural language and having AI generate the implementation.</p>
<h2>Definition</h2>
<p><strong>Vibe coding</strong> is an informal term for a programming approach where the developer focuses on describing desired behavior and outcomes in natural language, while an AI assistant or agent generates the actual implementation code. The developer's role shifts from line-by-line implementation to higher-level specification, review, and direction.</p>
<p>The term was popularized in early 2025 as AI coding assistants became capable enough to reliably implement complex functionality from natural language descriptions. It implies a more relaxed, intuitive style of software development — working with the "vibe" of what you want rather than the exact mechanics of how to implement it.</p>
<h2>What It Looks Like in Practice</h2>
<p>Traditional coding: "I need to write a Redis-backed rate limiter that allows 100 requests per minute per IP."</p>
<p>Then: writing the code, handling edge cases, testing, etc.</p>
<p>Vibe coding: Describe the behavior you want. Let an agent implement it. Review the output and iterate.</p>
<h2>What Vibe Coding Requires</h2>
<p>Despite the casual name, effective vibe coding requires real skill:</p>
<ul>
<li><strong>Clear specification:</strong> Describing intent precisely enough that AI generates correct code. Ambiguous descriptions produce ambiguous output.</li>
<li><strong>Critical review:</strong> Reading and evaluating AI-generated code for correctness, security, and quality — even code you didn't write.</li>
<li><strong>Architecture awareness:</strong> Understanding how generated code fits into the broader system — interfaces, dependencies, performance implications.</li>
<li><strong>Iteration:</strong> Knowing how to redirect AI when the first output isn't right.</li>
</ul>
<h2>Vibe Coding at Scale</h2>
<p>Vibe coding reaches its full potential when combined with parallel agent fleets. Instead of one developer describing tasks to one agent, the developer decomposes a problem into parallel workstreams and runs multiple agents simultaneously. The coordination and review work scales, but the implementation work doesn't — agents handle it in parallel. amux is the infrastructure for vibe coding at this scale.</p>
${CTA}`,
},
{
  slug: 'agent-fleet',
  title: 'Agent Fleet',
  description: 'A group of AI coding agents running in parallel, coordinated toward a shared goal.',
  keywords: 'agent fleet, multi-agent fleet, parallel AI agents',
  content: `<h1>Agent Fleet</h1>
<p class="subtitle">Multiple AI coding agents running in parallel, coordinated via a shared task board and REST API.</p>
<h2>Definition</h2>
<p>An <strong>agent fleet</strong> is a collection of AI coding agents running simultaneously on related tasks, coordinated toward a shared development goal. Rather than one agent working sequentially through a task list, a fleet of agents works in parallel — each handling a separate domain, feature, or workstream — dramatically compressing the wall-clock time for large-scale development work.</p>
<h2>Fleet Coordination Patterns</h2>
<p>Agent fleets require coordination infrastructure to avoid duplication and conflicts:</p>
<ul>
<li><strong>Task board:</strong> A shared kanban board where agents claim tasks atomically — preventing two agents from working on the same ticket simultaneously.</li>
<li><strong>Git branches:</strong> Each agent works on its own branch. A human (or orchestrator agent) reviews and merges the branches when complete.</li>
<li><strong>REST API:</strong> Agents can communicate with each other and the coordination layer via HTTP — reporting status, requesting information, delegating sub-tasks.</li>
<li><strong>Naming conventions:</strong> Clear session names (e.g., <code>auth-agent</code>, <code>payments-agent</code>) make fleet management legible.</li>
</ul>
<h2>Fleet Sizes in Practice</h2>
<table>
<thead><tr><th>Fleet size</th><th>Typical use case</th><th>Management overhead</th></tr></thead>
<tbody>
<tr><td>2–4 agents</td><td>Feature development across frontend/backend/tests</td><td>Low — easy to monitor</td></tr>
<tr><td>5–10 agents</td><td>Sprint-scale parallel execution</td><td>Medium — dashboard monitoring needed</td></tr>
<tr><td>10–20 agents</td><td>Large codebase refactoring, migration, coverage blitz</td><td>High — requires good task specs and triage</td></tr>
<tr><td>20+ agents</td><td>Monorepo-scale transformations, enterprise migrations</td><td>Very high — orchestrator agent recommended</td></tr>
</tbody>
</table>
<h2>amux as Fleet Infrastructure</h2>
<p>amux provides all the infrastructure needed for a production agent fleet: tmux session management, a web dashboard with live peek, a SQLite kanban board with atomic claiming, a REST API for agent coordination, and a self-healing watchdog that keeps agents running unattended. It's the operational layer that turns individual Claude Code sessions into a coordinated fleet.</p>
${CTA}`,
},
{
  slug: 'llm-context-window',
  title: 'LLM Context Window',
  description: 'The maximum amount of text an LLM can process in a single request — a key constraint in long-running agent sessions.',
  keywords: 'LLM context window, context limit, token window AI',
  content: `<h1>LLM Context Window</h1>
<p class="subtitle">The context window defines how much an AI agent can "remember" in a single session — and why long-running agents need careful management.</p>
<h2>Definition</h2>
<p>The <strong>context window</strong> of a large language model (LLM) is the maximum number of tokens (roughly, words and characters) that the model can process in a single request — including both the input (everything the model has seen) and the output (what it generates). Tokens that exceed the context window are either truncated or cause an error.</p>
<p>For modern models like Claude: Claude Sonnet and Opus have very large context windows (200k tokens or more). However, the effective working context — the amount the model reliably uses for reasoning — is often smaller in practice.</p>
<h2>Why It Matters for Coding Agents</h2>
<p>A long-running coding agent accumulates context over time: the initial task description, every file it has read, every command it has run, every error message and fix attempt. As the session progresses, this context grows. When the context window fills:</p>
<ul>
<li>Older context is pushed out (the agent "forgets" early information)</li>
<li>Or the agent must be compacted (conversation is summarized and condensed)</li>
<li>Or the agent errors out completely</li>
</ul>
<h2>Context Window Management Strategies</h2>
<ul>
<li><strong>Short-horizon tasks:</strong> Design tasks to complete within a fraction of the context window. This leaves headroom for error recovery and iteration.</li>
<li><strong>Explicit file references:</strong> Tell agents exactly which files to read rather than letting them search. Reduces speculative context loading.</li>
<li><strong>Frequent commits:</strong> Committing progress externalizes it — the agent can reference the committed code without keeping it all in context.</li>
<li><strong>Auto-compact:</strong> amux automatically sends <code>/compact</code> when context fills, summarizing the conversation and continuing. Quality can drop after compaction — design tasks to avoid it.</li>
</ul>
<h2>Context vs. Memory</h2>
<p>LLM context is not the same as long-term memory. Context is everything within the current session's window. Long-term memory (persisting information across sessions) requires external storage — files, databases, or dedicated memory systems. amux uses <code>.md</code> memory files and a global memory API for cross-session persistence.</p>
${CTA}`,
},
{
  slug: 'unattended-coding',
  title: 'Unattended Coding',
  description: 'Running AI coding agents without human supervision — enabled by self-healing, YOLO mode, and atomic task management.',
  keywords: 'unattended AI coding, autonomous coding agent, headless development',
  content: `<h1>Unattended Coding</h1>
<p class="subtitle">AI coding agents that run without supervision — completing tasks, recovering from failures, and reporting results without a human present.</p>
<h2>Definition</h2>
<p><strong>Unattended coding</strong> refers to AI coding agents that operate autonomously without requiring a human to monitor or intervene during execution. The agent receives a task, executes it, handles failures, and completes or reports a failure — all without a human in the loop.</p>
<p>True unattended coding requires four capabilities that most AI coding tools lack: YOLO mode (no confirmation prompts), self-healing (automatic recovery from crashes), task management (knowing what to do next), and remote monitoring (visibility without physical presence).</p>
<h2>Requirements for Unattended Operation</h2>
<table>
<thead><tr><th>Requirement</th><th>Why it matters</th><th>amux feature</th></tr></thead>
<tbody>
<tr><td>No confirmation prompts</td><td>Confirmations require human presence</td><td>YOLO mode (--dangerously-skip-permissions)</td></tr>
<tr><td>Crash recovery</td><td>Agents crash; a crashed agent does nothing</td><td>Self-healing watchdog</td></tr>
<tr><td>Context management</td><td>Context exhaustion stops agents</td><td>Auto-compact on context fill</td></tr>
<tr><td>Task queuing</td><td>Agents need a task source when current task is done</td><td>SQLite kanban board with atomic claiming</td></tr>
<tr><td>Remote monitoring</td><td>Human needs visibility without being present</td><td>Web dashboard + PWA</td></tr>
</tbody>
</table>
<h2>What Unattended Coding Enables</h2>
<ul>
<li><strong>Overnight runs:</strong> Launch an agent fleet before bed and wake up to completed work.</li>
<li><strong>Timezone-spanning delivery:</strong> Agents work while you're asleep, delivering output to teammates in other timezones.</li>
<li><strong>Continuous background tasks:</strong> A persistent agent watches for new issues, generates fix PRs, updates documentation.</li>
<li><strong>Weekend development:</strong> A fleet running over the weekend can complete a sprint's worth of routine work without any developer time.</li>
</ul>
<h2>What Still Requires Attention</h2>
<p>Unattended coding doesn't eliminate human judgment — it defers it. Agents complete implementation tasks unattended, but humans still need to: write the task specifications before the run, review and merge the output after, and triage any tasks that failed or went off-track. The human role shifts from implementation to specification and review.</p>
${CTA}`,
},
];

// ═══════════════════════════════════════════════════════════════════
// MORE SOLUTIONS (5)
// ═══════════════════════════════════════════════════════════════════
const moreSolutions = [
{
  slug: 'bootstrapped-founders',
  title: 'amux for Bootstrapped Founders',
  description: 'Ship a full product with one engineer\'s time and a fleet of AI agents.',
  keywords: 'bootstrapped founder AI coding, solo founder developer tools, indie hacker AI agents',
  content: `<h1>amux for Bootstrapped Founders</h1>
<p class="subtitle">One engineer. A fleet of agents. Team-speed execution without the payroll.</p>
<h2>The Bootstrapped Founder's Equation</h2>
<p>Bootstrapped founders make a deliberate trade: less capital, more control. The risk is execution speed — you can't hire a 10-person engineering team. The opportunity with amux is closing that speed gap without the headcount cost.</p>
<p>A bootstrapped founder running an amux agent fleet can execute product development at a pace that previously required 4-6 engineers. The cost: API tokens ($20-100/day depending on intensity) plus the time to write task specs and review output. The savings: $400k-800k/year in engineering salaries at typical startup rates.</p>
<h2>How Bootstrapped Founders Use amux</h2>
<ul>
<li><strong>Launch fast:</strong> Build an MVP in days instead of weeks. Parallel agents handle the full stack simultaneously — backend, frontend, auth, payments, deployment.</li>
<li><strong>Iterate on customer feedback:</strong> Gather feedback Monday, spec tasks Tuesday, agents ship changes by Thursday. A weekly product iteration cycle is achievable solo with a fleet.</li>
<li><strong>Cover all the bases:</strong> Bootstrapped founders can't afford gaps — no dedicated QA engineer, no DevOps specialist. Agent fleets cover test writing, CI setup, monitoring config, and documentation in parallel with feature work.</li>
<li><strong>Stay lean through growth:</strong> Scale development capacity by adding agent sessions, not headcount. The marginal cost of an additional agent is API tokens.</li>
</ul>
<h2>The Right Way to Think About It</h2>
<p>amux doesn't replace engineering judgment — it amplifies it. The founder still decides what to build, what the architecture should be, and what good looks like. The agents implement those decisions at scale. The bottleneck becomes product clarity and taste, not implementation capacity. That's actually a better bottleneck for a founder to have.</p>
<h2>Practical Cost</h2>
<p>A typical bootstrapped founder running amux might spend $30-60/day on API tokens during active development sprints and $5-10/day during lighter periods. Monthly: $300-600 during intensive phases. Compare to a single mid-level engineer at $150k/year ($12,500/month). The math is clear.</p>
${CTA}`,
},
{
  slug: 'agency-developers',
  title: 'amux for Agency Developers',
  description: 'Handle more client projects simultaneously — parallel agents for parallel clients.',
  keywords: 'agency developer tools, freelance AI coding, web agency automation',
  content: `<h1>amux for Agency Developers</h1>
<p class="subtitle">Run one agent fleet per client. Bill hours for oversight. Deliver at scale.</p>
<h2>The Agency Model + Parallel Agents</h2>
<p>Digital agencies and freelance developers have a capacity problem: they can only work on one thing at a time, but clients want things done simultaneously. amux's parallel agent model maps directly to the agency workflow: one agent fleet per client project, running in parallel, coordinated via a shared board.</p>
<h2>Agency Workflows with amux</h2>
<ul>
<li><strong>Multi-client delivery:</strong> Run 3-4 client projects simultaneously with separate agent sessions. Client A's feature work, Client B's bug fixes, and Client C's performance optimization — all in parallel.</li>
<li><strong>Sprint delivery:</strong> Take on 2-week sprint contracts and deliver in 5 days using a full agent fleet. The margin improvement is significant.</li>
<li><strong>Client-specific environments:</strong> Each client gets its own amux session with its own directory, environment variables, and MCP server config. Complete isolation between client codebases.</li>
<li><strong>Maintenance contracts:</strong> A persistent agent handles routine maintenance (dependency updates, security patches, minor bugs) for multiple clients simultaneously. Bill a flat monthly fee, use agents to keep the overhead low.</li>
</ul>
<h2>Scope Management</h2>
<p>Agency work lives and dies on scope management. amux's board is useful here: every task is specified before it's assigned to an agent. The board serves as a scope ledger — visible to clients, showing exactly what work is in progress and done. Less scope creep, clearer deliverables.</p>
<h2>The Efficiency Conversation</h2>
<p>Some agencies worry about billing transparency when agents are doing the implementation work. The framing that works: agencies charge for expertise, judgment, and outcomes — not keystrokes. The agent is a tool that enables higher-leverage work. The agency's value is knowing what to build, how to specify it, and how to ensure the output is right.</p>
${CTA}`,
},
{
  slug: 'senior-engineers',
  title: 'amux for Senior Engineers',
  description: 'Spend your time on architecture and decisions. Delegate implementation to agent fleets.',
  keywords: 'senior engineer AI tools, staff engineer AI, principal developer AI coding',
  content: `<h1>amux for Senior Engineers</h1>
<p class="subtitle">Senior engineers are most valuable when making decisions, not implementing them. amux handles the implementation.</p>
<h2>The Senior Engineer's Leverage Problem</h2>
<p>Senior, staff, and principal engineers are expensive and rare. Their value comes from system design, technical decisions, mentorship, and architectural judgment — not from writing CRUD endpoints. But in most organizations, senior engineers still spend significant time on implementation work: features that are below their skill ceiling, migrations that are tedious rather than interesting, coverage work that is mechanical rather than creative.</p>
<p>amux changes this calculation. Implementation work that doesn't require senior judgment can be delegated to an agent fleet. Senior engineers can operate almost entirely at the design and review layer.</p>
<h2>What Seniors Delegate to Agents</h2>
<ul>
<li><strong>Implementation of designed systems:</strong> Senior designs the API contract, data model, and interfaces. Agents implement the handlers, services, and tests.</li>
<li><strong>Migration execution:</strong> Senior designs the migration strategy and writes the critical path. Agents execute the per-file changes.</li>
<li><strong>Test coverage:</strong> Senior writes the integration tests that require deep system understanding. Agents write the unit tests that follow obvious patterns.</li>
<li><strong>Documentation:</strong> Senior reviews and approves. Agents generate the initial draft from code and comments.</li>
<li><strong>Dependency updates:</strong> Senior approves the upgrade path. Agents handle the per-file migration changes.</li>
</ul>
<h2>What Seniors Keep</h2>
<ul>
<li>Architecture decisions and tradeoff analysis</li>
<li>Ambiguous problem decomposition</li>
<li>Code review and quality enforcement</li>
<li>Mentorship and technical guidance to the team</li>
<li>Stakeholder communication and technical roadmap</li>
</ul>
<h2>The Multiplier Effect</h2>
<p>A senior engineer with an amux fleet operates at a higher leverage point. Instead of implementing one feature per week, they design and review four. Instead of writing one migration per sprint, they oversee three. Their architectural decisions get executed faster, giving faster feedback on whether the design was right.</p>
${CTA}`,
},
{
  slug: 'hackathon-teams',
  title: 'amux for Hackathon Teams',
  description: 'Win hackathons by running parallel agents — one per feature, one for tests, one for docs.',
  keywords: 'hackathon AI tools, hackathon coding agents, competitive programming AI',
  content: `<h1>amux for Hackathon Teams</h1>
<p class="subtitle">48 hours. One codebase. Multiple agents running simultaneously. You will ship more than the other teams.</p>
<h2>The Hackathon Speed Problem</h2>
<p>Hackathons reward execution speed above almost everything else. A well-executed simple idea beats a poorly-executed complex idea every time. amux doesn't make your idea better — it makes your execution dramatically faster. That's the edge.</p>
<h2>The Hackathon Agent Fleet Setup</h2>
<p>A hackathon team of 3-4 developers can run 8-12 agents simultaneously. The setup:</p>
<pre><code># Before the hackathon: install and test amux
# At the hackathon: spin up in 5 minutes
amux register backend --dir ~/hackathon --yolo
amux register frontend --dir ~/hackathon --yolo
amux register auth --dir ~/hackathon --yolo
amux register ai-features --dir ~/hackathon --yolo
amux register tests --dir ~/hackathon --yolo
amux register demo-polish --dir ~/hackathon --yolo</code></pre>
<h2>Hour-by-Hour Hackathon Workflow</h2>
<ul>
<li><strong>Hour 1-2 (Human):</strong> Pick the idea, spec the architecture, write task descriptions. This is the most important work and cannot be delegated.</li>
<li><strong>Hour 2-10 (Agents + Humans):</strong> Agents implement features in parallel. Humans review, integrate, redirect stuck agents, and handle the complex logic that needs judgment.</li>
<li><strong>Hour 10-18 (Overnight, Agents):</strong> Launch agents before sleeping. By morning, core features should be complete.</li>
<li><strong>Hour 18-24 (Human-led):</strong> Integration, demo prep, presentation polish. Agents handle last-minute feature additions and bug fixes while humans work on the pitch.</li>
</ul>
<h2>Demo-Ready in 24 Hours</h2>
<p>The goal isn't a perfect product — it's a compelling demo. amux helps you get there by parallelizing the implementation, giving you time for the things that make demos land: good UX, polished edge cases, a clear narrative, and backup plans.</p>
<p>Teams that have used amux at hackathons consistently report shipping 2-3x more features than they expected — and having more time left for presentation prep.</p>
${CTA}`,
},
{
  slug: 'research-engineers',
  title: 'amux for Research Engineers',
  description: 'Run experiments, generate implementations of papers, and prototype ideas in parallel.',
  keywords: 'research engineer AI, ML researcher coding agent, parallel research implementation',
  content: `<h1>amux for Research Engineers</h1>
<p class="subtitle">Research moves fast. Parallel agents implement your ideas faster than you can type.</p>
<h2>The Research Engineer's Bottleneck</h2>
<p>Research engineers — at AI labs, universities, and research-forward companies — are idea-rich and time-poor. A single researcher might have 10 hypotheses worth testing, but the implementation bottleneck means only 1-2 get explored in a given week. The rest are deferred, forgotten, or deprioritized. amux removes the implementation bottleneck.</p>
<h2>Research Workflows with amux</h2>
<ul>
<li><strong>Paper reproduction:</strong> Assign one agent per paper or architectural variant. While you're deep in one paper's implementation, agents are building the others. Start 5 reproductions, review all 5 by end of day.</li>
<li><strong>Ablation studies:</strong> An ablation study requires implementing many variants of a model or system. Parallel agents implement each variant simultaneously, not sequentially.</li>
<li><strong>Baseline implementations:</strong> Research comparisons require working baselines. Use agents to implement known baselines from papers while you focus on the novel contribution.</li>
<li><strong>Experiment scaffolding:</strong> Data loaders, training loops, evaluation harnesses, logging infrastructure — the scaffolding that surrounds every experiment is tedious to write. Agents build it while you focus on the research idea.</li>
</ul>
<h2>Overnight Research Runs</h2>
<p>Research engineers often run long experiments overnight. While the GPU is training, amux agents can be:</p>
<ul>
<li>Writing the evaluation code for the model currently training</li>
<li>Implementing the next architecture variant</li>
<li>Generating visualizations and analysis notebooks</li>
<li>Drafting the methods section based on the current implementation</li>
</ul>
<h2>Prototype Speed</h2>
<p>The gap between a research idea and a working prototype determines how many ideas get tested. amux compresses that gap dramatically. Ideas that previously took a week to prototype can be tested in a day. Ideas that would have been deprioritized get implemented quickly enough to validate or discard before they fade.</p>
<p>The result: research engineers who use amux test more hypotheses, discard bad ideas faster, and find good ideas sooner.</p>
${CTA}`,
},
];

module.exports = { comparisons, useCases, guides, glossary, solutions, blog, features, faqPage, geoPages, moreBlog, moreCompare, moreGuides, moreGlossary, moreSolutions };
