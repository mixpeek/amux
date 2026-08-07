"""Unit tests for herdr agent_status -> status loop injection (mixpeek/amux#84).

Covers the pure mapping function, the batched `agent list` -> by-name parse,
and its tick cache. The override logic itself lives inline in list_sessions()
(DB/tmux dependent) so it is not unit-tested directly; the mapping function's
"" fallback behavior is the safety valve that keeps an unmapped/unknown herdr
status from clobbering the scrape result, and that is what's pinned here.

Imported from amux-server.py via importlib, same as test_herdr_backend.py, so
no drift is possible. Mocks subprocess so these run with or without herdr
installed.
"""

import importlib.util
import json
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


@pytest.fixture(autouse=True)
def _reset_status_cache(amux_server):
    # Each test gets a cold cache so max_age_s timing from a prior test can't leak in.
    amux_server._herdr_status_cache["ts"] = 0.0
    amux_server._herdr_status_cache["by_name"] = {}
    yield
    amux_server._herdr_status_cache["ts"] = 0.0
    amux_server._herdr_status_cache["by_name"] = {}


class _FakeProc:
    def __init__(self, stdout="", returncode=0):
        self.stdout = stdout
        self.returncode = returncode


# ── Mapping ──────────────────────────────────────────────────────────────────

def test_status_map_working_to_active(amux_server):
    assert amux_server._herdr_status_to_amux("working") == "active"


def test_status_map_blocked_to_waiting(amux_server):
    assert amux_server._herdr_status_to_amux("blocked") == "waiting"


def test_status_map_idle_to_idle(amux_server):
    assert amux_server._herdr_status_to_amux("idle") == "idle"


def test_status_map_done_to_idle(amux_server):
    assert amux_server._herdr_status_to_amux("done") == "idle"


def test_status_map_unknown_is_empty(amux_server):
    assert amux_server._herdr_status_to_amux("unknown") == ""


def test_status_map_none_is_empty(amux_server):
    assert amux_server._herdr_status_to_amux(None) == ""


def test_status_map_empty_string_is_empty(amux_server):
    assert amux_server._herdr_status_to_amux("") == ""


def test_status_map_uppercase_is_tolerated(amux_server):
    assert amux_server._herdr_status_to_amux("WORKING") == "active"


# ── Batched agent list parse ─────────────────────────────────────────────────

def _agent_list_payload():
    return {
        "id": "cli:agent:list",
        "result": {
            "type": "agent_list",
            "agents": [
                {"agent": "claude", "name": "lane-one", "agent_status": "working",
                 "pane_id": "w1:p1", "workspace_id": "w1"},
                {"agent": "claude", "name": "lane-two", "agent_status": "blocked",
                 "pane_id": "w2:p1", "workspace_id": "w2"},
                # auto-recognized agent with no persisted name -> must be dropped
                {"agent": "claude", "agent_status": "idle", "pane_id": "w3:p1",
                 "workspace_id": "w3"},
            ],
        },
    }


def test_agent_statuses_parses_by_name(amux_server, monkeypatch):
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc(json.dumps(_agent_list_payload()), 0))
    by_name = amux_server._herdr_agent_statuses()
    assert set(by_name.keys()) == {"lane-one", "lane-two"}
    assert by_name["lane-one"]["agent_status"] == "working"
    assert by_name["lane-two"]["agent_status"] == "blocked"


def test_agent_statuses_drops_nameless_entries(amux_server, monkeypatch):
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc(json.dumps(_agent_list_payload()), 0))
    by_name = amux_server._herdr_agent_statuses()
    assert all(v.get("pane_id") != "w3:p1" for v in by_name.values())


def test_agent_statuses_failed_read_is_empty_map(amux_server, monkeypatch):
    monkeypatch.setattr(amux_server.subprocess, "run",
                        lambda *a, **k: _FakeProc("not json", 0))
    assert amux_server._herdr_agent_statuses() == {}


# ── Tick cache ───────────────────────────────────────────────────────────────

def test_agent_statuses_cached_within_window(amux_server, monkeypatch):
    calls = {"n": 0}

    def fake_run(*a, **k):
        calls["n"] += 1
        return _FakeProc(json.dumps(_agent_list_payload()), 0)

    monkeypatch.setattr(amux_server.subprocess, "run", fake_run)
    amux_server._herdr_agent_statuses(max_age_s=5.0)
    amux_server._herdr_agent_statuses(max_age_s=5.0)
    assert calls["n"] == 1


def test_agent_statuses_failed_read_also_cached(amux_server, monkeypatch):
    calls = {"n": 0}

    def fake_run(*a, **k):
        calls["n"] += 1
        return _FakeProc("not json", 0)

    monkeypatch.setattr(amux_server.subprocess, "run", fake_run)
    amux_server._herdr_agent_statuses(max_age_s=5.0)
    amux_server._herdr_agent_statuses(max_age_s=5.0)
    assert calls["n"] == 1
