"""apple.verify_identity_token against tokens signed with a generated key (the real verification code)."""
import json, os, sys, time, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jwt
from cryptography.hazmat.primitives.asymmetric import rsa
import apple  # noqa: E402

KEY = rsa.generate_private_key(public_exponent=65537, key_size=2048)
OTHER = rsa.generate_private_key(public_exponent=65537, key_size=2048)
JWK = json.loads(jwt.algorithms.RSAAlgorithm.to_jwk(KEY.public_key()))
JWK.update(kid="k1", alg="RS256", use="sig")


def token(key=KEY, kid="k1", **over):
    claims = {"iss": "https://appleid.apple.com", "aud": "com.example.app", "sub": "001234.abc", "iat": int(time.time()),
              "exp": int(time.time()) + 600, "email": "Pat@Example.com", "email_verified": "true", "is_private_email": "false"}
    claims.update(over)
    return jwt.encode(claims, key, algorithm="RS256", headers={"kid": kid})


class Verify(unittest.TestCase):
    def setUp(self):
        apple._keys.update(v=None, ts=0)

    def v(self, tok):
        return apple.verify_identity_token(tok, audience="com.example.app", fetch_keys=lambda: [JWK])

    def test_good_token(self):
        self.assertEqual(self.v(token()), {"sub": "001234.abc", "email": "pat@example.com", "email_verified": True, "private_relay": False})

    def test_relay_flag(self):
        self.assertTrue(self.v(token(is_private_email=True))["private_relay"])

    def test_refusals(self):
        for name, tok in [("wrong audience", token(aud="com.evil.app")), ("wrong issuer", token(iss="https://evil.example")),
                          ("expired", token(exp=int(time.time()) - 5, iat=int(time.time()) - 700)),
                          ("unknown key id", token(kid="nope")), ("wrong signing key", token(key=OTHER)), ("garbage", "not.a.jwt")]:
            with self.subTest(name), self.assertRaises(ValueError):
                self.v(tok)


if __name__ == "__main__":
    unittest.main()
