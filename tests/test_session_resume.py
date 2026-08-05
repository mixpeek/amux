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


def test_candidates_are_ordered_newest_first(amux_server, project):
    add, work_dir = project
    now = time.time()
    add("11111111-0000-0000-0000-000000000000", "S", now - 300)
    add("22222222-0000-0000-0000-000000000000", "S", now - 100)
    got = [p.stem[:8] for p in amux_server._cc_session_candidates("S", work_dir)]
    assert got == ["22222222", "11111111"]


def test_candidates_empty_when_project_name_resolution_raises(amux_server, tmp_path, monkeypatch):
    """A pathological work_dir (e.g. a symlink cycle, or a home directory that
    can't be determined) can raise RuntimeError out of Path.expanduser()/
    resolve() inside _project_name. That must not escape into session startup."""
    monkeypatch.setattr(amux_server, "CLAUDE_HOME", tmp_path)

    def boom(work_dir):
        raise RuntimeError("symlink cycle")

    monkeypatch.setattr(amux_server, "_project_name", boom)
    assert amux_server._cc_session_candidates("Amux-gtm", "/some/dir") == []


def test_candidates_empty_when_project_dir_cannot_be_listed(amux_server, project, monkeypatch):
    """A project directory that exists but raises OSError on iteration (e.g.
    permission denied) must not raise into session startup. Forced via
    monkeypatch rather than chmod so this passes when run as root too."""
    add, work_dir = project
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", time.time())

    def boom(self, pattern):
        raise OSError("permission denied")

    monkeypatch.setattr(Path, "glob", boom)
    assert amux_server._cc_session_candidates("Amux-gtm", work_dir) == []


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


# ── _resume_strategy ─────────────────────────────────────────────────────────
#
# start_session is ~700 lines with tmux side effects and cannot be unit
# tested directly, so the Claude-provider resume decision lives in this pure
# helper: given (meta, name, work_dir), decide the tmux launch flag and which
# meta keys (if any) should be cleared before the next start. start_session
# applies the result — it does not re-decide anything.

def test_resume_strategy_title_miss_falls_back_to_conv_id(amux_server, project):
    """Title match misses (stale/renamed title) but the PostToolUse hook's
    conv_id still points at a real, resumable conversation — the deterministic
    pointer must win over giving up."""
    add, work_dir = project
    add("bbbbbbbb-0000-0000-0000-000000000000", "Some-Other-Title", time.time())
    meta = {"cc_conversation_id": "bbbbbbbb-0000-0000-0000-000000000000"}
    flag, cleared, _ = amux_server._resume_strategy(meta, "Amux-gtm", work_dir)
    assert flag == "--resume bbbbbbbb-0000-0000-0000-000000000000"
    assert cleared == []


def test_resume_strategy_title_miss_conv_id_file_missing(amux_server, project):
    """conv_id is set but no such file exists — nothing to resume, and the
    stale pointer must be cleared so the next start doesn't re-check it."""
    add, work_dir = project
    meta = {"cc_conversation_id": "cccccccc-0000-0000-0000-000000000000"}
    flag, cleared, _ = amux_server._resume_strategy(meta, "Amux-gtm", work_dir)
    assert flag == "--name Amux-gtm"
    assert set(cleared) == {"cc_conversation_id"}


def test_resume_strategy_title_miss_conv_id_snapshot_only(amux_server, project):
    """conv_id's file exists but has no real turns (claude --resume would exit
    instantly on it) — treat it the same as missing: fresh start, cleared."""
    add, work_dir = project
    add("dddddddd-0000-0000-0000-000000000000", "Some-Other-Title", time.time(),
        with_messages=False)
    meta = {"cc_conversation_id": "dddddddd-0000-0000-0000-000000000000"}
    flag, cleared, _ = amux_server._resume_strategy(meta, "Amux-gtm", work_dir)
    assert flag == "--name Amux-gtm"
    assert set(cleared) == {"cc_conversation_id"}


