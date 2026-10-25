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

## When Muse is paired with a different Mac

Muse reads the inbox only on the Mac it is paired with. If amux runs on another
Mac, commands from a worker queue where nothing reads them. Once, on the paired
Mac, from an amux checkout:

```bash
bash skills/muse/install-paired-mac.sh https://<amux server>:8824
```

It asks for the owner token, writes `~/.muse-cli/amux.json` (mode 600), installs
`muse-reply` and `muse-sync`, and runs `muse-sync` every minute. That copies new
commands from the server's inbox into the local one for Muse's poll, and sends
back any reply Muse appended to `~/.muse-cli/outbox.jsonl` (for when Muse can
write files there but not run commands). Replies carry a stable `msg_id`, so a
retry after a timeout is delivered once. Log: `~/.muse-cli/sync.log`.

## When Muse can write files here but not run commands

Muse may be able to write files on its paired Mac without being able to run a
command there. Then it answers by appending `{"id": "<command id>", "text":
"<reply>"}` to `~/.muse-cli/outbox.jsonl`, and `muse-sync` sends each line back
once. On the Mac that runs amux, install it with no `amux.json`; it sees the local
server and only forwards the outbox:

```bash
install -m 755 skills/muse/scripts/muse-sync.py ~/.local/bin/muse-sync
muse-sync --install        # every minute, log ~/.muse-cli/sync.log
```

Keep amux's `muse-send` and `muse-reply` in `~/.local/bin`. A generic Muse
installer that links its own copies there drops the `reply_to` field and the
closing `muse-reply` line, so answers stop reaching the worker that asked.

## Muse sending work on its own

Muse can start a thread with any worker, not only answer one:
`muse-reply --to <worker> "<work and context>"`.
