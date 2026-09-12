#!/usr/bin/env python3
"""Consolidated amux acceptance: one command, explicit scope, durable evidence.

Exit 0: selected automated stages passed (not a full product certification).
Exit 1: failure. Exit 2: incomplete / unavailable / skipped required coverage.
"""
from __future__ import annotations
import argparse
import hashlib
import html
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]


def inventory(root=ROOT):
    paths = set()
    for pattern in ('e2e/**/*.spec.ts', 'crates/amux-server/tests/*.rs', 'tests/*', 'scripts/test-*'):
        paths.update(p for p in root.glob(pattern) if p.is_file())
    return [{'path': str(p.relative_to(root)), 'sha256': hashlib.sha256(p.read_bytes()).hexdigest(),
             'kind': 'browser' if p.suffix == '.ts' else 'supporting',
             'uses_stubs_or_mocks': bool(re.search(r'page\.route|MockProtocol|fixtures/', p.read_text(errors='replace'))),
             'contains_conditional_coverage': bool(re.search(r'#\[ignore|test\.skip|SKIPPED', p.read_text(errors='replace')))}
            for p in sorted(paths)]


def browser_counts(data):
    counts = {'passed': 0, 'failed': 0, 'skipped': 0, 'flaky': 0}
    def visit(suite):
        for spec in suite.get('specs', []):
            for test in spec.get('tests', []):
                status = test.get('status', 'skipped')
                key = {'expected': 'passed', 'unexpected': 'failed', 'flaky': 'flaky', 'skipped': 'skipped'}.get(status, 'failed')
                # expected failures are useful controls, not evidence of successful UX.
                if test.get('expectedStatus') == 'skipped': key = 'skipped'
                counts[key] += 1
        for child in suite.get('suites', []): visit(child)
    visit(data)
    return counts


def classify(code, output, browser=None, live=False):
    if code != 0: return 'FAIL'
    if browser is not None:
        counts = browser_counts(browser)
        if counts['failed'] or counts['flaky']: return 'FAIL'
        if not counts['passed'] or counts['skipped']: return 'INCOMPLETE'
    if live and re.search(r'\bSKIP(?:PED)?\b', output, re.I): return 'INCOMPLETE'
    return 'PASS'


