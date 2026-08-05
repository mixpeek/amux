# Session Resume Repair (Part A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `claude --resume` actually fire when an amux session restarts, so sessions stop waking up with no memory of prior work.

**Architecture:** Three defects in `amux-server.py` each independently break resume. Fix them bottom-up: first add two bounded JSONL-reading helpers, then rebuild the two name-lookup functions on top of them, then stop requiring that the session name be persisted at all. No new files in the server; one new test file.

**Tech Stack:** Python 3.14, pytest, single-file server (`amux-server.py`). Tests load the server via `importlib` — its `if __name__ == "__main__":` guard prevents the HTTP server from starting on import.

## Global Constraints

- Target file is `amux-server.py` at the repo root. It is one large module by design; do not restructure or split it.
- Tests go in `tests/`, loaded via the `importlib` fixture pattern established in `tests/test_shell_quote_flags.py`. Copy that fixture; do not invent a new loading scheme.
- All new file reads must be **bounded** — never `read_text()` a whole conversation JSONL. These run in the session-list hot path.
- Every new helper must swallow `OSError` and malformed JSON and return a falsy value. Nothing here may raise into session startup.
- Existing behaviour to preserve: zero matches still returns `""` and falls through to a fresh `--name` start.
- Spec: `docs/superpowers/specs/2026-07-31-session-context-recovery-design.md`.
- Part B (the resume brief) is **out of scope for this plan**. Do not add hooks or memory-writing code.

---

### Task 1: Bounded JSONL header helpers

**Files:**
- Modify: `amux-server.py` — insert both helpers immediately after `_validate_cc_session_name` (currently ends at line 2934), before `_cc_session_exists_in_project`
- Test: `tests/test_session_resume.py` (create)

**Interfaces:**
- Consumes: `CLAUDE_HOME`, `json`, `Path` (already imported at module top)
- Produces:
  - `_cc_session_title(path: Path, max_lines: int = 30) -> str`
  - `_jsonl_has_messages(path: Path, max_lines: int = 2000) -> bool`
  - `_CC_TITLE_SCAN_LINES: int = 30`

- [ ] **Step 1: Write the failing tests**

Create `tests/test_session_resume.py`:

