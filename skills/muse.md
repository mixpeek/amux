---
description: Send a command to Ethan's Muse (its main chat), e.g. /muse check my calendar for tomorrow. Muse runs it with its full conversation context and sends the result back to this worker. Use when asked to send, relay or hand something to Muse. Scripts: muse-send "<command>" and muse-reply <id> "<reply>".
argument-hint: <command for Muse>
allowed-tools: Bash
---

# Send to Muse

The request: **$ARGUMENTS**

`muse-send` appends one line to `~/.muse-cli/inbox.jsonl` on this Mac. Muse
polls that file about every 5 minutes through its paired Mac connection, relays
each new line into its main chat word for word, and runs it there with its full
context. Source: `skills/muse/scripts/` in the amux repo; install with
`install -m 755 skills/muse/scripts/muse-send.py ~/.local/bin/muse-send` and the
same for `muse-reply`.

## 1. Write the command

- If the request gives the command, send it as written.
- If it says to send "this" or "what we found", write it yourself. Muse cannot see
  this session, so the command has to stand alone: what to do, the facts it
  needs, and what you want back.

## 2. Send it

Use the heredoc (stdin), never an inline double-quoted argument: the shell would
run any backticks or `$(...)` in it. `muse-send` reads stdin when it has no
arguments.

```bash
muse-send <<'MSG'
<the command>
MSG
```

Sent from an amux worker, the command carries its way back: a closing line asks
Muse to run `muse-reply <id> "<reply>"`. That posts to this worker through the
local amux server, so the answer arrives here as owner input with your command
quoted above it. `--no-reply` sends without that line.

## 3. Report

Say the id `muse-send` printed and that Muse picks it up within about 5 minutes.
Do not wait in a loop for the reply: it arrives as a new message in this worker.
`muse-send --status` counts queued commands; `~/.muse-cli/replies.jsonl` records
every reply sent back.

## Muse sending work on its own

Muse can start a thread with any worker, not only answer one:
`muse-reply --to <worker> "<work and context>"`.
