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

The scheduler fires at an arbitrary time and the orchestrator wakes up with **no prior context**. The prompt is the only thing it has. It must be entirely self-contained — do not say "load the note and run the loop." The full brief goes in the prompt.

The scheduler prompt = the filled mission note content + the current constraints note content + the current state note content, concatenated, wrapped in a brief header and a closing action line.

Fetch each note and build the prompt in Python:

```bash
python3 << 'PYEOF'
import json, os, urllib.request, ssl

url  = os.environ['AMUX_URL']
slug = '<slug>'

ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE

def fetch_note(s):
    try:
        req = urllib.request.Request(f'{url}/api/notes/{s}')
        return json.loads(urllib.request.urlopen(req, context=ctx).read()).get('content', '')
    except:
        return ''

mission     = fetch_note(slug)
constraints = fetch_note(f'{slug}-constraints')
state       = fetch_note(f'{slug}-state')

prompt = f"""You are running the {slug} orchestration loop. This prompt is your complete brief — read it fully before acting.

--- MISSION ---
{mission}

--- CONSTRAINTS (append-only standing rules) ---
{constraints}

--- LAST STATE ---
{state}

--- ACTION ---
Run your orchestration loop now: observe sessions → 💭 think (diagnose) → act → verify → 💭 think (distill) → update state note ({slug}-state) with what happened, what's pending, and what you learned.
"""

with open('/tmp/orch-sched.json', 'w') as f:
    json.dump({
        'title': f'Orchestrator loop: {slug}',
        'session': 'mixpeek-orchestrator',
        'command': prompt,
        'schedule_expr': '<schedule>'
    }, f)
print('prompt ready, length:', len(prompt))
PYEOF

curl -sk -X POST -H 'Content-Type: application/json' -d @/tmp/orch-sched.json $AMUX_URL/api/schedules
```

**Important:** The prompt embeds the note content at schedule-creation time. If you later update the mission note, re-run `/orchestrate` with `--no-schedule` to get an updated note, then manually patch the schedule via `PATCH /api/schedules/<id>` with the new embedded content. The notes are the source of truth; the scheduler prompt is a snapshot.

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