def test_resume_strategy_title_hit_wins_and_conv_id_untouched(amux_server, project):
    """The common case: title match succeeds. conv_id (even if present and
    unrelated) is not consulted or cleared."""
    add, work_dir = project
    add("eeeeeeee-0000-0000-0000-000000000000", "Amux-gtm", time.time())
    meta = {"cc_conversation_id": "ffffffff-0000-0000-0000-000000000000"}
    flag, cleared, _ = amux_server._resume_strategy(meta, "Amux-gtm", work_dir)
    assert flag == "--resume eeeeeeee-0000-0000-0000-000000000000"
    assert cleared == []
    assert meta == {"cc_conversation_id": "ffffffff-0000-0000-0000-000000000000"}


def test_resume_strategy_first_ever_start_nothing_to_clear(amux_server, project):
    """A genuine first-ever start (empty meta, no conversations anywhere) must
    not be reported as a stale name, and there is nothing to clear or save."""
    _add, work_dir = project
    flag, cleared, msg = amux_server._resume_strategy({}, "Amux-gtm", work_dir)
    assert flag == "--name Amux-gtm"
    assert cleared == []
    assert "no prior conversation" in msg


def test_resume_strategy_migration_branch_rejects_snapshot_only(amux_server, project):
    """Migration path (no usable title, bare uuid in meta): a snapshot-only
    file is not resumable — `claude --resume` exits instantly on it — so the
    uuid-only branch must apply the same has-messages guard as step 2 rather
    than handing --resume a file it will bounce off."""
    add, work_dir = project
    add("dddddddd-0000-0000-0000-000000000000", "Whatever", time.time(),
        with_messages=False)
    # An invalid amux name forces the elif (migration) branch.
    meta = {"cc_session_name": "!!bad!!",
            "cc_conversation_id": "dddddddd-0000-0000-0000-000000000000"}
    flag, _cleared, msg = amux_server._resume_strategy(meta, "!!also-bad!!", work_dir)
    assert flag == "--name '!!also-bad!!'"
    assert "stale uuid" in msg


def test_resume_strategy_invalid_persisted_name_falls_back_to_amux_name(amux_server, project):
    """A corrupt/illegal `cc_session_name` in meta must not permanently poison
    resume for the session: fall back to the derived amux name, which is what
    Claude was actually launched with."""
    add, work_dir = project
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", time.time())
    meta = {"cc_session_name": "!! not a valid name !!"}
    flag, cleared, _ = amux_server._resume_strategy(meta, "Amux-gtm", work_dir)
    assert flag == "--resume aaaaaaaa-0000-0000-0000-000000000000"
    assert cleared == []


# ── the reset escape hatch: cc_fresh_after ───────────────────────────────────
#
# reset_session and PATCH /config {new_conversation:true} used to force a fresh
# start by DELETING cc_session_name / cc_conversation_id from meta. That only
# worked because the title lookup was dead. Now that the name is derived, the
# absence of meta says nothing — the next start would title-match and resume
# the very conversation just abandoned, while still reporting success. The
# signal is therefore positive and a TIMESTAMP: conversations older than the
# reset are excluded, conversations created after it are not.

def test_candidates_drop_conversations_older_than_fresh_marker(amux_server, project):
    add, work_dir = project
    now = time.time()
    add("11111111-0000-0000-0000-000000000000", "Amux-gtm", now - 500)
    assert amux_server._cc_session_candidates(
        "Amux-gtm", work_dir, fresh_after=now - 100) == []


def test_candidates_keep_conversations_newer_than_fresh_marker(amux_server, project):
    """The whole point of a timestamp rather than a flag: the conversation the
    reset itself creates must be resumable on the NEXT restart."""
    add, work_dir = project
    now = time.time()
    add("11111111-0000-0000-0000-000000000000", "Amux-gtm", now - 500)
    add("22222222-0000-0000-0000-000000000000", "Amux-gtm", now)
    got = [p.stem[:8] for p in amux_server._cc_session_candidates(
        "Amux-gtm", work_dir, fresh_after=now - 100)]
    assert got == ["22222222"]


