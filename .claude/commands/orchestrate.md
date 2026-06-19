---
description: Scaffold a closed orchestrator loop — creates the mission note, state/constraints notes, and a scheduler entry from the v2 template.
allowed-tools: Bash, Read, Write
argument-hint: -g "goal description" -s "session-a, session-b, ..."
---

# /orchestrate — Scaffold a closed orchestrator loop

Parse the arguments, generate a filled-in orchestrator loop note from the v2 template, create companion notes, and wire up a scheduler entry.

## Arguments

- `-g "<text>"` — The mission goal(s). Can be a sentence or a paragraph. Required.
- `-s "<list>"` — Comma-separated list of authorized session names. Required.
- `--schedule "<expr>"` — When to run. Optional. Default: `every 2h`
- `--slug "<name>"` — Note slug to use. Optional. Default: derived from the goal (kebab-case, ≤30 chars)
- `--no-schedule` — Create the note but skip the scheduler entry (useful for manual/one-shot loops)

## Procedure

### 1. Parse arguments

Extract `-g`, `-s`, `--schedule`, `--slug`, `--no-schedule` from the skill arguments. If `-g` or `-s` is missing, stop and ask the user before proceeding.

Derive a slug if not provided: lowercase the goal, strip punctuation, replace spaces with `-`, truncate to 30 chars. Example: "Make MVS robust end to end" → `mvs-robust-end-to-end`.

### 2. Infer session lanes

For each session in `-s`, derive its likely lane and issue prefix using this mapping. If a session isn't listed, use its name as the lane description and leave the prefix blank.

| Session name contains | Issue prefix | Lane |
|---|---|---|
| `mvs-infra` | `MI-` | shard, partition, scroll, scale |
| `mvs-build` | `MB-` | topology, image builds, cutover |
| `backend` | `BACKE-` | write pipeline, celery, analytics |
| `ts-gke` | `TG-` | TubeScience, GKE experiments |
| `observability` | `MO-` | metrics, alerts, dashboards |
| `studio` | `MS-` | UI, golden path, E2E |
| `orchestrator` | `AMUX-` | cross-cutting, escalations |
| `general` | `MG-` | diagnosis, root cause, unowned |

### 3. Build the mission note

Fetch the v2 template:
```bash
curl -sk $AMUX_URL/api/notes/orchestrator-loop-v2 | python3 -c "import json,sys; print(json.load(sys.stdin).get('content',''))"
```

Do the substitution in Python — fetch the template text, replace every placeholder with real values, write to a temp file, POST it. Do NOT write the note content from scratch.

```bash
python3 << 'PYEOF'
import json, os, urllib.request, ssl, re

url      = os.environ['AMUX_URL']
slug     = '<slug>'
goal     = '<goal text>'
schedule = '<schedule>'
prefixes = '<BACKE- MO- AMUX->'   # space-separated prefixes from step 2

# One row per session from -s plus orchestrator row always last
session_rows = [
    '| `<session-a>` | ✓ | ✓ | ✓ | <inferred lane> |',
    '| `<session-b>` | ✓ | ✓ | ✓ | <inferred lane> |',
    '| `mixpeek-orchestrator` | — | — | ✓ | AMUX- escalations to Ethan |',
]

ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
req = urllib.request.Request(f'{url}/api/notes/orchestrator-loop-v2')
tmpl = json.loads(urllib.request.urlopen(req, context=ctx).read())['content']

filled = tmpl
filled = filled.replace('[Loop Name]', slug)
filled = re.sub(r'> Fill in.*?---\n\n', '', filled, flags=re.DOTALL)
filled = filled.replace('`[e.g. mvs-robustness]`', f'`{slug}`')
filled = re.sub(r'\[loop-slug\]', slug, filled)
filled = filled.replace('`[e.g. MI- MB- BACKE- TG-]`', f'`{prefixes}`')
filled = filled.replace('[e.g. every 2h | daily at 09:00 | every weekday at 08:00]', schedule)
filled = filled.replace('[One sentence. The outcome, not the activity.]', goal)
table_ph = '| `[session-a]` | ✓ | ✓ | ✓ | [what it owns] |\n| `[session-b]` | ✓ | ✓ | ✓ | [what it owns] |\n| `[session-c]` | ✓ | read-only | ✗ | [observe only] |'
filled = filled.replace(table_ph, '\n'.join(session_rows))

with open('/tmp/orch-note.json', 'w') as f:
    json.dump({'content': filled}, f)
print('template filled')
PYEOF

curl -sk -X POST -H 'Content-Type: application/json' -d @/tmp/orch-note.json $AMUX_URL/api/notes/<slug>
```

