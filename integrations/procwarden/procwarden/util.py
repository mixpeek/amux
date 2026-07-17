"""Formatting + small terminal helpers."""
from __future__ import annotations

import sys

from .safety import NONE, LOW, MEDIUM, HIGH, FATAL


def human_bytes(n):
    n = float(n or 0)
    for unit in ("B", "K", "M", "G", "T", "P"):
        if abs(n) < 1024.0:
            if unit == "B":
                return f"{int(n)}{unit}"
            return f"{n:.1f}{unit}"
        n /= 1024.0
    return f"{n:.1f}E"


def human_duration(seconds):
    seconds = int(seconds or 0)
    if seconds < 0:
        return "?"
    d, rem = divmod(seconds, 86400)
    h, rem = divmod(rem, 3600)
    m, s = divmod(rem, 60)
    if d:
        return f"{d}d{h}h"
    if h:
        return f"{h}h{m}m"
    if m:
        return f"{m}m{s}s"
    return f"{s}s"


# --- color -----------------------------------------------------------------
_USE_COLOR = sys.stdout.isatty()

_CODES = {
    "reset": "\033[0m", "bold": "\033[1m", "dim": "\033[2m",
    "red": "\033[31m", "green": "\033[32m", "yellow": "\033[33m",
    "blue": "\033[34m", "magenta": "\033[35m", "cyan": "\033[36m",
    "gray": "\033[90m",
}

LEVEL_COLOR = {
    NONE: "gray", LOW: "green", MEDIUM: "yellow", HIGH: "magenta", FATAL: "red",
}


def set_color(enabled):
    global _USE_COLOR
    _USE_COLOR = enabled


def c(text, *styles):
    if not _USE_COLOR or not styles:
        return str(text)
    prefix = "".join(_CODES.get(s, "") for s in styles)
    return f"{prefix}{text}{_CODES['reset']}"


def level_colored(level):
    return c(level, LEVEL_COLOR.get(level, "reset"))
