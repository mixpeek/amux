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

    def make_profile(self, name, megabytes):
        directory = self.target / name
        directory.mkdir(parents=True, exist_ok=True)
        (directory / 'artifact').write_bytes(b'\0' * (megabytes * 1024 * 1024))
        return directory

    def test_a_release_build_is_not_refused_because_debug_grew(self):
        """AMUX-4945. The second half of the 2026-09-22 deploy deadlock.

        rust-auto-build.sh only ever builds --release. Measured that day: total
        72GB, debug/ 66GB, release/ 1.4GB, free disk 103GB throughout. A release
        build needing ~1.4GB of warm cache was refused because a profile it
        neither reads nor writes had grown large, and the refusal could never
        clear itself: refusing a build does not shrink debug/.
        """
        self.make_profile('debug', 4)
        self.make_profile('release', 1)

        rc, log = self.run_budget('import sys; sys.exit(0)',
                                  max_target=2 * 1024**2, profile='release')
        self.assertEqual(rc, 0, 'a release build must not be refused for debug/: ' + log)

        # The CONTROL, and the half that must not rot: the budget still refuses
        # when the profile THIS build uses is the one over the limit. Without
        # this, gating on nothing at all would satisfy the assertion above.
        rc, log = self.run_budget('import sys; sys.exit(0)',
                                  max_target=2 * 1024**2, profile='debug')
        self.assertEqual(rc, 75, 'an over-budget debug build must still refuse: ' + log)
        self.assertIn('"reason": "target_size"', log)

    def test_an_over_budget_total_is_announced_and_names_the_dominant_profile(self):
        """`reason='target_size'` named the symptom and could not say WHICH.

        An operator could not tell "your build is too big" from "someone else's
        test artifacts are too big". The total being over budget is real and
        wants a RECLAIM, so passing silently would hide a growing directory the
        way the old refusal hid which directory.
        """
        self.make_profile('debug', 4)
        self.make_profile('release', 1)
        rc, log = self.run_budget('import sys; sys.exit(0)',
                                  max_target=2 * 1024**2, profile='release')
        self.assertEqual(rc, 0, log)
        scoped = [json.loads(line) for line in log.splitlines()
                  if '"cargo_budget_profile_scoped"' in line]
        self.assertEqual(len(scoped), 1, 'the over-budget total must be announced: ' + log)
        self.assertEqual(scoped[0]['dominant'], 'debug')
        self.assertEqual(scoped[0]['profile'], 'release')
        self.assertGreater(scoped[0]['total_bytes'], scoped[0]['profile_bytes'])
        self.assertIn('debug', scoped[0]['by_profile'])

    def test_profile_dir_reads_the_subdirectory_cargo_will_actually_use(self):
        """Pure, so the mapping is pinned without building anything."""
        self.assertEqual(budget.profile_dir(['cargo', 'build', '--release']), 'release')
        self.assertEqual(budget.profile_dir(['cargo', 'build', '-r']), 'release')
        self.assertEqual(budget.profile_dir(['cargo', 'test']), 'debug')
        self.assertEqual(budget.profile_dir(['cargo', 'build', '--profile=bench']), 'bench')
        self.assertEqual(budget.profile_dir(['cargo', 'build', '--profile', 'bench']), 'bench')
        # cargo writes the `dev` profile to debug/, which is the one name where
        # the flag and the directory disagree.
        self.assertEqual(budget.profile_dir(['cargo', 'build', '--profile', 'dev']), 'debug')
        self.assertEqual(budget.profile_dir(['/usr/local/bin/cargo', 'build']), 'debug')
        # NOT a cargo command: gate on the whole directory, which is the old
        # behaviour and the safe direction to be wrong in.
        self.assertIsNone(budget.profile_dir(['python3', '-c', 'pass']))
        self.assertIsNone(budget.profile_dir([]))

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

    # ---- AMUX-4758: an unmeasurable budget is unenforced, not a refusal ----
    #
    # These replace `test_probe_failure_is_bounded_and_visible`, which asserted
    # rc == 124 and `"reason": "probe_failed"` — it pinned the defect. Measured
    # 2026-09-17 at load average 121: `du` timed out three times, this script
    # killed a healthy `cargo check`, and the commit was refused while the same
    # tree passed clippy clean minutes earlier and again on retry.

    def test_a_broken_probe_does_not_kill_a_build_that_finishes(self):
        """THE CARD. The command runs to completion and ITS exit code is returned."""
        for expected in (0, 7):
            with patch.object(budget, 'group_rss',
                              side_effect=subprocess.TimeoutExpired(['ps'], 5)):
                # THE SLEEP IS LOAD-BEARING (AMUX-4759). `cargo_budget_unenforced`
                # is emitted the first time a probe RAISES, so the probe has to
                # tick at least once — and a bare `sys.exit()` can finish inside
                # the .02s interval, with the whole probe loop never running.
                # Then no probe failed, nothing was unenforced, and the assertion
                # below fails on scheduling rather than on behaviour: measured
                # 3 of 6 red locally, in isolation, before this line.
                #
                # Same remedy the sibling test already carries for the same race,
                # where its comment records "2 of 6 suite runs red, green every
                # time in isolation". The exit code is still the command's, which
                # is what this cell is actually about.
                rc, log = self.run_budget(
                    f'import sys, time; time.sleep(.15); sys.exit({expected})')
            self.assertEqual(rc, expected, log)
            self.assertNotIn('probe_failed', log)
            # SAID SO, not silently. A run with every probe broken and a run with
            # every probe working must not print the same line.
            self.assertIn('cargo_budget_unenforced', log)
            self.assertIn('"still_enforced": ["timeout"]', log)

    def test_the_wall_clock_timeout_still_holds_when_every_probe_is_broken(self):
        """The residual guarantee, and the reason 'unenforced' is not 'unsupervised'.

        A runaway build is still stopped with no working probe at all, because
        the timeout reads a clock rather than shelling out. Without this cell
        the fix above would be indistinguishable from removing the budget.
        """
        # max_seconds COMFORTABLY EXCEEDS three probe intervals. At .12s the
        # timeout could fire before the third failure accumulated, so the
        # `unenforced` assertion below passed or failed on scheduling: 2 of 6
        # suite runs red, green every time in isolation. The timeout assertion
        # was never the racy half; the state it is asserted ALONGSIDE was.
        with patch.object(budget, 'group_rss',
                          side_effect=subprocess.TimeoutExpired(['ps'], 5)):
            rc, log = self.run_budget('import time; time.sleep(30)',
                                      max_seconds=.5, interval=.02)
        self.assertEqual(rc, 124)
        self.assertIn('"reason": "timeout"', log)
        self.assertIn('"unenforced": ["disk_reserve", "memory", "target_size"]', log)

    def test_a_preflight_probe_failure_still_runs_the_command(self):
        """`du` timing out BEFORE the build must not stop the build starting."""
        marker = self.base / 'ran'
        with patch.object(budget, 'disk_usage',
                          side_effect=subprocess.TimeoutExpired(['du'], 20)):
            rc, log = self.run_budget(f'open({str(marker)!r},"w").close()')
        self.assertEqual(rc, 0, log)
        self.assertTrue(marker.exists(), 'the command never ran: ' + log)
        self.assertIn('cargo_budget_unenforced', log)

    def test_a_final_probe_failure_does_not_discard_a_successful_build(self):
        """The worst arm: the build SUCCEEDED and its exit code was thrown away.

        The pre-flight probe works, so the run starts normally; the `du` after
        the command times out. That used to `return 75` over a green build.
        """
        real = budget.disk_usage
        calls = []

        def once_then_timeout(targets, profile=None):
            calls.append(1)
            if len(calls) == 1:
                return real(targets, profile)
            raise subprocess.TimeoutExpired(['du'], 20)

        with patch.object(budget, 'disk_usage', side_effect=once_then_timeout):
            rc, log = self.run_budget('import sys; sys.exit(0)', disk_interval=60)
        self.assertGreaterEqual(len(calls), 2, 'the final probe never ran: ' + log)
        self.assertEqual(rc, 0, log)
        self.assertIn('cargo_budget_unenforced', log)

    def test_enforced_means_measured_not_merely_unfailed(self):
        """Absence of failure is not evidence of measurement.

        A short command can finish with its ONLY `ps` sample having timed out.
        That is one failure, below the three-strike lapse threshold, so the
        limit is not `unenforced` — and it was still being reported under
        `enforced` while nothing had ever read RSS. Found while verifying this
        card, in the run that proves the fix.
        """
        # ONE PROBE ATTEMPT, DETERMINISTICALLY. A long `interval` means the loop
        # samples once and then waits out the whole command, so `failures` lands
        # at 1 — below the three-strike lapse — every run. Written first with the
        # default interval, it passed or failed depending on how many times the
        # loop spun during Python's own startup, which is a test whose result
        # depends on scheduling and is worse than no test.
        with patch.object(budget, 'group_rss',
                          side_effect=subprocess.TimeoutExpired(['ps'], 5)):
            rc, log = self.run_budget('import sys; sys.exit(0)', interval=5)
        self.assertEqual(rc, 0, log)
        finished = json.loads([l for l in log.splitlines()
                               if 'cargo_budget_finished' in l][-1])
        self.assertEqual(finished['probe_failures'], 0,
                         'this cell needs the single sample to stay below the lapse '
                         'threshold, or it is testing the other state: ' + str(finished))
        self.assertNotIn('memory', finished['enforced'],
                         'a limit nothing sampled must not read as enforced')
        self.assertIn('timeout', finished['enforced'])
        # THREE STATES, and they PARTITION the probed limits. "Never measured"
        # is not the same as "measured and lapsed", and folding either into the
        # other loses the distinction this cell exists for. Here only the memory
        # probe is broken, so it is the only one that is not enforced.
        self.assertEqual(finished['never_measured'], ['memory'])
        self.assertEqual(finished['unenforced'], [])
        buckets = (finished['enforced'] + finished['unenforced']
                   + finished['never_measured'])
        self.assertEqual(sorted(buckets), sorted(('timeout',) + budget.PROBED_LIMITS),
                         'every limit must land in exactly one bucket: ' + str(finished))

    def test_a_measured_violation_still_refuses(self):
        """THE CONTROL. Only an UNMEASURABLE limit degrades.

        Collapsing that distinction retires the budget instead of repairing it,
        and AMUX-70 is real: an OOM-killed cargo in a shared pane scope takes
        the whole session down. Every cell above passes on a script that simply
        never enforces anything; this one does not.
        """
        (self.target / 'cache').write_bytes(b'x' * 8192)
        marker = self.base / 'ran'
        rc, log = self.run_budget(f'open({str(marker)!r},"w").close()', max_target=1)
        self.assertEqual(rc, 75)
        self.assertFalse(marker.exists())
        self.assertIn('"measured": true', log)

    def test_cleanup_failure_is_an_event_not_a_traceback(self):
        """A stack out of a pre-commit gate reads as a broken toolchain.

        The live one was PermissionError from `os.killpg` plus TimeoutExpired
        from the `ps` that classifies the group, both raised out of the
        `finally`, printed under a line about `cargo check`. Nothing in it named
        this script.
        """
        with patch.object(budget, 'signal_group',
                          side_effect=PermissionError(1, 'Operation not permitted')):
            rc, log = self.run_budget('import sys; sys.exit(3)')
        self.assertEqual(rc, 3, 'the run result must survive a cleanup failure: ' + log)
        self.assertIn('cargo_budget_cleanup_failed', log)
        self.assertNotIn('Traceback (most recent call last)', log)

    def test_a_real_du_timeout_through_the_shipped_path_still_commits(self):
        """END TO END, with a REAL timeout rather than a substituted exception.

        `du` and `ps` are shimmed on PATH to outlive their own budgets, which are
        named so this can be fast. This is the path the incident took: the
        subprocess actually times out inside `disk_usage`/`group_rss`.
        """
        bindir = self.base / 'bin'
        bindir.mkdir()
        for name in ('du', 'ps'):
            shim = bindir / name
            shim.write_text('#!/bin/sh\nsleep 5\n')
            shim.chmod(0o755)
        marker = self.base / 'built'
        env = os.environ | dict(
            CARGO_TARGET_DIR=str(self.target),
            AMUX_CARGO_DU_TIMEOUT_S='0.3', AMUX_CARGO_PS_TIMEOUT_S='0.3',
            PATH=str(bindir) + os.pathsep + os.environ['PATH'])
        result = subprocess.run(
            [sys.executable, str(ROOT / 'cargo-budget.py'), '--',
             sys.executable, '-c', f'open({str(marker)!r},"w").close()'],
            env=env, capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(marker.exists(), 'the command never ran: ' + result.stderr)
        self.assertIn('"event": "cargo_budget_unenforced"', result.stderr)
        self.assertNotIn('Traceback (most recent call last)', result.stderr)

    def test_a_supervisor_crash_names_itself_instead_of_printing_a_bare_stack(self):
        """The backstop. If something here does raise, it must not look like Rust."""
        env = os.environ | dict(CARGO_TARGET_DIR=str(self.target))
        with patch.object(budget, 'supervise', side_effect=RuntimeError('probe exploded')), \
                patch.dict(os.environ, env), \
                patch.object(sys, 'argv', ['cargo-budget.py', '--', 'true']):
            output = io.StringIO()
            with contextlib.redirect_stderr(output):
                rc = budget.main()
        log = output.getvalue()
        self.assertEqual(rc, 75)
        self.assertIn('cargo_budget_crashed', log)
        self.assertIn('scripts/cargo-budget.py', log)
        # The stack is KEPT, inside the event, so a real supervisor defect is
        # not traded away for a quieter failure.
        self.assertIn('RuntimeError: probe exploded', log)

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
