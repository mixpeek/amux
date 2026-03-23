"""Unit tests for notes API path traversal protection.

The old code used `".." in string` checks which are bypassable with
URL encoding (%2e%2e), double encoding, or symlinks. The new
_safe_note_path() uses Path.resolve() + relative_to() for canonical
path validation.
"""

import tempfile
from pathlib import Path


# ── Replicated path traversal logic ──────────────────────────────────────────

def _safe_note_path(note_rel: str, base: Path = None) -> Path | None:
    """Standalone replica of _safe_note_path from amux-server.py."""
    if not note_rel or note_rel.startswith("/"):
        return None
    candidate = (base / note_rel).resolve()
    try:
        candidate.relative_to(base.resolve())
    except ValueError:
        return None  # traversal detected
    return candidate


# ── Tests: valid note paths ──────────────────────────────────────────────────

def test_simple_note():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        result = _safe_note_path("hello.md", base)
        assert result is not None
        assert result == (base / "hello.md").resolve()


def test_nested_note():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        result = _safe_note_path("folder/note.md", base)
        assert result is not None
        assert str(result).startswith(str(base.resolve()))


def test_deeply_nested_note():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        result = _safe_note_path("a/b/c/deep.md", base)
        assert result is not None


# ── Tests: traversal attacks blocked ─────────────────────────────────────────

def test_basic_traversal_blocked():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("../../../etc/passwd", base) is None


def test_dot_dot_in_middle_blocked():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("notes/../../../etc/shadow", base) is None


def test_double_dot_dot_blocked():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("../../.ssh/id_rsa", base) is None


def test_traversal_to_parent_blocked():
    """Even traversing one level up is blocked."""
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("../sibling_file", base) is None


def test_traversal_with_trailing_slash():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("../", base) is None


# ── Tests: absolute paths blocked ────────────────────────────────────────────

def test_absolute_path_blocked():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("/etc/passwd", base) is None


def test_absolute_home_path_blocked():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("/home/user/.ssh/id_rsa", base) is None


# ── Tests: empty/invalid input ───────────────────────────────────────────────

def test_empty_string_blocked():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("", base) is None


def test_just_dots_blocked():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        assert _safe_note_path("..", base) is None


# ── Tests: symlink bypass ────────────────────────────────────────────────────

def test_symlink_escape_blocked():
    """Symlink inside notes dir pointing outside is blocked by resolve()."""
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir) / "notes"
        base.mkdir()
        # Create a symlink inside notes pointing to /tmp
        link = base / "escape"
        link.symlink_to("/tmp")
        # Trying to read through the symlink should be blocked
        result = _safe_note_path("escape/some_file", base)
        assert result is None, "Symlink escape should be blocked"


# ── Tests: trash directory ───────────────────────────────────────────────────

def test_trash_valid_path():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        trash = base / ".trash"
        trash.mkdir()
        result = _safe_note_path("deleted-note.md", trash)
        assert result is not None
        assert str(result).startswith(str(trash.resolve()))


def test_trash_traversal_blocked():
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        trash = base / ".trash"
        trash.mkdir()
        assert _safe_note_path("../../etc/passwd", trash) is None


def test_trash_to_notes_traversal_blocked():
    """Can't escape from .trash back into notes parent."""
    with tempfile.TemporaryDirectory() as tmpdir:
        base = Path(tmpdir)
        trash = base / ".trash"
        trash.mkdir()
        assert _safe_note_path("../secret.md", trash) is None


if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for t in tests:
        t()
        print(f"  PASS: {t.__name__}")
    print(f"\nAll {len(tests)} path traversal tests passed!")
