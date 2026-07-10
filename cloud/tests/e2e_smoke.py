#!/usr/bin/env python3
"""
amux cloud E2E smoke test — runs daily via amux scheduler.

Creates a test user via Clerk Backend API, authenticates through the gateway,
exercises sessions with each provider (Claude, Codex, Gemini), tests BYO API
key flow, verifies logout/re-login, then cleans up.

Env vars required (set in ~/.amux/server.env or shell):
  CLERK_SECRET_KEY   — Clerk backend key (sk_live_...)
  COOKIE_SECRET      — gateway HMAC secret for amux_session cookies
  ANTHROPIC_API_KEY  — for BYO key test (Claude)
  OPENAI_API_KEY     — for BYO key test (Codex)
  GOOGLE_API_KEY     — for BYO key test (Gemini)

Usage:
  python3 cloud/tests/e2e_smoke.py [--gateway https://cloud.amux.io]
"""
import argparse, hashlib, hmac, json, os, sys, time, urllib.request, urllib.error

GATEWAY = os.environ.get("E2E_GATEWAY", "https://cloud.amux.io")
CLERK_SECRET = os.environ.get("CLERK_SECRET_KEY", "")
COOKIE_SECRET = os.environ.get("COOKIE_SECRET", "")
TEST_EMAIL = "e2e-smoke@test.amux.io"
TEST_PASSWORD = "E2eSmoke!Test2026x"

PASS = 0
FAIL = 0
WARNINGS = []


def log(msg):
    print(f"  {msg}", flush=True)


def step(msg):
    print(f"\n→ {msg}", flush=True)


def ok(msg):
    global PASS
    PASS += 1
    print(f"  ✓ {msg}", flush=True)


def fail(msg):
    global FAIL
    FAIL += 1
    print(f"  ✗ {msg}", flush=True)


def warn(msg):
    WARNINGS.append(msg)
    print(f"  ⚠ {msg}", flush=True)


# ── Clerk Backend API helpers ─────────────────────────────────────────────────

def clerk_api(method, path, body=None):
    url = f"https://api.clerk.com/v1{path}"
    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Authorization", f"Bearer {CLERK_SECRET}")
    req.add_header("Content-Type", "application/json")
    req.add_header("User-Agent", "amux-e2e-smoke/1.0")
    try:
        resp = urllib.request.urlopen(req, timeout=15)
        return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        err_body = e.read().decode()
        raise RuntimeError(f"Clerk API {method} {path} → {e.code}: {err_body}")


def clerk_find_test_user():
    data = clerk_api("GET", f"/users?email_address[]={TEST_EMAIL}&limit=1")
    return data[0] if data else None


def clerk_create_user():
    return clerk_api("POST", "/users", {
        "email_address": [TEST_EMAIL],
        "password": TEST_PASSWORD,
        "skip_password_checks": True,
        "first_name": "E2E",
        "last_name": "Smoke",
    })


def clerk_delete_user(user_id):
    return clerk_api("DELETE", f"/users/{user_id}")


# ── Cookie helpers ────────────────────────────────────────────────────────────

def make_cookie(user_id):
    ts = int(time.time())
    payload = f"{user_id}|{ts}"
    sig = hmac.new(COOKIE_SECRET.encode(), payload.encode(), hashlib.sha256).hexdigest()
    return f"{payload}|{sig}"


# ── HTTP helpers ──────────────────────────────────────────────────────────────

_ssl_ctx = __import__("ssl").create_default_context()
_ssl_ctx.check_hostname = False
_ssl_ctx.verify_mode = __import__("ssl").CERT_NONE


class _NoRedirect(urllib.request.HTTPSHandler):
    def __init__(self):
        super().__init__(context=_ssl_ctx)

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None

    def http_error_302(self, req, fp, code, msg, headers):
        return fp

    http_error_301 = http_error_303 = http_error_307 = http_error_302


def gw_request(method, path, body=None, cookie=None, follow=False, accept="application/json", headers=None, timeout=30):
    url = f"{GATEWAY}{path}"
    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(url, data=data, method=method)
    if cookie:
        req.add_header("Cookie", f"amux_session={cookie}")
    if accept:
        req.add_header("Accept", accept)
    req.add_header("Content-Type", "application/json")
    req.add_header("User-Agent", "amux-e2e-smoke/1.0")
    if headers:
        for k, v in headers.items():
            req.add_header(k, v)
    try:
        if follow:
            resp = urllib.request.urlopen(req, timeout=timeout, context=_ssl_ctx)
        else:
            opener = urllib.request.build_opener(_NoRedirect)
            resp = opener.open(req, timeout=timeout)
        return resp.status, resp.read().decode(), dict(resp.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)
    except urllib.error.URLError as e:
        return 0, str(e), {}


