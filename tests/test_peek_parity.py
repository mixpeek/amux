"""Peek ↔ tmux parity checks — the automated half of docs/peek-parity.md.

Unlike the other test files (which replicate small validation snippets),
these tests load the REAL parser functions out of amux-server.py via AST —
replication is how parser fixes drift out of test coverage, and every one of
these parsers exists because a production incident proved the naive version
wrong. Fixtures below are taken from those actual incidents (2026-07-10).

Run:
    python3 -m pytest tests/test_peek_parity.py -q        # pure logic
    AMUX_LIVE=1 python3 -m pytest ... -q                  # + live read-only checks
"""

import ast
import os
import re
import time

# ── Load the real definitions from the single-file server ────────────────────

_SERVER = os.path.join(os.path.dirname(__file__), "..", "amux-server.py")
_WANTED = {
    "_pending_input", "_agent_panel", "_live_limit_region",
    "_LIMIT_ACTIVITY_RE", "_MODEL_CREDIT_LIMIT_RE", "_STRIP_ANSI",
}

def _load_real_functions():
    ns = {"re": re, "time": time}
    tree = ast.parse(open(_SERVER, encoding="utf-8").read())
    for node in tree.body:
        name = None
        if isinstance(node, ast.FunctionDef):
            name = node.name
        elif isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name):
            name = node.targets[0].id
        if name in _WANTED:
            mod = ast.Module(body=[node], type_ignores=[])
            exec(compile(mod, "amux-server.py", "exec"), ns)
    missing = _WANTED - set(ns)
    assert not missing, f"definitions not found in amux-server.py: {missing}"
    return ns

NS = _load_real_functions()
_pending_input = NS["_pending_input"]
_agent_panel = NS["_agent_panel"]
_live_limit_region = NS["_live_limit_region"]
_CRED = NS["_MODEL_CREDIT_LIMIT_RE"]


# ── P5: wrap-aware pending input (the 'random' ghost, 2026-07-10) ───────────

WRAPPED_GHOST = """──────────────────────────────
❯ [09:59 AM] use the amux calendar to create a reminder 2 weeks from now to reach out to the IRS, verifying they received my
request for fee removal — same time should be for the NYC workers comp fiasco
──────────────────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)"""

def test_pending_input_sees_wrapped_tail():
    # The message tail lives on the wrapped continuation line; the old
    # single-line read missed it and 'verified' a stuck message as sent.
    tail = "".join("the NYC workers comp fiasco".split())
    assert tail in _pending_input(WRAPPED_GHOST)

def test_pending_input_ghost_carries_amux_prefix():
    # Ghost-rescue only touches text amux itself injected ([H:MM AM] prefix).
    p = _pending_input(WRAPPED_GHOST)
    assert re.match(r"^\[\d{1,2}:\d{2}[AP]M\]", p)

def test_pending_input_submitted_is_clear():
    submitted = """❯ [09:32 AM] continue
⏺ Tick 456 sealed
──────────────────────────────
❯
──────────────────────────────
  ⏵⏵ bypass permissions on"""
    p = _pending_input(submitted)
    assert not p or "continue" not in p

def test_pending_input_no_prompt_returns_none():
    assert _pending_input("just some text\nno prompt here") is None


# ── P6: agents panel parsing (traps: Left/x; clustering + rig captures) ─────

PANEL_SELECT = """⏺ Waiting for the background sleep to complete. I will respond once it finishes.
  sleeper-A done
──────────────────────────────
❯\xa0
──────────────────────────────
  Enter to view · x to clear
  ◯ main
❯ ⏺ general-purpose  sleeper-A sleep 240                                        19s · ↓ 19.3k tokens"""

def test_agent_panel_select_mode_and_rows():
    select, rows = _agent_panel(PANEL_SELECT)
    assert select, "❯ cursor on a row is select mode regardless of hint wording"
    assert [r["label"] for r in rows] == ["main", "general-purpose sleeper-A sleep 240"]
    assert rows[1]["cursor"] and rows[1]["viewed"]

def test_agent_panel_transcript_dot_not_a_row():
    # The '⏺ Waiting for...' transcript line must NOT parse as a panel row —
    # rows are only the contiguous glyph block at the very bottom.
    _, rows = _agent_panel(PANEL_SELECT)
    assert all("Waiting for" not in r["label"] for r in rows)

def test_agent_panel_absent():
    select, rows = _agent_panel("""❯ some prompt
──────────────────────────────
  ⏵⏵ bypass permissions on · ← for agents · ↓ to manage""")
    assert rows == [] and not select

def test_agent_panel_non_select_rows():
    _, rows = _agent_panel("""  ⏵⏵ bypass permissions on · 2 shells
  ⏺ main
  ◯ general-purpose  sleeper-B sleep 240                                         8s · ↓ 19.0k tokens""")
    assert len(rows) == 2 and rows[0]["viewed"] and not any(r["cursor"] for r in rows)


# ── P4: limit banners scoped below the session's own output ─────────────────

SUBAGENT_ECHO = """⏺ Agent "verify" failed: You've reached your Fable 5 limit. Run /usage-credits to continue
  ⎿  You've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.
✻ Crunched for 1s
❯ [09:32 AM] continue
  Ran 2 shell commands
⏺ Tick 456 sealed — all good
──────────────────────────────
❯ continue
──────────────────────────────"""

REAL_BANNER = """⏺ Working on the thing
  Ran 2 shell commands
  ⎿  You've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.
──────────────────────────────
❯
──────────────────────────────
  ⏵⏵ bypass permissions on"""

def test_limit_subagent_echo_not_flagged():
    # A dead subagent's banner echo (mixpeek-orchestrator incident) must not
    # flag the parent: the session produced output BELOW it.
    assert not _CRED.search(_live_limit_region(SUBAGENT_ECHO))

def test_limit_real_banner_still_flagged():
    # A real limit is the last content before the input box.
    assert _CRED.search(_live_limit_region(REAL_BANNER))


# ── Live read-only checks (opt-in: AMUX_LIVE=1, needs the local server) ─────

def _live():
    return os.environ.get("AMUX_LIVE") == "1"

def test_live_no_contradictory_badges():
    if not _live():
        return
    import json, ssl, urllib.request
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    url = os.environ.get("AMUX_URL", "https://localhost:8822")
    sessions = json.load(urllib.request.urlopen(url + "/api/sessions", context=ctx))
    bad = [s["name"] for s in sessions
           if s.get("status") == "active"
           and (s.get("credit_limited") or s.get("rate_limit_banner"))]
    assert not bad, f"sessions both WORKING and limited: {bad}"


if __name__ == "__main__":
    fns = [(n, f) for n, f in sorted(globals().items())
           if n.startswith("test_") and callable(f)]
    for n, f in fns:
        f()
        print(f"ok  {n}")
    print(f"{len(fns)} checks passed")
