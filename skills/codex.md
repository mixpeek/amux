---
description: Work with Codex threads (Desktop app, CLI, exec, cloud). Read, list, search and monitor them (what is Codex doing, read a thread, what commands ran, what changed since, find the thread about X), send a message to a chat by name, or start a new named thread, e.g. /codex what is running, /codex read #mxp onboarding flow, /codex #mxp onboarding flow <message>, /codex new thread <name>. Scripts: ~/.amux/scripts/codex-skill.py (read-only, JSON), codex-send.py <chat> -, codex-new.py <name> - (message on stdin).
argument-hint: <question about Codex work> | <chat name> [message] | new thread <name>
allowed-tools: Bash
---

# Codex threads: observe, send, start

The request: **$ARGUMENTS**

Three scripts, all under `~/.amux/scripts/`. Source: `skills/codex.md` and
`skills/codex/scripts/` in the amux repo; install with
`install -m 755 skills/codex/scripts/*.py ~/.amux/scripts/`.

- A question about Codex work (what is running, read, list, search, status, what
  changed, which failed): **section R**, `codex-skill.py`. Read-only.
- A message for an existing chat: sections 1 to 4, `codex-send.py`.
- A new chat or thread: section 0, `codex-new.py`.

Never start, resume, steer, archive or interrupt a thread unless the request asks for it.

## R. Observe: codex-skill.py

Read-only JSON over every local thread, whichever app made it. Run
`codex-skill.py capabilities` once if you need to know what this machine exposes.

```bash
CS=~/.amux/scripts/codex-skill.py
$CS list --active                       # what is Codex doing right now
$CS list --project mixpeek --days 14    # also --status, --source desktop|cli|exec|subagent, --keyword
$CS status <thread>                     # state, recent turns, failures, last agent message
$CS read <thread> [--since CKPT] [--types commandExecution] [--limit N] [--full]
$CS events <thread> [--since CKPT]      # compact timeline: messages, commands (failed flag), tool calls
$CS search "MXP-GS12" [--days 30]       # titles, messages, commands and tool outputs
$CS watch <thread> --until-idle --duration 900   # NDJSON: item and status events, bounded
$CS export [<thread>...] [--recent 5] [--exclude REGEX] [--out FILE]   # markdown transcripts
```

`export --out` replaces the file atomically, so a fetch-only `.mdai` can show it
with no model call: `fetch: https://localhost:8824/api/file/raw?path=<FILE>`.

`<thread>` is a UUID, an exact name, or a fragment that fits one name. Exit 2 means
no match or several; `candidates` lists them.

How to read the answers:

- **`status.basis`.** `observed` means a terminal record exists (`idle`,
  `interrupted`, `failed`). `inferred` means the last turn has no end record and
  the state comes from facts in `status.evidence`: `running` (rollout written in
  the last 180s), `waiting` (a live process holds it open but is quiet, maybe an
  approval), `abandoned` (no process holds it). Report inferred states as
  inferred, with the evidence.
- **Pending approvals and live events are not observable** for a thread another
  process owns (the Desktop app's server is a private pipe). Say so; do not guess.
- **`checkpoint`.** Save it and pass it as `--since` next time to get only new
  items, with no repeats.
- **`history_source`** says whether the API or the rollout file answered.
  **`measured: false` or `ok: false`** means the data was not read. Report that
  as unavailable, never as empty.
- **Output is bounded.** Long strings are cut at `--max-chars` with a
  `[... truncated, N chars total]` marker; secrets are replaced with `[REDACTED]`
  and counted in `redactions`. Do not try to recover redacted values.

Answer the user from these fields. Quote thread names and ids, and say which
facts were observed and which were inferred. Full capability matrix, status
table and schemas: `skills/codex/reference.md` in the amux repo.

## 0. A NEW thread: always named, never `codex exec`

When the request asks for a new chat or thread, create it with
`~/.amux/scripts/codex-new.py`, never with `codex exec`. An exec run is headless:
it gets a session id and no name, the Codex app does not list it, and
`/codex <name>` cannot reach it (2026-10-17: a tester started that way had to be
forked and restarted).

```bash
~/.amux/scripts/codex-new.py '<name>' - [--cwd DIR] [--full-access] [--fork <thread>] <<'MSG'
<the first message: a self-contained brief, same rules as section 2>
MSG
```

- Pick a short name the owner will recognise, `#<area> <topic>` (`#amux cloud e2e`).
  Exit 2 means the name is taken: send to that thread instead.
- `--full-access` gives the thread danger-full-access with no approvals, for
  unattended testers and builders that need the network or a browser. Without
  it the thread uses the owner's `~/.codex/config.toml`.
- `--fork <thread>` copies an existing thread's whole history into the new named
  one (use it to rescue a headless `codex exec` run: stop the run, then fork it).
- It prints `Started thread <id> named "<name>"`. Report the name and id. Later
  messages go through codex-send.py by that name.
- Want replies back in this worker? Tell the thread how: a JSON POST of
  `{"text": ...}` to `$(amux url)/api/sessions/$AMUX_SESSION/send` arrives as
  owner input, even for an isolated worker.

## 1. Find the chat name

The chat name is usually the first words of the request, often starting with `#`
(`#mxp onboarding flow`). Everything after it is the message. If you are unsure
where the name ends, list the names and pick the one that fits:

```bash
~/.amux/scripts/codex-send.py --list | grep -i '<a word from the name>'
```

The name match ignores case, a leading `#` and punctuation. A fragment works if it
fits exactly one chat.

## 2. Write the message

- If the request gives the message text, send that text as written.
- If the request says to send "this", "all this context" or "what we found", write
  it yourself from the conversation. The Codex chat cannot see this session, so the
  message must stand alone: the goal, what was done, the findings with file paths,
  commit shas or links, open questions, and what you want the chat to do. Lead with
  the ask.
- Never include secrets, tokens, passwords or card numbers.
- Run the anti-slop pass before sending: no em-dashes, no "not X, it's Y".

## 3. Send it

Always pass the message on stdin with a quoted heredoc, so backticks and `$(...)`
stay literal:

```bash
~/.amux/scripts/codex-send.py '<chat name>' - <<'MSG'
<message>
MSG
```

The script adds `From amux worker $AMUX_SESSION:` at the top. Add `--no-origin` to
leave it out, or `--dry-run` to resolve the name without sending.

## 4. Report

- Exit 0 prints `Queued message <id> for thread <id>`. Tell the user which chat it
  went to (name and thread id) and the queued message id.
- Exit 2 means the name matched no chat or several. The script lists the
  candidates and sent nothing. If one candidate is clearly the one meant, retry
  with its full name. Otherwise show the candidates and ask.
- Any other failure: show the output. Do not report it as sent.

Every send is logged to `~/.amux/logs/codex-send.log`.
