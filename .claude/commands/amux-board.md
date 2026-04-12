---
description: Use when the user says "add to board", "create a task", or wants to track a todo on the amux kanban board
allowed-tools: Bash
argument-hint: [task title or description]
---

# Add to amux board

You are adding an item to the **amux local kanban board** at `https://localhost:8822/api/board`.

## Board API

```bash
# Add item
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"title":"...","desc":"...","status":"todo","session":"..."}' \
  https://localhost:8822/api/board

# List all items
curl -sk https://localhost:8822/api/board

# Update item
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"status":"doing"}' https://localhost:8822/api/board/ITEM_ID

# Delete item
curl -sk -X DELETE https://localhost:8822/api/board/ITEM_ID
```

## Fields

| Field | Required | Values | Notes |
|-------|----------|--------|-------|
| `title` | yes | string | Short, imperative task name |
| `desc` | no | string | Full context: what, why, acceptance criteria |
| `status` | no | `todo` / `doing` / `done` | Defaults to `todo` |
| `session` | no | amux session name | Which project/session owns this task |

## Instructions

The user's request is: **$ARGUMENTS**

1. Determine the best **title** (concise, imperative — e.g. "Fix login bug", "Add dark mode toggle")
2. Write a **desc** with full context:
   - What needs to be done
   - Why it matters / what problem it solves
   - Any relevant technical details, file paths, or acceptance criteria
   - Current state if known
3. Set **status** to `todo` unless the user indicates it's in-progress (`doing`) or already done (`done`)
4. Set **session** to the most relevant amux session name if the task belongs to a specific project (leave empty if general)
5. Add the item using `curl -sk` (the server uses a self-signed cert)
6. Confirm success by showing the created item's title and ID

Do not ask clarifying questions — infer context from the arguments and current conversation. If the arguments are empty, add a generic task titled "Untitled task" with an empty desc.

## Gotchas

- Always use `curl -sk` — self-signed TLS cert.
- The `session` field should match an existing amux session name exactly (case-sensitive).
- Board item IDs are server-generated — never fabricate an ID; get it from the creation response or a list call.
- Custom status columns are allowed but the dashboard groups by `backlog`/`todo`/`doing`/`done` — other values appear in an "Other" column.
