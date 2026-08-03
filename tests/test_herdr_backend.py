"""Unit tests for the herdr terminal backend (mixpeek/amux#79).

Covers backend resolution (CC_BACKEND > AMUX_BACKEND > tmux), the
amux-session-name -> herdr-agent-name mapping, and the herdr CLI wrapper's
JSON/error handling. Imported from amux-server.py via importlib, same as
test_detect_active_model.py, so no drift is possible.

Live herdr behavior (workspace/agent start, prompt, stop) is covered by the
manual E2E pass, not here — these tests mock subprocess so they run on any
machine, with or without herdr installed.
"""

import importlib.util
import json
import re
import sys
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


@pytest.fixture
def sessions_dir(amux_server, tmp_path, monkeypatch):
    d = tmp_path / "sessions"
    d.mkdir()
    monkeypatch.setattr(amux_server, "CC_SESSIONS", d)
    return d


# ── Backend resolution ───────────────────────────────────────────────────────

def test_backend_unset_uses_server_default(amux_server):
    assert amux_server._backend_of_cfg({}) == amux_server._AMUX_BACKEND
    assert amux_server._backend_of_cfg({"CC_BACKEND": ""}) == amux_server._AMUX_BACKEND


def test_backend_explicit_herdr(amux_server):
    assert amux_server._backend_of_cfg({"CC_BACKEND": "herdr"}) == "herdr"
    assert amux_server._backend_of_cfg({"CC_BACKEND": "HERDR"}) == "herdr"


def test_backend_explicit_tmux(amux_server):
    assert amux_server._backend_of_cfg({"CC_BACKEND": "tmux"}) == "tmux"


def test_backend_invalid_value_falls_back(amux_server):
    assert amux_server._backend_of_cfg({"CC_BACKEND": "zellij"}) == amux_server._AMUX_BACKEND


def test_session_backend_missing_env_file(amux_server, sessions_dir):
    assert amux_server._session_backend("ghost") == amux_server._AMUX_BACKEND


def test_session_backend_reads_env_file(amux_server, sessions_dir):
    (sessions_dir / "s1.env").write_text('CC_BACKEND="herdr"\n')
    assert amux_server._session_backend("s1") == "herdr"


# ── Agent name mapping ───────────────────────────────────────────────────────

def test_agent_name_simple_passthrough(amux_server, sessions_dir):
    (sessions_dir / "worker.env").write_text("")
    assert amux_server._herdr_agent_name("worker") == "worker"


def test_agent_name_lowercases_and_maps_dots(amux_server, sessions_dir):
    (sessions_dir / "My.Session.env").write_text("")
    assert amux_server._herdr_agent_name("My.Session") == "my-session"


def test_agent_name_collapses_runs_and_strips_edges(amux_server, sessions_dir):
    (sessions_dir / "a..b--c.env").write_text("")
    n = amux_server._herdr_agent_name("a..b--c")
    assert n == "a-b-c"


def test_agent_name_leading_digit_gets_alpha_prefix(amux_server, sessions_dir):
    (sessions_dir / "9lives.env").write_text("")
    n = amux_server._herdr_agent_name("9lives")
    assert n[0].isalpha()
    assert "9lives" in n


def test_agent_name_truncated_to_32(amux_server, sessions_dir):
    long = "a" * 60
    (sessions_dir / f"{long}.env").write_text("")
    n = amux_server._herdr_agent_name(long)
    assert len(n) <= 32


def test_agent_name_matches_herdr_rules(amux_server, sessions_dir):
    (sessions_dir / "shape.env").write_text("")
    n = amux_server._herdr_agent_name("shape")
    assert re.fullmatch(r"[a-z][a-z0-9_-]{0,31}", n)


def test_agent_name_persisted_and_stable(amux_server, sessions_dir):
    env = sessions_dir / "persist.env"
    env.write_text("")
    first = amux_server._herdr_agent_name("persist")
    content = env.read_text()
    assert f'CC_HERDR_AGENT="{first}"' in content
    # Second call reads the persisted value, not a recomputation.
    assert amux_server._herdr_agent_name("persist") == first


# ── CLI wrapper ──────────────────────────────────────────────────────────────

class _FakeProc:
    def __init__(self, stdout="", returncode=0):
        self.stdout = stdout
        self.returncode = returncode


def test_herdr_json_parses_reply(amux_server, monkeypatch):
    payload = {"id": "x", "result": {"ok": True}}
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc(json.dumps(payload), 0))
    assert amux_server._herdr_json(["agent", "list"]) == payload


def test_herdr_json_error_payload_is_none(amux_server, monkeypatch):
    payload = {"id": "x", "error": {"code": "boom"}}
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc(json.dumps(payload), 0))
    assert amux_server._herdr_json(["agent", "list"]) is None


def test_herdr_json_nonzero_exit_is_none(amux_server, monkeypatch):
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc("{}", 1))
    assert amux_server._herdr_json(["agent", "list"]) is None


def test_herdr_json_bad_json_is_none(amux_server, monkeypatch):
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc("not json", 0))
    assert amux_server._herdr_json(["agent", "list"]) is None


def test_herdr_json_timeout_is_none(amux_server, monkeypatch):
    def _raise(*a, **k):
        raise amux_server.subprocess.TimeoutExpired(cmd="herdr", timeout=1)
    monkeypatch.setattr(amux_server.subprocess, "run", _raise)
    assert amux_server._herdr_json(["agent", "list"]) is None


def test_herdr_command_targets_amux_session(amux_server, monkeypatch):
    seen = {}

    def fake_run(cmd, **kw):
        seen["cmd"] = cmd
        return _FakeProc("{}", 0)

    monkeypatch.setattr(amux_server.subprocess, "run", fake_run)
    amux_server._herdr(["agent", "list"])
    assert seen["cmd"][:3] == ["herdr", "--session", amux_server._HERDR_SESSION]
    assert seen["cmd"][3:] == ["agent", "list"]


def test_herdr_agent_get_extracts_agent_dict(amux_server, monkeypatch):
    agent = {"name": "w1", "agent_status": "idle", "pane_id": "w1:p1"}
    payload = {"id": "x", "result": {"type": "agent_info", "agent": agent}}
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc(json.dumps(payload), 0))
    monkeypatch.setattr(amux_server, "_herdr_agent_name", lambda n: "w1")
    assert amux_server._herdr_agent_get("whatever") == agent


def test_herdr_agent_get_missing_agent_is_none(amux_server, monkeypatch):
    payload = {"id": "x", "error": {"code": "agent_name_not_found"}}
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc(json.dumps(payload), 0))
    monkeypatch.setattr(amux_server, "_herdr_agent_name", lambda n: "gone")
    assert amux_server._herdr_agent_get("whatever") is None