def test_resume_strategy_after_reset_starts_fresh(amux_server, project):
    """Post-reset meta (keys popped, marker set) in a project that still holds
    a matching conversation older than the reset. This is the gap that let the
    bug through: without the marker this returns --resume and reset is a no-op
    that reports success."""
    add, work_dir = project
    now = time.time()
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", now - 500)
    meta = {"cc_fresh_after": int(now - 100)}
    flag, cleared, msg = amux_server._resume_strategy(meta, "Amux-gtm", work_dir)
    assert flag == "--name Amux-gtm"
    assert cleared == []
    assert "aaaaaaaa" not in msg


def test_resume_strategy_resumes_conversation_created_after_reset(amux_server, project):
    """One reset must not disable resume forever."""
    add, work_dir = project
    now = time.time()
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", now - 500)
    add("bbbbbbbb-0000-0000-0000-000000000000", "Amux-gtm", now)
    meta = {"cc_fresh_after": int(now - 100)}
    flag, _cleared, _ = amux_server._resume_strategy(meta, "Amux-gtm", work_dir)
    assert flag == "--resume bbbbbbbb-0000-0000-0000-000000000000"


def test_resume_strategy_without_marker_is_unchanged(amux_server, project):
    """No marker in meta → byte-identical behaviour to before the marker
    existed. Every pre-existing session's meta is in this state."""
    add, work_dir = project
    now = time.time()
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", now - 500)
    flag, cleared, _ = amux_server._resume_strategy({}, "Amux-gtm", work_dir)
    assert flag == "--resume aaaaaaaa-0000-0000-0000-000000000000"
    assert cleared == []


def test_resume_strategy_marker_also_suppresses_conv_id_fallback(amux_server, project):
    """A reset must not be quietly undone via the conv_id fallback: a stale
    pointer to a pre-reset conversation is exactly what the reset dropped."""
    add, work_dir = project
    now = time.time()
    add("bbbbbbbb-0000-0000-0000-000000000000", "Some-Other-Title", now - 500)
    meta = {"cc_conversation_id": "bbbbbbbb-0000-0000-0000-000000000000",
            "cc_fresh_after": int(now - 100)}
    flag, cleared, _ = amux_server._resume_strategy(meta, "Amux-gtm", work_dir)
    assert flag == "--name Amux-gtm"
    assert set(cleared) == {"cc_conversation_id"}


def test_reset_session_records_the_fresh_marker(amux_server, tmp_path, monkeypatch):
    """The contract in reset_session's own docstring, pinned: after a reset the
    next start must be a fresh conversation. Popping the keys no longer
    achieves that on its own, so the marker must actually be written."""
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    (sessions / "Amux-gtm.env").write_text('CC_DIR="/Users/someone/Projects/demo"\n')
    monkeypatch.setattr(amux_server, "CC_SESSIONS", sessions)
    (sessions / "Amux-gtm.meta.json").write_text(json.dumps({
        "cc_conversation_id": "aaaaaaaa-0000-0000-0000-000000000000",
        "cc_session_name": "Amux-gtm",
    }))
    monkeypatch.setattr(amux_server, "_is_session_blocked", lambda n: False)
    monkeypatch.setattr(amux_server, "is_running", lambda n: False)
    monkeypatch.setattr(amux_server, "slog", lambda *a, **k: None)
    monkeypatch.setattr(amux_server, "_ilog", lambda *a, **k: None)
    monkeypatch.setattr(amux_server, "start_session", lambda n: (True, "started"))

    before = int(time.time())
    ok, _msg = amux_server.reset_session("Amux-gtm")
    assert ok
    meta = json.loads((sessions / "Amux-gtm.meta.json").read_text())
    assert "cc_conversation_id" not in meta
    assert "cc_session_name" not in meta
    assert meta.get("cc_fresh_after", 0) >= before


# ── ownership: never adopt a neighbour's conversation ────────────────────────

@pytest.fixture
def sessions_dir(amux_server, tmp_path, monkeypatch):
    d = tmp_path / "sessions"
    d.mkdir()
    monkeypatch.setattr(amux_server, "CC_SESSIONS", d)
    return d


