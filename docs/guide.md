# amux Board System Guide

The board is amux's task-tracking system -- a kanban board that every worker reads and writes to, making work visible across the entire fleet. It is the shared source of truth for what is being done, by whom, and in what state.

## Columns (Statuses)

Every board card sits in exactly one column:

| Status | Meaning | Terminal? |
|---|---|---|
| **backlog** | Acknowledged but not yet prioritized | No |
| **todo** | Ready to be picked up | No |
| **doing** | Actively being worked on by a worker | No |
| **review** | Work complete, awaiting peer review | No |
| **done** | Implemented/merged (NOT yet confirmed in prod) | Yes |
| **verified** | Confirmed working end-to-end in production | Yes |
| **discarded** | Abandoned or no longer relevant | Yes |

Custom columns can be added via `POST /api/board/statuses`.

### done vs verified

`done` is the normal terminal state for most cards -- the work is finished. Most cards stop here and that's fine.

`verified` is an optional stronger claim: confirmed working end-to-end in production. Use it when the stakes warrant it (deploys, infrastructure changes, customer-facing fixes). It requires CI green, deployed to prod, confirmed working, and no regressions. Evidence should be recorded on the card. Cards do NOT need to pass through `verified` -- it's opt-in.

## Card Types

Every card has a `type` that determines what gates apply. The default is `code` (strictest).

| Type | Use case | Gate flavor |
|---|---|---|
| `code` | Software changes (default) | Merge + tests required |
| `escalation` | Issues requiring human decision | Outcome recorded |
| `blocker` | Blocking issues | Outcome recorded |
| `investigation` | Research/debugging | Outcome recorded |
| `ops` | Operational tasks | Outcome recorded |
| `research` | Research tasks | Outcome recorded |
| `chore` | Maintenance/cleanup | Outcome recorded |
| `doc` | Documentation | Outcome recorded |
| `tripwire` | Armed watch condition | Trigger-specific gates |
| `watch` | Monitoring condition | Trigger-specific gates |

Set the type to match the work. A mistyped card forces the worker to lie to its gates -- if a decision card is typed `code`, the only way to close it is to falsely assert a merge. Fix the type, not the truth.

## Gates

Gates are checklists that must be acknowledged before a card can move to a gated status. They exist to enforce that `done` means something real.

### Gate resolution order

When moving a card to a new status, the gate is resolved in this order:
1. **Card-level override** -- a gate set directly on the card
2. **Type-derived gate** -- determined by the card's `type` field
3. **Per-worker override** -- custom gates configured per worker
4. **Global status default** -- the column's default gate checklist

### Acknowledging gates

To move a card past a gate, include one of:
- `"gate_ack": true` -- acknowledge the entire checklist
- `"gate_checked": [0, 2]` -- acknowledge specific items by index
- `"force": true` -- bypass the gate entirely (logged, judgment stays with the caller)

### Code-type gates (default)

For `code`-typed items, the gates enforce real engineering standards:
- **doing**: scope is clear, has an owner
- **review**: findings written up, ready for review
- **done**: implemented and merged, tests/lint pass
- **verified**: CI green, deployed, confirmed in prod, no regressions

### Non-code gates

For all other types, gates are lighter but still honest:
- **doing**: scope is clear, has an owner
- **review**: findings written up, ready for another set of eyes
- **done**: outcome recorded in the item (what happened and why it is closed)
- **verified**: outcome confirmed to still hold

### Editing gates

Gates can be configured at three levels:
- **Per-column**: click the gate icon on a column header in the dashboard
- **Per-worker**: `PATCH /api/board/session-gates`
- **Per-card**: include `"gate"` in the card's PATCH body

## Peer Review

Cards can optionally name a `reviewer` -- another worker responsible for sign-off.

When a reviewer is set:
- Moving to `done` or `verified` requires the ack to come FROM the reviewer worker (identified by `X-Amux-Session` header)
- The author cannot self-ack their own review (this is enforced server-side)
- `force: true` bypasses this (logged)

Set a reviewer on a card:
```bash
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"reviewer":"other-session"}' \
  $AMUX_URL/api/board/ITEM-ID
```

The reviewer acks by moving the card to done/verified with their worker header:
```bash
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -H "X-Amux-Session: other-session" \
  -d '{"status":"done","gate_ack":true}' \
  $AMUX_URL/api/board/ITEM-ID
```

## Groups

Cards can be tagged for filtering and organization:

```bash
# Add tags when creating
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"title":"...","tags":["needs:you","urgent"]}' \
  $AMUX_URL/api/board

# Update tags on existing card
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"tags":["frontend","p0"]}' \
  $AMUX_URL/api/board/ITEM-ID
```

