#!/usr/bin/env python3
"""Claude PreToolUse router for large, untargeted text reads.

The primary model keeps the user's question and all engineering judgment. A
full read above AMUX_BULK_READ_LINES (default 350) is refused with the exact
`amux delegate read` command that compresses caller-supplied bytes through the
configured helper model. Ranged reads of at most the threshold remain available
for edits and debugging.

This is routing, not a security boundary. It handles Claude's Read tool and the
ordinary direct-output shell readers that commonly bypass it. Pipelines and
redirections are allowed because their output is already being narrowed before
it enters context. Any parser/probe failure is fail-open and audit-logged.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import sys
import time
from typing import Any


DEFAULT_LINE_LIMIT = 350
MAX_AUDIT_BYTES = 4 * 1024 * 1024


def configured_limit() -> int:
    raw = os.environ.get("AMUX_BULK_READ_LINES", str(DEFAULT_LINE_LIMIT))
    try:
        value = int(raw)
    except ValueError:
        return DEFAULT_LINE_LIMIT
    return value if value > 0 else DEFAULT_LINE_LIMIT


def env_on(name: str) -> bool:
    return os.environ.get(name, "").strip().lower() in {"1", "true", "yes", "on"}


def is_managed_worker() -> bool:
    """Only alter amux-managed workers; raw/local Claude stays untouched."""
    if env_on("CC_ISOLATED"):
        return False
    return bool(os.environ.get("AMUX_SESSION") or os.environ.get("AMUX_WORKER"))


def audit(verdict: str, **fields: Any) -> None:
    """Append one bounded JSONL event where amux operators already sweep logs."""
    try:
        home = Path(os.environ.get("AMUX_HOME") or Path.home() / ".amux")
        log = home / "logs" / "read-delegation.jsonl"
        log.parent.mkdir(parents=True, exist_ok=True)
        if log.exists() and log.stat().st_size > MAX_AUDIT_BYTES:
            rotated = log.with_suffix(".jsonl.1")
            try:
                os.replace(log, rotated)
            except OSError:
                pass
        record = {
            "ts": time.time(),
            "event": "bulk_read_guard",
            "verdict": verdict,
            "session": os.environ.get("AMUX_SESSION") or os.environ.get("AMUX_WORKER") or "",
            **fields,
        }
        with log.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n")
    except Exception:
        pass


def resolve_path(raw: str, cwd: str) -> Path:
    path = Path(os.path.expandvars(os.path.expanduser(raw)))
    return path if path.is_absolute() else Path(cwd) / path


def exceeds_limit(path: Path, limit: int) -> tuple[bool, int]:
    """Return (too_large, lines_seen); stop at limit+1 instead of scanning GBs."""
    if not path.is_file():
        return False, 0
    seen = 0
    with path.open("rb") as stream:
        for seen, _line in enumerate(stream, 1):
            if seen > limit:
                return True, seen
    return False, seen


def read_tool_paths(data: dict[str, Any], limit: int) -> list[tuple[str, int]]:
    tool_input = data.get("tool_input") or {}
    # A bounded Read is the primary model's truthful path for editing and
    # debugging. Offset alone is not bounded: it can still consume the rest of
    # a 20k-line file, so only an explicit small limit exempts the call.
    requested = tool_input.get("limit")
    if isinstance(requested, int) and 0 < requested <= limit:
        return []
    raw = str(tool_input.get("file_path") or "").strip()
    if not raw:
        return []
    path = resolve_path(raw, str(data.get("cwd") or os.getcwd()))
    too_large, seen = exceeds_limit(path, limit)
    return [(raw, seen)] if too_large else []


def simple_shell_argv(command: str) -> list[str]:
    """Parse only one direct-output command; complex shell programs pass."""
    lexer = shlex.shlex(command, posix=True, punctuation_chars="|&;<>")
    lexer.whitespace_split = True
    lexer.commenters = ""
    argv = []
    for token in lexer:
        # Stop at the first shell operator. shlex is not a heredoc parser:
        # lexing its body can mistake ordinary prose apostrophes for an
        # unterminated shell string and emit probe_failed on valid commands.
        if token and all(ch in "|&;<>" for ch in token):
            return []
        argv.append(token)
    while argv and "=" in argv[0] and argv[0].split("=", 1)[0].isidentifier():
        argv.pop(0)
    if argv and Path(argv[0]).name == "env":
        argv.pop(0)
        while argv and "=" in argv[0] and argv[0].split("=", 1)[0].isidentifier():
            argv.pop(0)
    if argv and Path(argv[0]).name == "command":
        argv.pop(0)
    return argv


def shell_reader_paths(command: str, cwd: str, limit: int) -> list[tuple[str, int]]:
    argv = simple_shell_argv(command)
    if not argv:
        return []
    reader = Path(argv[0]).name
    args = argv[1:]
    if reader not in {"cat", "less", "more", "head", "tail"}:
        return []

    # head/tail default to ten lines and are already bounded. Explicit counts at
    # or below the threshold are equally safe. `tail -n +1` is an unbounded
    # whole-file read and deliberately falls through to the file-size probe.
    if reader in {"head", "tail"}:
        count: str | None = None
        for index, arg in enumerate(args):
            if arg in {"-n", "--lines"} and index + 1 < len(args):
                count = args[index + 1]
                break
            if arg.startswith("--lines="):
                count = arg.split("=", 1)[1]
                break
            if arg.startswith("-") and arg[1:].isdigit():
                count = arg[1:]
                break
        if count is None:
            return []
        if not count.startswith("+"):
            try:
                if abs(int(count)) <= limit:
                    return []
            except ValueError:
                return []

    paths: list[str] = []
    skip_next = False
    for arg in args:
        if skip_next:
            skip_next = False
            continue
        if arg in {"-n", "--lines", "-c", "--bytes"}:
            skip_next = True
            continue
        if arg == "--":
            continue
        if arg.startswith("-"):
            continue
        paths.append(arg)

    blocked: list[tuple[str, int]] = []
    total_seen = 0
    for raw in paths:
        path = resolve_path(raw, cwd)
        too_large, seen = exceeds_limit(path, max(0, limit - total_seen))
        total_seen += seen
        if too_large or total_seen > limit:
            blocked.append((raw, total_seen))
            break
    return blocked


# GMA-123. The refusal recommended `amux delegate read`, which is a TEXT
# summarizer and died on the first byte of any PNG. So the guard was routing the
# one file type its remedy cannot open — and for a lane that ships images, the
# blocked artifact IS the deliverable.
#
# Extension-based, deliberately, not content sniffing: this runs on every Read,
# the path is all it has cheaply, and a wrong guess costs only a slightly worse
# suggestion. It is not a security boundary.
BINARY_SUFFIXES = frozenset(
    {
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".tif", ".ico",
        ".pdf", ".zip", ".gz", ".tar", ".bz2", ".xz", ".7z",
        ".mp4", ".mov", ".webm", ".mp3", ".wav", ".ogg",
        ".woff", ".woff2", ".ttf", ".otf", ".so", ".dylib", ".wasm",
    }
)


def looks_binary(label: str) -> bool:
    return Path(label).suffix.lower() in BINARY_SUFFIXES


def refusal(paths: list[tuple[str, int]], limit: int) -> int:
    labels = [raw for raw, _seen in paths]
    shown = ", ".join(shlex.quote(path) for path in labels)
    minimum = max(seen for _raw, seen in paths)
    audit(
        "delegation_required",
        tool="large_read",
        paths=labels,
        threshold_lines=limit,
        lines_seen_min=minimum,
    )
    # Name the remedy that WORKS for what was actually blocked. Recommending the
    # text helper for an image is worse than no advice: it sends the caller down
    # a path that ends in a decode error, which is how GMA-123 was found.
    binary = [lbl for lbl in labels if looks_binary(lbl)]
    if binary:
        audit("delegation_required_binary", tool="large_read", paths=binary)
        sys.stderr.write(
            f"BLOCKED by amux bulk-read router: {shown} would send more than {limit} "
            "untargeted lines to the primary model.\n"
            f"This looks BINARY ({', '.join(shlex.quote(b) for b in binary)}), so do NOT "
            "delegate — `amux delegate read` is a text summarizer and cannot open it.\n"
            "The primary model can see images directly. Read it with an explicit small "
            "limit, which is all this guard asks for:\n"
            f"  Read({shlex.quote(labels[0])}, limit={limit})\n"
            "Downscaling is not required.\n"
            "Audit: ~/.amux/logs/read-delegation.jsonl verdict=delegation_required_binary\n"
        )
        return 2
    sys.stderr.write(
        f"BLOCKED by amux bulk-read router: {shown} would send more than {limit} "
        "untargeted lines to the primary model.\n"
        "Delegate navigation/compression with:\n"
        f"  amux delegate read --question '<what you need to find or summarize>' -- {shown}\n"
        f"For editing or debugging, keep the primary model and use Read with an explicit "
        f"limit of {limit} lines or fewer (plus offset as needed).\n"
        "The helper cannot edit or make correctness, architecture, product, or security decisions.\n"
        "Audit: ~/.amux/logs/read-delegation.jsonl verdict=delegation_required\n"
    )
    return 2


def main() -> int:
    if os.environ.get("AMUX_BULK_READ_GUARD", "1").strip().lower() in {"0", "false", "off"}:
        return 0
    # The installer edits Claude's global settings, but the policy belongs to
    # amux workers. CC_ISOLATED deliberately strips the amux harness, while an
    # unrelated local Claude process has no worker identity; neither should be
    # changed just because the hook file is globally wired.
    if not is_managed_worker():
        return 0
    try:
        data = json.load(sys.stdin)
        limit = configured_limit()
        tool = data.get("tool_name")
        if tool == "Read":
            blocked = read_tool_paths(data, limit)
        elif tool == "Bash":
            tool_input = data.get("tool_input") or {}
            blocked = shell_reader_paths(
                str(tool_input.get("command") or ""),
                str(data.get("cwd") or os.getcwd()),
                limit,
            )
        else:
            return 0
        return refusal(blocked, limit) if blocked else 0
    except Exception as exc:
        audit("probe_failed", error=type(exc).__name__, detail=str(exc)[:240])
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