def test_candidates_exclude_conversation_owned_by_another_session(
        amux_server, project, sessions_dir):
    """AMUX-1730: two amux sessions collided onto one conversation (shared
    CC_DIR + a borrowed id) and each pane mirrored the other. Title matching
    can reproduce that collision, so a conversation another session's meta
    already claims is not a candidate."""
    add, work_dir = project
    now = time.time()
    add("aaaaaaaa-0000-0000-0000-000000000000", "Amux-gtm", now - 10)
    add("bbbbbbbb-0000-0000-0000-000000000000", "Amux-gtm", now)
    (sessions_dir / "neighbour.meta.json").write_text(json.dumps(
        {"cc_conversation_id": "bbbbbbbb-0000-0000-0000-000000000000"}))
    got = [p.stem[:8] for p in amux_server._cc_session_candidates(
        "Amux-gtm", work_dir, this_session="Amux-gtm")]
    assert got == ["aaaaaaaa"]


def test_candidates_keep_conversation_owned_by_this_session(
        amux_server, project, sessions_dir):
    """Our own claim is not a collision."""
    add, work_dir = project
    add("bbbbbbbb-0000-0000-0000-000000000000", "Amux-gtm", time.time())
    (sessions_dir / "Amux-gtm.meta.json").write_text(json.dumps(
        {"cc_conversation_id": "bbbbbbbb-0000-0000-0000-000000000000"}))
    got = [p.stem[:8] for p in amux_server._cc_session_candidates(
        "Amux-gtm", work_dir, this_session="Amux-gtm")]
    assert got == ["bbbbbbbb"]


def test_resume_strategy_skips_a_neighbours_conversation(
        amux_server, project, sessions_dir):
    add, work_dir = project
    add("bbbbbbbb-0000-0000-0000-000000000000", "Amux-gtm", time.time())
    (sessions_dir / "neighbour.meta.json").write_text(json.dumps(
        {"cc_conversation_id": "bbbbbbbb-0000-0000-0000-000000000000"}))
    flag, _cleared, _ = amux_server._resume_strategy({}, "Amux-gtm", work_dir)
    assert flag == "--name Amux-gtm"


def test_candidates_survive_unreadable_sessions_dir(
        amux_server, project, sessions_dir, monkeypatch):
    """The ownership guard must keep the function total — it may never raise
    into session startup."""
    add, work_dir = project
    add("bbbbbbbb-0000-0000-0000-000000000000", "Amux-gtm", time.time())

    def boom(*a, **k):
        raise OSError("permission denied")

    monkeypatch.setattr(amux_server, "_conversation_owned_by_other", boom)
    got = [p.stem[:8] for p in amux_server._cc_session_candidates(
        "Amux-gtm", work_dir, this_session="Amux-gtm")]
    assert got == ["bbbbbbbb"]


def test_candidates_tie_break_is_deterministic(amux_server, project):
    """Identical mtimes previously fell back to glob (filesystem) order, so the
    resume target could differ between runs on the same data."""
    add, work_dir = project
    same = time.time() - 60
    add("22222222-0000-0000-0000-000000000000", "S", same)
    add("11111111-0000-0000-0000-000000000000", "S", same)
    add("33333333-0000-0000-0000-000000000000", "S", same)
    got = [p.stem[:8] for p in amux_server._cc_session_candidates("S", work_dir)]
    assert got == ["11111111", "22222222", "33333333"]


# ── peek transcript resolution (the second title lookup) ─────────────────────

def test_peek_path_matches_title_recorded_on_line_two(
        amux_server, project, sessions_dir):
    """`_session_jsonl_path_uncached` had its own hand-rolled line-1-only title
    read — the exact bug this branch exists to fix, in a second place. With it
    broken the title branch never matched and peek fell through to the
    ambiguity guard, rendering live-only or a sibling's transcript."""
    add, work_dir = project
    now = time.time()
    add("aaaaaaaa-0000-0000-0000-000000000000", "mine", now - 100)
    add("bbbbbbbb-0000-0000-0000-000000000000", "neighbour", now)
    for n in ("mine", "neighbour"):
        (sessions_dir / f"{n}.env").write_text(f'CC_DIR="{work_dir}"\n')
    got = amux_server._session_jsonl_path_uncached("mine")
    assert got is not None and got.stem == "aaaaaaaa-0000-0000-0000-000000000000"
