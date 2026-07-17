"""Deterministic test of steering delivery semantics (settle window, waiting-hold,
subagent-hold, one-per-boundary FIFO, guard folding, commit-nudge revalidation).
Loads the REAL _steer_try_deliver from amux-server.py via AST — no server needed.

Loads the REAL _steer_try_deliver out of amux-server.py via AST and drives it
with a fake monotonic clock + stubbed send/db, proving:
  - a lone idle frame does NOT deliver (only starts the settle timer)
  - continuous idle for >= _STEER_SETTLE_SECS DOES deliver, exactly once
  - an 'active' frame mid-settle RESETS the window (no premature delivery)
  - 'waiting' settles the same way as 'idle'
  - an in-flight background subagent HOLDS steering even at an idle main loop
"""
import ast, os, re, threading

SERVER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                      "amux-server.py")

# ── fake clock ───────────────────────────────────────────────────────────────
class FakeClock:
    def __init__(self): self.t = 1000.0
    def monotonic(self): return self.t
    def time(self): return self.t
CLOCK = FakeClock()

# ── stubs ────────────────────────────────────────────────────────────────────
SENT = []
def send_text(name, text, _from_steering=False):
    SENT.append((CLOCK.t, name, text))
    return True, None

class _DB:
    def execute(self, *a, **k): return self
    def commit(self): pass
def get_db(): return _DB()
def _push_alert(*a, **k): pass
def _steer_record_history(*a, **k): pass
def _emit_event(*a, **k): pass
# Commit-guard revalidation stubs (AMUX-1737)
DIRTY = ["amux-server.py"]           # test flips this to simulate a commit
def _session_work_dir(n): return "/tmp/wd"
def _session_dirty_files(n, w): return DIRTY
_commit_guard_nudged = {}

steering_queue = {}
steer_settle_since = {}
steering_lock = threading.Lock()

# ── load the real function + constant via AST ────────────────────────────────
ns = {
    "time": CLOCK,
    "re": re,
    "send_text": send_text,
    "get_db": get_db,
    "_push_alert": _push_alert,
    "_steer_record_history": _steer_record_history,
    "_emit_event": _emit_event,
    "_steering_queue": steering_queue,
    "_steer_settle_since": steer_settle_since,
    "_steering_lock": steering_lock,
    "_session_work_dir": _session_work_dir,
    "_session_dirty_files": lambda n, w: DIRTY,
    "_commit_guard_nudged": _commit_guard_nudged,
    "slog": lambda *a, **k: None,
    "os": os,
}
tree = ast.parse(open(SERVER, encoding="utf-8").read())
_want_assign = {"_STEER_SETTLE_SECS", "_STRIP_ANSI"}
_want_func = {"_steer_try_deliver", "_has_running_subagent"}
for node in tree.body:
    if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name) \
            and node.targets[0].id in _want_assign:
        exec(compile(ast.Module(body=[node], type_ignores=[]), SERVER, "exec"), ns)
    if isinstance(node, ast.FunctionDef) and node.name in _want_func:
        exec(compile(ast.Module(body=[node], type_ignores=[]), SERVER, "exec"), ns)

deliver = ns["_steer_try_deliver"]
SETTLE = ns["_STEER_SETTLE_SECS"]
print(f"_STEER_SETTLE_SECS = {SETTLE}")

def enqueue(name, text):
    steering_queue.setdefault(name, []).append({"id": f"m-{len(SENT)}-{text}", "text": text})

def reset():
    SENT.clear(); steering_queue.clear(); steer_settle_since.clear()

# ── T1: lone idle frame does not deliver; sustained idle delivers once ────────
reset()
enqueue("s1", "continue")
CLOCK.t = 1000.0; deliver("s1", "idle")           # t=0  → start timer, no send
assert SENT == [], f"T1: delivered too early: {SENT}"
CLOCK.t = 1004.0; deliver("s1", "idle")           # t=4  → 4s < settle, no send
assert SENT == [], f"T1: delivered at 4s (< {SETTLE}): {SENT}"
CLOCK.t = 1008.0; deliver("s1", "idle")           # t=8  → 8s >= settle, send
assert len(SENT) == 1, f"T1: expected 1 send by 8s, got {SENT}"
assert SENT[0][2] == "continue"
CLOCK.t = 1012.0; deliver("s1", "idle")           # queue now empty → no double
assert len(SENT) == 1, f"T1: double-delivered: {SENT}"
print("T1 ok  — lone idle frame held; sustained idle delivered exactly once")

