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
import sys
import time

# The provider waits on this process (installed with a 3 s timeout). On a host
# at load 30+ with dozens of subagents firing PreToolUse at once, a blocking
# write lock, two fsyncs and a deliverer spawned per edge made about 690 hook
# calls in one lane exceed that timeout (gs12 review, finding 11). The observer
# now waits at most this long for the spool lock and records every overrun in
# status-observer-errors.jsonl, which the server counts
# (native_status::observer_error_census).
LOCK_BUDGET_S = float(os.environ.get('AMUX_STATUS_HOOK_LOCK_BUDGET_S', '') or 1.0)
SLOW_HOOK_S = 1.5
# StopFailure fields worth carrying. The first non-empty one rides the report.
FAILURE_FIELDS = ('error', 'error_type', 'error_message', 'error_details', 'reason', 'stop_reason', 'message')


class LockBudgetExceeded(Exception):
    pass


def record_error(root, error_type, **extra):
    try:
        with (Path(root) / 'status-observer-errors.jsonl').open('a') as out:
            out.write(json.dumps(dict({'at': time.time(), 'worker': os.environ.get('AMUX_STATUS_WORKER', ''),
                                       'error_type': error_type}, **extra)) + '\n')
    except Exception:
        pass


def acquire(lock, budget):
    """Take an exclusive flock, waiting at most `budget` seconds."""
    deadline = time.monotonic() + budget
    delay = 0.005
    while True:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return
        except BlockingIOError:
            if time.monotonic() >= deadline:
                raise LockBudgetExceeded()
            time.sleep(delay)
            delay = min(delay * 2, 0.05)


def read_counter(counter):
    try:
        return json.loads(counter.read_text())
    except (FileNotFoundError, ValueError):
        return {}


def failure_reason(data):
    """(field, reason) a StopFailure carries, or None. Bounded."""
    for key in FAILURE_FIELDS:
        value = data.get(key)
        if isinstance(value, dict):
            value = value.get('type') or value.get('message')
        if isinstance(value, str) and value.strip():
            return key, value.strip()[:500]
    return None


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


def is_compaction(data):
    event = data.get('hook_event_name')
    return event in ('PreCompact', 'PostCompact') or (event == 'SessionStart' and data.get('source') == 'compact')


def pending(folder):
    return sorted(p for p in folder.glob('*.json') if p.name != 'counter.json')


