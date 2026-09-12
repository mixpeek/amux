#!/usr/bin/env python3
"""Bound a local Cargo process group; never signal a worker or another build.

Sampled limits, not kernel reservations: a process can overshoot between probes.
The caller holds cargo-target-guard's lease throughout supervision and cleanup.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time


def emit(event, **fields):
    print(json.dumps(dict(event=event, **fields)), file=sys.stderr, flush=True)


def positive(name, default):
    value = int(os.environ.get(name, default))
    if value <= 0:
        raise ValueError(name + ' must be a positive integer')
    return value


def group_rss(pgid):
    result = subprocess.run(['ps', '-A', '-o', 'pgid=,rss='],
                            capture_output=True, text=True, check=True, timeout=5)
    rows = [line.split() for line in result.stdout.splitlines() if line.strip()]
    if not rows or any(len(row) != 2 for row in rows):
        raise ValueError('process memory probe returned no usable population')
    return sum(int(rss) * 1024 for group, rss in rows if int(group) == pgid), len(rows)


def disk_usage(targets):
    total, free = 0, None
    for target in targets:
        if target.exists():
            result = subprocess.run(['du', '-sk', str(target)], capture_output=True,
                                    text=True, check=True, timeout=20)
            total += int(result.stdout.split()[0]) * 1024
        parent = target
        while not parent.exists():
            parent = parent.parent
        available = shutil.disk_usage(parent).free
        free = available if free is None else min(free, available)
    return total, free


def signal_group(pgid, sig):
    try:
        os.killpg(pgid, sig)
    except ProcessLookupError:
        pass
    except PermissionError:
        # Darwin can return EPERM for a group containing only reparented
        # zombies. Verify that state; never hide a refusal for a live child.
        result = subprocess.run(['ps', '-A', '-o', 'pgid=,stat='],
                                capture_output=True, text=True, check=True, timeout=5)
        rows = [line.split() for line in result.stdout.splitlines() if line.strip()]
        if not rows or any(len(row) != 2 for row in rows) or any(
                int(group) == pgid and not state.startswith('Z') for group, state in rows):
            raise


def stop_group(proc):
    # Cargo can exit before its compiler/test children. Always reap its group.
    signal_group(proc.pid, signal.SIGTERM)
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        pass
    # Even after the parent exited, a child can still be ignoring TERM.
    signal_group(proc.pid, signal.SIGKILL)
    proc.wait()


def supervise(command, targets, *, max_rss, max_seconds, max_target, min_free,
              interval=2, disk_interval=30):
    started = time.monotonic()
    try:
        size, free = disk_usage(targets)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        emit('cargo_budget_unmeasured', measured=False, reason=str(error))
        return 75
    if size > max_target or free < min_free:
        emit('cargo_budget_refused', measured=True, target_bytes=size, free_bytes=free,
             reason='target_size' if size > max_target else 'disk_reserve')
        return 75
    emit('cargo_budget_started', max_rss_bytes=max_rss, max_seconds=max_seconds,
         max_target_bytes=max_target, min_free_bytes=min_free)
    # Preserve the inherited Cargo lifetime lease in the child. If this monitor
    # is killed, cleanup must still see the active compiler/test as leased.
    proc = subprocess.Popen(command, start_new_session=True, close_fds=False)
    interrupted = []
    prior = {}
    for sig in (signal.SIGINT, signal.SIGTERM):
        prior[sig] = signal.signal(sig, lambda signum, _: interrupted.append(signum))
    peak, considered, next_disk, failures = 0, 0, started + disk_interval, 0
    try:
        while proc.poll() is None:
            now = time.monotonic()
            reason = 'signal' if interrupted else ('timeout' if now - started >= max_seconds else None)
            try:
                rss, considered = group_rss(proc.pid)
                peak = max(peak, rss)
                if rss > max_rss:
                    reason = reason or 'memory'
                if now >= next_disk:
                    size, free = disk_usage(targets)
                    next_disk = now + disk_interval
                    if size > max_target or free < min_free:
                        reason = reason or ('target_size' if size > max_target else 'disk_reserve')
                failures = 0
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                failures += 1
                emit('cargo_budget_unmeasured', measured=False, reason=str(error), failures=failures)
                if failures >= 3:
                    reason = reason or 'probe_failed'
            if reason:
                emit('cargo_budget_stopped', measured=failures == 0, n_considered=considered,
                     reason=reason, peak_rss_bytes=peak, target_bytes=size, free_bytes=free)
                return 128 + interrupted[0] if interrupted else 124
            try:
                proc.wait(timeout=interval)
            except subprocess.TimeoutExpired:
                pass
        # A short build may finish before the next periodic disk probe. Check
        # its final artifacts too, before the builder can install that result.
        try:
            size, free = disk_usage(targets)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            emit('cargo_budget_unmeasured', measured=False, reason=str(error))
            return 75
        if size > max_target or free < min_free:
            emit('cargo_budget_stopped', measured=True, n_considered=len(targets),
                 reason='target_size' if size > max_target else 'disk_reserve',
                 target_bytes=size, free_bytes=free)
            return 124
        emit('cargo_budget_finished', measured=considered > 0, n_considered=considered,
             elapsed_s=round(time.monotonic() - started, 2), peak_rss_bytes=peak,
             target_bytes=size, free_bytes=free,
             exit_code=proc.returncode)
        return proc.returncode if proc.returncode >= 0 else 128 - proc.returncode
    finally:
        stop_group(proc)
        for sig, handler in prior.items():
            signal.signal(sig, handler)


def main():
    command = sys.argv[1:]
    if command[:1] == ['--']:
        command = command[1:]
    if not command:
        emit('cargo_budget_refused', measured=False, reason='missing command')
        return 75
    targets = [Path(os.environ['CARGO_TARGET_DIR']).resolve()]
    for i, arg in enumerate(command):
        if arg.startswith('--target-dir='):
            targets.append(Path(arg.split('=', 1)[1]).resolve())
        elif arg == '--target-dir' and i + 1 < len(command):
            targets.append(Path(command[i + 1]).resolve())
    targets = sorted(set(targets))
    # Avoid double-counting a target nested inside another target.
    targets = [p for p in targets if not any(q in p.parents for q in targets)]
    try:
        return supervise(command, targets,
                         max_rss=positive('AMUX_CARGO_MAX_RSS_MB', 12288) * 1024**2,
                         max_seconds=positive('AMUX_CARGO_MAX_SECONDS', 3600),
                         max_target=positive('AMUX_CARGO_MAX_TARGET_GB', 40) * 1024**3,
                         min_free=positive('AMUX_CARGO_MIN_FREE_GB', 4) * 1024**3)
    except (OSError, ValueError) as error:
        emit('cargo_budget_refused', measured=False, reason=str(error))
        return 75


if __name__ == '__main__':
    sys.exit(main())
