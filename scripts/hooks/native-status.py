#!/usr/bin/env python3
"""Passive provider lifecycle observer. No stdout, prompt changes or policy decisions.

Persist before delivery; the server replays pending events after an outage. The
launch identity is inherited, never looked up afresh by an old hook process.
"""
import fcntl
import json
import os
from pathlib import Path
import re
import ssl
import subprocess
import sys
import time
import urllib.request


def state_for(data):
    event = data.get('hook_event_name')
    if event == 'Notification':
        return {'permission_prompt': 'blocked', 'elicitation_dialog': 'waiting',
                'agent_needs_input': 'waiting'}.get(data.get('notification_type'))
    if event == 'SessionStart':
        return 'active' if data.get('source') == 'compact' else 'idle'
    if event in ('PreToolUse', 'PermissionRequest') and data.get('tool_name', '').split('.')[-1] in ('AskUserQuestion', 'request_user_input'):
        return 'waiting'
    return {'UserPromptSubmit': 'active', 'PreToolUse': 'active',
            'PermissionRequest': 'blocked', 'PostToolUse': 'active',
            'PostToolUseFailure': 'active', 'PreCompact': 'active',
            'PostCompact': 'active', 'Stop': 'idle', 'Interrupt': 'idle',
            'SessionEnd': 'idle', 'StopFailure': 'error'}.get(event)


def deliver(root, worker, run):
    folder = root / 'status-events' / worker / run
    with (folder / 'deliver.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return
        token = (root / 'auth_token').read_text().strip()
        context = ssl.create_default_context(cafile=str(root / 'tls' / 'cert.pem'))
        for path in sorted(folder.glob('*.json')):
            if path.name == 'counter.json':
                continue
            payload = json.loads(path.read_text())
            url = os.environ['AMUX_STATUS_URL'] + '/api/sessions/' + worker + '/report'
            req = urllib.request.Request(url, data=json.dumps(payload).encode(), headers={
                'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json',
                'X-Amux-Session': worker,
                # Contract rule 10 (AH-387): the lane's minted identity.
                **({'X-Amux-Worker-Token': os.environ['AMUX_WORKER_TOKEN']} if os.environ.get('AMUX_WORKER_TOKEN') else {})})
            with urllib.request.urlopen(req, context=context, timeout=2) as response:
                ack = json.load(response)
            if ack.get('ok'):
                path.unlink(missing_ok=True)
            else:
                break


def observe(data, root, worker, run, provider, occurred_at):
    state = state_for(data)
    if state is None:
        return False
    # A subagent's lifecycle is not the lane's: its Stop must not mark a
    # working lane idle. But a subagent's permission prompt or question
    # blocks the whole lane on the human, so those edges ARE the lane's
    # (tubescience-parity, 2026-09-30: WORKING over a subagent's "Do you want
    # to proceed?"). The same subagent's next active edge clears its own block.
    agent = data.get('agent_id') or data.get('subagent_id') or ''
    if agent and state not in ('blocked', 'waiting', 'active'):
        return False
    folder = root / 'status-events' / worker / run
    folder.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (folder / 'write.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        counter = folder / 'counter.json'
        previous = json.loads(counter.read_text()) if counter.exists() else {}
        if agent and state == 'active' and not (
                previous.get('state') in ('blocked', 'waiting') and previous.get('agent') == agent):
            return False
        # Claude also calls an AskUserQuestion notification 'permission_prompt'.
        # Preserve the explicit tool's waiting state until a real next edge.
        if data.get('hook_event_name') == 'Notification' and previous.get('state') == 'waiting':
            state = 'waiting'
        seq = previous.get('sequence', 0) + 1
        payload = {'native_status': True, 'provider': provider, 'run_id': run,
                   'sequence': seq, 'event_ts': occurred_at, 'state': state,
                   'event': data['hook_event_name'], 'source': provider + '-hook',
                   'session_id': data.get('session_id', ''),
                   'turn_id': data.get('turn_id', ''), 'model': data.get('model', '')}
        # The submitted text, so amux can record a prompt a person typed into
        # the pane (it never passed through amux's send path). Bounded; the
        # server strips it before storing the status report (F8(c)).
        if data.get('hook_event_name') == 'UserPromptSubmit' and isinstance(data.get('prompt'), str):
            payload['prompt'] = data['prompt'][:20000]
        # startup / resume / clear / compact: the server drops queued
        # auto-approvals on an owner /clear.
        if data.get('hook_event_name') == 'SessionStart' and isinstance(data.get('source'), str):
            payload['start_source'] = data['source']
        target = folder / ('%020d.json' % seq)
        if agent:
            payload['subagent'] = True
        for path, value in ((counter, {'sequence': seq, 'state': state, 'agent': agent}), (target, payload)):
            temp = path.with_suffix('.tmp')
            with temp.open('w') as out:
                json.dump(value, out)
                out.flush()
                os.fsync(out.fileno())
            temp.replace(path)
    return True


def main():
    occurred_at = time.time()
    worker = os.environ.get('AMUX_STATUS_WORKER', '')
    run = os.environ.get('AMUX_STATUS_RUN_ID', '')
    root = os.environ.get('AMUX_STATUS_HOME', '')
    if not root or not re.fullmatch(r'[A-Za-z0-9_-]+', worker) or not re.fullmatch(r'[a-f0-9-]+', run):
        return
    root = Path(root)
    if len(sys.argv) > 1 and sys.argv[1] == '--deliver':
        deliver(root, worker, run)
        return
    provider = sys.argv[1] if len(sys.argv) > 1 else ''
    if provider not in ('codex', 'claude'):
        return
    if observe(json.load(sys.stdin), root, worker, run, provider, occurred_at):
        subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '--deliver'],
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True, close_fds=True)


if __name__ == '__main__':
    # Observation must never block or alter the provider's turn. Pending files
    # remain available to the server even if immediate network delivery fails.
    try:
        main()
    except Exception as error:
        try:
            root = Path(os.environ['AMUX_STATUS_HOME'])
            with (root / 'status-observer-errors.jsonl').open('a') as out:
                out.write(json.dumps({'at': time.time(), 'worker': os.environ.get('AMUX_STATUS_WORKER', ''),
                                      'error_type': type(error).__name__}) + '\n')
        except Exception:
            pass