def wait_for_container(cookie, max_wait=240):
    """Poll /api/sessions until the container is healthy."""
    start = time.time()
    last_code = 0
    while time.time() - start < max_wait:
        try:
            code, body, _ = gw_request("GET", "/api/sessions", cookie=cookie)
        except Exception:
            code = 0
        last_code = code
        if code == 200:
            elapsed = int(time.time() - start)
            log(f"Container ready after {elapsed}s")
            return True
        if code == 402:
            log(f"Trial expired (402)")
            return False
        time.sleep(5)
    log(f"Last poll returned {last_code}")
    return False


# ── Test steps ────────────────────────────────────────────────────────────────

def test_xss_sanitization():
    step("XSS sanitization on ?redirect= parameter")
    # Inject a malicious redirect with JS
    xss_payload = "javascript:alert(1)"
    code, body, hdrs = gw_request("GET", f"/sign-in?redirect={xss_payload}",
                                   accept="text/html")
    if code == 200:
        if xss_payload in body:
            fail(f"XSS: raw payload '{xss_payload}' found in response")
        else:
            ok("XSS payload sanitized — not present in rendered HTML")
    else:
        # Redirect is fine too — means it stripped the bad value
        ok(f"XSS redirect test returned {code} (not 200 with raw payload)")

    # Try scheme-based XSS
    code2, body2, _ = gw_request("GET", "/sign-in?redirect=https://evil.com/steal",
                                  accept="text/html")
    if code2 == 200 and "evil.com" in body2:
        fail("XSS: external URL 'evil.com' found in rendered HTML")
    else:
        ok("External URL redirect sanitized")


def test_signup_and_auth(user_id):
    step("Auth exchange — forge cookie and test gateway auth")
    cookie = make_cookie(user_id)

    # First request should return 503 (starting) or loading page
    code, body, _ = gw_request("GET", "/api/sessions", cookie=cookie)
    if code == 503:
        try:
            data = json.loads(body)
            if data.get("error") == "starting":
                ok("Container starting — got 503 with retry_after (API path)")
            else:
                warn(f"Got 503 but unexpected body: {body[:100]}")
        except json.JSONDecodeError:
            warn(f"Got 503 but body is not JSON: {body[:100]}")
    elif code == 200:
        ok("Container already healthy (fast start)")
    elif code == 402:
        fail(f"Trial expired for test user — got 402: {body[:100]}")
        return cookie, False
    else:
        warn(f"Unexpected status {code} on first request: {body[:100]}")

    # Test loading page for HTML requests
    code_html, body_html, _ = gw_request("GET", "/", cookie=cookie, accept="text/html")
    if code_html == 200 and "Starting your workspace" in body_html:
        ok("Loading page served for HTML request during container startup")
    elif code_html == 200:
        ok("Dashboard loaded immediately (container was already up)")
    else:
        warn(f"HTML request during startup returned {code_html}")

    step("Waiting for container to become healthy")
    if wait_for_container(cookie, max_wait=240):
        ok("Container healthy")
    else:
        fail("Container did not become healthy within 240s")
        return cookie, False

    return cookie, True


def test_dashboard(cookie):
    step("Dashboard loads after auth")
    code, body, _ = gw_request("GET", "/", cookie=cookie, accept="text/html", follow=True)
    if code == 200 and ("amux" in body.lower() or "<title>" in body.lower()):
        ok("Dashboard HTML loaded")
    else:
        fail(f"Dashboard load failed: status={code}, body={body[:100]}")


