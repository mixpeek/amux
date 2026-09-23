#!/usr/bin/env python3
"""ATE-92: one Cargo cleanup guard for the builder and the embedded reclaim API.

The lifetime lease lives OUTSIDE target/ and survives exec into cargo and tests.
Reclaim also takes Cargo's native locks and probes processes for older/unwrapped
builds. Native lock files and their ancestors never move or disappear. Failed
probes/locks defer cleanup, regardless of free disk; the next idle tick retries.
"""
import argparse
import contextlib
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


class Deferred(Exception):
    def __init__(self, reason, measured=False, considered=0):
        super().__init__(reason)
        self.measured = measured
        self.considered = considered


def overlaps(left, right):
    return left == right or left in right.parents or right in left.parents


def lease_path(target):
    return target.parent / ('.' + target.name + '.reclaim.lock')


# A DEFERRAL STREAK ON THE SAME CONDITION IS A WEDGE, NOT A PAUSE (AMUX-4944).
#
# The deferral logs with measured:true and a real pid, so it reads as the guard
# working. Nothing counted CONSECUTIVE deferrals, so 243 identical refusals
# looked exactly like one and the fleet's deploys were dark for days before
# anyone asked why their own work was not live.
WEDGE_STREAK = int(os.environ.get('AMUX_CARGO_GUARD_WEDGE_STREAK', '10'))


def streak_path(target):
    return target.parent / ('.' + target.name + '.reclaim.streak')


def bump_streak(targets, reason, now=None):
    """Consecutive deferrals for the SAME reason, as a count.

    Keyed on the reason text, so a DIFFERENT blocker resets it: a new pid is a
    new condition, and calling that a continuing wedge would be the same class
    of lie as not counting at all. Both fields are computed from stored state;
    neither can disagree with the run that produced it.
    """
    if not targets:
        return None
    path = streak_path(sorted(targets)[0])
    now = int(time.time()) if now is None else now
    try:
        previous = json.loads(path.read_text())
    except (OSError, ValueError):
        previous = {}
    if previous.get('reason') == reason:
        count = int(previous.get('count', 0)) + 1
        since = int(previous.get('since', now))
    else:
        count, since = 1, now
    try:
        path.write_text(json.dumps({'reason': reason, 'count': count, 'since': since}))
    except OSError:
        pass  # an unwritable streak file must never fail the reclaim itself
    return {'deferral_streak': count, 'streak_since': since,
            'wedged': count >= WEDGE_STREAK}


def clear_streak(targets):
    """A reclaim that got through ends the streak: the condition cleared."""
    for target in targets:
        try:
            streak_path(target).unlink(missing_ok=True)
        except OSError:
            pass


