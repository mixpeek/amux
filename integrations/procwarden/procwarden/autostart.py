"""Install the procwarden menu bar app as a login item via LaunchAgent.

Unlike the sweep agent (a background CLI on a timer), the menu bar is a GUI
app: it must run inside the user's Aqua (window-server) session, start at
login, and be relaunched if it ever quits. We pin the exact interpreter +
project path so launchd's minimal environment can still import the package.
"""
from __future__ import annotations

import os
import plistlib
import subprocess
import sys

LABEL = "com.procwarden.menubar"
LA_DIR = os.path.expanduser("~/Library/LaunchAgents")
PLIST_PATH = os.path.join(LA_DIR, f"{LABEL}.plist")

_CFG_DIR = os.path.expanduser("~/.procwarden")
STDOUT_LOG = os.path.join(_CFG_DIR, "menubar.out.log")
STDERR_LOG = os.path.join(_CFG_DIR, "menubar.err.log")

_PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _uid_domain():
    return f"gui/{os.getuid()}"


def build_plist():
    return {
        "Label": LABEL,
        "ProgramArguments": [
            sys.executable, "-m", "procwarden.cli", "menubar",
        ],
        "EnvironmentVariables": {
            "PYTHONPATH": _PROJECT_ROOT,
            "PATH": "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        },
        "WorkingDirectory": _PROJECT_ROOT,
        # GUI app: start at login and bring it back if it dies/quits.
        "RunAtLoad": True,
        "KeepAlive": True,
        # only load it inside the graphical login session — never headless.
        "LimitLoadToSessionType": "Aqua",
        "ProcessType": "Interactive",
        "StandardOutPath": STDOUT_LOG,
        "StandardErrorPath": STDERR_LOG,
    }


def install():
    os.makedirs(LA_DIR, exist_ok=True)
    os.makedirs(_CFG_DIR, exist_ok=True)

    with open(PLIST_PATH, "wb") as f:
        plistlib.dump(build_plist(), f)

    # reload if already present, then bootstrap (fall back to legacy load)
    subprocess.run(["launchctl", "bootout", _uid_domain(), PLIST_PATH],
                   capture_output=True)
    r = subprocess.run(["launchctl", "bootstrap", _uid_domain(), PLIST_PATH],
                       capture_output=True, text=True)
    if r.returncode != 0:
        subprocess.run(["launchctl", "unload", PLIST_PATH], capture_output=True)
        subprocess.run(["launchctl", "load", "-w", PLIST_PATH], capture_output=True)
    # nudge it to start right now without waiting for the next login
    subprocess.run(["launchctl", "kickstart", f"{_uid_domain()}/{LABEL}"],
                   capture_output=True)
    return PLIST_PATH


def uninstall():
    subprocess.run(["launchctl", "bootout", _uid_domain(), PLIST_PATH],
                   capture_output=True)
    subprocess.run(["launchctl", "unload", PLIST_PATH], capture_output=True)
    if os.path.exists(PLIST_PATH):
        os.remove(PLIST_PATH)


def status():
    if not os.path.exists(PLIST_PATH):
        return "not installed"
    r = subprocess.run(["launchctl", "list"], capture_output=True, text=True)
    loaded = any(LABEL in line for line in r.stdout.splitlines())
    lines = [f"plist:  {PLIST_PATH}",
             f"loaded: {'yes' if loaded else 'no'}",
             f"stdout: {STDOUT_LOG}",
             f"stderr: {STDERR_LOG}"]
    return "\n".join(lines)
