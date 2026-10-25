"""Short-lived signed tokens (docs/design/cloud-self-service.md, J8 and section 3).

Door tokens let a signed-in customer into their own server. Each server has its
own secret (servers.fleet_secret), shared only with that server, so a token for
one server can never open another (T-SSO-1). HMAC-SHA256 keeps the door on the
standard library. The same scheme signs the control plane's own session cookie
with CONTROL_SECRET.
"""
import base64, hashlib, hmac, json, secrets, time


def _b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _unb64(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def sign(claims, secret, ttl):
    body = dict(claims, exp=int(time.time() + ttl), nonce=secrets.token_hex(8))
    payload = _b64(json.dumps(body, separators=(",", ":"), sort_keys=True).encode())
    mac = hmac.new(secret.encode(), payload.encode(), hashlib.sha256).digest()
    return f"{payload}.{_b64(mac)}"


def verify(token, secret, audience=None, now=None):
    """Returns the claims, or raises ValueError naming why the token was refused."""
    try:
        payload, mac = token.split(".")
    except (AttributeError, ValueError):
        raise ValueError("malformed token")
    expected = hmac.new(secret.encode(), payload.encode(), hashlib.sha256).digest()
    if not hmac.compare_digest(expected, _unb64(mac)):
        raise ValueError("bad signature")
    claims = json.loads(_unb64(payload))
    if claims.get("exp", 0) < (now or time.time()):
        raise ValueError("expired")
    if audience is not None and claims.get("aud") != audience:
        raise ValueError("wrong audience")
    return claims


def door_token(server, member_email, role="owner", ttl=60, invite=None):
    claims = {"aud": server["slug"], "sub": member_email, "role": role}
    if invite:
        claims["invite"] = invite
    return sign(claims, server["fleet_secret"], ttl)


def new_secret():
    return secrets.token_urlsafe(32)