```python
"""Unit tests for amux session resume — the path that decides whether a
restarting session resumes its conversation or wakes up blank.

Three defects made this dead code on every install. These tests pin all three:
the title lives on line 2 and the lookup only read line 1; the lookup demanded
a unique name match while every fresh start added another identically-named
conversation; and the name was only persisted on graceful stop.

Loaded via importlib like tests/test_shell_quote_flags.py so no drift is possible.
"""

import importlib.util
import json
import os
import sys
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).parent.parent
SERVER_PATH = REPO_ROOT / "amux-server.py"


@pytest.fixture(scope="module")
def amux_server():
    spec = importlib.util.spec_from_file_location("amux_server", SERVER_PATH)
    assert spec is not None and spec.loader is not None, f"could not load {SERVER_PATH}"
    mod = importlib.util.module_from_spec(spec)
    sys.modules["amux_server"] = mod
    spec.loader.exec_module(mod)
    return mod


def _write_jsonl(path: Path, entries):
    path.write_text("\n".join(json.dumps(e) for e in entries) + "\n")


def _header(title):
    """The header block Claude Code writes: custom-title is NOT line 1."""
    return [
        {"type": "last-prompt", "leafUuid": "abc"},
        {"type": "custom-title", "customTitle": title, "sessionId": "x"},
    ]


def _msg(role="user"):
    return {"type": role, "message": {"role": role, "content": "hi"}}


# ── _cc_session_title ────────────────────────────────────────────────────────

def test_title_on_line_two_is_found(amux_server, tmp_path):
    """The regression: Claude Code writes custom-title on line 2, and the old
    lookup read only line 1, so it matched nothing on any install."""
    f = tmp_path / "conv.jsonl"
    _write_jsonl(f, _header("Amux-gtm") + [_msg()])
    assert amux_server._cc_session_title(f) == "Amux-gtm"


def test_title_on_line_one_is_found(amux_server, tmp_path):
    f = tmp_path / "conv.jsonl"
    _write_jsonl(f, [{"type": "custom-title", "customTitle": "Solo"}])
    assert amux_server._cc_session_title(f) == "Solo"


def test_session_name_key_is_accepted(amux_server, tmp_path):
    f = tmp_path / "conv.jsonl"
    _write_jsonl(f, [{"type": "meta"}, {"sessionName": "Legacy"}])
    assert amux_server._cc_session_title(f) == "Legacy"


def test_title_absent_returns_empty(amux_server, tmp_path):
    f = tmp_path / "conv.jsonl"
    _write_jsonl(f, [_msg(), _msg("assistant")])
    assert amux_server._cc_session_title(f) == ""


def test_title_scan_is_bounded(amux_server, tmp_path):
    """A title past the scan window is not found — the read must stay bounded
    because this runs on every session-list refresh."""
    f = tmp_path / "conv.jsonl"
    _write_jsonl(f, [_msg()] * 40 + [{"type": "custom-title", "customTitle": "TooLate"}])
    assert amux_server._cc_session_title(f, max_lines=30) == ""


def test_malformed_lines_are_skipped(amux_server, tmp_path):
    f = tmp_path / "conv.jsonl"
    f.write_text("not json\n" + json.dumps({"customTitle": "Survivor"}) + "\n")
    assert amux_server._cc_session_title(f) == "Survivor"


def test_missing_file_returns_empty(amux_server, tmp_path):
    assert amux_server._cc_session_title(tmp_path / "nope.jsonl") == ""


def test_non_dict_json_line_is_skipped(amux_server, tmp_path):
    f = tmp_path / "conv.jsonl"
    f.write_text('["a list"]\n' + json.dumps({"customTitle": "Survivor"}) + "\n")
    assert amux_server._cc_session_title(f) == "Survivor"


# ── _jsonl_has_messages ─────────────────────────────────────────────────────

def test_has_messages_true_for_real_conversation(amux_server, tmp_path):
    f = tmp_path / "conv.jsonl"
    _write_jsonl(f, _header("X") + [_msg("assistant")])
    assert amux_server._jsonl_has_messages(f) is True


def test_has_messages_false_for_snapshot_only(amux_server, tmp_path):
    """claude --resume exits instantly on these, so resuming one is a fresh
    start with extra steps."""
    f = tmp_path / "conv.jsonl"
    _write_jsonl(f, _header("X") + [{"type": "file-history-snapshot"}])
    assert amux_server._jsonl_has_messages(f) is False


def test_has_messages_false_for_missing_file(amux_server, tmp_path):
    assert amux_server._jsonl_has_messages(tmp_path / "nope.jsonl") is False
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `python3 -m pytest tests/test_session_resume.py -q`
Expected: FAIL — `AttributeError: module 'amux_server' has no attribute '_cc_session_title'`

If `pytest` is missing, create a venv first:
`python3 -m venv .venv && .venv/bin/pip install -q pytest` then use `.venv/bin/python -m pytest`.

- [ ] **Step 3: Write the implementation**

In `amux-server.py`, directly after `_validate_cc_session_name` (line 2934) and before `_cc_session_exists_in_project`, insert:

```python
# Claude Code writes the session title as a `custom-title` entry inside the
# conversation file's header block — in practice line 2, behind `last-prompt`
# or `queue-operation`. The lookups below used to read only the first line,
# which matched nothing on any install and left resume permanently dead.
_CC_TITLE_SCAN_LINES = 30


def _cc_session_title(path: Path, max_lines: int = _CC_TITLE_SCAN_LINES) -> str:
    """Return the session title recorded in a conversation JSONL, or ''.

    Reads at most `max_lines` lines — this runs for every conversation file in
    a project on each session-list refresh, so it must not scan whole files.
    """
    try:
        with path.open(errors="replace") as fh:
            for i, line in enumerate(fh):
                if i >= max_lines:
                    break
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(rec, dict):
                    continue
                title = rec.get("customTitle") or rec.get("sessionName")
                if title:
                    return title
    except OSError:
        pass
    return ""


