"""Install procwarden's recurring sweep as a macOS LaunchAgent.

launchd is the native way to run an unattended CLI on a schedule: it survives
reboots, logs out-of-band, and needs no daemon of our own. We bake in the
exact python interpreter + project path so it works regardless of launchd's
minimal PATH.
"""
from __future__ import annotations

import os
import plistlib
import subprocess
import sys

LABEL = "com.procwarden.sweep"
MAINT_LABEL = "com.procwarden.maintain"
LA_DIR = os.path.expanduser("~/Library/LaunchAgents")
PLIST_PATH = os.path.join(LA_DIR, f"{LABEL}.plist")
MAINT_PLIST_PATH = os.path.join(LA_DIR, f"{MAINT_LABEL}.plist")

_CFG_DIR = os.path.expanduser("~/.procwarden")
STDOUT_LOG = os.path.join(_CFG_DIR, "launchd.out.log")
STDERR_LOG = os.path.join(_CFG_DIR, "launchd.err.log")
MAINT_OUT_LOG = os.path.join(_CFG_DIR, "maintain.out.log")
MAINT_ERR_LOG = os.path.join(_CFG_DIR, "maintain.err.log")

_PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _uid_domain():
    return f"gui/{os.getuid()}"


def build_plist(interval_minutes):
    return {
        "Label": LABEL,
        "ProgramArguments": [
            sys.executable, "-m", "procwarden.cli", "sweep", "--quiet",
        ],
        "EnvironmentVariables": {
            "PYTHONPATH": _PROJECT_ROOT,
            "PATH": "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        },
        "WorkingDirectory": _PROJECT_ROOT,
        "StartInterval": int(interval_minutes) * 60,
        "RunAtLoad": False,
        "StandardOutPath": STDOUT_LOG,
        "StandardErrorPath": STDERR_LOG,
        "ProcessType": "Background",
        "LowPriorityIO": True,
        "Nice": 5,
    }


def build_maint_plist(hour, minute):
    """Nightly `procwarden maintain` at a fixed wall-clock time.

    StartCalendarInterval (not StartInterval) so it fires once a day; launchd
    runs a missed job at next wake if the Mac was asleep at the scheduled time."""
    return {
        "Label": MAINT_LABEL,
        "ProgramArguments": [
            sys.executable, "-m", "procwarden.cli", "maintain", "--quiet",
        ],
        "EnvironmentVariables": {
            "PYTHONPATH": _PROJECT_ROOT,
            "PATH": "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        },
        "WorkingDirectory": _PROJECT_ROOT,
        "StartCalendarInterval": {"Hour": int(hour), "Minute": int(minute)},
        "RunAtLoad": False,
        "StandardOutPath": MAINT_OUT_LOG,
        "StandardErrorPath": MAINT_ERR_LOG,
        "ProcessType": "Background",
        "LowPriorityIO": True,
        "Nice": 5,
    }


def _bootstrap(plist_path):
    """(Re)load a plist into the user GUI domain, tolerating older launchctl."""
    subprocess.run(["launchctl", "bootout", _uid_domain(), plist_path],
                   capture_output=True)
    r = subprocess.run(["launchctl", "bootstrap", _uid_domain(), plist_path],
                       capture_output=True, text=True)
    if r.returncode != 0:
        subprocess.run(["launchctl", "unload", plist_path], capture_output=True)
        subprocess.run(["launchctl", "load", "-w", plist_path], capture_output=True)


def install(interval_minutes=30):
    os.makedirs(LA_DIR, exist_ok=True)
    os.makedirs(_CFG_DIR, exist_ok=True)
    # ensure a config exists so the first run has rules
    from . import config as cfgmod
    cfgmod.write_default(overwrite=False)

    with open(PLIST_PATH, "wb") as f:
        plistlib.dump(build_plist(interval_minutes), f)
    _bootstrap(PLIST_PATH)
    return PLIST_PATH


def install_nightly(hour=3, minute=0):
    """Install the nightly maintenance LaunchAgent (default 03:00)."""
    os.makedirs(LA_DIR, exist_ok=True)
    os.makedirs(_CFG_DIR, exist_ok=True)
    from . import config as cfgmod
    cfgmod.write_default(overwrite=False)

    with open(MAINT_PLIST_PATH, "wb") as f:
        plistlib.dump(build_maint_plist(hour, minute), f)
    _bootstrap(MAINT_PLIST_PATH)
    return MAINT_PLIST_PATH


def uninstall(maintain=False):
    path = MAINT_PLIST_PATH if maintain else PLIST_PATH
    subprocess.run(["launchctl", "bootout", _uid_domain(), path],
                   capture_output=True)
    subprocess.run(["launchctl", "unload", path], capture_output=True)
    if os.path.exists(path):
        os.remove(path)


def _one_status(label, path, out_log, err_log):
    if not os.path.exists(path):
        return f"{label}: not installed"
    r = subprocess.run(["launchctl", "list"], capture_output=True, text=True)
    loaded = any(label in line for line in r.stdout.splitlines())
    return "\n".join([
        f"{label}:",
        f"  plist:  {path}",
        f"  loaded: {'yes' if loaded else 'no'}",
        f"  stdout: {out_log}",
        f"  stderr: {err_log}",
    ])


def status():
    return "\n".join([
        _one_status(LABEL, PLIST_PATH, STDOUT_LOG, STDERR_LOG),
        _one_status(MAINT_LABEL, MAINT_PLIST_PATH, MAINT_OUT_LOG, MAINT_ERR_LOG),
    ])
