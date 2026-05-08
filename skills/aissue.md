---
description: Pick up an open issue from the amux issue tracker, mark it doing, work it, and mark it done.
allowed-tools: Bash, Read, Edit, Write
argument-hint: [ITEM-ID or leave empty to pick next open issue]
---

# aissue — amux issue runner

Pull an open issue from the local amux issue tracker, transition it through the board (`todo` → `doing` → `done`), and do the actual work in between.

## Procedure

### 1. List open issues

Query the local sqlite DB directly:

```bash
sqlite3 ~/.amux/amux.db "SELECT id, title, status, desc FROM issues WHERE deleted IS NULL AND status != 'done' ORDER BY id;"
```

The result is pipe-delimited: `id|title|status|desc`. Show the user the list (id + title + status) and either:

- If the user named a specific issue (e.g. "work on `ITEM-42`"), use that one.
- If they said something open-ended like "/aissue" with no target, pick the highest-priority open issue (status `todo` first, then `doing` items that aren't already in flight) and confirm with the user before transitioning. Don't grab `doing` items without asking — someone else might be on them.

### 2. Mark the issue `doing`

Prefer the CLI; fall back to the REST endpoint if the CLI is unavailable or errors. Replace `<ITEM-ID>` with the actual id.

```bash
amux board doing <ITEM-ID>
```

Or via REST:

```bash
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"status":"doing"}' \
  https://localhost:8822/api/board/<ITEM-ID>
```

Verify the transition by re-querying the row, or by trusting a successful exit code from `amux board doing`.

### 3. Do the work

Read the issue's `title` and `desc` carefully — that's the spec. Then execute the task using normal Claude Code workflow (Read/Edit/Bash/etc.). If the description is ambiguous, ask the user before guessing. If the work pulls in unrelated cleanups, surface them rather than silently expanding scope.

If the task can't be completed (blocked, needs more info, out-of-scope), do **not** mark it `done`. Stop, explain what's blocking, and leave the status at `doing` so the issue stays visible on the board.

### 4. Mark the issue `done`

After the work is genuinely finished (code edited, tests/builds run if applicable, change verified), transition the board:

```bash
amux board done <ITEM-ID>
```

Or via REST:

```bash
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"status":"done"}' \
  https://localhost:8822/api/board/<ITEM-ID>
```

## Notes

- The DB lives at `~/.amux/amux.db`. Read-only queries are safe; do not write to it directly — go through `amux board` or the REST API so audit trail / triggers fire.
- The REST API uses a self-signed cert on `localhost:8822`, so `curl -k` (insecure) is required.
- If both the CLI and REST endpoint fail, stop and report — don't fall back to writing to sqlite directly.
- Only one issue per skill invocation. If the user wants several, run the skill multiple times so each transition is intentional.
- Don't auto-commit when finishing. The user decides when to commit; the skill just flips the board.
