#!/usr/bin/env python3
"""Bound a local Cargo process group; never signal a worker or another build.

Sampled limits, not kernel reservations: a process can overshoot between probes.
The caller holds cargo-target-guard's lease throughout supervision and cleanup.

A BROKEN PROBE IS NOT A BROKEN BUILD (AMUX-4758). Every limit here except the
wall-clock timeout is measured by shelling out to `du` or `ps`, and both of
those time out when the box is loaded — which is exactly when peers are most
active and when a lane most wants its commit to land. Measured 2026-09-17 at
load average 121: `du` timed out three times, this script killed a healthy
`cargo check`, then raised PermissionError out of its own cleanup and printed a
traceback. The commit was refused and HEAD did not move; the same tree passed
clippy clean minutes earlier and again on retry once load fell to 55.

So an unmeasurable limit degrades to UNENFORCED AND SAID SO, never to a refusal.
The script already knew how to say it could not measure — it emitted
`cargo_budget_unmeasured` with `measured: false` three times before aborting —
and then aborted anyway, which is ethos rule 3: a constraint with no truthful
path through. The budget itself stays; AMUX-70 is real.

It also fails LOUDLY IN THE WRONG DIRECTION, which is why the traceback matters
as much as the abort: a Python stack out of a pre-commit gate reads as a broken
toolchain, and the next lane to see it goes looking for a Rust problem that does
not exist. Cleanup here never raises, and `main` cannot emit a bare traceback.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import traceback


# Limits that need a working external probe. The wall-clock timeout is
# deliberately NOT one of them: it reads `time.monotonic()` and cannot become
# unmeasurable, so it is the bound that still holds when every probe is failing.
# Saying which of the four survives is the difference between "unenforced" and
# "unsupervised".
PROBED_LIMITS = ('memory', 'target_size', 'disk_reserve')

# A probe that fails under load fails on EVERY tick. At the 2s default an
# hour-long build would emit 1800 identical lines and bury the one line that
# says what actually happened.
UNMEASURED_EMIT_EVERY = 30

# How long a probe may take before it counts as unreadable. Named and settable
# because the REAL failure is a timeout, and a test that cannot produce one
# cannot test the path the incident took: shimming `du` to exit non-zero
# exercises the same `except` branch by a different route, which is the
# paraphrase ethos rule 7 warns about. A slower box can also legitimately want
# longer than 20s for a `du` over an 89 GB target.
DU_TIMEOUT_S = float(os.environ.get('AMUX_CARGO_DU_TIMEOUT_S', 20))
PS_TIMEOUT_S = float(os.environ.get('AMUX_CARGO_PS_TIMEOUT_S', 5))


def emit(event, **fields):
    print(json.dumps(dict(event=event, **fields)), file=sys.stderr, flush=True)


def positive(name, default):
    value = int(os.environ.get(name, default))
    if value <= 0:
        raise ValueError(name + ' must be a positive integer')
    return value


def group_rss(pgid):
    result = subprocess.run(['ps', '-A', '-o', 'pgid=,rss='],
                            capture_output=True, text=True, check=True, timeout=PS_TIMEOUT_S)
    rows = [line.split() for line in result.stdout.splitlines() if line.strip()]
    if not rows or any(len(row) != 2 for row in rows):
        raise ValueError('process memory probe returned no usable population')
    return sum(int(rss) * 1024 for group, rss in rows if int(group) == pgid), len(rows)


def profile_dir(command):
    """The target subdirectory this cargo command will read and write.

    `--release`/`-r` is `release/`; `--profile X` is `X/`, except `dev`, which
    cargo writes to `debug/`; a cargo command with neither builds `debug/`.

    Returns None when the command is not a cargo invocation this can read, and
    the budget then gates on the WHOLE directory. That is the old behaviour and
    it is the safe direction to be wrong in.
    """
    if not command or 'cargo' not in Path(command[0]).name:
        return None
    for i, arg in enumerate(command):
        if arg in ('--release', '-r'):
            return 'release'
        if arg.startswith('--profile='):
            name = arg.split('=', 1)[1]
            return 'debug' if name == 'dev' else name
        if arg == '--profile' and i + 1 < len(command):
            name = command[i + 1]
            return 'debug' if name == 'dev' else name
    return 'debug'


def disk_usage(targets, profile=None):
    """Free space, the total under `targets`, and the per-profile breakdown.

    Returns `(gated, free, total, by_profile)`. `gated` is what the budget
    REFUSES on: the profile this command will actually read and write, when
    that profile is known. The whole shared directory is still reported.

    WHY NOT THE TOTAL (AMUX-4945). rust-auto-build.sh only ever builds
    `--release`. Measured 2026-09-22: total 72GB, of which debug/ was 66GB and
    release/ was 1.4GB, with 103GB free disk throughout. A release build
    needing ~1.4GB of warm cache was refused because a profile it neither
    reads nor writes had grown large.

    And the refusal could never clear itself: refusing a build does not shrink
    debug/. Reclaiming debug/ does, and that is a different mechanism, which
    AMUX-4944 was simultaneously deadlocking. A constraint with no truthful
    path forward is ethos rule 3, and the retry loop below it is the tell.

    Disk exhaustion stays guarded by `min_free` against real free space, which
    is the limit that actually expresses "the disk is filling up".
    """
    total, free, by_profile = 0, None, {}
    for target in targets:
        if target.exists():
            children = sorted(p for p in target.iterdir() if p.is_dir() and not p.is_symlink())
            if children:
                # ONE du walk over the children, not one over the parent plus
                # one per child: the same tree and the same cost, and it yields
                # the breakdown the refusal needs to name a dominant profile.
                result = subprocess.run(['du', '-sk', *[str(p) for p in children]],
                                        capture_output=True, text=True, check=True,
                                        timeout=DU_TIMEOUT_S)
                for line in result.stdout.splitlines():
                    if not line.strip():
                        continue
                    kilobytes, _, path = line.partition('\t')
                    name = Path(path.strip()).name
                    by_profile[name] = by_profile.get(name, 0) + int(kilobytes) * 1024
                total += sum(by_profile.values())
            else:
                # NO SUBDIRECTORIES STILL PROBES. Skipping `du` here would be
                # correct arithmetic (there is nothing to sum) and would quietly
                # remove the timeout surface this probe is supposed to have:
                # `test_a_real_du_timeout_through_the_shipped_path_still_commits`
                # stopped seeing a timeout at all, because `du` was never run.
                # A probe that cannot fail is not a probe (ethos rule 7).
                result = subprocess.run(['du', '-sk', str(target)], capture_output=True,
                                        text=True, check=True, timeout=DU_TIMEOUT_S)
                total += int(result.stdout.split()[0]) * 1024
            # Loose files at the target root (CACHEDIR.TAG, .rustc_info.json)
            # are kilobytes, but counting them keeps `total` a total.
            total += sum(p.stat().st_size for p in target.iterdir()
                         if p.is_file() and not p.is_symlink())
        parent = target
        while not parent.exists():
            parent = parent.parent
        available = shutil.disk_usage(parent).free
        free = available if free is None else min(free, available)
    gated = by_profile.get(profile, 0) if profile else total
    return gated, free, total, by_profile


def signal_group(pgid, sig):
    try:
        os.killpg(pgid, sig)
    except ProcessLookupError:
        pass
    except PermissionError:
        # Darwin can return EPERM for a group containing only reparented
        # zombies. Verify that state; never hide a refusal for a live child.
        result = subprocess.run(['ps', '-A', '-o', 'pgid=,stat='],
                                capture_output=True, text=True, check=True, timeout=PS_TIMEOUT_S)
        rows = [line.split() for line in result.stdout.splitlines() if line.strip()]
        if not rows or any(len(row) != 2 for row in rows) or any(
                int(group) == pgid and not state.startswith('Z') for group, state in rows):
            raise


def stop_group(proc):
    """Reap the supervised group. NEVER RAISES (AMUX-4758).

    This runs in a `finally`, so anything it raises replaces the run's real
    outcome with a traceback. Both of its failure modes are live specimens:
    `os.killpg` returned EPERM, and the `ps` that `signal_group` uses to tell a
    zombie group from a live one timed out at 5s under load. Either one turned a
    finished build into a stack trace out of a pre-commit hook.

    The broad catch is the point rather than laziness: there is no exception
    from cleanup that should outrank the result of the command being supervised.
    Every one is published as an event instead, so a sweep still sees it.
    """
    # Cargo can exit before its compiler/test children. Always reap its group.
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            signal_group(proc.pid, sig)
        except Exception as error:  # noqa: BLE001 - see the docstring
            emit('cargo_budget_cleanup_failed', measured=False, pgid=proc.pid,
                 signal=sig.name if hasattr(sig, 'name') else int(sig),
                 reason='{}: {}'.format(type(error).__name__, error),
                 note='the supervised group may have survived; the run result below still stands')
        if sig is signal.SIGTERM:
            try:
                proc.wait(timeout=3)
            except Exception:  # noqa: BLE001 - a TimeoutExpired here is expected
                pass
        # Even after the parent exited, a child can still be ignoring TERM.
    # BOUNDED. A bare wait() blocks forever when killpg could not reach the
    # group, which turns a cleanup failure into a hung commit gate — quieter
    # than the traceback and worse.
    try:
        proc.wait(timeout=10)
    except Exception as error:  # noqa: BLE001 - see the docstring
        emit('cargo_budget_cleanup_failed', measured=False, pgid=proc.pid,
             reason='{}: {}'.format(type(error).__name__, error),
             note='the supervised process did not exit within 10s of SIGKILL')


def supervise(command, targets, *, max_rss, max_seconds, max_target, min_free,
              interval=2, disk_interval=30, profile=None):
    started = time.monotonic()
    size = free = total = None
    profiles = {}

    def dominant():
        """The biggest subdirectory, so a refusal can NAME what filled the disk.

        `reason='target_size'` names the symptom and cannot say WHICH profile,
        so an operator reading it could not tell "your build is too big" from
        "someone else's test artifacts are too big" (AMUX-4945).
        """
        return max(profiles.items(), key=lambda kv: kv[1])[0] if profiles else None
    unenforced = set()
    # A limit is ENFORCED only once a probe has actually returned a reading for
    # it. Absence of failure is not evidence of measurement: a short command can
    # finish with its single `ps` sample having timed out, which is below the
    # three-strike lapse threshold and was still reported as "memory enforced".
    # Seen while verifying this card, in the run that proves the fix.
    measured_limits = set()
    probe_failures = 0

    def lapse(error, limits):
        """Record that a limit could not be measured, and say so once."""
        nonlocal probe_failures
        probe_failures += 1
        fresh = [limit for limit in limits if limit not in unenforced]
        unenforced.update(limits)
        if fresh:
            # ONE LINE PER LIMIT THAT LAPSED, naming what still holds. An
            # operator reading this has to be able to tell "nothing was checked"
            # from "memory was not checked", and a bare `measured: false` cannot.
            emit('cargo_budget_unenforced', measured=False,
                 reason='{}: {}'.format(type(error).__name__, error), unenforced=fresh,
                 still_enforced=['timeout'],
                 note='this probe cannot be read right now (usually load); the supervised '
                      'command RUNS and its own exit code is returned. AMUX-4758: an '
                      'unmeasurable budget degrades to unenforced, never to a refusal')

    try:
        size, free, total, profiles = disk_usage(targets, profile)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        lapse(error, ['target_size', 'disk_reserve'])
    else:
        measured_limits.update(('target_size', 'disk_reserve'))
        # THE CACHE IS OVER BUDGET, BUT NOT THE PART THIS BUILD USES. Say so
        # rather than passing silently: the size is real and a reclaim is what
        # answers it, so a quiet pass here would hide a growing directory the
        # way the old refusal hid WHICH directory (AMUX-4945).
        if total is not None and total > max_target and size <= max_target:
            emit('cargo_budget_profile_scoped', measured=True, profile=profile,
                 profile_bytes=size, total_bytes=total, max_target_bytes=max_target,
                 dominant=dominant(), by_profile=profiles,
                 note='gated on the profile this command builds; the total is over budget '
                      'and wants a RECLAIM, which refusing this build would not deliver')
        # A MEASURED violation still refuses. Only an unmeasurable one degrades:
        # the distinction is the whole fix, and collapsing it would retire the
        # budget rather than repair it.
        if size > max_target or free < min_free:
            emit('cargo_budget_refused', measured=True, target_bytes=size, free_bytes=free,
                 total_bytes=total, profile=profile, dominant=dominant(), by_profile=profiles,
                 reason='target_size' if size > max_target else 'disk_reserve')
            return 75
    emit('cargo_budget_started', max_rss_bytes=max_rss, max_seconds=max_seconds,
         max_target_bytes=max_target, min_free_bytes=min_free,
         unenforced=sorted(unenforced))
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
                measured_limits.add('memory')
                peak = max(peak, rss)
                if rss > max_rss:
                    reason = reason or 'memory'
                if now >= next_disk:
                    size, free, total, profiles = disk_usage(targets, profile)
                    measured_limits.update(('target_size', 'disk_reserve'))
                    next_disk = now + disk_interval
                    if size > max_target or free < min_free:
                        reason = reason or ('target_size' if size > max_target else 'disk_reserve')
                failures = 0
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                failures += 1
                if failures == 1 or failures % UNMEASURED_EMIT_EVERY == 0:
                    emit('cargo_budget_unmeasured', measured=False, reason=str(error),
                         failures=failures)
                # NO `reason` HERE. This used to set `reason = 'probe_failed'` on
                # the third consecutive failure, which KILLED A HEALTHY BUILD
                # because `du` was slow. The supervised command keeps running;
                # what stops is the pretence that these limits are enforced.
                if failures >= 3:
                    lapse(error, PROBED_LIMITS)
            if reason:
                emit('cargo_budget_stopped', measured=failures == 0, n_considered=considered,
                     reason=reason, peak_rss_bytes=peak, target_bytes=size, free_bytes=free,
                     total_bytes=total, profile=profile, dominant=dominant(),
                     unenforced=sorted(unenforced))
                return 128 + interrupted[0] if interrupted else 124
            try:
                proc.wait(timeout=interval)
            except subprocess.TimeoutExpired:
                pass
        # A short build may finish before the next periodic disk probe. Check
        # its final artifacts too, before the builder can install that result.
        try:
            size, free, total, profiles = disk_usage(targets, profile)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            # THE WORST ONE, and it was `return 75`: the command had already
            # SUCCEEDED and its exit code was discarded because a `du` run after
            # it timed out. Nothing about the build was wrong.
            lapse(error, ['target_size', 'disk_reserve'])
        else:
            measured_limits.update(('target_size', 'disk_reserve'))
            if size > max_target or free < min_free:
                emit('cargo_budget_stopped', measured=True, n_considered=len(targets),
                     reason='target_size' if size > max_target else 'disk_reserve',
                     target_bytes=size, free_bytes=free, total_bytes=total,
                     profile=profile, dominant=dominant(), by_profile=profiles,
                     unenforced=sorted(unenforced))
                return 124
        # WHAT WAS ACTUALLY ENFORCED, beside the result (ethos rule 4). A green
        # run with every probe broken and a green run with every probe working
        # used to print the same line.
        emit('cargo_budget_finished',
             measured=considered > 0 and not unenforced, n_considered=considered,
             elapsed_s=round(time.monotonic() - started, 2), peak_rss_bytes=peak,
             target_bytes=size, free_bytes=free, total_bytes=total, profile=profile,
             enforced=['timeout'] + [l for l in PROBED_LIMITS
                                     if l in measured_limits and l not in unenforced],
             unenforced=sorted(unenforced), probe_failures=probe_failures,
             never_measured=sorted(l for l in PROBED_LIMITS
                                   if l not in measured_limits and l not in unenforced),
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
        return supervise(command, targets, profile=profile_dir(command),
                         max_rss=positive('AMUX_CARGO_MAX_RSS_MB', 12288) * 1024**2,
                         max_seconds=positive('AMUX_CARGO_MAX_SECONDS', 3600),
                         max_target=positive('AMUX_CARGO_MAX_TARGET_GB', 40) * 1024**3,
                         min_free=positive('AMUX_CARGO_MIN_FREE_GB', 4) * 1024**3)
    except (OSError, ValueError) as error:
        emit('cargo_budget_refused', measured=False, reason=str(error))
        return 75
    except Exception as error:  # noqa: BLE001 - see below
        # A BARE TRACEBACK OUT OF A PRE-COMMIT GATE READS AS A BROKEN TOOLCHAIN
        # (AMUX-4758). The live one was a PermissionError from os.killpg and a
        # TimeoutExpired from `ps`, both raised out of cleanup, printed under a
        # line about `cargo check`. Nothing named this script, so the next lane
        # to see it goes looking for a Rust problem that is not there.
        #
        # The traceback is KEPT, in the event, because a supervisor crashing is
        # a real defect and hiding it would trade one bad failure for a quieter
        # one. What changes is that it arrives labelled.
        emit('cargo_budget_crashed', measured=False,
             reason='{}: {}'.format(type(error).__name__, error),
             component='scripts/cargo-budget.py',
             note='the amux cargo SUPERVISOR failed, not the cargo command it supervises; '
                  'retry, and file this traceback against AMUX-4758 rather than the build',
             traceback=traceback.format_exc())
        return 75


if __name__ == '__main__':
    sys.exit(main())
