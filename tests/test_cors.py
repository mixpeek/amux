"""Unit tests for CORS origin validation.

These tests replicate the CORS origin validation logic from amux-server.py
in isolation. The actual _cors() method sends HTTP headers — these tests
verify the allow/deny decision logic.
"""

from urllib.parse import urlparse


# ── Replicated CORS logic ────────────────────────────────────────────────────

def _is_origin_allowed(origin: str, lan_ip: str = "192.168.1.100") -> bool:
    """Standalone replica of the origin check in CCHandler._cors."""
    if not origin:
        return False
    parsed = urlparse(origin)
    host = parsed.hostname or ""
    return (
        host in ("localhost", "127.0.0.1", "0.0.0.0")
        or host == lan_ip
        or host.endswith(".ts.net")
    )


# ── Tests: allowed origins ───────────────────────────────────────────────────

def test_localhost_allowed():
    assert _is_origin_allowed("http://localhost:8822") is True
    assert _is_origin_allowed("https://localhost:8822") is True
    assert _is_origin_allowed("http://localhost") is True


def test_loopback_ip_allowed():
    assert _is_origin_allowed("http://127.0.0.1:8822") is True
    assert _is_origin_allowed("https://127.0.0.1:8822") is True


def test_lan_ip_allowed():
    assert _is_origin_allowed("http://192.168.1.100:8822", "192.168.1.100") is True
    assert _is_origin_allowed("http://10.0.0.5:8822", "10.0.0.5") is True


def test_tailscale_hostname_allowed():
    assert _is_origin_allowed("https://nuc.tail37bf06.ts.net:8822") is True
    assert _is_origin_allowed("https://myhost.ts.net") is True
    assert _is_origin_allowed("http://anything.ts.net:3000") is True


# ── Tests: blocked origins ───────────────────────────────────────────────────

def test_wildcard_not_used():
    """The old code used Access-Control-Allow-Origin: * — verify it's gone."""
    # Any random external origin must be rejected
    assert _is_origin_allowed("https://evil.com") is False
    assert _is_origin_allowed("https://attacker.example.com") is False


def test_external_domains_blocked():
    assert _is_origin_allowed("https://google.com") is False
    assert _is_origin_allowed("https://github.com") is False
    assert _is_origin_allowed("http://malicious-site.com:8822") is False


def test_similar_looking_domains_blocked():
    """Domains that look like they could be local but aren't."""
    assert _is_origin_allowed("http://localhost.evil.com") is False
    assert _is_origin_allowed("http://fake-ts.net") is False
    assert _is_origin_allowed("http://not-a.ts.net.evil.com") is False


def test_different_lan_ip_blocked():
    """Only the server's own LAN IP is allowed, not arbitrary LAN IPs."""
    assert _is_origin_allowed("http://192.168.1.200:8822", "192.168.1.100") is False
    assert _is_origin_allowed("http://10.0.0.99:8822", "10.0.0.5") is False


def test_empty_origin_blocked():
    assert _is_origin_allowed("") is False


def test_no_origin_blocked():
    """Requests without Origin header (non-CORS) don't get ACAO header."""
    assert _is_origin_allowed("") is False


# ── Tests: edge cases ────────────────────────────────────────────────────────

def test_ts_net_suffix_only():
    """Only *.ts.net is allowed, not ts.net itself or substrings."""
    assert _is_origin_allowed("http://ts.net") is False  # no subdomain
    assert _is_origin_allowed("http://evil-ts.net") is False  # different TLD


def test_different_ports_same_host_allowed():
    """Port doesn't affect the origin check — only hostname matters."""
    assert _is_origin_allowed("http://localhost:3000") is True
    assert _is_origin_allowed("http://localhost:9999") is True


if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for t in tests:
        t()
        print(f"  PASS: {t.__name__}")
    print(f"\nAll {len(tests)} CORS tests passed!")