def _jsonl_has_messages(path: Path, max_lines: int = 2000) -> bool:
    """True if a conversation has at least one user/assistant turn.

    `claude --resume` exits immediately on a snapshot-only file, so offering
    one as a resume target produces a fresh start with extra steps.
    """
    try:
        with path.open(errors="replace") as fh:
            for i, line in enumerate(fh):
                if i >= max_lines:
                    break
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(rec, dict) and rec.get("type") in ("user", "assistant"):
                    return True
    except OSError:
        pass
    return False
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `python3 -m pytest tests/test_session_resume.py -q`
Expected: PASS (12 passed)

- [ ] **Step 5: Commit**

```bash
git add amux-server.py tests/test_session_resume.py
git commit -m "feat(resume): bounded helpers for JSONL title and message detection

Claude Code writes custom-title inside the conversation header block, not on
line 1. Both helpers cap their reads because they run for every conversation
file in a project on each session-list refresh."
```

---

### Task 2: Rebuild the name lookups on the helpers

**Files:**
- Modify: `amux-server.py:2937-2982` — replace `_cc_session_exists_in_project` and `_cc_session_id_for_name` wholesale
- Test: `tests/test_session_resume.py` (append)

**Interfaces:**
- Consumes: `_cc_session_title`, `_jsonl_has_messages` (Task 1), `_project_name`, `CLAUDE_HOME`
- Produces:
  - `_cc_session_candidates(session_name: str, work_dir: str) -> list[Path]` — resumable matches, newest first
  - `_cc_session_exists_in_project(session_name: str, work_dir: str) -> bool` — unchanged signature
  - `_cc_session_id_for_name(session_name: str, work_dir: str) -> str` — unchanged signature, returns the conversation UUID (the file stem, which is what `--resume` takes)

- [ ] **Step 1: Write the failing tests**

Append to `tests/test_session_resume.py`:

```python
# ── name → conversation id ──────────────────────────────────────────────────

@pytest.fixture
def project(amux_server, tmp_path, monkeypatch):
    """Build a fake ~/.claude/projects/<slug>/ and return a writer + work_dir."""
    monkeypatch.setattr(amux_server, "CLAUDE_HOME", tmp_path)
    work_dir = "/Users/someone/Projects/demo"
    proj = tmp_path / "projects" / amux_server._project_name(work_dir)
    proj.mkdir(parents=True)

    def add(uuid, title, mtime, with_messages=True):
        f = proj / f"{uuid}.jsonl"
        entries = _header(title) + ([_msg()] if with_messages else
                                    [{"type": "file-history-snapshot"}])
        _write_jsonl(f, entries)
        os.utime(f, (mtime, mtime))
        return f

    return add, work_dir


def test_resolves_title_recorded_on_line_two(amux_server, project):
    """End-to-end for the line-1 bug: nothing resolved before this."""
    add, work_dir = project
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", time.time())
    assert amux_server._cc_session_id_for_name("Amux-gtm", work_dir) == \
        "aaaaaaaa-0000-0000-0000-000000000000"


def test_many_same_named_sessions_resolve_to_newest(amux_server, project):
    """The death spiral: each fresh start added another 'Amux-gtm' conversation,
    and the old code required exactly one match — so once it had failed twice it
    could never succeed again. Newest wins instead."""
    add, work_dir = project
    now = time.time()
    add("11111111-0000-0000-0000-000000000000", "Amux-gtm", now - 500_000)
    add("22222222-0000-0000-0000-000000000000", "Amux-gtm", now - 100_000)
    add("33333333-0000-0000-0000-000000000000", "Amux-gtm", now - 10)
    add("44444444-0000-0000-0000-000000000000", "Amux-gtm", now - 200_000)
    assert amux_server._cc_session_id_for_name("Amux-gtm", work_dir) == \
        "33333333-0000-0000-0000-000000000000"


def test_snapshot_only_files_are_not_resume_targets(amux_server, project):
    """A newer snapshot-only file must not beat an older real conversation."""
    add, work_dir = project
    now = time.time()
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", now - 1000)
    add("bbbbbbbb-0000-0000-0000-000000000000", "Amux-gtm", now, with_messages=False)
    assert amux_server._cc_session_id_for_name("Amux-gtm", work_dir) == \
        "aaaaaaaa-0000-0000-0000-000000000000"


def test_no_match_returns_empty(amux_server, project):
    add, work_dir = project
    add("aaaaaaaa-0000-0000-0000-000000000000", "Other-Session", time.time())
    assert amux_server._cc_session_id_for_name("Amux-gtm", work_dir) == ""


def test_missing_project_dir_returns_empty(amux_server, tmp_path, monkeypatch):
    monkeypatch.setattr(amux_server, "CLAUDE_HOME", tmp_path)
    assert amux_server._cc_session_id_for_name("Amux-gtm", "/no/such/dir") == ""


def test_exists_in_project_sees_line_two_title(amux_server, project):
    add, work_dir = project
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", time.time())
    assert amux_server._cc_session_exists_in_project("Amux-gtm", work_dir) is True


def test_exists_in_project_false_when_absent(amux_server, project):
    add, work_dir = project
    add("aaaaaaaa-0000-0000-0000-000000000000", "Other", time.time())
    assert amux_server._cc_session_exists_in_project("Amux-gtm", work_dir) is False


def test_candidates_are_ordered_newest_first(amux_server, project):
    add, work_dir = project
    now = time.time()
    add("11111111-0000-0000-0000-000000000000", "S", now - 300)
    add("22222222-0000-0000-0000-000000000000", "S", now - 100)
    got = [p.stem[:8] for p in amux_server._cc_session_candidates("S", work_dir)]
    assert got == ["22222222", "11111111"]
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `python3 -m pytest tests/test_session_resume.py -q`
Expected: FAIL — `AttributeError: ... has no attribute '_cc_session_candidates'`, and `test_many_same_named_sessions_resolve_to_newest` fails returning `''`.

- [ ] **Step 3: Write the implementation**

Replace `amux-server.py:2937-2982` entirely (both existing functions) with:

```python
def _cc_session_candidates(session_name: str, work_dir: str) -> list[Path]:
    """Resumable conversation files titled `session_name`, newest first."""
    proj_dir = CLAUDE_HOME / "projects" / _project_name(work_dir)
    if not proj_dir.is_dir():
        return []
    scored: list[tuple[float, Path]] = []
    try:
        for jf in proj_dir.glob("*.jsonl"):
            try:
                if _cc_session_title(jf) != session_name:
                    continue
                if not _jsonl_has_messages(jf):
                    continue
                scored.append((jf.stat().st_mtime, jf))
            except OSError:
                continue
    except OSError:
        return []
    scored.sort(key=lambda pair: pair[0], reverse=True)
    return [p for _, p in scored]


def _cc_session_exists_in_project(session_name: str, work_dir: str) -> bool:
    """Check if a resumable Claude Code session with this name exists."""
    return bool(_cc_session_candidates(session_name, work_dir))


def _cc_session_id_for_name(session_name: str, work_dir: str) -> str:
    """Return the UUID of the most recent resumable session with this name, or ''.

    Multiple matches are not genuinely ambiguous: two *live* amux sessions
    cannot share a name within one install and project directory, so extra
    matches are always older incarnations of the same logical session.

    Requiring a unique match (as this did) was self-defeating — every fresh
    start wrote another identically-named conversation, so the second failure
    guaranteed all subsequent ones. Taking the newest breaks that spiral.
    """
    candidates = _cc_session_candidates(session_name, work_dir)
    return candidates[0].stem if candidates else ""
