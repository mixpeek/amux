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

    # AMUX-5037. `fan-out` must REFUSE, not call a route that no longer exists.
    #
    # c5601217 ("retire replaced orchestration") deleted the axum route, its
    # 968-line handler, api/orchestrations.rs and the 1479-line fan_out_e2e.rs,
    # and never touched the CLI. So the client half kept POSTing to a path that
    # answers 404 while `amux board` advertised it in its own help. Measured
    # across 3.7M rows of _amux_request_log, the only request ever made to
    # /api/board/<id>/fan-out was the 404 probe run while diagnosing it.
    #
    # The curl stub above is what makes this assertable: a regression that
    # re-adds the call shows up as a recorded network request, not as prose.
    gone = subprocess.run(['bash', str(source), 'board', 'fan-out', 'LCT-1'], env=env, capture_output=True, text=True, timeout=10)
    assert gone.returncode != 0, 'a retired verb must fail, not appear to work'
    assert not (root/'calls').exists(), 'fan-out still reaches the network for a 404 route'
    assert 'retired' in gone.stderr, gone.stderr
    assert 'decompose' in gone.stderr, 'the refusal must name what to use instead: ' + gone.stderr
    print('PASS fan-out: refused, names the replacement, no network request')

    # And the help must not advertise it. A verb that 404s is worse in the help
    # than absent from it.
    help_out = subprocess.run(['bash', str(source), 'board'], env=env, capture_output=True, text=True, timeout=10)
    combined = help_out.stdout + help_out.stderr
    assert 'amux board decompose' in combined, 'the help itself did not render; this cell would pass vacuously'
    assert 'amux board fan-out <EPIC-ID>' not in combined, 'board help still advertises the retired fan-out verb'
    print('PASS board help: no longer advertises fan-out')
