"""Sign in with Apple for the iOS app (docs/design/cloud-self-service.md 8.3, J13, J18).

The app sends Apple's identity token and authorization code. The control plane
verifies the token itself (Apple's published keys, issuer, audience = the app's
bundle id, expiry), so no Clerk SDK is needed in the app. It then trades the
authorization code for Apple's refresh token, which it keeps only so account
deletion can revoke it as Apple requires.

The app never creates an account: an Apple ID nobody has linked returns
link_required, and the app sends the person through web sign-in once, which
links the Apple ID to the account they already pay for.

Env: APPLE_BUNDLE_ID, APPLE_TEAM_ID, APPLE_KEY_ID, APPLE_PRIVATE_KEY (the .p8 PEM)
"""
import json, os, threading, time, urllib.parse, urllib.request

ISSUER = "https://appleid.apple.com"
KEYS_URL = "https://appleid.apple.com/auth/keys"
TOKEN_URL = "https://appleid.apple.com/auth/token"
REVOKE_URL = "https://appleid.apple.com/auth/revoke"

_keys = {"v": None, "ts": 0.0}
_lock = threading.Lock()


def _jwks(fetch=None):
    with _lock:
        if _keys["v"] and time.time() - _keys["ts"] < 3600:
            return _keys["v"]
    if fetch:
        keys = fetch()
    else:
        with urllib.request.urlopen(KEYS_URL, timeout=10) as r:
            keys = json.load(r)["keys"]
    with _lock:
        _keys.update(v=keys, ts=time.time())
    return keys


def verify_identity_token(token, audience=None, fetch_keys=None, now=None):
    """Returns {"sub", "email", "email_verified", "private_relay"}; raises ValueError on any doubt."""
    import jwt as pyjwt
    audience = audience or os.environ.get("APPLE_BUNDLE_ID")
    if not audience:
        raise ValueError("APPLE_BUNDLE_ID is not configured")
    try:
        kid = pyjwt.get_unverified_header(token).get("kid")
    except Exception:
        raise ValueError("malformed identity token")
    key = next((k for k in _jwks(fetch_keys) if k.get("kid") == kid), None)
    if not key:
        raise ValueError("identity token signed with an unknown key")
    try:
        claims = pyjwt.decode(token, pyjwt.algorithms.RSAAlgorithm.from_jwk(json.dumps(key)), algorithms=["RS256"],
                              audience=audience, issuer=ISSUER,
                              options={"verify_exp": now is None})
    except pyjwt.PyJWTError as e:
        raise ValueError(f"identity token refused: {e}")
    if now is not None and claims.get("exp", 0) < now:
        raise ValueError("identity token expired")
    truthy = lambda v: v is True or v == "true"
    return {"sub": claims["sub"], "email": (claims.get("email") or "").lower() or None,
            "email_verified": truthy(claims.get("email_verified")),
            "private_relay": truthy(claims.get("is_private_email"))}


def client_secret(now=None):
    """Apple's client secret: an ES256 JWT signed with our Sign in with Apple key, valid 10 minutes."""
    import jwt as pyjwt
    now = int(now or time.time())
    return pyjwt.encode({"iss": os.environ["APPLE_TEAM_ID"], "iat": now, "exp": now + 600, "aud": ISSUER,
                         "sub": os.environ["APPLE_BUNDLE_ID"]},
                        os.environ["APPLE_PRIVATE_KEY"], algorithm="ES256",
                        headers={"kid": os.environ["APPLE_KEY_ID"]})


def _post(url, form):
    req = urllib.request.Request(url, data=urllib.parse.urlencode(form).encode(), method="POST",
                                 headers={"Content-Type": "application/x-www-form-urlencoded"})
    with urllib.request.urlopen(req, timeout=15) as r:
        raw = r.read()
        return json.loads(raw) if raw else {}


def exchange_code(code, post=_post):
    """Authorization code -> refresh token (kept only to revoke on deletion)."""
    r = post(TOKEN_URL, {"client_id": os.environ["APPLE_BUNDLE_ID"], "client_secret": client_secret(),
                         "code": code, "grant_type": "authorization_code"})
    return r.get("refresh_token")


def revoke(refresh_token, post=_post):
    post(REVOKE_URL, {"client_id": os.environ["APPLE_BUNDLE_ID"], "client_secret": client_secret(),
                      "token": refresh_token, "token_type_hint": "refresh_token"})
    return True


def configured():
    return all(os.environ.get(k) for k in ("APPLE_BUNDLE_ID", "APPLE_TEAM_ID", "APPLE_KEY_ID", "APPLE_PRIVATE_KEY"))