```

Note: the returned id is the file stem. That is the conversation UUID `--resume` accepts, and the same key `detect_active_model` uses to locate `{conversation_id}.jsonl`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `python3 -m pytest tests/test_session_resume.py -q`
Expected: PASS (20 passed)

- [ ] **Step 5: Run the full suite for regressions**

Run: `python3 -m pytest tests/ -q`
Expected: PASS, no failures. (Baseline before this plan: 178 passed.)

- [ ] **Step 6: Commit**

```bash
git add amux-server.py tests/test_session_resume.py
git commit -m "fix(resume): match titles past line 1 and break the ambiguity spiral

The lookups read only the first line of each conversation JSONL while Claude
Code writes custom-title on line 2, so they matched nothing. They also required
a unique name match — but every fresh start appends another identically-named
conversation, so once the lookup had failed twice it could never succeed again.

Scan the header block, skip snapshot-only files that --resume exits on, and
resolve multiple matches to the most recently modified."
```

---

### Task 3: Stop requiring the session name to be persisted

**Files:**
- Modify: `amux-server.py` — add `_resolve_cc_session_name` immediately after `_cc_session_id_for_name` (the function Task 2 rewrites)
- Modify: `amux-server.py:14922` (one line, inside `start_session`)
- Test: `tests/test_session_resume.py` (append)

**Interfaces:**
- Consumes: `_cc_session_id_for_name` (Task 2)
- Produces: `_resolve_cc_session_name(meta: dict, name: str) -> str`

The derivation lives in its own helper rather than inline in `start_session`.
`start_session` is ~700 lines with tmux side effects and cannot be unit tested;
inlining the logic would leave the fix's only real decision covered by nothing
but a test that re-implements it. A named helper makes the production code
itself the thing under test.

- [ ] **Step 1: Write the failing tests**

Append to `tests/test_session_resume.py`:

```python
# ── name derivation ─────────────────────────────────────────────────────────

def test_derives_amux_name_when_meta_is_empty(amux_server):
    """The crash case. cc_session_name was written only in stop_session(), so
    any ending that was not a graceful stop — crash, reboot, sleep, server
    restart — left meta empty and forced a fresh start on a session that was
    fully resumable. amux always launches with `--name <session name>`, so the
    name was derivable the whole time."""
    assert amux_server._resolve_cc_session_name({}, "Amux-gtm") == "Amux-gtm"


def test_persisted_meta_name_wins(amux_server):
    """A /rename inside Claude must still be honoured over the amux name."""
    assert amux_server._resolve_cc_session_name(
        {"cc_session_name": "Renamed-By-User"}, "Amux-gtm") == "Renamed-By-User"


def test_blank_meta_name_falls_back_to_amux_name(amux_server):
    """An empty string is not a rename — it is absence, and must not win."""
    assert amux_server._resolve_cc_session_name(
        {"cc_session_name": ""}, "Amux-gtm") == "Amux-gtm"


def test_derived_name_resolves_to_a_conversation(amux_server, project):
    """End to end: empty meta, as after a crash, still finds the conversation."""
    add, work_dir = project
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", time.time())
    resolved = amux_server._resolve_cc_session_name({}, "Amux-gtm")
    assert amux_server._cc_session_id_for_name(resolved, work_dir) == \
        "aaaaaaaa-0000-0000-0000-000000000000"
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `.venv/bin/python -m pytest tests/test_session_resume.py -k resolve_cc -v`
Expected: FAIL — `AttributeError: module 'amux_server' has no attribute '_resolve_cc_session_name'`

- [ ] **Step 3: Write the implementation**

In `amux-server.py`, immediately after `_cc_session_id_for_name`, add:

```python
def _resolve_cc_session_name(meta: dict, name: str) -> str:
    """Return the Claude-side session name for an amux session.

    amux always launches Claude with `--name <amux session name>`, so this is
    derivable and never needed persisting. It used to be read from meta alone,
    and meta was only written in stop_session() — so any ending that was not a
    graceful stop lost it and forced a fresh start on a resumable session.

    A name persisted in meta still wins, so `/rename` inside Claude is honoured.
    """
    return meta.get("cc_session_name") or name
```

