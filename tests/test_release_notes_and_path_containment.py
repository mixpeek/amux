import importlib.util
import json
import sys
import types
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).parent.parent
SERVER_PATH = REPO_ROOT / "amux-server.py"


@pytest.fixture(scope="module")
def amux_server():
    # Windows Python cannot import POSIX-only terminal modules used by live
    # PTY routes. These tests do not exercise PTY behavior, so lightweight
    # stubs keep the production module importable.
    with pytest.MonkeyPatch.context() as mp:
        mp.setenv("AMUX_AUTH_TOKEN", "none")

        pty_stub = types.ModuleType("pty")
        pty_stub.fork = lambda: (_ for _ in ()).throw(NotImplementedError("pty unavailable"))
        fcntl_stub = types.ModuleType("fcntl")
        fcntl_stub.F_GETFL = 3
        fcntl_stub.F_SETFL = 4
        fcntl_stub.fcntl = lambda *args, **kwargs: 0
        fcntl_stub.ioctl = lambda *args, **kwargs: 0
        termios_stub = types.ModuleType("termios")
        termios_stub.TIOCSWINSZ = 0x5414

        mp.setitem(sys.modules, "pty", pty_stub)
        mp.setitem(sys.modules, "fcntl", fcntl_stub)
        mp.setitem(sys.modules, "termios", termios_stub)

        spec = importlib.util.spec_from_file_location("amux_server_release_notes", SERVER_PATH)
        assert spec is not None and spec.loader is not None
        mod = importlib.util.module_from_spec(spec)
        sys.modules["amux_server_release_notes"] = mod
        spec.loader.exec_module(mod)
        yield mod


def _handler_for_json(amux_server):
    handler = object.__new__(amux_server.CCHandler)
    calls = []

    def _json(data, status=200):
        calls.append((status, data))
        return data

    handler._check_auth = lambda method, path: True
    handler._json = _json
    handler._html = lambda html: html
    return handler, calls


def test_release_notes_route_uses_parsed_query_string(amux_server):
    handler, calls = _handler_for_json(amux_server)
    result = amux_server.CCHandler._route_inner(
        handler,
        "GET",
        "/api/release-notes",
        {"page": ["2"], "per_page": ["3"]},
    )

    all_notes = json.loads((REPO_ROOT / "docs" / "release-notes" / "notes.json").read_text())
    assert result["page"] == 2
    assert result["per_page"] == 3
    assert result["notes"] == all_notes[3:6]
    assert calls == [(200, result)]


def test_release_notes_route_rejects_invalid_pagination(amux_server):
    handler, calls = _handler_for_json(amux_server)
    result = amux_server.CCHandler._route_inner(
        handler,
        "GET",
        "/api/release-notes",
        {"page": ["not-a-number"]},
    )

    assert result == {"error": "invalid pagination"}
    assert calls == [(400, result)]


def test_path_is_within_rejects_prefix_sibling(tmp_path, amux_server):
    base = tmp_path / "work"
    sibling = tmp_path / "work2"
    base.mkdir()
    sibling.mkdir()

    assert amux_server._path_is_within(base / "file.txt", base)
    assert not amux_server._path_is_within(sibling / "secret.txt", base)
