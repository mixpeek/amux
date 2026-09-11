#!/usr/bin/env python3
"""Regression tests for the external listener recovery path."""

import importlib.util
import io
import json
import urllib.error
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("watchdog.py")


def load_watchdog(home: str):
    with mock.patch.dict(os.environ, {"HOME": home}, clear=False):
        spec = importlib.util.spec_from_file_location("amux_watchdog_test", SCRIPT)
        module = importlib.util.module_from_spec(spec)
        assert spec.loader is not None
        spec.loader.exec_module(module)
        return module


class WatchdogRecoveryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        plist = (
            Path(self.tmp.name)
            / "Library"
            / "LaunchAgents"
            / "com.amux.server-rs.plist"
        )
        plist.parent.mkdir(parents=True)
        plist.write_text("fixture")
        self.w = load_watchdog(self.tmp.name)
        self.w.IS_MACOS = True
        self.w.DRY_RUN = False
        self.w.KICKSTART_COOLDOWN = 0

    def test_failed_kickstart_reloads_exact_agent_and_recovers(self):
        calls = []

        def run(argv, **_kwargs):
            calls.append(argv)
            if argv[:3] == ["launchctl", "kickstart", "-k"]:
                return subprocess.CompletedProcess(argv, 78, b"", b"EX_CONFIG")
            return subprocess.CompletedProcess(argv, 0, b"", b"")

        with mock.patch.object(self.w.subprocess, "run", side_effect=run), mock.patch.object(
            self.w.time, "sleep", return_value=None
        ), mock.patch.object(self.w, "probe", return_value=("ok", {})):
            self.assertTrue(self.w.restart_server(reload_if_needed=True))

        domain = f"gui/{os.getuid()}"
        target = f"{domain}/com.amux.server-rs"
        plist = str(
            Path(self.tmp.name)
            / "Library"
            / "LaunchAgents"
            / "com.amux.server-rs.plist"
        )
        self.assertIn(["launchctl", "bootout", domain, plist], calls)
        self.assertIn(["launchctl", "bootstrap", domain, plist], calls)
        self.assertIn(["launchctl", "enable", target], calls)
        self.assertIn(["launchctl", "kickstart", target], calls)

    def test_reload_refuses_an_unresolved_plist(self):
        missing_home = tempfile.TemporaryDirectory()
        self.addCleanup(missing_home.cleanup)
        w = load_watchdog(missing_home.name)
        w.IS_MACOS = True
        with mock.patch.object(w.subprocess, "run") as run:
            self.assertFalse(
                w._reload_launch_agent(f"gui/{os.getuid()}/com.amux.server-rs")
            )
            run.assert_not_called()

    def test_slow_probe_with_measured_progress_does_not_imply_a_dead_store(self):
        data = {"store":"hung", "status":"degraded",
                "board":{"measured":False,"error":"probe_deadline_exceeded"},
                "store_probe":{"last_success_age_ms":1000,"in_flight_age_ms":250}}
        self.assertEqual(self.w.classify_health(data), "busy")
        # A real failure is never excused by an older healthy sample.
        data["board"]["error"] = "writer_probe_failed"
        self.assertEqual(self.w.classify_health(data), "degraded")
        data["board"]["error"] = "read_pool_exhausted"
        self.assertEqual(self.w.classify_health(data), "degraded")
        data["board"]["error"] = "probe_already_in_flight"
        data["store_probe"]["last_success_age_ms"] = 90000
        self.assertEqual(self.w.classify_health(data), "degraded")
        # No progress receipt from an older server is not proof of progress.
        del data["store_probe"]
        self.assertEqual(self.w.classify_health(data), "degraded")

    def test_initial_probe_grace_is_bounded_and_cannot_refresh_stale_success(self):
        data = {"store":"hung", "status":"degraded",
                "board":{"measured":False,"error":"probe_already_in_flight"},
                "store_probe":{"last_success_age_ms":None,"in_flight_age_ms":250}}
        self.assertEqual(self.w.classify_health(data), "busy")
        data["store_probe"]["in_flight_age_ms"] = 90000
        self.assertEqual(self.w.classify_health(data), "degraded")
        data["store_probe"]["in_flight_age_ms"] = 1
        data["store_probe"]["last_success_age_ms"] = 90000
        self.assertEqual(self.w.classify_health(data), "degraded")

    def test_http_503_uses_probe_progress_and_does_not_restart_a_serving_process(self):
        data = {"store":"hung", "status":"degraded",
                "board":{"measured":False,"error":"probe_deadline_exceeded"},
                "store_probe":{"last_success_age_ms":1000,"in_flight_age_ms":250}}
        error = urllib.error.HTTPError(self.w.HEALTH_URL, 503, "busy", {}, io.BytesIO(json.dumps(data).encode()))
        with mock.patch.object(self.w, "_port_is_open", return_value=True), mock.patch.object(
            self.w.urllib.request, "urlopen", side_effect=error
        ):
            verdict, detail = self.w.probe()
        self.assertEqual(verdict, "busy")
        with mock.patch.object(self.w, "probe", side_effect=[(verdict,detail)] * 4 + [KeyboardInterrupt]), \
             mock.patch.object(self.w, "restart_server") as restart, \
             mock.patch.object(self.w.time, "sleep"):
            with self.assertRaises(KeyboardInterrupt): self.w.run()
        restart.assert_not_called()

    def test_a_stalled_probe_still_restarts_after_the_recovery_threshold(self):
        data = {"store":"hung", "status":"degraded",
                "board":{"measured":False,"error":"probe_already_in_flight"},
                "store_probe":{"last_success_age_ms":120000,"in_flight_age_ms":120000}}
        verdict = self.w.classify_health(data)
        with mock.patch.object(self.w, "probe", side_effect=[(verdict,data)] * self.w.HUNG_THRESHOLD + [KeyboardInterrupt]), \
             mock.patch.object(self.w, "restart_server", return_value=True) as restart, \
             mock.patch.object(self.w.time, "sleep"):
            with self.assertRaises(KeyboardInterrupt): self.w.run()
        restart.assert_called_once()

    def test_default_down_budget_is_one_minute(self):
        self.assertEqual(self.w.DOWN_ESCALATE * self.w.CHECK_INTERVAL, 60)


if __name__ == "__main__":
    unittest.main()
