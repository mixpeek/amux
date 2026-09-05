#!/usr/bin/env python3
"""Regression tests for the external listener recovery path."""

import importlib.util
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

    def test_default_down_budget_is_one_minute(self):
        self.assertEqual(self.w.DOWN_ESCALATE * self.w.CHECK_INTERVAL, 60)


if __name__ == "__main__":
    unittest.main()
