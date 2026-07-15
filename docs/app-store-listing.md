# amux iOS App — App Store Listing

**Status:** Updated 2026-07-15. Fastlane metadata is now the source of truth (`ios/fastlane/metadata/en-US/`). Push with `fastlane release`.
**Remaining:** App name change (requires App Store Connect), screenshots (zero currently).

---

## App Name

**Current:** amux - agent multiplexer  
**Proposed:** amux – Agent Control

*Rationale: "multiplexer" is an implementation detail nobody searches for. "Agent Control" communicates what it does — you control agents. If name changes cause issues with existing app identity, keep "amux" and change subtitle only.*

---

## Subtitle (30 chars max)

**Current:** Run AI agents from your phone  
**Proposed:** Monitor Claude Code from anywhere

*Alternatives:*
- Your AI engineering team, in your pocket
- Control your AI coding agents remotely
- AI agent fleet dashboard

---

## Description

### First paragraph (most important — shown before "more")

Keep your AI coding agents running—even when you're away from your desk.

amux is the remote control for your AI engineering team. Monitor live Claude Code sessions, approve pending actions, inspect terminal output, coordinate work across agents, and recover from problems — directly from your iPhone.

Think of it like GitHub Mobile or Tailscale: the work happens on your laptop or server. amux puts the control panel in your pocket.

---

### Full description

**What amux does**

amux runs on your Mac or Linux server, launching and managing dozens of parallel Claude Code, Codex, or Gemini CLI sessions — your AI engineering team. The iOS app is the remote control: it connects to your amux server and gives you full visibility and control from anywhere.

**What you can do from your phone**

• Check overnight runs before getting out of bed
• Approve permission prompts before agents stall
• See exactly why an agent stopped (peek into live terminal output)
• Assign a new task to an idle session
• Send a steering instruction to redirect an agent mid-run
• Monitor your whole fleet at once: status dots, active tasks, token usage
• Manage the kanban board: see what's done, doing, and stuck
• Keep coding while traveling — your fleet keeps shipping

**Two access modes**

Native iOS app (this app): SwiftUI, optimized touch targets, full peek panel and plan strip. Connects to your amux server over Wi-Fi, Tailscale, or the amux cloud tunnel.

Progressive Web App: Open your amux dashboard URL in Safari on any iPhone — install to home screen for a full-screen experience without the App Store.

**Remote access**

amux tunnel (requires amux cloud subscription) gives you a stable public HTTPS URL for your dashboard. Tailscale is free and works great for personal use. Both work with this app.

**Requirements**

- amux server running on Mac or Linux (free, open source: github.com/mixpeek/amux)
- Claude Code, OpenAI Codex CLI, or Gemini CLI installed on your server
- Wi-Fi, Tailscale, or amux tunnel for remote access

---

## Keywords (100 chars max, comma-separated)

claude code,AI agents,coding agents,agent monitor,remote control,developer tools,tmux,AI engineering,agent fleet

---

## Screenshots (6 recommended)

1. **Your AI engineering team. Anywhere.**
   — Full sessions view: status dots, active task labels, token counts

2. **See every Claude Code session live.**
   — Dashboard showing 8 active sessions, each with a task description and status

3. **Approve agents from your phone.**
   — Peek panel showing a permission prompt with one-tap approve/deny

4. **Peek into terminal output.**
   — Scrollable terminal transcript showing agent working in real time

5. **Coordinate dozens of agents.**
   — Board view: todo/doing/done kanban with agent-assigned tasks

6. **Never SSH in just to check progress.**
   — Split view: sessions list on left, peek panel on right on iPad

---

## App Store Category

Primary: Developer Tools  
Secondary: Productivity

---

## What's New (next version notes template)

The amux iOS app is your remote control for an AI coding agent fleet running on your Mac or server. This update brings [X]. Full changelog at amux.io/changelog.
