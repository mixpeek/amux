#!/usr/bin/env python3
"""Idempotently wire amux's canonical Claude lifecycle and read-routing hooks.

Unrelated settings and hooks are preserved. Older amux report commands are
removed before the canonical six-event set is added, and the large-read router
is installed once for both Read and Bash. Re-running install cannot multiply
hooks or leave an inline fork active beside the real scripts.
"""

from __future__ import annotations

import argparse
import json
import os
import stat
import tempfile
from pathlib import Path
from typing import Any


REPORT_MARKERS = ("hook-report.sh", "amux-report.sh")
READ_GUARD_MARKERS = ("large-read-guard.py",)
ASK_GUARD_MARKERS = ("ask-guard.py",)
# mail-app-guard.py was folded into boundary-guard.py; an old wiring of it is
# removed as managed rather than left running beside its replacement.
BOUNDARY_GUARD_MARKERS = ("boundary-guard.py", "mail-app-guard.py")


def is_amux_report(command: Any) -> bool:
    if not isinstance(command, str):
        return False
    return any(marker in command for marker in REPORT_MARKERS) or (
        "/api/sessions/" in command and "/report" in command
    )


def is_amux_read_guard(command: Any) -> bool:
    return isinstance(command, str) and any(marker in command for marker in READ_GUARD_MARKERS)


def is_amux_ask_guard(command: Any) -> bool:
    return isinstance(command, str) and any(marker in command for marker in ASK_GUARD_MARKERS)


def is_amux_boundary_guard(command: Any) -> bool:
    return isinstance(command, str) and any(marker in command for marker in BOUNDARY_GUARD_MARKERS)


def group(command: str, matcher: str | None = None) -> dict[str, Any]:
    out: dict[str, Any] = {
        "hooks": [{"type": "command", "command": command, "timeout": 10}]
    }
    if matcher is not None:
        out["matcher"] = matcher
    return out


def canonical(hook_path: str) -> dict[str, dict[str, Any]]:
    quoted = '"' + hook_path.replace('"', '\\"') + '"'
    base = f"bash {quoted}"
    return {
        # SessionStart is the leak bound for a process that died before its
        # final SubagentStop. hook-report skips source=compact because compact
        # preserves the process and its live background agents.
        "SessionStart": group(f"{base} subagent-reset session-start-hook"),
        "UserPromptSubmit": group(f"{base} active prompt-hook"),
        "PostToolUse": group(f"{base} active tool-hook", ".*"),
        "Stop": group(f"{base} idle stop-hook"),
        # THE MISSING PRODUCER FOR `blocked` (AMUX-4723). The server has
        # accepted the state since sessions_legacy.rs:145 and carries a test for
        # it, `lane_is_blocked()` reads it, and one caller refuses automation
        # sends into a lane parked on a dialog with a 409. Nothing ever set it:
        # 0 of 43,562 status reports, because a rejected call, a permission
        # prompt and a finished turn all ended on `Stop` and reported `idle`.
        #
        # NO MATCHER ON PURPOSE. Notification covers several types and the hook
        # discriminates on the payload's own `notification_type` instead, which
        # is testable here and does not depend on matcher semantics for this
        # event being what I assume. It costs a no-op hook run per non-permission
        # notification and buys a filter whose behaviour is pinned by a test.
        #
        # THE CLEARING EDGES ALREADY EXIST, which is what makes this safe to set
        # at all: an approval runs the tool and PostToolUse reports `active`; a
        # rejection or an ended turn reports `idle` via Stop. Both are above and
        # both predate this change.
        "Notification": group(f"{base} blocked notification-hook"),
        "SubagentStart": group(f"{base} subagent-start subagent-start-hook"),
        "SubagentStop": group(f"{base} subagent-stop subagent-stop-hook"),
    }


def canonical_read_guard(hook_path: str) -> list[dict[str, Any]]:
    quoted = '"' + hook_path.replace('"', '\\"') + '"'
    command = f"python3 {quoted}"
    return [group(command, "Read"), group(command, "Bash")]


