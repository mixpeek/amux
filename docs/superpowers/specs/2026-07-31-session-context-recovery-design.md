# Session context recovery

**Date:** 2026-07-31
**Status:** approved, not yet implemented

## Problem

Reopening an amux session frequently shows an agent with no memory of prior work.
The available workaround is the peek UI's "Load log" button, which sends the
session a prompt telling it to read `~/.amux/logs/.plain/<name>.log`. For
`Amux-gtm` that file is 2.6 MB / 59,900 lines, of which roughly 74% is redundant
terminal redraw (35,176 lines with content, 9,123 of them unique). It does not
fit in a context window, so the recovery path does not work.

A context-less session is not merely forgetful. It reverses decisions that were
already made — re-doing work, undoing conventions, contradicting earlier
instructions — because nothing tells it those decisions exist.

## Root cause

"Load log" is a workaround for a resume path that has never functioned.

`start_session` already tries to resume: it reads `cc_session_name` /
`cc_conversation_id` from the session's `meta.json` and launches
`claude --resume <uuid>` (`amux-server.py:14917-14963`). That branch has never
been taken. From `~/.amux/serve.log`, every session on this install:

```
[start] Amux-gtm: fresh start (new session)
[start] Amux: fresh start (new session)
[start] amux-helper: fresh start (new session)
[start] Kids-Daily-Prep: fresh start (new session)
[start] windsor-suits-rebuild: fresh start (new session)
[start] Family-Command-and-Control-Center: fresh start (new session)
[start] AI---Work-Course: fresh start (new session)
```

None of the 8 sessions has either key in `meta.json`. `Amux-gtm.meta.json` shows
`start_count: 13` — thirteen restarts, thirteen blank slates.

Three independent defects each break resume on their own.

### Bug 1 — the session name is only persisted on graceful stop

`cc_session_name` is written in exactly two places, both inside `stop_session()`
(`amux-server.py:15559` and `:15584`). The only other write (`:15415`) is a
migration path gated on `cc_conversation_id` already being set, which it never
is.

So the name survives only when a session is shut down through amux's stop path.
Every other ending — crash, reboot, machine sleep, tmux kill, amux server
restart — leaves it unset, and the next start is fresh. Long-lived sessions
rarely end gracefully, which is why this looks intermittent but is in practice
total.

### Bug 2 — the name lookup reads only the first line

`_cc_session_id_for_name` (`:2970`) and `_cc_session_exists_in_project`
(`:2945`) both do `first_line = jf.open().readline()` and match `customTitle` /
`sessionName` on it.

Claude Code writes `custom-title` on **line 2**. Line 1 is `last-prompt` or
`queue-operation`. Measured across all 26 JSONL files in the `amux-gtm` project
directory: zero first-line matches. The lookup cannot succeed for any session.

### Bug 3 — ambiguity is fatal, and self-inflicted

`_cc_session_id_for_name` ends with `return matches[0] if len(matches) == 1 else ""`.

Scanning past line 1, six files in the `amux-gtm` project carry
`customTitle: "Amux-gtm"` — one per fresh start. With Bug 2 fixed, the lookup
would find six matches, fail the uniqueness test, and still return `""`.

This is the reason the condition never self-corrected: each fresh start creates
another identically-named session, which makes the next lookup more ambiguous,
which guarantees another fresh start.

## Design

Two parts, shipped in order. Part A removes the cause; Part B is the safety net
for context loss that resume cannot address. **Part A is independently
shippable and is expected to resolve the majority of the reported pain.**

### Part A — repair resume

**A1. Derive the session name rather than persisting it.**

amux always launches Claude with `--name <amux session name>`, so the Claude
session name is knowable without being stored. Change the lookup in
`start_session` to:

```python
cc_session_name = meta.get("cc_session_name") or name
```

This removes the crash-window entirely — there is no longer state that can be
lost — and retroactively repairs all existing sessions with no migration step,
because their historical JSONL files already carry the correct `customTitle`.
The `meta` field is retained as an override for sessions renamed with `/rename`
inside Claude.

**A2. Scan the JSONL header block instead of line 1.**

Add a helper:

```python
def _cc_session_title(path: Path, max_lines: int = 30) -> str:
    """Return a JSONL conversation's session title, or "".

    Claude Code writes the title as a `custom-title` entry within the file's
    header block, not necessarily on line 1.
    """
```

It reads at most `max_lines` lines and returns on the first match of
`custom-title` / `customTitle` / `sessionName`. Both `_cc_session_id_for_name`
and `_cc_session_exists_in_project` call it. The read stays bounded — no
full-file scan is introduced.

**A3. Resolve ambiguity by recency, and skip unresumable files.**

Replace the uniqueness requirement with "most recently modified match wins."
Within a single amux install and project directory, two *live* sessions cannot
share a name, so multiple matches are always older incarnations of the same
logical session; the newest is the one the user means. Zero matches still
returns `""`.

Candidates are additionally filtered to files containing at least one
user/assistant message, reusing the existing snapshot-only check noted at
`:13113` — `claude --resume` exits immediately on a snapshot-only file, which
would present as a fresh start with extra steps.

