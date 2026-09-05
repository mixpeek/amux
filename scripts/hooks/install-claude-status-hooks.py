#!/usr/bin/env python3
"""Idempotently wire amux's canonical Claude lifecycle reporter.

Unrelated settings and hooks are preserved. Older amux report commands are
removed before the canonical six-event set is added, so re-running install
cannot multiply reports or leave an inline fork active beside the real hook.
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


def is_amux_report(command: Any) -> bool:
    if not isinstance(command, str):
        return False
    return any(marker in command for marker in REPORT_MARKERS) or (
        "/api/sessions/" in command and "/report" in command
    )


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
        "SubagentStart": group(f"{base} subagent-start subagent-start-hook"),
        "SubagentStop": group(f"{base} subagent-stop subagent-stop-hook"),
    }


def merge(data: dict[str, Any], hook_path: str) -> dict[str, Any]:
    raw_hooks = data.setdefault("hooks", {})
    if not isinstance(raw_hooks, dict):
        raise ValueError("settings 'hooks' must be an object")

    # Remove only amux report commands. A group can contain unrelated commands
    # beside one old reporter; keep the group and every unrelated hook intact.
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
                if not (isinstance(item, dict) and is_amux_report(item.get("command")))
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
    args = parser.parse_args()

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
        merged = merge(data, args.hook_path)
    except ValueError as exc:
        raise SystemExit(f"refusing to rewrite {args.settings}: {exc}")
    write_atomic(args.settings, merged)
    print(f"wired amux status hooks in {args.settings}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
