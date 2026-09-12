#!/usr/bin/env python3
"""Real disposable processes and files exercise the shipped resource supervisor."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('budget', ROOT / 'cargo-budget.py')
budget = importlib.util.module_from_spec(spec)
spec.loader.exec_module(budget)


class CargoBudgetTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='amux-budget-test.')
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.target = self.base / 'target'
        self.target.mkdir()

    def run_budget(self, source, **overrides):
        options = dict(max_rss=1024**3, max_seconds=5, max_target=1024**3,
                       min_free=1, interval=.02, disk_interval=.03)
        options.update(overrides)
        output = io.StringIO()
        with contextlib.redirect_stderr(output):
            rc = budget.supervise([sys.executable, '-c', source], [self.target], **options)
        return rc, output.getvalue()

    def test_success_and_failure_exit_codes_survive(self):
        for expected in (0, 7):
            rc, log = self.run_budget(f'import sys; sys.exit({expected})')
            self.assertEqual(rc, expected)
            self.assertIn('cargo_budget_finished', log)

    def test_timeout_kills_only_owned_group(self):
        peer = subprocess.Popen(['sleep', '30'])
        self.addCleanup(lambda: (peer.terminate(), peer.wait()))
        rc, log = self.run_budget('import time; time.sleep(30)', max_seconds=.12)
        self.assertEqual(rc, 124)
        self.assertIn('"reason": "timeout"', log)
        self.assertIsNone(peer.poll())

    def test_memory_counts_compiler_child_and_reaps_it(self):
        pidfile = self.base / 'child.pid'
        child = ('import os,time; from pathlib import Path; '
                 f'Path({str(pidfile)!r}).write_text(str(os.getpid())); '
                 'data=bytearray(48*1024*1024); time.sleep(30)')
        rc, log = self.run_budget(
            f'import subprocess,sys,time; subprocess.Popen([sys.executable,"-c",{child!r}]); time.sleep(30)',
            max_rss=40*1024**2)
        self.assertEqual(rc, 124)
        self.assertIn('"reason": "memory"', log)
        self.assertTrue(pidfile.exists())
        # A reparented zombie may briefly remain in ps on Linux; it consumes no
        # memory and cannot write artifacts. No live member of that group survives.
        pid = int(pidfile.read_text())
        row = subprocess.run(['ps', '-p', str(pid), '-o', 'stat='], capture_output=True, text=True)
        self.assertTrue(not row.stdout.strip() or row.stdout.strip().startswith('Z'))

    def test_oversize_target_refuses_before_execution(self):
        (self.target / 'cache').write_bytes(b'x' * 8192)
        marker = self.base / 'ran'
        rc, log = self.run_budget(f'open({str(marker)!r},"w").close()', max_target=1)
        self.assertEqual(rc, 75)
        self.assertFalse(marker.exists())
        self.assertIn('"reason": "target_size"', log)
        self.assertTrue((self.target / 'cache').exists())

    def test_growing_target_stops_without_deleting_artifacts(self):
        artifact = self.target / 'growing'
        rc, log = self.run_budget(
            f'import time; open({str(artifact)!r},"wb").write(b"x"*2097152); time.sleep(30)',
            max_target=1024**2)
        self.assertEqual(rc, 124)
        self.assertIn('"reason": "target_size"', log)
        self.assertEqual(artifact.stat().st_size, 2097152)

    def test_disk_reserve_refuses_before_execution(self):
        rc, log = self.run_budget('raise Exception("must not run")', min_free=2**63)
        self.assertEqual(rc, 75)
        self.assertIn('"reason": "disk_reserve"', log)

    def test_fast_build_cannot_skip_final_disk_budget(self):
        artifact = self.target / 'fast-output'
        rc, log = self.run_budget(
            f'open({str(artifact)!r},"wb").write(b"x"*2097152)',
            max_target=1024**2, disk_interval=60)
        self.assertEqual(rc, 124)
        self.assertIn('"reason": "target_size"', log)

    def test_probe_failure_is_bounded_and_visible(self):
        with patch.object(budget, 'group_rss', side_effect=ValueError('unreadable ps')):
            rc, log = self.run_budget('import time; time.sleep(30)')
        self.assertEqual(rc, 124)
        self.assertIn('"reason": "probe_failed"', log)
        self.assertIn('"measured": false', log)

    def test_interrupt_forwards_to_owned_cargo_process(self):
        marker = self.base / 'pid'
        source = f'import os,time; open({str(marker)!r},"w").write(str(os.getpid())); time.sleep(30)'
        env = os.environ | dict(CARGO_TARGET_DIR=str(self.target))
        proc = subprocess.Popen([sys.executable, str(ROOT / 'cargo-budget.py'), '--',
                                 sys.executable, '-c', source], env=env, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 5
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertTrue(marker.exists())
            proc.send_signal(signal.SIGTERM)
            _, log = proc.communicate(timeout=8)
            self.assertEqual(proc.returncode, 143, log)
            self.assertIn('"reason": "signal"', log)
            with self.assertRaises(ProcessLookupError):
                os.kill(int(marker.read_text()), 0)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

    def test_wrapper_reaches_budget_and_sets_bounded_defaults(self):
        bindir = self.base / 'bin'
        bindir.mkdir()
        fake = bindir / 'cargo'
        fake.write_text('#!' + sys.executable + '\nimport os,json\nprint(json.dumps({k:v for k,v in os.environ.items() if k.startswith("CARGO_") or k == "RUST_TEST_THREADS"}))\n')
        fake.chmod(0o755)
        env = {k:v for k,v in os.environ.items() if not k.startswith(('CARGO_', 'AMUX_CARGO_', 'RUST_TEST_'))}
        env.update(HOME=str(self.base), PATH=str(bindir) + os.pathsep + os.environ['PATH'])
        result = subprocess.run(['bash', str(ROOT / 'safe-cargo.sh'), 'check'], env=env,
                                capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(result.stdout)
        self.assertEqual(data['CARGO_BUILD_JOBS'], '2')
        self.assertEqual(data['RUST_TEST_THREADS'], '2')
        self.assertEqual(data['CARGO_INCREMENTAL'], '0')
        self.assertEqual(data['CARGO_PROFILE_DEV_DEBUG'], '0')
        self.assertEqual(data['CARGO_PROFILE_TEST_DEBUG'], '0')
        self.assertIn('cargo_budget_started', result.stderr)
        self.assertFalse(list((self.base / '.amux/cargo-throttle').glob('slot-*')))
        env['AMUX_CARGO_MAX_SECONDS'] = '0'
        invalid = subprocess.run(['bash', str(ROOT / 'safe-cargo.sh'), 'check'], env=env,
                                 capture_output=True, text=True, timeout=20)
        self.assertEqual(invalid.returncode, 75)
        self.assertIn('must be a positive integer', invalid.stderr)


class BuildRetryTests(unittest.TestCase):
    def test_normal_workspace_cache_is_kept_and_oversized_cache_is_selected(self):
        with tempfile.TemporaryDirectory(prefix='amux-budget-cache.') as directory:
            base = Path(directory)
            shim = base / '.cargo/bin'
            shim.mkdir(parents=True)
            debug = base / '.amux/rust-build-target/debug'
            debug.mkdir(parents=True)
            marker = debug / 'artifact'
            marker.write_text('warm cache')
            du = shim / 'du'
            du.write_text('#!/bin/sh\necho "$FIXTURE_TARGET_KB path"\n')
            du.chmod(0o755)
            env = os.environ | dict(HOME=str(base), AMUX_BUILD_MIN_FREE_GB='0',
                AMUX_RS_DISK_CLEAR_ONLY='1', AMUX_RS_DISK_CLEAR_DRYRUN='1',
                AMUX_RS_BUILD_LOG=str(base / 'build.log'))
            env.pop('AMUX_BUILD_DEBUG_CLEAR_ABOVE_GB', None)
            for gb in (20, 33):
                (base / 'build.log').unlink(missing_ok=True)
                subprocess.run(['bash', str(ROOT / 'rust-auto-build.sh')],
                               env=env | dict(FIXTURE_TARGET_KB=str(gb * 1024**2)),
                               check=True, capture_output=True, timeout=20)
                log = (base / 'build.log').read_text()
                self.assertEqual('DEBUG ARTIFACTS' in log, gb > 32, log)
                self.assertEqual(marker.read_text(), 'warm cache')

    def test_failed_inputs_back_off_changed_inputs_retry_and_success_clears(self):
        with tempfile.TemporaryDirectory(prefix='amux-retry-test.') as directory:
            base = Path(directory)
            repo = base / 'repo'
            repo.mkdir()
            (repo / 'crates').mkdir()
            (repo / 'scripts').mkdir()
            source = repo / 'crates/input.rs'
            source.write_text('first')
            (repo / 'Cargo.toml').write_text('[workspace]\n')
            stub = repo / 'scripts/safe-cargo.sh'
            trace = base / 'attempts'
            stub.write_text('#!/bin/sh\necho attempt >> "$ATTEMPTS"\nexit 1\n')
            stub.chmod(0o755)
            def git(*args):
                subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)
            def commit():
                git('add', '-A')
                git('-c', 'user.name=Test', '-c', 'user.email=test@example.com', 'commit', '-qm', 'fixture')
            git('init', '-b', 'main')
            commit()
            env = os.environ | dict(HOME=str(base), AMUX_REPO=str(repo), ATTEMPTS=str(trace),
                AMUX_RS_BUILD_STAMP=str(base / 'stamp'), AMUX_RS_BUILD_LOG=str(base / 'build.log'),
                AMUX_RS_ACTIVATION_REF='HEAD', AMUX_BUILD_MIN_FREE_GB='0', AMUX_BUILD_FAILURE_RETRY_SECS='900')
            def run():
                subprocess.run(['bash', str(ROOT / 'rust-auto-build.sh')], env=env,
                               check=True, capture_output=True, timeout=30)
            run()
            run()
            self.assertEqual(trace.read_text().count('attempt'), 1)
            self.assertIn('cargo_build_backoff', (base / 'build.log').read_text())
            # An elapsed cooldown permits retry for unchanged source.
            failure = base / 'stamp.failed'
            key = failure.read_text().split()[0]
            failure.write_text(key + ' 1\n')
            run()
            self.assertEqual(trace.read_text().count('attempt'), 2)
            # Changed actual build inputs retry immediately, then reset failure.
            source.write_text('fixed')
            stub.write_text('#!/bin/sh\necho attempt >> "$ATTEMPTS"\nmkdir -p "$CARGO_TARGET_DIR/release"\nprintf "#!/bin/sh\\nexit 0\\n" > "$CARGO_TARGET_DIR/release/amux-server"\nchmod +x "$CARGO_TARGET_DIR/release/amux-server"\n')
            commit()
            run()
            self.assertEqual(trace.read_text().count('attempt'), 3)
            self.assertFalse(failure.exists())
            self.assertTrue((base / 'stamp').exists())


if __name__ == '__main__':
    unittest.main()
