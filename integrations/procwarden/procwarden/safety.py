"""Safety classification for processes.

The whole point of this module is to stop you (or the menu bar) from killing
something that will hard-crash your login session or the OS. Every process gets
a disruption level and a `killable` flag. The CLI/menu bar refuse to kill
anything that isn't `killable` unless you pass an explicit override.
"""
from __future__ import annotations

from dataclasses import dataclass

# --- disruption levels (ordered) -------------------------------------------
NONE = "NONE"      # already dead / no impact
LOW = "LOW"        # your app or an auto-relaunching agent; safe
MEDIUM = "MEDIUM"  # someone else's / root, non-system — verify first
HIGH = "HIGH"      # root-owned Apple system daemon — may degrade a feature
FATAL = "FATAL"    # killing this crashes your session or the machine

LEVEL_ORDER = {NONE: 0, LOW: 1, MEDIUM: 2, HIGH: 3, FATAL: 4}

# Processes that must NEVER be killed. Doing so logs you out, freezes the GUI,
# or panics the kernel. The tool hard-refuses these.
FATAL_NAMES = {
    "kernel_task", "launchd", "WindowServer", "loginwindow", "logind",
    "configd", "powerd", "kernelmanagerd", "kextd", "diskarbitrationd",
    "opendirectoryd", "securityd", "trustd", "syslogd", "notifyd",
    "distnoted", "cfprefsd", "mds", "mds_stores", "mdsync", "coreaudiod",
    "hidd", "backboardd", "amfid", "watchdogd", "UserEventAgent",
    "thermalmonitord", "bluetoothd", "endpointsecurityd", "syspolicyd",
    "runningboardd", "nsurlsessiond",
}

# Apple UI agents that launchd/loginwindow relaunch instantly. Killing them is
# a harmless "restart" — handy when the Dock or Finder wedges.
RELAUNCH_OK = {
    "Finder", "Dock", "SystemUIServer", "ControlCenter", "NotificationCenter",
    "Spotlight", "TextInputMenuAgent", "talagent", "WindowManager",
    "ViewBridgeAuxiliary",
}

# Executable path prefixes that mark an OS-shipped binary.
SYSTEM_PATH_PREFIXES = (
    "/System/", "/usr/libexec/", "/usr/sbin/", "/sbin/", "/usr/bin/",
    "/Library/Apple/",
)


@dataclass
class Safety:
    level: str       # one of the levels above
    killable: bool   # may the tool kill this without an explicit override?
    reason: str      # human explanation


def classify(pid, ppid, name, exe, username, status, current_user):
    """Return a Safety verdict for one process."""
    if pid in (0, 1):
        return Safety(FATAL, False, f"core OS process (PID {pid}) — never kill")
    if name in FATAL_NAMES:
        return Safety(FATAL, False,
                      "critical system service — killing crashes the session/OS")
    if status == psutil_zombie():
        return Safety(NONE, False,
                      f"zombie — already dead; reaped when parent (PID {ppid}) exits")

    if name in RELAUNCH_OK:
        return Safety(LOW, True, "Apple UI agent — relaunches automatically if killed")

    exe = exe or ""
    is_system = any(exe.startswith(p) for p in SYSTEM_PATH_PREFIXES)

    if username == "root":
        if is_system:
            return Safety(HIGH, False,
                          "root Apple system daemon — may degrade a feature; "
                          "usually relaunched by launchd")
        return Safety(MEDIUM, False,
                      "root-owned process outside your account — verify before killing")

    if username == current_user:
        if is_system:
            return Safety(LOW, True, "your own Apple-provided agent — typically relaunches")
        return Safety(LOW, True, "your own process — safe to kill")

    return Safety(MEDIUM, False, f"owned by another user ({username})")


def psutil_zombie():
    # Local import keeps this module importable even if psutil is missing.
    try:
        import psutil
        return psutil.STATUS_ZOMBIE
    except Exception:
        return "zombie"