Groups are free-form strings. Common conventions:
- `needs:you` -- requires human (owner) action
- `p0`, `p1`, `p2` -- priority levels
- Topic groups matching the worker's focus area

## Dependencies

Cards can declare dependencies on other cards:

```bash
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"depends_on":["OTHER-123","OTHER-456"]}' \
  $AMUX_URL/api/board/ITEM-ID
```

A card with unresolved dependencies (cards not in done/verified/discarded) will be flagged. The auto-pickup system respects dependencies and will not assign blocked cards.

## Owner Types

Cards track who owns them:
- `agent` -- owned by an AI worker (can be auto-managed, picked up, reassigned)
- `human` -- owned by a person (never auto-reassigned, never hijacked by autotask)

This distinction prevents automation from overwriting human commitments.

## Auto-Task Creation

When `board_autotask` is enabled (default: on), every human prompt sent to a worker automatically creates a board card. This ensures the board captures what was actually asked.

### Skip rules

Not everything needs a card. Auto-creation is skipped for:
- **Control words**: "continue", "yes", "ok", "done", etc.
- **Short messages**: under 12 characters
- **Slash commands**: `/compact`, `/model`, `/help`, etc.
- **`[no-board]` prefix**: explicitly skip card creation for one-off questions
- **`no_board: true` API param**: same, via the send API

### Title derivation

Card titles are derived from the prompt's first clause -- no model call needed. Conversational filler ("can you please", "I'd like you to") is stripped, and the result is sentence-cased.

## WIP Limits

Each worker is soft-capped at **1 card in `doing`** at a time. Taking a second requires `override_doing: true`. This prevents the "164 items in doing" state where the status means nothing.

## Staleness Detection

Cards in `doing` for more than 3 days without board updates AND without evidence of progress (commit sha, PR link, merge reference) are flagged as rotting. This is advisory -- nothing is auto-flipped.

## Auto-Pickup

Idle workers can automatically pick up `todo` cards assigned to them. The pickup system:
- Respects dependencies (blocked cards are skipped)
- Skips `tripwire` and `watch` types (not workable tasks)
- Skips cards that are just prompt captures without real task content
- Never picks up `owner_type: human` cards

## Board Operations (CLI)

```bash
# List all items
amux board list

# Create a card (auto-tagged to $AMUX_SESSION)
amux board add "Task title"

# Move status
amux board doing ITEM-ID
amux board done ITEM-ID
amux board verified ITEM-ID
amux board discarded ITEM-ID

# Force past gates
amux board done ITEM-ID --force
```

## Board Operations (API)

```bash
# List all items
curl -sk $AMUX_URL/api/board

# Create
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"title":"...","status":"todo","session":"my-session","type":"code"}' \
  $AMUX_URL/api/board

# Update
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"status":"done","desc":"Result: ...","force":true}' \
  $AMUX_URL/api/board/ITEM-ID

# Claim atomically
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"session":"my-session"}' \
  $AMUX_URL/api/board/ITEM-ID/claim

# Clear all done items (soft-delete)
curl -sk -X POST $AMUX_URL/api/board/clear-done

# Request status from owning session
curl -sk -X POST $AMUX_URL/api/board/ITEM-ID/status-request
```

## Archiving

Items can be archived (`"archived": 1`) to hide them from the default board view without deleting them. Archived items are still queryable but don't clutter the active board.

`clear-done` soft-deletes all `done` items (sets `deleted` timestamp). This is different from archiving -- deleted items don't appear in any view.

## The Board Log

Every card has an append-only system log (`log` field) that survives description rewrites. Key events are stamped here:
- `capture: session prompt` -- card was auto-created from a prompt
- Status changes and gate forces
- Pickup and assignment events

The log is never exposed to PATCH -- it cannot be accidentally overwritten.

## No-Board: Skipping Card Creation

For one-off questions, status checks, or other prompts that don't represent tasks:

**Inline group** (works from any model):
```
[no-board] What's the status of the deploy?
```

**API parameter**:
```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"text":"quick question","no_board":true}' \
  $AMUX_URL/api/sessions/my-session/send
```

**CLI flag**:
```bash
amux send my-session --no-board "what's the deploy status?"
```

## Multi-Provider Support

The board works with all AI providers (Claude, Gemini, ChatGPT). Non-Claude providers receive harness instructions via their native instruction file (e.g., `GEMINI.md`) that teach them how to use the board API, send messages, and work within amux.