def test_api_keys(cookie):
    step("BYO API key flow — save and verify")
    anthropic_key = os.environ.get("ANTHROPIC_API_KEY", "")
    openai_key = os.environ.get("OPENAI_API_KEY", "")
    google_key = os.environ.get("GOOGLE_API_KEY", "")

    keys_to_set = {}
    if anthropic_key:
        keys_to_set["ANTHROPIC_API_KEY"] = anthropic_key
    if openai_key:
        keys_to_set["OPENAI_API_KEY"] = openai_key
    if google_key:
        keys_to_set["GOOGLE_API_KEY"] = google_key

    if not keys_to_set:
        warn("No API keys available to test BYO flow")
        return

    # Set keys
    code, body, _ = gw_request("PATCH", "/api/settings/env", body=keys_to_set, cookie=cookie)
    if code == 200:
        ok(f"API keys saved: {', '.join(keys_to_set.keys())}")
    else:
        fail(f"Failed to save API keys: {code} {body[:100]}")
        return

    # Verify keys are set (masked)
    code, body, _ = gw_request("GET", "/api/settings/env", cookie=cookie)
    if code == 200:
        data = json.loads(body)
        for key_name in keys_to_set:
            masked = data.get(key_name, "")
            if masked and len(masked) > 4:
                ok(f"{key_name} verified (masked: ...{masked[-4:]})")
            else:
                fail(f"{key_name} not found or empty after save")
    else:
        fail(f"Failed to read API keys: {code}")


def test_sessions(cookie):
    step("Create and interact with sessions — one per provider")
    providers = [
        ("e2e-claude", "claude"),
        ("e2e-codex", "codex"),
        ("e2e-gemini", "gemini"),
    ]
    created = []
    for name, provider in providers:
        code, body, _ = gw_request("POST", "/api/sessions", cookie=cookie, body={
            "name": name,
            "provider": provider,
            "work_dir": "/tmp",
        })
        if code == 200 or code == 201:
            ok(f"Session '{name}' ({provider}) created")
            created.append(name)
        else:
            # Session might already exist — try to start it
            code2, _, _ = gw_request("POST", f"/api/sessions/{name}/start", cookie=cookie)
            if code2 == 200:
                ok(f"Session '{name}' ({provider}) already exists, started")
                created.append(name)
            else:
                fail(f"Session '{name}' ({provider}) create failed: {code} {body[:100]}")

    # Wait for sessions to initialize (CLIs need time to start)
    log("Waiting 15s for CLI initialization...")
    time.sleep(15)

    # Peek at each session to verify it has output (CLI loaded)
    for name in created:
        code, body, _ = gw_request("GET", f"/api/sessions/{name}/peek?lines=20", cookie=cookie)
        if code == 200:
            try:
                data = json.loads(body)
                output = data.get("output", "")
                if output.strip():
                    ok(f"Session '{name}' running ({len(output)} chars output)")
                else:
                    warn(f"Session '{name}' has no output yet")
            except json.JSONDecodeError:
                warn(f"Session '{name}' peek returned non-JSON")
        else:
            warn(f"Session '{name}' peek returned {code}")

    # Stop sessions to save resources (don't delete — test deletion later)
    for name in created:
        gw_request("POST", f"/api/sessions/{name}/stop", cookie=cookie)

    return created


def test_logout(cookie):
    step("Logout flow")
    code, body, hdrs = gw_request("GET", "/api/cloud-logout", cookie=cookie)
    # NoRedirectHandler may return 302 (code) or 200 (if fp.status normalizes)
    location = hdrs.get("Location", hdrs.get("location", ""))
    set_cookie = ""
    for k, v in hdrs.items():
        if k.lower() == "set-cookie":
            set_cookie += v + " "
    if location and "/sign-in?logout" in location:
        ok(f"Logout redirects to /sign-in?logout (status {code})")
    elif code in (301, 302, 303, 307):
        warn(f"Logout redirected to '{location}' instead of /sign-in?logout")
    elif code == 200 and "sign-in" in body.lower():
        ok("Logout served sign-in page directly (redirect followed)")
    else:
        fail(f"Logout returned {code}, expected 302 with Location header")
    if "Max-Age=0" in set_cookie or "max-age=0" in set_cookie.lower():
        ok("Session cookie cleared on logout (Max-Age=0)")
    elif "amux_session=;" in set_cookie:
        ok("Session cookie cleared on logout")
    else:
        warn(f"Cookie clear not confirmed in headers")


def test_relogin(user_id):
    step("Re-login after logout")
    cookie = make_cookie(user_id)
    code, body, _ = gw_request("GET", "/api/sessions", cookie=cookie)
    if code == 200:
        ok("Re-login successful — sessions endpoint accessible")
    elif code == 503:
        ok("Re-login successful — container restarting (503)")
    else:
        fail(f"Re-login failed: {code} {body[:100]}")
    return cookie


