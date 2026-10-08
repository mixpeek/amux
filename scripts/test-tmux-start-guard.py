#!/usr/bin/env python3
"""Drive the shipped bash cmd_start with isolated tmux/lsof process doubles."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
DRIVER = r'''
# The offset-safe wrapper exits even when sourced; suppress only that exit.
exit() { return "${1:-0}"; }
source "$1/amux"
unset -f exit
resolve_session() { echo "$1"; }
session_backend() { echo tmux; }
need_tmux() { :; }
# Exercise the local launch guard, independent of whether a real server is
# reachable on this host. Never let a process-double fixture call that server.
_curl() { return 7; }
load_defaults() { :; }
load_session() { CC_DIR="$GUARD_FIXTURE"; CC_PROVIDER=claude; CC_FLAGS="--model sonnet"; }
is_running() { return 1; }
tmux_name() { echo amux-guard-fixture; }
tmux() {
  if [[ "$1" == -N && "$2" == display-message ]]; then
    [[ "$GUARD_CASE" == healthy || "$GUARD_CASE" == race ]]
    return $?
  fi
  printf '%s\n' "$*" >> "$GUARD_FIXTURE/calls"
  if [[ "$GUARD_CASE" == race && "$1" == -N && "$2" == new-session ]]; then return 1; fi
}
lsof() {
  case "$GUARD_CASE" in
    live) printf 'n%s\n' "$GUARD_FIXTURE/default" ;;
    other) printf 'n%s\n' "$GUARD_FIXTURE/other" ;;
    unknown) echo 'probe unavailable' >&2; return 127 ;;
    *) return 1 ;;
  esac
}
cmd_start guard-fixture --detach
'''

PROBE_TIMEOUT_DRIVER = r'''
exit() { return "${1:-0}"; }
source "$1/amux"
unset -f exit
set +e
AMUX_TMUX_PROBE_TIMEOUT_S=1 tmux_has_session_exact "=amux-stuck"
rc=$?
set -e
printf 'rc=%s\n' "$rc"
exit "$rc"
'''

START_ALL_TIMEOUT_DRIVER = r'''
exit() { return "${1:-0}"; }
source "$1/amux"
unset -f exit
cmd_start() { printf 'START %s\n' "$1" >> "$GUARD_FIXTURE/calls"; return 0; }
AMUX_TMUX_PROBE_TIMEOUT_S=1 cmd_start_all
'''


class TmuxStartGuard(unittest.TestCase):
    def run_case(self, mode):
        with tempfile.TemporaryDirectory(prefix="amux-tmux-guard-") as tmp:
            env = dict(os.environ, CC_HOME=tmp, GUARD_FIXTURE=tmp, GUARD_CASE=mode,
                       AMUX_SESSION="guard-fixture", AMUX_API="https://localhost:8824",
                       TMUX=f"{tmp}/default,123,0")
            result = subprocess.run(["bash", "-c", DRIVER, "guard-test", str(REPO)],
                                    env=env, capture_output=True, text=True, timeout=10)
            calls = Path(tmp, "calls")
            log = Path(tmp, "logs/server-rs.log")
            return result, calls.read_text() if calls.exists() else "", log.read_text() if log.exists() else ""

    def test_existing_server_start_cannot_create_replacement(self):
        result, calls, _ = self.run_case("healthy")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(calls.startswith("-N new-session "), calls)

    def test_server_disappearing_after_probe_fails_without_replacement(self):
        result, calls, _ = self.run_case("race")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(calls.splitlines()), 1)
        self.assertTrue(calls.startswith("-N new-session "), calls)

    def test_live_or_unmeasured_owner_refuses_and_logs(self):
        for mode in ("live", "unknown"):
            with self.subTest(mode=mode):
                result, calls, log = self.run_case(mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(calls, "")
                self.assertIn("spawn_refused_live_or_unmeasured_server", log)
                self.assertIn("refusing to replace tmux socket", result.stderr)

    def test_cold_start_and_unrelated_named_server_are_allowed(self):
        for mode in ("cold", "other"):
            with self.subTest(mode=mode):
                result, calls, _ = self.run_case(mode)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(calls.startswith("new-session "), calls)


class TmuxProbeTimeouts(unittest.TestCase):
    def run_driver(self, driver, tmux_stub):
        with tempfile.TemporaryDirectory(prefix="amux-tmux-timeout-") as tmp:
            sessions = Path(tmp, "sessions")
            sessions.mkdir()
            Path(sessions, "alpha.env").write_text("CC_DIR=/tmp\n")
            Path(sessions, "beta.env").write_text("CC_DIR=/tmp\n")
            Path(sessions, "gamma.env").write_text("CC_ARCHIVED=1\n")
            bindir = Path(tmp, "bin")
            bindir.mkdir()
            tmux = Path(bindir, "tmux")
            tmux.write_text(tmux_stub)
            tmux.chmod(0o755)
            env = dict(os.environ, CC_HOME=tmp, GUARD_FIXTURE=tmp,
                       AMUX_SESSION="guard-fixture", AMUX_API="https://localhost:8824",
                       PATH=f"{bindir}:{os.environ.get('PATH', '')}")
            result = subprocess.run(["bash", "-c", driver, "guard-test", str(REPO)],
                                    env=env, capture_output=True, text=True, timeout=10)
            calls = Path(tmp, "calls")
            return result, calls.read_text() if calls.exists() else ""

    def test_tmux_has_session_probe_is_bounded(self):
        result, _ = self.run_driver(PROBE_TIMEOUT_DRIVER, "#!/usr/bin/env bash\nsleep 5\n")
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("rc=124", result.stdout)

    def test_start_all_records_tmux_probe_timeout_without_starting_ambiguous_worker(self):
        tmux_stub = """#!/usr/bin/env bash
if [[ "$1" == "has-session" && "$3" == "=amux-alpha" ]]; then sleep 5; exit 1; fi
if [[ "$1" == "has-session" && "$3" == "=amux-beta" ]]; then exit 0; fi
exit 1
"""
        result, calls = self.run_driver(START_ALL_TIMEOUT_DRIVER, tmux_stub)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, "")
        self.assertIn("0 started", result.stdout)
        self.assertIn("1 already running", result.stdout)
        self.assertIn("1 failed", result.stdout)
        self.assertIn("1 archived", result.stdout)
        self.assertIn("alpha: tmux has-session timed out", result.stderr)


if __name__ == "__main__":
    unittest.main()
