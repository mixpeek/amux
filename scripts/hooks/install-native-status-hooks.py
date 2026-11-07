#!/usr/bin/env python3
"""Install passive observers without touching unrelated hooks or hook trust."""
import argparse
import json
from pathlib import Path
import shlex

# 10 s, not 3 (AF-1054): under load the hook's runs reached 2,955 ms and the
# provider killed the ones past 3 s, losing their edge (a lost Stop read the
# lane as mid-turn for hours). The send is already detached, so a normal run
# takes a few ms; the room is for an overloaded host.
HOOK_TIMEOUT_S = 10

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
        group = {'hooks': [{'type': 'command', 'command': command, 'timeout': HOOK_TIMEOUT_S}]}
        if event in ('PreToolUse', 'PermissionRequest', 'PostToolUse', 'PostToolUseFailure'):
            group['matcher'] = '.*'
        hooks.setdefault(event, []).append(group)
    return data


def wire_boundary_guard(data, guard):
    """Replace amux's boundary-guard PreToolUse entry with the canonical one. Cloud servers get
    their hooks from this script, not install.sh, so before 2026-11-06 they never ran the guard
    (`amux boundaries` read installed=False wired=False on the sandbox)."""
    import importlib.util
    spec = importlib.util.spec_from_file_location(
        'claude_hooks', Path(__file__).with_name('install-claude-status-hooks.py'))
    claude_hooks = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(claude_hooks)
    markers = claude_hooks.BOUNDARY_GUARD_MARKERS
    pre = []
    for group in data.setdefault('hooks', {}).get('PreToolUse', []):
        items = [h for h in group.get('hooks', []) if not any(m in h.get('command', '') for m in markers)]
        if items:
            pre.append(dict(group, hooks=items))
    data['hooks']['PreToolUse'] = pre + claude_hooks.canonical_boundary_guard(str(guard))
    return data


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--provider', required=True, choices=('codex', 'claude'))
    parser.add_argument('--settings', type=Path, required=True)
    parser.add_argument('--script', type=Path, required=True)
    parser.add_argument('--boundary-guard', type=Path, default=None,
                        help='also wire the boundaries.toml PreToolUse guard (claude only), with the '
                             'canonical matcher from install-claude-status-hooks.py')
    args = parser.parse_args()
    data = json.loads(args.settings.read_text()) if args.settings.exists() else {}
    result = merge(data, args.provider, args.script)
    if args.boundary_guard and args.provider == 'claude':
        result = wire_boundary_guard(result, args.boundary_guard)
    args.settings.parent.mkdir(parents=True, exist_ok=True)
    tmp = args.settings.with_suffix('.status-hook.tmp')
    tmp.write_text(json.dumps(result, indent=2) + '\n')
    tmp.chmod(args.settings.stat().st_mode & 0o777 if args.settings.exists() else 0o600)
    tmp.replace(args.settings)
    print('Installed passive ' + args.provider + ' observations; provider hook trust is unchanged.')