Then in `start_session`, replace the `cc_session_name` assignment (locate it by
content — Tasks 1 and 2 shifted the line numbers):

```python
            cc_session_name = meta.get("cc_session_name", "")
```

with:

```python
            cc_session_name = _resolve_cc_session_name(meta, name)
```

- [ ] **Step 3b: Remove the branch Task 2 made unreachable**

**Amendment, ruled by the human partner after Task 2's review.** The plan
originally said "change nothing else in the branch." That is overridden here:
Task 2 made one branch of this exact `if/elif` chain dead, and this task is
already editing the chain.

Task 2 rebuilt `_cc_session_id_for_name` and `_cc_session_exists_in_project` as
thin wrappers over the same `_cc_session_candidates` call, so each is truthy
exactly when the other is. That makes the `elif` below unreachable: whenever it
would be `True`, `_sid` is already truthy and the `if` above it has fired. The
branch existed to handle an ambiguous name, a state Task 2 deliberately made
impossible. Delete the whole `elif` clause:

```python
                elif _cc_session_exists_in_project(cc_session_name, work_dir):
                    # Multiple sessions with this name — fall back to UUID if available
                    if conv_id and _uuid_re.match(conv_id):
                        conv_file = CLAUDE_HOME / "projects" / _project_name(work_dir) / f"{conv_id}.jsonl"
                        if conv_file.exists():
                            session_flag = f'--resume {conv_id}'
                            print(f"[start] {name}: resume via UUID fallback (ambiguous name '{cc_session_name}', uuid={conv_id})")
                        else:
                            session_flag = f'--name {shlex.quote(name)}'
                            print(f"[start] {name}: fresh start (ambiguous name, stale uuid)")
                    else:
                        session_flag = f'--name {shlex.quote(name)}'
                        print(f"[start] {name}: fresh start (ambiguous session name '{cc_session_name}')")
```

leaving the chain as:

```python
            if cc_session_name and _validate_cc_session_name(cc_session_name):
                _sid = _cc_session_id_for_name(cc_session_name, work_dir)
                if _sid:
                    # Use UUID to resume — bypasses interactive picker
                    session_flag = f'--resume {_sid}'
                    print(f"[start] {name}: resume={cc_session_name} (uuid={_sid})")
                else:
                    meta.pop("cc_session_name", None)
                    meta.pop("cc_conversation_id", None)
                    _save_meta(name, meta)
                    session_flag = f'--name {shlex.quote(name)}'
                    print(f"[start] {name}: fresh start (session '{cc_session_name}' not found in project)")
```

Nothing else in the enclosing `if not _skip_conv_id and provider == "claude":`
block changes. In particular **keep** the outer `elif conv_id and
_uuid_re.match(conv_id):` migration branch that follows. It is still reachable:
it runs when `cc_session_name` fails `_validate_cc_session_name`, which happens
for amux session names that do not match `^[a-zA-Z0-9][a-zA-Z0-9_.\-]*$`. Both
`conv_id` and `_uuid_re` therefore remain in use — do not delete them.

The remaining not-found path stays correct: with derivation, popping the meta
keys simply means the next start derives the name again.

- [ ] **Step 4: Run the full suite**

Run: `python3 -m pytest tests/ -q`
Expected: PASS, no failures.

- [ ] **Step 5: Verify against the real home directory**

This asserts the fix works on actual data rather than fixtures. Run from the repo root:

```bash
python3 - <<'EOF'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("amux_server", "amux-server.py")
m = importlib.util.module_from_spec(spec); sys.modules["amux_server"] = m
spec.loader.exec_module(m)
wd = "/Users/dorongreenspan/Projects/amux-gtm"
print("resolved:", m._cc_session_id_for_name("Amux-gtm", wd) or "(none)")
EOF
```