### Part B — the resume brief

Covers what resume cannot: deliberate `/clear`, auto-compaction, and any
residual resume failure.

**Storage and delivery.** The brief is a marker-delimited block inside
`~/.amux/memory/<session>.md`. No new delivery mechanism is required:
`_ensure_memory` → `_write_claude_memory` (`:13927`) already composes that file
into `~/.claude/projects/<project>/memory/MEMORY.md`, which Claude Code loads
into context at session start. This was verified directly — the session that
produced this spec received that file's contents in its opening context.
`_capture_claude_memory_changes` handles the write-back, and
`_fold_memory_overflow` already bounds the file's size.

**Format.** Markers let the agent rewrite only its own block, preserving
hand-written memory above and below it:

```markdown
<!-- amux:brief 2026-07-31T18:12 -->
**Working on:** amux session resume — 3 bugs found, fix pending

**Standing decisions** — do not reverse without asking
- Fix resume before building the brief (07-31)
- Agent commits keep the `[bot]` prefix

**In flight**
- PR #75 (model badge) — open, awaiting review

**Next step**
- Implement A1–A3 + tests
<!-- /amux:brief -->
```

Capped at 25 lines. **Standing decisions is the load-bearing section**: it is
what prevents a recovered session from reversing settled decisions, which is the
specific harm being designed against.

The opening timestamp is required. It lets a recovering session reason about its
own staleness — "this brief is 40 minutes old, so check `git log` for anything
after it" — rather than trusting it blindly.

**Write triggers.** Three Claude Code hooks:

| Hook | Why |
|---|---|
| `PreCompact` | Fires exactly when context is about to be lost. The highest-value trigger. |
| `SessionEnd` | Clean exits. |
| `Stop` | Throttled: rewrite only if the existing brief is >10 minutes old, bounding crash loss to 10 minutes without paying tokens every turn. |

**Hook configuration and session identity.** The hooks are configured once, in
the user-level `~/.claude/settings.json`, not per project — amux drives many
sessions across many directories and per-project configuration would have to be
re-established for each new one.

A single global hook therefore has to know *which* session it is running in. It
reads `$AMUX_SESSION`, which amux already exports into every session's pane
(verified: `AMUX_SESSION=Amux-gtm` in this session's environment, and it is the
same variable the board and scheduler integrations already rely on, e.g.
`amux-server.py:8300`). The hook writes to
`~/.amux/memory/$AMUX_SESSION.md` and is a no-op when `$AMUX_SESSION` is unset,
so a plain non-amux `claude` session in any directory is unaffected.

### Part C — retier "Load log" (optional, deferred)

`peekLoadLogIntoSession` (`:32425`) currently instructs the session to read the
entire log. Replace with a tiered prompt: read the brief, then `git log`, then
the log's last ~2,000 lines with a note that the file is heavily
redraw-duplicated and should be deduplicated before analysis. Reading the full
log remains available as an explicit choice.

**Not scheduled.** Included for completeness; sequence after A and B land and
re-evaluate whether it is still needed.

## Error handling

| Failure | Behaviour |
|---|---|
| Hook does not fire (hard crash) | Brief is stale but present and timestamped; the session cross-checks `git log` for later activity. |
| Brief missing or malformed | Fall back to `git log` plus recent prompts. A malformed file is never overwritten wholesale — if the markers cannot be located, the block is appended rather than replacing unrecognised content. |
| Resume target deleted between lookup and launch | Existing stale-uuid path already falls back to a fresh `--name` start. |
| `--resume` opens the interactive picker | Existing detection at `:15315` already catches this and recovers. |
| Memory compose fails | `_write_claude_memory` is already wrapped in a bare `except` and cannot block startup. Preserve that property. |

## Testing

**Part A** — unit tests over synthetic `~/.claude/projects` trees, following the
`importlib` loading convention established in `tests/test_shell_quote_flags.py`:

- title on line 2 resolves (Bug 2 regression)
- six identically-named files resolve to the most recently modified (Bug 3)
- snapshot-only files are excluded from candidates
- zero matches returns `""`
- a session with no `cc_session_name` in meta still resolves via the amux name (A1)
- `_cc_session_title` stops within `max_lines` and does not read whole files

Plus a live assertion that `Amux-gtm` resolves to `642f49e4-…`, the currently
newest matching conversation.

**Part B**

- the hook replaces only the marked block; surrounding memory is byte-identical
- the 25-line cap is enforced
- a file with absent or corrupted markers is not truncated or wiped
- the brief survives a `_write_claude_memory` / `_capture_claude_memory_changes`
  round trip without duplication

**Manual acceptance:** restart `Amux-gtm` and confirm `~/.amux/serve.log` reports
`resume=Amux-gtm (uuid=…)` rather than `fresh start (new session)`, and that the
session can answer what it was working on beforehand.

## Out of scope

- Changing how Claude Code itself compacts or names conversations.
- Populating `~/.amux/notes/` and `~/.amux/transcripts/`, which exist but are
  unused; unrelated to this failure.
- Any change to the `~/.amux/memory` global/session composition scheme beyond
  adding one marked block.
