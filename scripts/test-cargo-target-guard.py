#!/usr/bin/env python3
"""ATE-92: real process/lock lifetimes, preserved artifacts, and idle retry."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('cargo-target-guard.py').resolve()
spec = importlib.util.spec_from_file_location('guard', SCRIPT)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


def quiet(_):
    return [], 1


class CargoReclaimTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = self.base / 'rust-build-target'
        self.artifacts = self.root / 'debug' / 'deps'
        self.artifacts.mkdir(parents=True)
        self.marker = self.artifacts / 'artifact'
        self.marker.write_text('must survive active builds')

    def mutate(self, action='clear', path=None, destination=None, probe=quiet, protected=()):
        return guard.mutate(action, path or self.root, destination, [self.root], protected,
                            process_probe=probe)

    def test_active_build_lease_blocks_clear_and_rotation_then_idle_retry_reclaims(self):
        # The actual sanctioned wrapper with a deterministic Cargo executable.
        # It execs into the tool so the test pins lease inheritance, not a parent
        # shell that happens to remain alive during a subprocess.
        bin_dir = self.base / 'bin'
        bin_dir.mkdir()
        # Exercise the Linux scope command without requiring a CI user manager.
        scope = bin_dir / 'systemd-run'
        scope.write_text('#!/bin/sh\nwhile [ "$1" != -- ]; do shift; done\nshift\nexec "$@"\n')
        scope.chmod(0o755)
        cargo = bin_dir / 'cargo'
        cargo.write_text('#!/bin/sh\nprintf "ready\\n"\nread -r release\n')
        cargo.chmod(0o755)
        env = {**os.environ, 'PATH': str(bin_dir) + ':' + os.environ['PATH'],
               'CARGO_TARGET_DIR': str(self.root)}
        proc = subprocess.Popen(['bash', str(SCRIPT.with_name('safe-cargo.sh')), 'check'],
                                env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.DEVNULL, text=True)
        try:
            self.assertEqual(proc.stdout.readline().strip(), 'ready')
            for action, path, dest in [('clear', self.root, None),
                                       ('move', self.artifacts, self.base / 'staged')]:
                with self.assertRaisesRegex(guard.Deferred, 'lock busy'):
                    self.mutate(action, path, dest)
                self.assertTrue(self.marker.exists())
        finally:
            proc.communicate('finished\n', timeout=10)
        self.assertEqual(proc.returncode, 0)
        self.mutate()
        self.assertFalse(self.marker.exists())
        self.assertTrue((self.root / 'debug' / '.cargo-lock').exists())
        self.assertTrue(guard.lease_path(self.root).exists())

    def test_native_cargo_lock_blocks_unwrapped_build(self):
        lock = self.root / 'debug' / '.cargo-lock'
        fd = guard.open_lock(lock)
        try:
            with self.assertRaisesRegex(guard.Deferred, 'lock busy'):
                self.mutate()
            self.assertTrue(self.marker.exists())
        finally:
            os.close(fd)
        before = lock.stat().st_ino
        self.mutate()
        self.assertEqual(lock.stat().st_ino, before)
        self.assertFalse(self.marker.exists())

    def measured_active(self, targets):
        """`active_processes` over the AMBIENT table, or a skip naming why not.

        These two cells spawn a real process and assert about the real probe,
        which is the point of them. But the probe walks EVERY pid, and on a
        hardened runner one uninspectable `/proc/<pid>/exe` makes it fail
        closed for the whole table before it can answer about our own pid.
        That is deliberate (process_executable's docstring says so) and it is
        not something these cells can assert through.

        Measured 2026-09-08: both errored on GitHub with
        `PermissionError: [Errno 13] Permission denied: '/proc/1226/exe'`,
        reddening `checks` on main and on every PR that merged main. They pass
        on this project's dev box only because it is macOS, where
        process_executable's linux branch never runs at all, so the ambient
        table is never walked. Green on the author's machine by construction.

        A test cannot assert from a probe that could not run (ethos rule 4), so
        an unmeasured table is a SKIP that names the pid, not a pass and not a
        failure. The hardened path itself stays covered deterministically by
        test_hardened_linux_proc_uses_known_command_identity and
        test_hardened_linux_proc_still_fails_closed_for_unknown_binary, which
        inject readlink/which instead of reading the host.
        """
        try:
            return guard.active_processes(targets)
        except guard.Deferred as unmeasured:
            self.skipTest('ambient process table unmeasurable on this host: %s' % unmeasured)

    def test_direct_test_binary_without_cargo_parent_prevents_deletion(self):
        binary = self.artifacts / 'orphan-test-0123456789abcdef'
        source = self.base / 'test.c'
        source.write_text('#include <unistd.h>\nint main(void) { sleep(60); return 0; }\n')
        subprocess.run(['cc', str(source), '-o', str(binary)], check=True, capture_output=True)
        proc = subprocess.Popen([str(binary)])
        try:
            active, count = self.measured_active([self.root])
            self.assertIn(str(proc.pid), active)
            self.assertGreater(count, 0)
            with self.assertRaises(guard.Deferred):
                self.mutate(probe=guard.active_processes)
            self.assertTrue(binary.exists())
        finally:
            proc.terminate()
            proc.wait(timeout=10)
        self.mutate()
        self.assertFalse(binary.exists())

    def test_unmeasured_probe_fails_closed_and_idle_move_restore_purge_work(self):
        def failed(_):
            raise guard.Deferred('process probe unmeasured')
        with self.assertRaisesRegex(guard.Deferred, 'unmeasured'):
            self.mutate(probe=failed)
        staged = self.base / 'quarantine' / 'deps'
        self.mutate('move', self.artifacts, staged)
        self.assertTrue((staged / 'artifact').exists())
        self.mutate('move', staged, self.artifacts)
        self.assertTrue(self.marker.exists())
        self.mutate('move', self.artifacts, staged)
        with self.assertRaisesRegex(guard.Deferred, 'unmeasured'):
            self.mutate('purge', staged, protected=[self.artifacts], probe=failed)
        self.assertTrue(staged.exists())
        self.mutate('purge', staged, protected=[self.artifacts])
        self.assertFalse(staged.exists())

    def test_cargo_lock_directories_never_rotate(self):
        for path in (self.root, self.root / 'debug'):
            with self.assertRaisesRegex(guard.Deferred, 'cannot rotate'):
                self.mutate('move', path, self.base / 'staged')
        self.assertTrue(self.marker.exists())

    def test_fixture_probe_seam_cannot_authorize_production_mutation(self):
        env = {**os.environ, 'AMUX_CARGO_GUARD_TEST_FIXTURE': str(self.base),
               'AMUX_RS_DISK_CLEAR_ONLY': '1'}
        result = subprocess.run([sys.executable, str(SCRIPT), 'clear', '--target',
                                 str(self.root), '--path', str(self.root)],
                                env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 75)
        self.assertIn('invalid temporary fixture', result.stdout)
        self.assertTrue(self.marker.exists())

    def test_valid_fixture_cannot_move_or_restore_across_its_boundary(self):
        with tempfile.TemporaryDirectory(prefix='amux-cargo-guard-test.') as fixture_dir:
            fixture = Path(fixture_dir).resolve()
            target = fixture / 'target'
            artifacts = target / 'debug' / 'deps'
            artifacts.mkdir(parents=True)
            marker = artifacts / 'artifact'
            marker.write_text('fixture must stay inside its boundary')
            env = {**os.environ, 'AMUX_CARGO_GUARD_TEST_FIXTURE': str(fixture),
                   'AMUX_RS_DISK_CLEAR_ONLY': '1'}
            outside = self.base / 'outside'
            for action, extra in [
                    ('move', ['--destination', str(outside)]),
                    ('purge', ['--protected-path', str(outside)])]:
                with self.subTest(action=action):
                    result = subprocess.run([sys.executable, str(SCRIPT), action,
                                             '--target', str(target), '--path', str(artifacts),
                                             *extra], env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 75, result.stdout)
                    self.assertIn('invalid temporary fixture', result.stdout)
                    self.assertTrue(marker.exists())
                    self.assertFalse(outside.exists())

    def test_command_line_mentions_do_not_become_builds(self):
        proc = subprocess.Popen(['sh', '-c', 'sleep 30 # cargo rustc build'])
        try:
            active, _ = self.measured_active([self.root])
            self.assertNotIn(str(proc.pid), active)
        finally:
            proc.terminate()
            proc.wait(timeout=10)

    def test_hardened_linux_proc_uses_known_command_identity(self):
        proc = self.base / 'proc' / '1177'
        proc.mkdir(parents=True)

        def denied(_):
            raise PermissionError('hardened procfs')

        resolved = guard.process_executable(
            '1177', 'sleep', linux=True, proc_root=self.base / 'proc',
            readlink=denied, which=lambda name: '/usr/bin/sleep' if name == 'sleep' else None)
        self.assertEqual(resolved, Path('/usr/bin/sleep'))

    def test_hardened_linux_proc_still_fails_closed_for_unknown_binary(self):
        proc = self.base / 'proc' / '1178'
        proc.mkdir(parents=True)

        def denied(_):
            raise PermissionError('hardened procfs')

        with self.assertRaisesRegex(guard.Deferred, 'process executable unmeasured'):
            guard.process_executable(
                '1178', 'orphan-test-012', linux=True, proc_root=self.base / 'proc',
                readlink=denied, which=lambda _: None)

    def test_etime_parses_every_shape_ps_emits_and_refuses_the_rest(self):
        """AMUX-4944. The age rule is only as good as the parse under it.

        `[[DD-]HH:]MM:SS` is the one format macOS and procps agree on; `etimes`
        is Linux-only and macOS `ps` rejects the keyword outright, which is why
        this parses rather than reads seconds.
        """
        self.assertEqual(guard.parse_etime('00:07'), 7)
        self.assertEqual(guard.parse_etime('30:23'), 30 * 60 + 23)
        self.assertEqual(guard.parse_etime('04:47:38'), 4 * 3600 + 47 * 60 + 38)
        self.assertEqual(guard.parse_etime('3-04:47:38'), 3 * 86400 + 4 * 3600 + 47 * 60 + 38)
        self.assertEqual(guard.parse_etime('  30:23  '), 30 * 60 + 23)
        # Unparseable must be None, NOT zero: the probe fails CLOSED on None and
        # would treat a zero as a brand-new build. Opposite outcomes.
        for junk in ('', '-', 'nope', '1:2:3:4', 'aa:bb', '-04:47:38', '30'):
            self.assertIsNone(guard.parse_etime(junk), junk)

    def test_process_age_tells_a_daemon_from_an_in_flight_build(self):
        """AMUX-4944. The same real process, two bounds, opposite verdicts.

        pid 13664 was an orphaned `debug/amux-server` from a PAUSED lane. It
        deferred the builder's cleanup on every tick (243 recorded deferrals),
        debug/ grew 65.5 -> 72.0 GB, the budget then refused every build, and
        no commit deployed fleet-wide for days. The predicate could not express
        the difference between a build using the directory and a daemon that
        happens to live in it.
        """
        binary = self.artifacts / 'daemon-0123456789abcdef'
        source = self.base / 'daemon.c'
        source.write_text('#include <unistd.h>\nint main(void) { sleep(60); return 0; }\n')
        subprocess.run(['cc', str(source), '-o', str(binary)], check=True, capture_output=True)
        proc = subprocess.Popen([str(binary)])
        try:
            # CONTROL, and the behaviour ATE-92 exists to protect: a brand-new
            # binary under the target IS an in-flight build and still defers.
            active, _ = self.measured_active([self.root])
            self.assertIn(str(proc.pid), active)

            original = guard.STALE_BUILD_AGE_S
            guard.STALE_BUILD_AGE_S = 0
            self.addCleanup(setattr, guard, 'STALE_BUILD_AGE_S', original)
            aged, _ = self.measured_active([self.root])
            self.assertNotIn(str(proc.pid), aged,
                             'past the build-age bound this is a daemon, not an in-flight build')
            # ...and the reclaim it had been deadlocking now gets through.
            self.mutate(probe=guard.active_processes)
            self.assertFalse(self.marker.exists())
        finally:
            proc.kill()
            proc.wait()

    def test_a_compiler_is_exempt_by_name_however_old_it_is(self):
        """A long compile is still a compile, so age must not reach it.

        Without this, the age bound would silently become a licence to delete
        the target dir out from under a genuinely slow build, which is the
        failure ATE-92 made this guard refuse in the first place.
        """
        binary = self.artifacts / 'cargo'
        source = self.base / 'slow.c'
        source.write_text('#include <unistd.h>\nint main(void) { sleep(60); return 0; }\n')
        subprocess.run(['cc', str(source), '-o', str(binary)], check=True, capture_output=True)
        proc = subprocess.Popen([str(binary)])
        try:
            original = guard.STALE_BUILD_AGE_S
            guard.STALE_BUILD_AGE_S = 0
            self.addCleanup(setattr, guard, 'STALE_BUILD_AGE_S', original)
            active, _ = self.measured_active([self.root])
            self.assertIn(str(proc.pid), active,
                          'a compiler is active by NAME; the age bound must not reach it')
        finally:
            proc.kill()
            proc.wait()

    def test_consecutive_deferrals_on_one_condition_count_up_and_reset_on_a_new_one(self):
        """AMUX-4944. 243 identical refusals logged exactly like one.

        The deferral carries measured:true and a real pid, so it reads as the
        guard working. Without a count, a wedge and a pause are the same line.
        """
        first = guard.bump_streak([self.root], 'active cargo/rustc/test process(es): 13664')
        self.assertEqual(first['deferral_streak'], 1)
        self.assertFalse(first['wedged'])

        for expected in range(2, guard.WEDGE_STREAK + 1):
            latest = guard.bump_streak([self.root], 'active cargo/rustc/test process(es): 13664')
            self.assertEqual(latest['deferral_streak'], expected)
        self.assertTrue(latest['wedged'], 'a run of identical refusals must declare itself a wedge')
        self.assertEqual(latest['streak_since'], first['streak_since'], 'the streak keeps its start')

        # A DIFFERENT blocker is a different condition, not a continuing wedge.
        moved = guard.bump_streak([self.root], 'active cargo/rustc/test process(es): 99999')
        self.assertEqual(moved['deferral_streak'], 1)
        self.assertFalse(moved['wedged'])

        # A reclaim that got through ends it.
        guard.clear_streak([self.root])
        self.assertEqual(guard.bump_streak([self.root], 'anything')['deferral_streak'], 1)


if __name__ == '__main__':
    unittest.main()