Expected: a UUID, not `(none)`. Before this plan it returned `''` for every session.

- [ ] **Step 6: Commit**

```bash
git add amux-server.py tests/test_session_resume.py
git commit -m "fix(resume): derive the Claude session name instead of persisting it

cc_session_name was written only in stop_session(), so any ending that was not
a graceful stop lost it and forced a fresh start. amux always launches with
--name <session name>, so the name was derivable all along; meta still takes
precedence so a /rename inside Claude is honoured.

Removes the crash window entirely and repairs existing sessions with no
migration — their conversation files already carry the right title."
```

---

### Task 4: Manual acceptance

**Files:** none — this task verifies behaviour, it does not change code.

**Interfaces:**
- Consumes: all of Tasks 1–3

- [ ] **Step 1: Restart a session through amux**

Restart `Amux-gtm` from the amux dashboard (card menu → Restart), or:

```bash
curl -sk -X POST "$AMUX_URL/api/sessions/Amux-gtm/restart"
```

- [ ] **Step 2: Confirm the resume path was taken**

```bash
grep "\[start\] Amux-gtm" ~/.amux/serve.log | tail -3
```

Expected: `[start] Amux-gtm: resume=Amux-gtm (uuid=…)`
Failure signal: `fresh start (new session)` — resume is still broken; do not mark this plan complete.

- [ ] **Step 3: Confirm the session actually retained context**

Send the restarted session: `what were you working on before this restart?`

Expected: it answers from the prior conversation without being handed a log. This is the acceptance criterion the whole plan exists for — a passing test suite with a session that still wakes up blank is a failure.

- [ ] **Step 4: Confirm no cross-session contamination**

```bash
grep "^\[start\]" ~/.amux/serve.log | tail -8
```

Expected: each session resolves to its own uuid; no two sessions report the same one. If two do, `_cc_session_candidates` is matching across project directories — stop and re-check `_project_name(work_dir)` scoping.

- [ ] **Step 5: Open the PR**

```bash
git push -u origin fix/session-resume-and-recovery-brief
gh pr create --base main \
  --title "fix(resume): repair amux session resume — three compounding bugs" \
  --body "See docs/superpowers/specs/2026-07-31-session-context-recovery-design.md"
```

Note: the repo's pre-push guard mis-fires on new branches, listing every ancestor commit as "foreign". Verify with `git log --oneline origin/main..HEAD` that only your commits are present, then re-run with `AMUX_ALLOW_FOREIGN=1` prefixed.

---

## Self-Review

**Spec coverage (Part A):**
| Spec item | Task |
|---|---|
| A1 derive session name | Task 3 (`_resolve_cc_session_name`) |
| A2 scan JSONL header block | Task 1 (`_cc_session_title`) + Task 2 (both call sites) |
| A3 recency tie-break | Task 2 (`_cc_session_candidates` sort) |
| A3 skip snapshot-only files | Task 1 (`_jsonl_has_messages`) + Task 2 (filter) |
| Bounded reads | Task 1, `max_lines` on both helpers; `test_title_scan_is_bounded` |
| Zero matches → `""` | Task 2, `test_no_match_returns_empty` |
| Live `Amux-gtm` resolution | Task 3 Step 5 |
| Manual `serve.log` acceptance | Task 4 |

Part B (resume brief) and Part C (Load log retier) are explicitly out of scope, per the Global Constraints.

**Placeholder scan:** none — every step carries runnable code or an exact command.

**Type consistency:** `_cc_session_title(Path, int) -> str`, `_jsonl_has_messages(Path, int) -> bool`, `_cc_session_candidates(str, str) -> list[Path]`, `_cc_session_id_for_name(str, str) -> str`, `_cc_session_exists_in_project(str, str) -> bool`. Task 2 consumes Task 1's helpers under the names Task 1 defines. `_cc_session_id_for_name` returns `.stem` (a `str`), matching its existing callers at `:14925` and `:14932`, which interpolate it into a `--resume` flag.
