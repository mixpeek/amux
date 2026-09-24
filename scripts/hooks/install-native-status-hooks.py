#!/usr/bin/env python3
"""Install passive observers without touching unrelated hooks or hook trust."""
import argparse
import json
from pathlib import Path
import shlex

COMMON = ('SessionStart', 'SessionEnd', 'UserPromptSubmit', 'PreToolUse',
          'PermissionRequest', 'PostToolUse', 'PreCompact', 'PostCompact', 'Stop')


def merge(data, provider, script):
    hooks = data.setdefault('hooks', {})
    # This observer coexists with the managed subagent/token producer. That
    # producer yields main-state authority on native-instrumented launches.
    for event, groups in list(hooks.items()):
        kept = []
        for group in groups:
            items = [h for h in group.get('hooks', []) if 'native-status.py' not in h.get('command', '')]
            if items:
                kept.append(dict(group, hooks=items))
        hooks[event] = kept
    events = COMMON + (('Interrupt',) if provider == 'codex' else ('Notification', 'PostToolUseFailure', 'StopFailure'))
    command = 'python3 ' + shlex.quote(str(script)) + ' ' + provider
    for event in events:
        group = {'hooks': [{'type': 'command', 'command': command, 'timeout': 3}]}
        if event in ('PreToolUse', 'PermissionRequest', 'PostToolUse', 'PostToolUseFailure'):
            group['matcher'] = '.*'
        hooks.setdefault(event, []).append(group)
    return data


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--provider', required=True, choices=('codex', 'claude'))
    parser.add_argument('--settings', type=Path, required=True)
    parser.add_argument('--script', type=Path, required=True)
    args = parser.parse_args()
    data = json.loads(args.settings.read_text()) if args.settings.exists() else {}
    result = merge(data, args.provider, args.script)
    args.settings.parent.mkdir(parents=True, exist_ok=True)
    tmp = args.settings.with_suffix('.status-hook.tmp')
    tmp.write_text(json.dumps(result, indent=2) + '\n')
    tmp.chmod(args.settings.stat().st_mode & 0o777 if args.settings.exists() else 0o600)
    tmp.replace(args.settings)
    print('Installed passive ' + args.provider + ' observations; provider hook trust is unchanged.')
