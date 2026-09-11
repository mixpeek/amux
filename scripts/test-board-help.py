#!/usr/bin/env python3
"""Real CLI help/unknown-option paths must never register a board artifact."""
import os
from pathlib import Path
import subprocess
import tempfile

source = Path(__file__).resolve().parents[1] / 'amux'
with tempfile.TemporaryDirectory(prefix='amux-board-help-') as directory:
    root = Path(directory)
    curl = root / 'curl'
    curl.write_text('#!/bin/sh\nprintf called >> "$HELP_TEST_ROOT/calls"\nprintf "{}"\n')
    curl.chmod(0o755)
    env = {k:v for k,v in os.environ.items() if not k.startswith(('AMUX_', 'CC_')) and k not in ('TMUX', 'TMUX_PANE')}
    env.update(CC_HOME=directory, AMUX_HOME=directory, AMUX_API='https://localhost:1', HELP_TEST_ROOT=directory, PATH=directory+':'+os.environ['PATH'])
    for verb in ['decompose', 'artifact']:
        for args in [['--help'], ['LCT-1', '--help']]:
            result = subprocess.run(['bash', str(source), 'board', verb, *args], env=env, capture_output=True, text=True, timeout=10)
            assert result.returncode == 0, (verb, args, result.stderr)
            assert 'Usage: amux board '+verb in result.stdout
            assert not (root/'calls').exists(), 'Help performed a network mutation'
            if verb == 'decompose':
                assert '"tasks"' in result.stdout and '1-based' in result.stdout
            print('PASS', verb, *args, ': no network request')
    bad = subprocess.run(['bash', str(source), 'board', 'artifact', 'LCT-1', '--unknown'], env=env, capture_output=True, text=True, timeout=10)
    assert bad.returncode != 0 and not (root/'calls').exists(), bad
    print('PASS unknown artifact flag: refused without a network request')