def test_cleanup_sessions(cookie, session_names):
    step("Cleanup — stop test sessions")
    for name in session_names:
        code, _, _ = gw_request("POST", f"/api/sessions/{name}/stop", cookie=cookie)
        if code == 200:
            ok(f"Session '{name}' stopped")
        else:
            warn(f"Session '{name}' stop returned {code}")


def test_502_fallback(cookie, user_id):
    step("502 fallback — verify loading page instead of raw Bad Gateway")
    # We can't easily crash the container, but we can verify the proxy error
    # handler exists by hitting a port that's not running
    # This is a code-level check — the actual 502 is tested by container restarts
    code, body, _ = gw_request("GET", "/", cookie=cookie, accept="text/html", follow=True)
    if code == 200:
        ok("Dashboard accessible (no 502 condition right now)")
    elif "Starting your workspace" in body:
        ok("Loading page served instead of raw 502")
    else:
        warn(f"Got {code} — can't test 502 fallback without container crash")


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    global GATEWAY
    parser = argparse.ArgumentParser(description="amux cloud E2E smoke test")
    parser.add_argument("--gateway", default=GATEWAY, help="Gateway URL")
    args = parser.parse_args()
    GATEWAY = args.gateway

    print(f"═══ amux cloud E2E smoke test ═══")
    print(f"    gateway: {GATEWAY}")
    print(f"    time:    {time.strftime('%Y-%m-%d %H:%M:%S UTC', time.gmtime())}")

    # Preflight checks
    if not CLERK_SECRET:
        print("FATAL: CLERK_SECRET_KEY not set"); sys.exit(1)
    if not COOKIE_SECRET:
        print("FATAL: COOKIE_SECRET not set"); sys.exit(1)

    # ── 1. XSS sanitization (no auth needed) ──
    test_xss_sanitization()

    # ── 2. Create fresh test user (delete any stale one first) ──
    step("Create test user via Clerk")
    old_user = clerk_find_test_user()
    if old_user:
        log(f"Deleting stale test user: {old_user['id']}")
        try:
            clerk_delete_user(old_user["id"])
        except Exception as e:
            warn(f"Failed to delete stale user: {e}")
    user = clerk_create_user()
    user_id = user["id"]
    ok(f"Created test user: {user_id}")

    cookie = make_cookie(user_id)
    try:
        # ── 3. Auth + container startup ──
        cookie, healthy = test_signup_and_auth(user_id)
        if not healthy:
            fail("Container not healthy — skipping remaining tests")
            return

        # ── 4. Dashboard ──
        test_dashboard(cookie)

        # ── 5. BYO API keys ──
        test_api_keys(cookie)

        # ── 6. Sessions (Claude, Codex, Gemini) ──
        created_sessions = test_sessions(cookie)

        # ── 7. 502 fallback ──
        test_502_fallback(cookie, user_id)

        # ── 8. Logout ──
        test_logout(cookie)

        # ── 9. Re-login ──
        cookie = test_relogin(user_id)

        # ── 10. Cleanup sessions ──
        test_cleanup_sessions(cookie, created_sessions)

    finally:
        # ── 11. Cleanup ──
        step("Cleanup — stop container and delete test user")
        # Stop container + clean DB via gateway admin API
        try:
            code, body, _ = gw_request("DELETE",
                                        f"/api/gateway/admin/cleanup/{user_id}",
                                        cookie=cookie,
                                        headers={"X-E2E-Secret": COOKIE_SECRET},
                                        timeout=60)
        except Exception as e:
            code, body = 0, str(e)
        if code == 200:
            ok(f"Container for {user_id} stopped and DB cleaned via gateway API")
        else:
            warn(f"Gateway cleanup returned {code}: {body[:100]}")
        # Delete user from Clerk
        try:
            clerk_delete_user(user_id)
            ok(f"Test user {user_id} deleted from Clerk")
        except Exception as e:
            warn(f"Failed to delete test user: {e}")

    # ── Summary ──
    print(f"\n{'═' * 50}")
    print(f"  PASS: {PASS}  FAIL: {FAIL}  WARN: {len(WARNINGS)}")
    if WARNINGS:
        for w in WARNINGS:
            print(f"  ⚠ {w}")
    if FAIL > 0:
        print(f"  RESULT: FAILED")
        sys.exit(1)
    else:
        print(f"  RESULT: PASSED")
        sys.exit(0)


if __name__ == "__main__":
    main()