def canonical_ask_guard(hook_path: str) -> list[dict[str, Any]]:
    # AMUX-5234: AskUserQuestion under an active /goal becomes a needsyou card.
    # ANCHORED, for the reason the subagent matcher in hook-report.sh gives:
    # the matcher is an unanchored regex over the tool name.
    # FAILS OPEN WHEN THE SCRIPT IS MISSING, as the script itself does on every
    # other fault. `python3 <missing file>` exits 2, which Claude Code reads as
    # a deliberate block, so a settings entry that outlived its script turned
    # every AskUserQuestion into "can't open file" (2026-10-21: the entry was
    # registered, ~/.amux/hooks/ask-guard.py was absent, and no ask-guard.jsonl
    # line had ever been written). The missing case is logged with its own
    # verdict so a sweep of ask-guard.jsonl finds it.
    quoted = '"' + hook_path.replace('"', '\\"') + '"'
    command = (
        f"f={quoted}; if [ -f \"$f\" ]; then exec python3 \"$f\"; fi; "
        'mkdir -p "$HOME/.amux/logs" && '
        'printf \'{"ts":%s,"verdict":"script_missing","path":"%s"}\\n\' "$(date +%s)" "$f" '
        '>> "$HOME/.amux/logs/ask-guard.jsonl"; exit 0'
    )
    return [group(command, "^AskUserQuestion$")]


def canonical_boundary_guard(hook_path: str) -> list[dict[str, Any]]:
    # boundaries.toml, enforced by one guard. Anchored for the reason
    # canonical_ask_guard gives: the matcher is an unanchored regex. Agent and
    # Task carry the `subagents` boundary (the running-subagent cap).
    quoted = '"' + hook_path.replace('"', '\\"') + '"'
    return [group(f"python3 {quoted}", "^(Bash|Read|Edit|Write|MultiEdit|NotebookEdit|Grep|Glob|Agent|Task)$")]


# What an event's HANDLER must contain for the wiring to mean anything
# (AMUX-4783). This script writes settings.json; only install.sh copies
# hook-report.sh. Those are two different commands, and on 2026-09-18 they came
# apart: Notification was wired at 04:27 against a 2026-09-04 script with no
# notification_type discriminator, so every notification type reported
# `blocked`, pinning lanes for the 600s trust window for about four hours.
#
# settings.json passes the literal argument `blocked`, so the SCRIPT is what
# decides which notification types actually mean blocked. Wiring the event
# while the handler cannot discriminate is not a partial install, it is an
# active fault — worse than not wiring it at all.
HANDLER_REQUIREMENTS = {"Notification": "notification_type"}


def unsupported_events(hook_source: str | None) -> list[str]:
    """Canonical events whose handler cannot answer them.

    An unreadable handler returns nothing: this refuses on what it can SEE, and
    a missing file is already the caller's problem to report. Returning [] for
    "I could not look" would be the same false-negative the invariant that
    checks this wiring used to have.
    """
    if hook_source is None:
        return []
    return [event for event, token in HANDLER_REQUIREMENTS.items() if token not in hook_source]


def read_hook_source(hook_path: str) -> str | None:
    """The INSTALLED handler's bytes, resolving the shell-style $HOME the
    settings command uses. Never the recorded .sha256 beside it: on this box
    that record read 840a65d4 against an installed 4ba44266 and nothing ever
    compared the two, so it certified a file it had not seen."""
    resolved = Path(os.path.expandvars(hook_path))
    try:
        return resolved.read_text()
    except OSError:
        return None