# ── T2: active frame mid-settle resets the window ────────────────────────────
reset()
enqueue("s2", "do the thing")
CLOCK.t = 2000.0; deliver("s2", "idle")           # start timer
CLOCK.t = 2004.0; deliver("s2", "active")         # session busy again → reset
assert SENT == [] and "s2" not in steer_settle_since, "T2: not reset on active"
CLOCK.t = 2005.0; deliver("s2", "idle")           # timer restarts here
CLOCK.t = 2009.0; deliver("s2", "idle")           # only 4s since restart → hold
assert SENT == [], f"T2: delivered only 4s after reset: {SENT}"
CLOCK.t = 2012.0; deliver("s2", "idle")           # 7s since restart → deliver
assert len(SENT) == 1, f"T2: expected 1 send after full settle post-reset, got {SENT}"
print("T2 ok  — active frame mid-settle reset the window; no premature send")

# ── T3: unknown ('') frame also resets / never delivers on its own ───────────
reset()
enqueue("s3", "x")
CLOCK.t = 3000.0; deliver("s3", "idle")
CLOCK.t = 3004.0; deliver("s3", "")               # torn/unknown frame → reset
assert SENT == [] and "s3" not in steer_settle_since, "T3: '' did not reset"
print("T3 ok  — unknown frame resets the settle window")

# ── T4: 'waiting' (a live selector) now HOLDS — never delivered into ──────────
# Injecting into a tool-approval/AskUserQuestion selector would reject the
# pending tool ("[Request interrupted by user]"), so steering must wait for a
# genuinely idle prompt (gtm-engine contention fix, 2026-07-15).
reset()
enqueue("s4", "answer")
CLOCK.t = 4000.0; deliver("s4", "waiting")
assert SENT == [], "T4: waiting delivered (must hold at a selector)"
CLOCK.t = 4008.0; deliver("s4", "waiting")
assert SENT == [], f"T4: waiting delivered after settle (must hold): {SENT}"
assert "s4" not in steer_settle_since, "T4: 'waiting' must not start the settle timer"
# Once it returns to a genuinely idle prompt, it settles + delivers as before.
CLOCK.t = 5000.0; deliver("s4", "idle")
CLOCK.t = 5008.0; deliver("s4", "idle")
assert len(SENT) == 1 and SENT[0][2] == "answer", f"T4: idle did not deliver after selector cleared: {SENT}"
print("T4 ok  — 'waiting' selector holds steering; delivers only once idle")

# ── T5: multiple queued messages deliver ONE PER TURN BOUNDARY, oldest first ──
reset()
enqueue("s5", "first"); enqueue("s5", "second")
CLOCK.t = 5000.0; deliver("s5", "idle")
CLOCK.t = 5008.0; deliver("s5", "idle")           # settled → deliver ONLY the oldest
assert len(SENT) == 1, f"T5: expected one send, got {SENT}"
body = SENT[0][2]
assert body.startswith("first"), f"T5: wrong message first: {body[:40]}"
assert "second" not in body.split("[amux:")[0], "T5: second message leaked into first turn"
assert "1 more queued message" in body, f"T5: remaining-work trailer missing: {body}"
assert len(steering_queue.get("s5", [])) == 1, "T5: second message should remain queued"
# next idle stretch → the second delivers alone, no trailer
CLOCK.t = 5010.0; deliver("s5", "idle")           # busy-settle restart
CLOCK.t = 5018.0; deliver("s5", "idle")
assert len(SENT) == 2, f"T5: second not delivered at next boundary: {SENT}"
assert SENT[1][2] == "second", f"T5: second body wrong: {SENT[1][2]!r}"
assert not steering_queue.get("s5"), "T5: queue not drained after both turns"
print("T5 ok  — queue delivers one message per boundary, oldest first, with trailer")

# ── T7: dedup-on-enqueue replaces identical queued text ──────────────────────
# (exercised against the real _steer_enqueue)
print("T7 covered separately via _steer_enqueue live test")

