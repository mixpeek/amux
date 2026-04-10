---
description: Review the full terminal log of an amux session — summarize what happened, find errors, extract key decisions
allowed-tools: Bash, Read, Grep
argument-hint: <session-name>
---

# /review-session-log — Review a session's terminal log

You are reviewing the full terminal output log of an amux session.

## How to get the log

Session logs are stored at `~/.amux/logs/<session-name>.log` (up to 10 MB each, oldest output trimmed).

1. Determine the session name from `$ARGUMENTS`. If empty, ask the user.
2. Read the log file directly: `~/.amux/logs/<session-name>.log`
3. If the file is large (>2000 lines), read it in chunks — start from the end (most recent) and work backwards as needed.

## What to look for

Analyze the log and produce a structured review covering:

### 1. Summary
- What was the session working on? (1-3 sentences)
- How long has it been active / how much output is there?

### 2. Current State
- What is it doing right now? (last ~50 lines)
- Is it waiting for input, actively working, errored out, or idle?

### 3. Key Actions Taken
- Major tasks completed (commits, deploys, file changes, API calls)
- Important decisions or branching points

### 4. Errors & Warnings
- Any errors, exceptions, tracebacks, or failed commands
- Permission denials, timeouts, or retry loops
- If none found, say so explicitly

### 5. Notable Output
- Any interesting results, URLs, or artifacts produced
- Test results, build outputs, or deployment confirmations

## Instructions

The user wants to review: **$ARGUMENTS**

- Strip ANSI escape codes mentally — the log contains raw terminal output
- Focus on substance, not boilerplate (skip tool call formatting, progress spinners, etc.)
- If the log is very long, prioritize recent activity but note if earlier sections contain important context
- Quote specific log lines when citing errors or key moments
- Be concise — the user wants a quick situational overview, not a line-by-line walkthrough