def merge(
    data: dict[str, Any],
    hook_path: str,
    read_guard_path: str = "$HOME/.amux/hooks/large-read-guard.py",
    ask_guard_path: str | None = None,
    boundary_guard_path: str | None = None,
) -> dict[str, Any]:
    raw_hooks = data.setdefault("hooks", {})
    if not isinstance(raw_hooks, dict):
        raise ValueError("settings 'hooks' must be an object")

    # Remove only managed amux commands. A group can contain unrelated commands
    # beside one old hook; keep the group and every unrelated hook intact.
    for event, groups in list(raw_hooks.items()):
        if not isinstance(groups, list):
            raise ValueError(f"settings hooks.{event} must be an array")
        kept_groups = []
        for raw_group in groups:
            if not isinstance(raw_group, dict):
                kept_groups.append(raw_group)
                continue
            commands = raw_group.get("hooks")
            if not isinstance(commands, list):
                kept_groups.append(raw_group)
                continue
            kept_commands = [
                item
                for item in commands
                if not (
                    isinstance(item, dict)
                    and (
                        is_amux_report(item.get("command"))
                        or is_amux_read_guard(item.get("command"))
                        or (ask_guard_path is not None and is_amux_ask_guard(item.get("command")))
                        or (boundary_guard_path is not None and is_amux_boundary_guard(item.get("command")))
                    )
                )
            ]
            if kept_commands:
                next_group = dict(raw_group)
                next_group["hooks"] = kept_commands
                kept_groups.append(next_group)
        if kept_groups:
            raw_hooks[event] = kept_groups
        else:
            raw_hooks.pop(event, None)

    for event, report_group in canonical(hook_path).items():
        raw_hooks.setdefault(event, []).append(report_group)
    raw_hooks.setdefault("PreToolUse", []).extend(canonical_read_guard(read_guard_path))
    if ask_guard_path is not None:
        raw_hooks["PreToolUse"].extend(canonical_ask_guard(ask_guard_path))
    if boundary_guard_path is not None:
        raw_hooks["PreToolUse"].extend(canonical_boundary_guard(boundary_guard_path))
    return data


def write_atomic(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    mode = stat.S_IMODE(path.stat().st_mode) if path.exists() else 0o600
    encoded = (json.dumps(data, indent=2, ensure_ascii=False) + "\n").encode()
    fd, temp_name = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as stream:
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp_name, path)
    except BaseException:
        try:
            os.unlink(temp_name)
        except FileNotFoundError:
            pass
        raise


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--settings",
        type=Path,
        default=Path.home() / ".claude" / "settings.json",
    )
    parser.add_argument("--hook-path", default="$HOME/.amux/hook-report.sh")
    parser.add_argument(
        "--read-guard-path",
        default="$HOME/.amux/hooks/large-read-guard.py",
    )
    parser.add_argument(
        "--ask-guard-path",
        default=None,
        help="wire the AskUserQuestion goal guard (AMUX-5234); omitted = leave any existing wiring alone",
    )
    parser.add_argument(
        "--boundary-guard-path",
        default=None,
        help="wire the boundaries.toml guard; omitted = leave any existing wiring alone",
    )
    parser.add_argument(
        "--allow-unsupported-events",
        action="store_true",
        help="wire canonical events even when the installed handler cannot answer them",
    )
    args = parser.parse_args()

    # REFUSE BEFORE WRITING. The failure this prevents is not a missing hook,
    # it is a wired hook backed by a handler that answers every notification
    # with `blocked`. Checked against the installed bytes, and the remedy names
    # the command that ships the handler, because that is the half a caller
    # running this script has not run.
    unsupported = unsupported_events(read_hook_source(args.hook_path))
    if unsupported and not args.allow_unsupported_events:
        raise SystemExit(
            f"refusing to wire {', '.join(unsupported)}: the installed handler at "
            f"{args.hook_path} does not support "
            f"{', '.join(HANDLER_REQUIREMENTS[e] for e in unsupported)}. "
            "This script writes settings.json only; run install.sh to ship the handler, "
            "then re-run. (--allow-unsupported-events overrides, and is how AMUX-4783 "
            "happened by accident.)"
        )

    if args.settings.exists():
        try:
            data = json.loads(args.settings.read_text())
        except json.JSONDecodeError as exc:
            raise SystemExit(f"refusing to overwrite invalid JSON in {args.settings}: {exc}")
        if not isinstance(data, dict):
            raise SystemExit(f"refusing to overwrite non-object settings in {args.settings}")
    else:
        data = {}
    try:
        merged = merge(data, args.hook_path, args.read_guard_path, args.ask_guard_path, args.boundary_guard_path)
    except ValueError as exc:
        raise SystemExit(f"refusing to rewrite {args.settings}: {exc}")
    write_atomic(args.settings, merged)
    print(f"wired amux status and read-routing hooks in {args.settings}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
