"""Unit tests for bearer token authentication logic.

These tests replicate the auth logic from amux-server.py in isolation
because importing the full server triggers initialization side effects
(mkcert, tmux detection, directory creation).
"""

import os
import secrets
import tempfile
from pathlib import Path
from urllib.parse import parse_qs, urlparse


# ── Replicated auth logic ────────────────────────────────────────────────────

_PUBLIC_PATHS = frozenset({
    "/", "/manifest.json", "/sw.js", "/icon.svg", "/icon.png",
    "/icon-192.png", "/icon-512.png", "/ca", "/release-notes",
    "/api/release-notes",
})
_PUBLIC_PREFIXES = ("/s/", "/api/share/", "/invite/")
_AUTH_REQUIRED_PATHS = frozenset({"/ca"})


def _check_auth(auth_token, method, path, auth_header="", full_url=None):
    """Standalone replica of CCHandler._check_auth."""
    if not auth_token:
        return True
    if path in _PUBLIC_PATHS or any(path.startswith(p) for p in _PUBLIC_PREFIXES):
        return True
    if (method == "GET" and not path.startswith("/api/")
            and not path.startswith("/proxy/") and path not in _AUTH_REQUIRED_PATHS):
        return True
    if auth_header == f"Bearer {auth_token}":
        return True
    url = full_url or path
    token_qs = parse_qs(urlparse(url).query).get("_token", [""])[0]
    if token_qs == auth_token:
        return True
    return False


def _load_or_create_auth_token(token_file, env_token=""):
    """Standalone replica of _load_or_create_auth_token."""
    if env_token:
        return "" if env_token.lower() == "none" else env_token
    if token_file.exists():
        token = token_file.read_text().strip()
        if token:
            return token
    token = secrets.token_urlsafe(32)
    token_file.parent.mkdir(parents=True, exist_ok=True)
    token_file.write_text(token + "\n")
    os.chmod(str(token_file), 0o600)
    return token


# ── Tests ─────────────────────────────────────────────────────────────────────

TOKEN = "test-secret-token-abc123"


def test_api_rejects_without_token():
    assert _check_auth(TOKEN, "GET", "/api/sessions") is False
    assert _check_auth(TOKEN, "POST", "/api/board/items") is False
    assert _check_auth(TOKEN, "DELETE", "/api/notes/test.md") is False


def test_proxy_rejects_without_token():
    assert _check_auth(TOKEN, "GET", "/proxy/3000/") is False
    assert _check_auth(TOKEN, "POST", "/proxy/8080/api") is False


def test_correct_token_accepted():
    assert _check_auth(TOKEN, "GET", "/api/sessions", f"Bearer {TOKEN}") is True
    assert _check_auth(TOKEN, "POST", "/api/board/items", f"Bearer {TOKEN}") is True
    assert _check_auth(TOKEN, "GET", "/proxy/3000/", f"Bearer {TOKEN}") is True


def test_wrong_token_rejected():
    assert _check_auth(TOKEN, "GET", "/api/sessions", "Bearer wrong") is False
    assert _check_auth(TOKEN, "GET", "/api/sessions", "Bearer ") is False
    assert _check_auth(TOKEN, "GET", "/api/sessions", TOKEN) is False  # missing "Bearer "


def test_public_paths_bypass_auth():
    for path in ["/", "/manifest.json", "/sw.js", "/icon.svg", "/icon.png",
                 "/release-notes", "/api/release-notes"]:
        assert _check_auth(TOKEN, "GET", path) is True, f"{path} should be public"


def test_share_links_bypass_auth():
    assert _check_auth(TOKEN, "GET", "/s/abc123") is True
    assert _check_auth(TOKEN, "GET", "/api/share/xyz") is True
    assert _check_auth(TOKEN, "GET", "/invite/token123") is True


def test_static_asset_gets_bypass_auth():
    assert _check_auth(TOKEN, "GET", "/styles.css") is True
    assert _check_auth(TOKEN, "GET", "/app.js") is True
    assert _check_auth(TOKEN, "GET", "/favicon.ico") is True


def test_static_asset_post_requires_auth():
    assert _check_auth(TOKEN, "POST", "/styles.css") is False


def test_sse_query_param_fallback():
    url = f"/api/sessions/stream?_token={TOKEN}"
    assert _check_auth(TOKEN, "GET", "/api/sessions/stream", "", url) is True


def test_sse_wrong_query_param_rejected():
    url = "/api/sessions/stream?_token=wrong"
    assert _check_auth(TOKEN, "GET", "/api/sessions/stream", "", url) is False


def test_disabled_auth_allows_everything():
    assert _check_auth("", "GET", "/api/sessions") is True
    assert _check_auth("", "POST", "/api/board/items") is True
    assert _check_auth("", "GET", "/proxy/3000/") is True


def test_token_file_creation():
    with tempfile.TemporaryDirectory() as tmpdir:
        token_file = Path(tmpdir) / "auth_token"
        token = _load_or_create_auth_token(token_file)
        assert len(token) > 20
        assert token_file.exists()
        assert token_file.read_text().strip() == token
        perms = oct(token_file.stat().st_mode)[-3:]
        assert perms == "600", f"Expected 600, got {perms}"


def test_token_file_reuse():
    with tempfile.TemporaryDirectory() as tmpdir:
        token_file = Path(tmpdir) / "auth_token"
        token1 = _load_or_create_auth_token(token_file)
        token2 = _load_or_create_auth_token(token_file)
        assert token1 == token2


def test_env_token_override():
    with tempfile.TemporaryDirectory() as tmpdir:
        token_file = Path(tmpdir) / "auth_token"
        token = _load_or_create_auth_token(token_file, env_token="custom-token")
        assert token == "custom-token"
        assert not token_file.exists()


def test_env_token_none_disables():
    with tempfile.TemporaryDirectory() as tmpdir:
        token_file = Path(tmpdir) / "auth_token"
        token = _load_or_create_auth_token(token_file, env_token="none")
        assert token == ""


if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for t in tests:
        t()
        print(f"  PASS: {t.__name__}")
    print(f"\nAll {len(tests)} auth tests passed!")
