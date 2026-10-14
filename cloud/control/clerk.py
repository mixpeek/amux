"""Clerk session verification for the control plane (docs/design/cloud-self-service.md, D5, J20).

Same approach as cloud/gateway/gateway.py: verify the session JWT against the
instance's JWKS (derived from the publishable key), then read the primary email
and whether Clerk verified it from the Backend API. Accounts created through the
Backend API come back verified, so the control plane keeps its own
users.email_verified and only sets it from a real sign-in or the email link.
"""
import base64, json, os, threading, time, urllib.request

_jwks = {"keys": None, "ts": 0.0}
_lock = threading.Lock()
UA = {"User-Agent": "amux-control/1.0"}   # Cloudflare in front of Clerk rejects urllib's default agent


def _frontend_domain():
    raw = os.environ["CLERK_PUBLISHABLE_KEY"].split("_", 2)[2]
    raw += "=" * (-len(raw) % 4)
    return base64.b64decode(raw).decode().strip("$")


def _keys():
    with _lock:
        if _jwks["keys"] and time.time() - _jwks["ts"] < 3600:
            return _jwks["keys"]
    req = urllib.request.Request(f"https://{_frontend_domain()}/.well-known/jwks.json", headers=UA)
    with urllib.request.urlopen(req, timeout=5) as r:
        keys = json.load(r)["keys"]
    with _lock:
        _jwks.update(keys=keys, ts=time.time())
    return keys


def verify_session(token):
    """Returns (email, verified). Raises on any doubt."""
    import jwt as pyjwt
    kid = pyjwt.get_unverified_header(token).get("kid")
    key = next((k for k in _keys() if k["kid"] == kid), None)
    if not key:
        raise ValueError("unknown signing key")
    claims = pyjwt.decode(token, pyjwt.algorithms.RSAAlgorithm.from_jwk(json.dumps(key)), algorithms=["RS256"],
                          options={"verify_aud": False})
    req = urllib.request.Request(f"https://api.clerk.com/v1/users/{claims['sub']}",
                                 headers=dict(UA, Authorization=f"Bearer {os.environ['CLERK_SECRET_KEY']}"))
    with urllib.request.urlopen(req, timeout=5) as r:
        user = json.load(r)
    primary = next((a for a in user.get("email_addresses", []) if a.get("id") == user.get("primary_email_address_id")), None)
    if not primary:
        raise ValueError("no primary email")
    verified = (primary.get("verification") or {}).get("status") == "verified"
    return primary["email_address"].strip().lower(), verified