def open_lock(path, shared=False, wait=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(fd, (fcntl.LOCK_SH if shared else fcntl.LOCK_EX)
                    | (0 if wait else fcntl.LOCK_NB))
    except OSError:
        os.close(fd)
        raise Deferred('lock busy or unmeasured: ' + str(path))
    return fd


def native_locks(target):
    # Cargo profiles (debug/release/custom) and target triples. Do not traverse
    # deps/incremental/build: these can contain millions of artifacts, not locks.
    result = set()
    frontier = [(target, 0)]
    while frontier:
        directory, depth = frontier.pop()
        if not directory.exists():
            continue
        for entry in directory.iterdir():
            if entry.name in ('.cargo-lock', '.cargo-build-lock', '.cargo-artifact-lock'):
                result.add(entry)
            elif depth < 2 and entry.is_dir() and not entry.is_symlink() and entry.name not in (
                    'deps', 'incremental', 'build', 'examples', '.fingerprint', 'doc'):
                frontier.append((entry, depth + 1))
    # Create locks in existing standard profiles too, so a just-starting Cargo
    # cannot acquire a different inode after the process snapshot.
    for profile in ('debug', 'release'):
        if (target / profile).is_dir():
            result.add(target / profile / '.cargo-lock')
    return sorted(result)


def process_executable(pid, command, *, linux=None, proc_root=Path('/proc'),
                       readlink=None, which=None):
    """Resolve one process without treating an unreadable /proc link as a build.

    Hardened Linux runners can deny ``/proc/<pid>/exe`` for ordinary sibling
    processes.  ``ps comm`` is still measured evidence: when its complete name
    resolves through PATH (for example ``sleep`` or ``bash``), that executable
    cannot be a test artifact under a Cargo target.  Truncated/custom names do
    not resolve and therefore continue to fail closed.
    """
    executable = Path(command.removesuffix(' (deleted)'))
    linux = sys.platform.startswith('linux') if linux is None else linux
    if not linux:
        return executable
    readlink = os.readlink if readlink is None else readlink
    which = shutil.which if which is None else which
    proc = proc_root / pid
    try:
        if proc.stat().st_uid == os.getuid():
            executable = Path(readlink(proc / 'exe').removesuffix(' (deleted)'))
    except FileNotFoundError:
        return None  # process exited during the snapshot
    except PermissionError as error:
        known = which(executable.name)
        if known:
            known = Path(known).resolve()
            if known.name == executable.name:
                return known
        raise Deferred('process executable unmeasured: pid ' + pid) from error
    return executable


# A BUILD EXITS; A DAEMON DOES NOT (AMUX-4944).
#
# Anything older than this that merely LIVES under a target dir is not treated
# as an in-flight build. Compilers are exempt by name below: a long compile is
# still a compile. Deliberately generous — the longest build recorded on this
# hardware is 26m48s (CLAUDE.md) and the CI lib suite is ~7m — so no real build
# is near it, while the process that deadlocked the fleet had been up for days.
#
# RESIDUAL EXPOSURE, stated rather than hidden: an UNWRAPPED build older than
# this would no longer defer cleanup. Anything started through `guard run`
# holds a shared lease fd that `exclusive()` blocks on regardless of age, so
# the exposure is only a bare `cargo` invocation running over four hours.
STALE_BUILD_AGE_S = int(os.environ.get('AMUX_CARGO_GUARD_MAX_BUILD_AGE_S', '14400'))


def parse_etime(text):
    """`ps -o etime` as seconds, or None when it does not parse.

    `[[DD-]HH:]MM:SS`, the one format macOS and procps agree on — `etimes`
    (seconds) is Linux-only and macOS `ps` rejects the keyword outright.

    Pure and separate from the probe so the age rule can be pinned without
    spawning a process that has been running for four hours.
    """
    days = 0
    text = text.strip()
    if '-' in text:
        head, _, text = text.partition('-')
        if not head.isdigit():
            return None
        days = int(head)
    parts = text.split(':')
    if not (2 <= len(parts) <= 3) or not all(p.isdigit() for p in parts):
        return None
    parts = [int(p) for p in parts]
    hours, minutes, seconds = ([0] + parts) if len(parts) == 2 else parts
    return days * 86400 + hours * 3600 + minutes * 60 + seconds


def active_processes(targets):
    try:
        output = subprocess.run(['ps', '-A', '-ww', '-o', 'pid=,etime=,comm='],
                                capture_output=True, text=True, check=True, timeout=5).stdout
    except (OSError, subprocess.SubprocessError) as error:
        raise Deferred('process probe unmeasured: ' + str(error)) from error
    rows = [line.split(None, 2) for line in output.splitlines() if line.strip()]
    if not rows or any(len(row) != 3 or not row[0].isdigit() for row in rows):
        raise Deferred('process probe unmeasured: empty or malformed ps output')
    active = []
    stale = []
    for pid, etime, command in rows:
        # Linux ps comm is truncated. Resolve same-user executable paths so a
        # directly launched test binary is protected after its Cargo parent exits.
        executable = process_executable(pid, command)
        if executable is None:
            continue
        if executable.name in ('cargo', 'rustc', 'rustdoc', 'clippy-driver'):
            active.append(pid)
            continue
        if any(executable == root or root in executable.parents for root in targets):
            age = parse_etime(etime)
            # FAIL CLOSED on an unparseable age: ATE-92 made this refuse rather
            # than delete a live build, and an unmeasured age is not evidence
            # that the process is stale.
            if age is None or age < STALE_BUILD_AGE_S:
                active.append(pid)
            else:
                stale.append((pid, age, executable))
    if stale:
        # SURFACED, not silently dropped: this is the judgement that unblocks
        # the reclaim, so it has to be auditable in the builder log.
        print('cargo_guard_stale_under_target: not treating as in-flight build(s) '
              + ', '.join('pid %s age %ds %s' % (p, a, e) for p, a, e in stale),
              file=sys.stderr)
    return active, len(rows)


@contextlib.contextmanager
def exclusive(targets, process_probe=active_processes):
    descriptors = []
    locks = []
    try:
        for target in sorted(set(targets)):
            descriptors.append(open_lock(lease_path(target)))
            for lock in native_locks(target):
                descriptors.append(open_lock(lock))
                locks.append(lock)
        active, considered = process_probe(targets)
        if active:
            raise Deferred('active cargo/rustc/test process(es): ' + ','.join(active), True, considered)
        yield locks, considered
    finally:
        for descriptor in reversed(descriptors):
            os.close(descriptor)


def clear_preserving_locks(path, locks):
    if path in locks:
        return
    if any(path in lock.parents for lock in locks):
        for child in path.iterdir():
            clear_preserving_locks(child, locks)
    elif path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink(missing_ok=True)


def mutate(action, path, destination, targets, protected=(), dry_run=False,
           process_probe=active_processes):
    path = path.resolve()
    destination = destination.resolve() if destination else None
    touched = [root for root in targets if any(overlaps(root, candidate) for candidate in
               [path, *protected, *([destination] if destination else [])])]
    # A quarantine batch may contain artifacts moved by an older, unsafe server.
    # Probe those staged executable paths too, while locking their original roots.
    guard = exclusive(touched, lambda roots: process_probe([*roots, path])) if touched else contextlib.nullcontext(([], 0))
    with guard as (locks, considered):
        if action == 'move' and any(overlaps(path, lock) and (path == lock or path in lock.parents)
                                    for lock in locks):
            raise Deferred('Cargo lock directories cannot rotate; select obsolete artifact subdirectories')
        if any(path == lease_path(root) for root in targets):
            raise Deferred('Cargo lifetime lease cannot be reclaimed')
        if action == 'move' and destination.exists():
            raise Deferred('destination already exists')
        if action == 'move' and any(path == root or destination == root for root in touched):
            raise Deferred('Cargo target roots cannot rotate; select obsolete artifact subdirectories')
        if not dry_run:
            if action == 'clear':
                clear_preserving_locks(path, locks)
            elif action == 'move':
                destination.parent.mkdir(parents=True, exist_ok=True)
                path.rename(destination)
            elif action == 'purge':
                if path.exists():
                    clear_preserving_locks(path, locks)
        return {'verdict': 'dry_run' if dry_run else 'reclaimed',
                'measured': bool(touched), 'n_considered': considered}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['run', 'clear', 'move', 'purge'])
    parser.add_argument('--target', action='append', required=True)
    parser.add_argument('--path')
    parser.add_argument('--destination')
    parser.add_argument('--protected-path', action='append', default=[])
    parser.add_argument('--dry-run', action='store_true')
    # Parse command separately: argparse REMAINDER would swallow guard options.
    argsv = sys.argv[1:]
    command = []
    if '--' in argsv:
        split = argsv.index('--')
        argsv, command = argsv[:split], argsv[split + 1:]
    args = parser.parse_args(argsv)
    targets = sorted(set(Path(root).resolve() for root in args.target))
    try:
        if args.action == 'run':
            if not command:
                raise Deferred('no build command')
            # Acquired inside the systemd scope, and inherited by cargo and its
            # children. No parent polling window between build and test binaries.
            for target in targets:
                fd = open_lock(lease_path(target), shared=True, wait=True)
                os.set_inheritable(fd, True)
            os.execvp(command[0], command)
        if not args.path or (args.action == 'move' and not args.destination):
            raise Deferred('missing mutation path or destination')
        probe = active_processes
        fixture = os.environ.get('AMUX_CARGO_GUARD_TEST_FIXTURE')
        if fixture:
            fixture = Path(fixture).resolve()
            temp_roots = [Path(tempfile.gettempdir()).resolve(), Path('/tmp').resolve()]
            mutation_paths = [Path(args.path).resolve(),
                              *[Path(p).resolve() for p in args.protected_path],
                              *([Path(args.destination).resolve()] if args.destination else [])]
            if (os.environ.get('AMUX_RS_DISK_CLEAR_ONLY') != '1'
                    or not fixture.name.startswith('amux-cargo-guard-test.')
                    or not any(root in fixture.parents for root in temp_roots)
                    or not all(fixture in root.parents for root in targets)
                    or not all(fixture in path.parents for path in mutation_paths)):
                raise Deferred('invalid temporary fixture process-probe seam')
            # Tests may isolate host processes ONLY for throwaway target trees.
            # Locks still run; no real shared target can pass these constraints.
            probe = lambda _: ([], 1)
        result = mutate(args.action, Path(args.path),
                        Path(args.destination) if args.destination else None, targets,
                        [Path(p).resolve() for p in args.protected_path], args.dry_run, probe)
        clear_streak(targets)
        print(json.dumps(result))
    except (Deferred, OSError) as error:
        payload = {'verdict': 'cargo_reclaim_deferred', 'measured': getattr(error, 'measured', False),
                   'n_considered': getattr(error, 'considered', 0), 'reason': str(error)}
        payload.update(bump_streak(targets, str(error)) or {})
        print(json.dumps(payload))
        return 75
    return 0


if __name__ == '__main__':
    sys.exit(main())