def report(out, state):
    (out / 'summary.json').write_text(json.dumps(state, indent=2) + '\n')
    esc = html.escape
    rows = ''.join(f'<tr><td>{esc(s["name"])}</td><td>{esc(s["status"])}</td>'
                   f'<td><a href="{esc(s.get("log", ""))}">log</a></td>'
                   f'<td>{esc(s.get("note", ""))}</td></tr>' for s in state['stages'])
    selection = json.dumps(state.get('selection', {}))
    binary = state.get('binary') or {}
    provenance = 'built from this checkout' if binary.get('source_verified') else 'not built / supplied binary source unverified'
    (out / 'index.html').write_text(f'''<!doctype html><meta charset="utf-8">
<title>Amux lifecycle acceptance</title><style>body{{font:16px system-ui;max-width:1100px;margin:40px auto;padding:0 20px;background:#10141b;color:#e9edf5}}a{{color:#8dc7ff}}table{{border-collapse:collapse;width:100%}}td,th{{padding:12px;border:1px solid #465164;text-align:left}}pre{{white-space:pre-wrap}}p{{line-height:1.5}}</style>
<h1>Amux lifecycle acceptance — {esc(state['status'])}</h1>
<p>Scope: {esc(state['mode'])}. Source: {esc(state['commit'])}. Inventoried {len(state['sources'])} test sources.</p>
<p>Selection: {esc(selection)}. Binary: {esc(provenance)}.</p>
<p>PASS on an automated stage does not certify every button or real-world integration.
Review screenshots and complete the guided case ledger. Discovery counts controls; it does not verify their effects.</p>
<table><tr><th>Stage</th><th>Result</th><th>Evidence</th><th>Coverage</th></tr>{rows}</table>
<p><a href="browser-report/index.html">Browser traces, videos and screenshots</a> ·
<a href="live-report/index.html">Live worker evidence</a> ·
<a href="cases.json">Guided acceptance ledger</a> · <a href="summary.json">Machine-readable summary</a></p>
<p>Do not mark visual review complete merely because images were generated. Missing prerequisites, skips,
failed assertions and unfinished tasks remain visible. No deployment is performed by this suite.</p>''')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['plan', 'browser', 'live', 'full'])
    parser.add_argument('--output', type=Path)
    parser.add_argument('--binary', type=Path, help='existing test binary; records hash and marks source provenance unverified')
    parser.add_argument('--project', choices=['desktop', 'mobile', 'ios-safari'])
    parser.add_argument('--grep', help='focused browser or live validation; always reported as partial')
    args = parser.parse_args()
    out = (args.output or ROOT / 'test-results' / f'lifecycle-{time.strftime("%Y%m%d-%H%M%S")}').resolve()
    # Never reuse old reports as current evidence.
    out.mkdir(parents=True, exist_ok=True)
    if (out / 'summary.json').exists(): parser.error('output already has a run; choose a fresh directory')
    env = dict(os.environ, AMUX_LIFECYCLE_OUTPUT=str(out))
    state = {'mode': args.mode, 'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
             'dirty': subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT, text=True).splitlines(),
             'sources': inventory(), 'stages': [], 'status': 'INCOMPLETE',
             'selection': {'project': args.project, 'grep': args.grep}, 'binary': None}
    cases = json.loads((ROOT / 'e2e/lifecycle/cases.json').read_text())
    (out / 'cases.json').write_text(json.dumps([{**case, 'status': 'NOT_RUN', 'evidence': []} for case in cases], indent=2) + '\n')
    report(out, state)

    def run(name, command, *, browser_json=None, live=False, timeout=7200):
        print(f'[lifecycle] {name}: {" ".join(command)}', flush=True)
        log = out / f'{name}.log'
        started = time.monotonic()
        try:
            with log.open('w') as stream:
                process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=stream,
                                           stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    code = process.wait(timeout=timeout)
                except (subprocess.TimeoutExpired, KeyboardInterrupt):
                    # Stop the run's server/browser process group as well as its parent.
                    os.killpg(process.pid, signal.SIGTERM)
                    try: process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
                    raise
            text = log.read_text(errors='replace')
            data = json.loads((out / browser_json).read_text()) if browser_json and (out / browser_json).exists() else None
            verdict = classify(code, text, data, live)
            if browser_json and data is None and verdict == 'PASS': verdict = 'INCOMPLETE'
            note = json.dumps(browser_counts(data)) if data is not None else ''
        except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
            verdict, note = 'FAIL', str(error)
        state['stages'].append({'name': name, 'status': verdict, 'log': log.name, 'note': note,
                                'command': command, 'seconds': round(time.monotonic() - started, 1)})
        report(out, state)
        print(f'[lifecycle] {name}: {verdict} {note} ({log})', flush=True)
        return verdict == 'PASS'

    if args.mode == 'plan':
        run('discovery', ['node', 'node_modules/@playwright/test/cli.js', 'test', '--config=e2e/lifecycle/playwright.config.ts', '--list'])
    if args.mode == 'full':
        run('runner-contracts', [sys.executable, '-m', 'unittest', 'discover', '-s', 'scripts/lifecycle', '-p', 'test_*.py'])
        run('syntax', ['bash', 'scripts/safe-cargo.sh', 'check', '--workspace'])
        run('contracts', ['bash', 'scripts/test-contended.sh', '--workspace', '--no-fail-fast'], live=True)
    if args.mode in ('browser', 'full'):
        run('outbox-contracts', ['node', '--test', 'tests/dashboard-outage-recovery.mjs'])
        assets = {'/' + name: hashlib.sha256((ROOT / 'crates/amux-dashboard/static' / name).read_bytes()).hexdigest()
                  for name in ('app.js', 'app.css', 'sw.js')}
        manifest = out / 'expected-assets.json'
        manifest.write_text(json.dumps(assets, indent=2) + '\n')
        env['AMUX_LIFECYCLE_ASSET_MANIFEST'] = str(manifest)
        binary = args.binary
        if binary is None:
            # Respect the shared build target; pin a private copy before running tests.
            env.setdefault('CARGO_TARGET_DIR', str(Path.home() / '.amux/rust-build-target'))
            if run('build', ['bash', 'scripts/safe-cargo.sh', 'build', '-p', 'amux-server']):
                binary = Path(env['CARGO_TARGET_DIR']) / 'debug/amux-server'
        if binary and binary.is_file():
            pinned = out / 'amux-server'
            shutil.copy2(binary, pinned)
            env['AMUX_LIFECYCLE_BINARY'] = str(pinned)
            state['binary'] = {'sha256': hashlib.sha256(pinned.read_bytes()).hexdigest(),
                               'source_verified': False, 'built_from_checkout': args.binary is None}
            command = ['node', 'node_modules/@playwright/test/cli.js', 'test', '--config=e2e/lifecycle/playwright.config.ts']
            if args.project: command += ['--project', args.project]
            if args.grep: command += ['--grep', args.grep]
            run('browser', command, browser_json='browser.json')
            receipts = [json.loads(p.read_text()) for p in out.glob('asset-provenance-*.json')]
            state['binary']['asset_provenance'] = receipts
            state['binary']['source_verified'] = args.binary is None and bool(receipts) and all(r['verdict'] == 'assets_match' for r in receipts)
            report(out, state)
        else:
            state['stages'].append({'name': 'browser', 'status': 'INCOMPLETE', 'note': 'No usable server binary'})
    if args.mode == 'full':
        run('live-protocols', ['bash', 'scripts/test-contended.sh', '-p', 'amux-server', '--test', 'golden_live',
                              '--test', 'golden_remaining', '--', '--ignored', '--nocapture'], live=True)
    if args.mode in ('live', 'full'):
        required = ['AMUX_LIFECYCLE_LAB_URL', 'AMUX_LIFECYCLE_LAB_WORKSPACE']
        missing = [key for key in required if not env.get(key)]
        if missing or env.get('AMUX_LIFECYCLE_LAB_ACK') != 'dedicated-test-instance':
            state['stages'].append({'name': 'live-journey', 'status': 'INCOMPLETE',
                                    'note': 'Dedicated lab required: URL, WORKSPACE, and LAB_ACK=dedicated-test-instance'})
        else:
            command = ['node', 'node_modules/@playwright/test/cli.js', 'test', '--config=e2e/lifecycle/live.config.ts']
            if args.grep: command += ['--grep', args.grep]
            run('live-journey', command, browser_json='live.json', live=True)
    statuses = [stage['status'] for stage in state['stages']]
    state['status'] = 'FAIL' if 'FAIL' in statuses else 'INCOMPLETE'
    if statuses and all(status == 'PASS' for status in statuses) and args.mode in ('browser', 'live'):
        state['status'] = 'AUTOMATED_SCOPE_PASSED'
    if args.mode == 'full':
        state['stages'].append({'name': 'guided-and-visual-review', 'status': 'INCOMPLETE',
                                'note': 'Complete cases.json using the canonical acceptance guide; screenshots require inspection'})
    report(out, state)
    print(f'[lifecycle] {state["status"]}: {out / "index.html"}')
    return 1 if state['status'] == 'FAIL' else 0 if state['status'] == 'AUTOMATED_SCOPE_PASSED' else 2


if __name__ == '__main__': sys.exit(main())