# ── T6: in-flight subagent holds steering even at an idle main loop ───────────
SUBAGENT_RUNNING = (
    "⏺ main\n"
    "◯ general-purpose  Make board the source of truth   14m 59s "
    "· ↓ 136.0k tokens\n"
)
DONE_NO_SUBAGENT = "⏺ Brewed for 2m 3s\n❯\n"
reset()
enqueue("s6", "next task")
# Main loop reads idle, but a subagent is still running → treated like active.
CLOCK.t = 6000.0; deliver("s6", "idle", SUBAGENT_RUNNING)
assert SENT == [] and "s6" not in steer_settle_since, "T6: subagent didn't hold"
CLOCK.t = 6008.0; deliver("s6", "idle", SUBAGENT_RUNNING)   # 8s idle main loop
assert SENT == [], f"T6: delivered while subagent still running: {SENT}"
# Subagent finishes — now the frame has no ◯ row; settle starts fresh here.
CLOCK.t = 6009.0; deliver("s6", "idle", DONE_NO_SUBAGENT)   # start timer
assert SENT == [], "T6: delivered instantly once subagent gone"
CLOCK.t = 6016.0; deliver("s6", "idle", DONE_NO_SUBAGENT)   # settled → deliver
assert len(SENT) == 1 and SENT[0][2] == "next task", f"T6: {SENT}"
print("T6 ok  — running subagent held steering; delivered only after it finished + settled")

# ── T8: commit-guard nudge DROPPED when the tree is clean at delivery (AMUX-1737)
reset()
DIRTY = []   # session committed → tree clean by delivery time
steering_queue["s8"] = [{"id": "g8", "text": "you have uncommitted changes", "guard": "commit"}]
CLOCK.t = 8000.0; deliver("s8", "idle")            # start settle
CLOCK.t = 8008.0; deliver("s8", "idle")            # settled — but clean → DROP, don't deliver
assert SENT == [], f"T8: stale commit nudge delivered against a clean tree: {SENT}"
assert not steering_queue.get("s8"), f"T8: stale guard entry not dropped: {steering_queue.get('s8')}"
print("T8 ok  — commit-guard nudge dropped when tree clean at delivery (no stale nudge)")

# ── T9: commit-guard nudge STILL delivers when the tree is genuinely dirty ─────
reset()
DIRTY = ["amux-server.py"]
steering_queue["s9"] = [{"id": "g9", "text": "commit nudge 9", "guard": "commit"}]
CLOCK.t = 9000.0; deliver("s9", "idle")
CLOCK.t = 9008.0; deliver("s9", "idle")            # settled + still dirty → deliver
assert len(SENT) == 1 and SENT[0][2] == "commit nudge 9", f"T9: dirty-tree nudge not delivered: {SENT}"
print("T9 ok  — commit-guard nudge delivered when tree genuinely dirty")

# ── T10: guard nudge FOLDS into a real delivery instead of its own turn ──────
reset()
DIRTY = ["some-file.py"]   # tree genuinely dirty → nudge stays valid
steering_queue["s10"] = [
    {"id": "g10", "text": "You went idle with 1 uncommitted change(s) under your working directory (/tmp/wd):\n  x.py", "guard": "commit"},
    {"id": "r10", "text": "real task ten"},
]
CLOCK.t = 10000.0; deliver("s10", "idle")
CLOCK.t = 10008.0; deliver("s10", "idle")
assert len(SENT) == 1, f"T10: expected one combined send, got {SENT}"
b = SENT[0][2]
assert b.startswith("real task ten"), f"T10: real task must lead: {b[:50]}"
assert "[amux reminder] You went idle with 1 uncommitted" in b, f"T10: nudge not folded: {b}"
assert not steering_queue.get("s10"), f"T10: queue should be fully drained: {steering_queue.get('s10')}"
print("T10 ok — guard nudge folded as trailer onto the real delivery; no extra turn")

# ── T11: all-guards queue still delivers standalone (revalidated) ─────────────
reset()
DIRTY = ["some-file.py"]
steering_queue["s11"] = [{"id": "g11", "text": "You went idle with 2 uncommitted change(s) under your working directory (/tmp/wd):", "guard": "commit"}]
CLOCK.t = 11000.0; deliver("s11", "idle")
CLOCK.t = 11008.0; deliver("s11", "idle")
assert len(SENT) == 1 and SENT[0][2].startswith("You went idle"), f"T11: {SENT}"
print("T11 ok — lone guard nudge still delivers standalone when queue has nothing else")

print("\nALL STEER-SETTLE CHECKS PASSED")


def test_steering_contracts():
    """The scenario asserts above execute at import time (module top level) and
    raise on any regression — reaching this test means every contract held:
    settle window, waiting-hold, subagent-hold, one-per-boundary FIFO with
    remaining-count trailer, guard folding, and commit-nudge revalidation."""
    assert True