### 4. Create companion notes

**State note** (blank initial state):
```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"content": "## Status\n\nNot yet run. Orchestrator will populate on first tick."}' \
  $AMUX_URL/api/notes/<slug>-state
```

**Constraints note** (seed with one standing rule):
```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"content": "# Constraints — <slug>\n\nAppend-only. Never edit existing lines.\n\n```\n[YYYY-MM-DD] — stage explicit git paths only, never git add -A. Reason: 5407ac1473 swept another session'\''s deletions into wrong commit (AMUX-1315).\n```\n"}' \
  $AMUX_URL/api/notes/<slug>-constraints
```

### 5. Construct the scheduler prompt

Before creating the schedule entry, reason about what the orchestrator needs to know the moment it wakes up cold with no context. The prompt must be self-contained — it cannot assume the orchestrator remembers anything from last tick.

A good scheduler prompt has three parts:
1. **Orientation** — what this loop is, in one sentence drawn from the goal
2. **Fleet** — which sessions are authorized and what each owns (so the orchestrator knows where to look and what to touch)
3. **Mechanics** — where to load the full mission, constraints, and where to write state

Compose the prompt by distilling the `-g` text into a crisp one-sentence orientation, then list sessions with their lanes and issue prefixes, then add the note references. Do not use the generic template — write a prompt that would orient a fresh orchestrator instance instantly.

Example for `-g "validate batch objects in MVS, run hybrid evals measuring precision/recall/latency, fix issues as found" -s "ts-gke, backend"`:

```
You are running the mmv2-retrieval-eval orchestration loop.

Mission: Validate 10 batch objects in MVS and run hybrid search evals (RRF, weighted) measuring precision, recall, and latency. Fix issues as found. Reference the latest footage collection in ts iconik ns.

Authorized sessions:
- ts-gke (TG-): search quality, batch verification, evals execution
- backend (BACKE-): evals/benchmarks API, write pipeline

Load full mission from note: mmv2-retrieval-eval
Load constraints from: mmv2-retrieval-eval-constraints
Write state to: mmv2-retrieval-eval-state

Run your orchestration loop (load state → observe sessions → think → act → verify → distill → persist).
```

### 6. Create the scheduler entry (skip if --no-schedule)

```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d "{
    \"title\": \"Orchestrator loop: <slug>\",
    \"session\": \"mixpeek-orchestrator\",
    \"command\": \"<constructed prompt from step 5>\",
    \"schedule_expr\": \"<schedule>\"
  }" \
  $AMUX_URL/api/schedules
```

Capture the returned schedule ID.

### 6. Report back to the user

Print a summary:

```
✓ Loop created: <slug>

Notes:
  Mission:     $AMUX_URL → Notes → <slug>
  State:       <slug>-state  (updated each tick)
  Constraints: <slug>-constraints  (append-only skill library)

Sessions: <list from -s>
Schedule: <expr>  [Schedule ID: <id>]

Scheduler prompt (sent each tick):
  "Load note <slug> and run your orchestration loop.
   Apply constraints from <slug>-constraints.
   Write state to <slug>-state."

Next steps:
  1. Open the mission note and fill in Critical Path if you know it now
     (or leave it — the orchestrator derives it from the goal on first tick)
  2. Run the loop manually once to validate: send the prompt above to mixpeek-orchestrator
  3. The scheduler fires automatically on: <expr>
```

If `--no-schedule` was set, omit the schedule line and say "No scheduler entry created — run manually or add one later with /schedule."

## Edge cases

- If a note slug already exists, stop and ask: overwrite, pick a new slug, or abort.
- If `$AMUX_URL` is not set, stop and tell the user to check their environment.
- If the v2 template note doesn't exist (`orchestrator-loop-v2`), stop and tell the user to run the template setup first.
- Session names with spaces or special characters: quote them in the access control table, use as-is.