def deliverer_running(folder):
    """True when another process holds deliver.lock; it rescans on release."""
    try:
        with (folder / 'deliver.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return False
    except BlockingIOError:
        return True
    except OSError:
        return False


def deliver(root, worker, run):
    # Imported here, in the detached deliverer, so the hook process the
    # provider waits on never pays for ssl and urllib (about 60 ms cold).
    import ssl
    import urllib.request
    folder = root / 'status-events' / worker / run
    # One deliverer per run: a hook that finds deliver.lock held does not
    # spawn another. This loop rescans after releasing the lock, so an edge
    # spooled while it was held is still sent.
    for _ in range(5):
        if not deliver_once(root, worker, folder, ssl, urllib.request) or not pending(folder):
            return


def deliver_once(root, worker, folder, ssl, urllib_request):
    """Send every spooled edge. False when another deliverer holds the lock."""
    with (folder / 'deliver.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        token = (root / 'auth_token').read_text().strip()
        context = ssl.create_default_context(cafile=str(root / 'tls' / 'cert.pem'))
        for path in pending(folder):
            payload = json.loads(path.read_text())
            url = os.environ['AMUX_STATUS_URL'] + '/api/sessions/' + worker + '/report'
            req = urllib_request.Request(url, data=json.dumps(payload).encode(), headers={
                'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json',
                'X-Amux-Session': worker,
                # Contract rule 10 (AH-387): the lane's minted identity.
                **({'X-Amux-Worker-Token': os.environ['AMUX_WORKER_TOKEN']} if os.environ.get('AMUX_WORKER_TOKEN') else {})})
            with urllib_request.urlopen(req, context=context, timeout=2) as response:
                ack = json.load(response)
            if ack.get('ok'):
                path.unlink(missing_ok=True)
            else:
                break
    return True


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
    counter = folder / 'counter.json'
    # Most subagent edges are `active` with no block of their own to clear,
    # and are dropped. counter.json is replaced atomically, so it reads
    # consistently without the lock; only a subagent that may own the current
    # block queues for the lock (and re-checks under it).
    if agent and state == 'active':
        seen = read_counter(counter)
        if not (seen.get('state') in ('blocked', 'waiting') and seen.get('agent') == agent):
            return False
    folder.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (folder / 'write.lock').open('a') as lock:
        try:
            acquire(lock, LOCK_BUDGET_S)
        except LockBudgetExceeded:
            record_error(root, 'lock_budget_exceeded', event=data.get('hook_event_name', ''),
                         budget_s=LOCK_BUDGET_S, subagent=bool(agent))
            return False
        previous = read_counter(counter)
        if agent and state == 'active' and not (
                previous.get('state') in ('blocked', 'waiting') and previous.get('agent') == agent):
            return False
        # Claude also calls an AskUserQuestion notification 'permission_prompt'.
        # Preserve the explicit tool's waiting state until a real next edge.
        if data.get('hook_event_name') == 'Notification' and previous.get('state') == 'waiting':
            state = 'waiting'
        # A compaction is not a turn edge: it keeps whatever state the lane was
        # in (AF-1046). Compacting an IDLE lane mapped to 'active' and nothing
        # followed it, so the lane read mid-turn until its next prompt
        # (mixpeek-general 2026-10-30: Stop idle at 07:02:56, then PreCompact,
        # SessionStart(compact), PostCompact all 'active' at 07:57) and
        # session.self_reports_landing called its hooks broken. A compaction
        # inside a turn still reads 'active', from the edge before it.
        if is_compaction(data) and previous.get('state'):
            state = previous['state']
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
        # 37 StopFailure edges in one lane said nothing about why (gs12
        # review). Carry the provider's reason; when it sent none, say so and
        # list the key NAMES it did send (never their values).
        if data.get('hook_event_name') == 'StopFailure':
            found = failure_reason(data)
            if found:
                payload['failure_field'], payload['failure_reason'] = found
            else:
                payload['failure_reason'] = None
                payload['failure_reason_missing'] = True
                payload['failure_payload_keys'] = sorted(k for k in data if isinstance(k, str))[:40]
        target = folder / ('%020d.json' % seq)
        if agent:
            payload['subagent'] = True
        # The edge is fsynced before it is named; the counter is only renamed.
        # A counter lost to power failure re-issues a sequence the server
        # refuses as a replay, which costs one edge at most.
        for path, value, durable in ((target, payload, True),
                                     (counter, {'sequence': seq, 'state': state, 'agent': agent}, False)):
            temp = path.with_suffix('.tmp')
            with temp.open('w') as out:
                json.dump(value, out)
                if durable:
                    out.flush()
                    os.fsync(out.fileno())
            temp.replace(path)
    return True


def main():
    occurred_at = time.time()
    started = time.monotonic()
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
    data = json.load(sys.stdin)
    if observe(data, root, worker, run, provider, occurred_at) and \
            not deliverer_running(root / 'status-events' / worker / run):
        import subprocess
        subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '--deliver'],
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL, start_new_session=True, close_fds=True)
    elapsed = time.monotonic() - started
    if elapsed > SLOW_HOOK_S:
        record_error(root, 'slow_hook', event=data.get('hook_event_name', ''), elapsed_ms=int(elapsed * 1000))


if __name__ == '__main__':
    # Observation must never block or alter the provider's turn. Pending files
    # remain available to the server even if immediate network delivery fails.
    try:
        main()
    except Exception as error:
        if os.environ.get('AMUX_STATUS_HOME'):
            record_error(os.environ['AMUX_STATUS_HOME'], type(error).__name__)
