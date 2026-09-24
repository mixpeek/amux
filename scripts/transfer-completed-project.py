#!/usr/bin/env python3
"""Transfer one accepted project between local Amux homes without changing its proof.

Dry-run by default. Only terminal boards with published human approval qualify.
The source is paused before publishing the target. Conflicts refuse, never overwrite.
Command IDs (part of acceptance intent) are preserved; append-only event IDs are
allocated at the destination in original order. Files retain their content hashes.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import ssl
import tempfile
import time
import urllib.request


def api(home, url, path, body=None):
    context = ssl.create_default_context(cafile=str(home / 'tls/cert.pem'))
    headers = {'Authorization': 'Bearer ' + (home / 'auth_token').read_text().strip(),
               'Content-Type': 'application/json'}
    request = urllib.request.Request(url.rstrip('/') + path, headers=headers,
        data=json.dumps(body).encode() if body is not None else None,
        method='PUT' if body is not None else 'GET')
    with urllib.request.urlopen(request, context=context, timeout=60) as response:
        return json.load(response)


def connect(home):
    conn = sqlite3.connect(str(home / 'amux.db'), timeout=60)
    conn.row_factory = sqlite3.Row
    conn.execute('PRAGMA foreign_keys=ON')
    return conn


def rows(conn, table, column, values):
    if not values:
        return []
    return [dict(r) for r in conn.execute(
        f'SELECT * FROM "{table}" WHERE "{column}" IN ({",".join("?" for _ in values)})', values)]


def rewrite(value, source, target):
    # Rebase retained locations, not the contents of hashed evidence files.
    if isinstance(value, str):
        try:
            decoded = json.loads(value)
        except (ValueError, TypeError):
            return value.replace(str(source) + '/', str(target) + '/')
        return json.dumps(rewrite(decoded, source, target), separators=(',', ':'), ensure_ascii=False)
    if isinstance(value, dict):
        return {k: rewrite(v, source, target) for k, v in value.items()}
    if isinstance(value, list):
        return [rewrite(v, source, target) for v in value]
    return value


def artifact_paths(value):
    if isinstance(value, str):
        try:
            yield from artifact_paths(json.loads(value))
        except (ValueError, TypeError):
            if '/artifacts/project-reports/' in value and '\n' not in value:
                yield Path(value)
    elif isinstance(value, dict):
        for v in value.values():
            yield from artifact_paths(v)
    elif isinstance(value, list):
        for v in value:
            yield from artifact_paths(v)


def transfer(args):
    source, target = args.source_home.resolve(), args.target_home.resolve()
    assert source != target, 'source and target must differ'
    assert re.fullmatch(r'[a-zA-Z0-9_-]+', args.project), 'invalid project name'
    view = api(source, args.source_url, '/api/projects/' + args.project)
    assert view['acceptance']['state'] == 'accepted', 'project must have current human-approved publication'
    assert view['cards'] and all(c['phase'] in ('verified', 'closed') for c in view['cards']), 'board not terminal'
    assert all(w['lifecycle'] == 'expired' for w in view['workers']), 'current workers must be expired'
    target_projects = api(target, args.target_url, '/api/projects')['projects']
    assert not any(p['name'] == args.project for p in target_projects), 'target project already exists; refusing duplicate'
    with tempfile.TemporaryDirectory(prefix='amux-project-transfer-') as tmp:
        snapshot = sqlite3.connect(str(Path(tmp) / 'source.db'))
        with connect(source) as live:
            live.backup(snapshot)
        snapshot.row_factory = sqlite3.Row
        cards = rows(snapshot, 'issues', 'project_group', [args.project])
        ids = [r['id'] for r in cards]
        attempts = rows(snapshot, 'task_attempts', 'card', ids)
        workers = sorted({w['name'] for w in view['workers']} | {r['worker'] for r in attempts if r['worker']})
        events = rows(snapshot, 'session_events', 'session', workers + ['project:' + args.project])
        commands = {r['id']: r for r in rows(snapshot, 'cmd_history', 'project_group', [args.project]) + rows(snapshot, 'cmd_history', 'session', workers)}
        assert all(not r['capture_pending'] for r in commands.values()), 'pending intake cannot be transferred'
        data = {
            'issues': cards,
            'issue_tags': rows(snapshot, 'issue_tags', 'issue_id', ids),
            'issue_files': rows(snapshot, 'issue_files', 'issue_id', ids),
            '_amux_task_artifacts': rows(snapshot, '_amux_task_artifacts', 'task_id', ids),
            '_amux_verifications': rows(snapshot, '_amux_verifications', 'task_id', ids),
            'cmd_history': sorted(commands.values(), key=lambda r: r['id']),
            'session_events': sorted(events, key=lambda r: r['id']),
            'task_attempts': attempts,
            'task_windows': rows(snapshot, 'task_windows', 'task', ids),
            'steering_history': rows(snapshot, 'steering_history', 'session', workers),
            'token_ledger': rows(snapshot, 'token_ledger', 'session', workers),
            'group_config': rows(snapshot, 'group_config', 'name', [args.project]),
        }
        assert len(data['group_config']) == 1
        files = {}
        missing = []
        for path in sorted(set(artifact_paths(data))):
            assert path.is_relative_to(source / 'artifacts/project-reports'), f'asset outside source store: {path}'
            if not path.is_file():
                missing.append(str(path)); continue
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            assert digest == path.stem, f'asset digest mismatch: {path}'
            files[str(path.relative_to(source))] = path.read_bytes()
        # Current review evidence must be readable even if old failed runs lack assets.
        for path in artifact_paths(view['acceptance']):
            assert path.is_file(), f'current acceptance evidence missing: {path}'
        for worker in workers:
            for directory in ('sessions', 'workspaces', 'logs'):
                for path in (source / directory).glob(worker + '.*'):
                    if not path.is_file():
                        continue
                    relative = str(path.relative_to(source))
                    payload = path.read_bytes()
                    if directory == 'sessions' and path.suffix == '.env':
                        # A historical failed attempt must never resurrect on import.
                        relative += '.reaped'
                    if directory != 'logs':
                        payload = payload.replace((str(source) + '/').encode(), (str(target) + '/').encode())
                    files[relative] = payload
        summary = {'project': args.project, 'candidate': view['acceptance']['candidate'],
                   'source': str(source), 'target': str(target), 'rows': {t: len(r) for t, r in data.items()},
                   'workers': workers, 'files': len(files), 'missing_historical_assets': missing}
        # Integer append-only identities are local; acceptance-bound command IDs are not.
        generated = {'session_events': 'id', 'token_ledger': 'id', 'task_attempts': 'id', 'task_windows': 'id'}
        with connect(target) as dest:
            # Both servers may have observed the same provider transcript. Usage is
            # global per provider message, never charged twice for moving a project.
            ledger = data['token_ledger']
            data['token_ledger'] = [r for r in ledger if r['message_id'] is None or not dest.execute(
                'SELECT 1 FROM token_ledger WHERE conversation=? AND message_id=?',
                (r['conversation'], r['message_id'])).fetchone()]
            summary['usage_rows_already_present'] = len(ledger) - len(data['token_ledger'])
            summary['rows']['token_ledger'] = len(data['token_ledger'])
            for table, entries in data.items():
                schema = list(dest.execute(f'PRAGMA table_info("{table}")'))
                columns = {r['name'] for r in schema}
                pk = [r['name'] for r in schema if r['pk']]
                for entry in entries:
                    assert set(entry) <= columns, f'target schema cannot retain {table}'
                    if table not in generated and pk:
                        found = dest.execute(f'SELECT 1 FROM "{table}" WHERE ' + ' AND '.join(f'"{k}"=?' for k in pk), [entry[k] for k in pk]).fetchone()
                        assert not found, f'identity collision in {table}: {[entry[k] for k in pk]}'
            for relative, payload in files.items():
                path = target / relative
                assert not path.exists() or path.read_bytes() == payload, f'file collision: {relative}'
            print(json.dumps(summary, indent=2))
            if not args.apply:
                return
            receipt = target / 'project-transfers' / (args.project + '-' + str(int(time.time())))
            receipt.mkdir(parents=True, mode=0o700)
            (receipt / 'source-rows.json').write_text(json.dumps(data, ensure_ascii=False))
            (receipt / 'source-view.json').write_text(json.dumps(view, ensure_ascii=False))
            with sqlite3.connect(str(receipt / 'destination-before.db')) as backup:
                dest.backup(backup)
            policy = dict(view['project']['policy']); policy['paused'] = True
            api(source, args.source_url, '/api/projects/' + args.project,
                {'expect_rev': view['project']['revision'], 'policy': policy})
            dest.execute('BEGIN IMMEDIATE')
            try:
                # Concurrent imports/configuration cannot replace a project after preflight.
                assert not dest.execute('SELECT 1 FROM group_config WHERE name=?', (args.project,)).fetchone()
                for relative, payload in files.items():
                    path = target / relative; path.parent.mkdir(parents=True, exist_ok=True)
                    if not path.exists():
                        with path.open('xb') as output:
                            output.write(payload)
                for table, entries in data.items():
                    for original in entries:
                        entry = {k: rewrite(v, source, target) for k, v in original.items() if k != generated.get(table)}
                        if table == 'issues':
                            # These bytes bind human acceptance; storage relocation
                            # must not change what the person approved.
                            for key in ('id', 'type', 'title', 'desc', 'acceptance_criteria', 'depends_on'):
                                entry[key] = original[key]
                        cols = ','.join('"' + k + '"' for k in entry)
                        dest.execute(f'INSERT INTO "{table}" ({cols}) VALUES ({",".join("?" for _ in entry)})', list(entry.values()))
                for prefix in {i.rsplit('-', 1)[0] for i in ids if i.rsplit('-', 1)[-1].isdigit()}:
                    high = max(int(i.rsplit('-', 1)[1]) for i in ids if i.startswith(prefix + '-')) + 1
                    dest.execute('INSERT INTO issue_counters(prefix,next_n) VALUES(?,?) ON CONFLICT(prefix) DO UPDATE SET next_n=MAX(next_n,excluded.next_n)', (prefix, high))
                dest.execute('INSERT INTO session_events(ts,session,type,data,source) VALUES(?,?,?,?,?)',
                    (time.time(), 'project:' + args.project, 'project.transferred', json.dumps(summary), 'operator'))
                dest.commit()
            except BaseException:
                dest.rollback()
                raise
            summary['file_hashes'] = {r: hashlib.sha256(b).hexdigest() for r, b in files.items()}
            (receipt / 'receipt.json').write_text(json.dumps(summary, indent=2))
            print('project_transfer_committed: ' + str(receipt / 'receipt.json'))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-home', type=Path, required=True)
    parser.add_argument('--target-home', type=Path, required=True)
    parser.add_argument('--source-url', required=True)
    parser.add_argument('--target-url', required=True)
    parser.add_argument('--project', required=True)
    parser.add_argument('--apply', action='store_true')
    transfer(parser.parse_args())
