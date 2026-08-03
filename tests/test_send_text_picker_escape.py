"""Regression test: a deliberate human send at a live picker must not send a
pre-emptive Escape.

Bug (amux board AI-3 / user report, 2026-08-02..03): the Terminal tab's
composer can't answer an AskUserQuestion / tool-approval / confirm-dialog
picker. `send_text()`'s cleanup step ("close any picker left open by a
previous attempt") sends Escape unconditionally whenever the session is not
mid-turn -- including when `_waiting` is True, i.e. the pane IS a genuine,
CURRENT live selector, not a stale leftover. That Escape REJECTS the pending
tool ("[Request interrupted by user]" -- the same contention documented
in-code for the gtm-engine incident, 2026-07-15) before the typed answer ever
reaches it. The quick-action chips (arrow keys via send_keys) never hit this
path and work correctly today, which is the tell.

Loads the REAL send_text out of amux-server.py via AST (same technique as
test_steering_delivery.py) and stubs every I/O boundary (tmux, disk, locks)
so the escape-guard LOGIC runs unmodified against a fake clock/pane.
"""
import ast, os, re, threading

SERVER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "amux-server.py")

# ── recorded tmux calls ───────────────────────────────────────────────────────
CALLS = []

class _FakeSubprocess:
    class CalledProcessError(Exception):
        pass
    class TimeoutExpired(Exception):
        pass

    @staticmethod
    def run(args, **kwargs):
        CALLS.append(list(args))
        class _R:
            returncode = 0
            stdout = b""
            stderr = b""
        return _R()


# ── controllable fake state ──────────────────────────────────────────────────
STATUS = "waiting"          # what _detect_claude_status should report
PANE_TEXT = "some pane text without the interrupt hint"   # raw capture text


def _fake_tmux_capture(name, lines=500):
    return PANE_TEXT


def _fake_detect_status(raw):
    return STATUS


ns = {
    "subprocess": _FakeSubprocess,
    "threading": threading,
    "time": __import__("time"),
    "re": re,
    "tempfile": __import__("tempfile"),
    "os": os,
    # I/O boundary stubs — none of these paths are under test here.
    "_session_iterm2_id": lambda name: None,
    "_iterm2_send": lambda *a, **k: (True, "ok"),
    "_session_auto_actions": {},
    "_load_meta": lambda name: {"last_started": 0},
    "tmux_capture": _fake_tmux_capture,
    "_at_resume_picker": lambda out: False,
    "_at_shell_prompt": lambda out: False,
    "is_running": lambda name: True,
    "_get_send_lock": lambda name: threading.Lock(),
    "tmux_name": lambda name: name,
    "tmux_target": lambda name: name,
    "_detect_claude_status": _fake_detect_status,
    "_steer_enqueue": lambda name, text, guard="": "queued",
    "_verify_submitted": lambda name, target, text, esc_at=0.0, sent_at=0.0: True,
    "_send_after_ready": lambda *a, **k: None,
    "_auto_waking": set(),
    "CC_SESSIONS": None,
    "start_session": lambda name: (True, "ok"),
}

tree = ast.parse(open(SERVER, encoding="utf-8").read())
_want_assign = {"_AT_PICKER_RE"}
_want_func = {"send_text"}
for node in tree.body:
    if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name) \
            and node.targets[0].id in _want_assign:
        exec(compile(ast.Module(body=[node], type_ignores=[]), SERVER, "exec"), ns)
    if isinstance(node, ast.FunctionDef) and node.name in _want_func:
        exec(compile(ast.Module(body=[node], type_ignores=[]), SERVER, "exec"), ns)

send_text = ns["send_text"]


def _escape_calls():
    return [c for c in CALLS if len(c) >= 5 and c[1] == "send-keys" and c[-1] == "Escape"]


def _text_calls():
    return [c for c in CALLS if len(c) >= 5 and c[1] == "send-keys" and "-l" in c]


# ── T1: live picker (waiting) — a deliberate human send must NOT escape ───────
CALLS.clear()
STATUS = "waiting"
PANE_TEXT = "❯ 1. Yes\n  2. No\n(Enter to select)"
ok, msg = send_text("s1", "my answer")
assert ok, f"T1: send_text failed: {msg}"
assert _escape_calls() == [], (
    f"T1: BUG — Escape sent while at a live selector (_waiting=True); "
    f"this rejects the pending tool call instead of answering it. Calls: {CALLS}"
)
assert _text_calls(), f"T1: the answer text was never typed at all: {CALLS}"
print("T1 ok — live selector: no pre-emptive Escape, answer typed directly")

# ── T2: plain idle prompt — cleanup Escape should still fire as before ────────
CALLS.clear()
STATUS = "idle"
PANE_TEXT = "some idle prompt, no selector here"
ok, msg = send_text("s2", "a normal prompt")
assert ok, f"T2: send_text failed: {msg}"
assert _escape_calls(), (
    f"T2: cleanup Escape regressed for the genuine stale-popup case (idle, not "
    f"at a selector) — this Escape is needed there to close a leftover "
    f"autocomplete popup so C-u actually reaches the input. Calls: {CALLS}"
)
print("T2 ok — plain idle prompt: cleanup Escape still fires (unchanged)")

print("\nAll send_text picker-escape tests passed!")
