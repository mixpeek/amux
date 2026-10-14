"""The HTTP layer must at least import and route: a syntax error in an f-string page once
passed every other unit test because nothing imported app.py."""
import os, sys, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


class Imports(unittest.TestCase):
    def test_app_imports_and_every_route_has_a_handler(self):
        import app
        paths = {(m, p) for m, p, _ in app.ROUTES}
        for need in [("GET", "/"), ("GET", "/start"), ("GET", "/welcome"), ("POST", "/webhooks/stripe"),
                     ("POST", "/api/fleet/"), ("GET", "/invite/"), ("POST", "/api/session"), ("POST", "/api/notify")]:
            self.assertIn(need, paths)
        self.assertTrue(all(callable(fn) for _, _, fn in app.ROUTES))

    def test_clerk_module_imports(self):
        import clerk  # noqa: F401


if __name__ == "__main__":
    unittest.main()
