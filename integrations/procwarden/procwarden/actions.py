"""Killing processes — with the safety gate enforced in one place."""
from __future__ import annotations

import signal
import time
from dataclasses import dataclass

import psutil

from .safety import FATAL, HIGH, MEDIUM, Safety


@dataclass
class GateDecision:
    allowed: bool       # may we proceed at all?
    needs_override: bool  # required --force / explicit override?
    note: str


def gate(safety: Safety, force: bool) -> GateDecision:
    """Decide whether a kill may proceed given the process safety + force flag.

    FATAL is *never* allowed through this gate — there is no force flag here on
    purpose. HIGH/MEDIUM require an explicit override (force=True).
    """
    if safety is None:
        return GateDecision(True, False, "")
    if safety.level == FATAL:
        return GateDecision(False, False,
                            "REFUSED: " + safety.reason)
    if safety.level in (HIGH, MEDIUM):
        if not force:
            return GateDecision(False, True,
                                f"needs --force ({safety.level}: {safety.reason})")
        return GateDecision(True, True, f"override accepted ({safety.level})")
    return GateDecision(True, False, "")


def kill_pid(pid, sig=signal.SIGTERM, escalate=False, wait=2.0):
    """Send `sig` to pid. If escalate and it survives `wait`s after SIGTERM,
    follow up with SIGKILL. Returns (ok, message)."""
    try:
        p = psutil.Process(pid)
    except psutil.NoSuchProcess:
        return False, "no such process (already gone)"
    name = p.name()
    try:
        p.send_signal(sig)
    except psutil.NoSuchProcess:
        return True, f"{name} already exited"
    except psutil.AccessDenied:
        return False, f"permission denied (try sudo) for {name}"

    if sig in (signal.SIGTERM, signal.SIGINT) and escalate:
        gone, _alive = psutil.wait_procs([p], timeout=wait)
        if not gone:
            try:
                p.send_signal(signal.SIGKILL)
                return True, f"{name} ignored SIGTERM -> sent SIGKILL"
            except psutil.NoSuchProcess:
                return True, f"{name} exited"
            except psutil.AccessDenied:
                return False, f"SIGKILL denied (try sudo) for {name}"
    return True, f"sent {signal.Signals(sig).name} to {name} (pid {pid})"
