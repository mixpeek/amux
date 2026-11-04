#!/usr/bin/env python3
"""Measure the 26 proposals in docs/design/velocity-audit-harness-changes.md
against live amux data, one predicate each.

    scripts/velocity-invariants-probe.py [--json]

Read-only: it reads the amux API, amux.db (read-only), git, docker and ps.
Every result carries `measured` and `n_considered` (tests/diagnostic_contract.rs
convention): a probe that could not run says so instead of reading as a pass.
`verdict` is pass, fail or unmeasured. A proposal whose data amux does not keep
yet is unmeasured, and the note names what is missing; that gap is part of the
finding.
"""
import json, os, re, sqlite3, subprocess, sys, time, glob, hashlib

NOW = time.time()
DB = os.path.expanduser('~/.amux/amux.db')
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TERMINAL = {'done', 'verified', 'discarded'}


def sh(*a, timeout=60):
    try:
        return subprocess.run(a, capture_output=True, text=True, timeout=timeout).stdout
    except Exception as e:  # a probe that cannot run is reported, never raised
        return ''


URL = sh('amux', 'url').strip() or 'https://localhost:8824'


def api(path, timeout=90):
    out = sh('curl', '-sk', '-m', str(timeout), URL + path, timeout=timeout + 5)
    try:
        return json.loads(out)
    except ValueError:
        return None


def q(sql, args=()):
    con = sqlite3.connect('file:' + DB + '?mode=ro', uri=True, timeout=30)
    try:
        return con.execute(sql, args).fetchall()
    finally:
        con.close()


def res(n, inv, measured, n_considered, value, ok, note=''):
    verdict = 'unmeasured' if not measured else ('pass' if ok else 'fail')
    return {'item': n, 'invariant': inv, 'measured': measured, 'n_considered': n_considered,
            'value': value, 'verdict': verdict, 'note': note}


SERVER_LOG = os.path.expanduser('~/.amux/logs/server-rs.log')


def log_count(pattern):
    """Matching lines in the last 40 MB of the server log, or None when the log
    could not be read: grep prints 0 for empty input, which would read as clean."""
    if not os.path.isfile(SERVER_LOG) or os.path.getsize(SERVER_LOG) == 0:
        return None
    try:
        with open(SERVER_LOG, 'rb') as f:
            f.seek(max(0, os.path.getsize(SERVER_LOG) - 40_000_000))
            data = f.read().decode('utf-8', 'replace')
    except OSError:
        return None
    return sum(1 for line in data.splitlines() if re.search(pattern, line))


_sess = api('/api/sessions')
_board = api('/api/board?all=1', timeout=180)
# An API that timed out answers None and an empty list reads as "no cards".
# Either must make a probe unmeasured, never a pass (the review of 674b75264).
SESS_OK = isinstance(_sess, list) and len(_sess) > 0
BOARD_OK = isinstance(_board, list) and len(_board) > 0
SESS = _sess if SESS_OK else []
BOARD = _board if BOARD_OK else []
LIVE_CARDS = [c for c in BOARD if not c.get('archived')]
BY_ID = {c['id']: c for c in BOARD}
RUNNING = [s for s in SESS if s.get('running') and not s.get('archived')]


def p1():
    idle = [s for s in RUNNING if s.get('status') == 'idle' and not s.get('isolated')
            and (NOW - (s.get('last_activity') or NOW)) >= 1800]
    bad = []
    for s in idle:
        bd = s.get('board_drive') or {}
        if (bd.get('eligible_todos') or 0) > 0 and (not bd.get('reason') or NOW - (bd.get('checked_at') or 0) > 600):
            bad.append(s['name'])
    idle_with_todo = [s['name'] for s in idle if ((s.get('board_drive') or {}).get('eligible_todos') or 0) > 0]
    return res(1, 'dispatch.idle_lane_has_reason', SESS_OK, len(idle), {'idle_30m_with_eligible_todo': idle_with_todo, 'without_fresh_reason': bad},
               not bad, 'board_drive.reason is recorded per lane, not per card; a per-card reason is the change')


ID_RE = re.compile(r'\b[A-Z][A-Z0-9]+-\d+\b')


def p2():
    parked = [c for c in LIVE_CARDS if c.get('status') not in TERMINAL and c.get('waiting_on')]
    met = []
    unstructured = 0
    for c in parked:
        w = c['waiting_on'] if isinstance(c['waiting_on'], str) else json.dumps(c['waiting_on'])
        ids = ID_RE.findall(w)
        if not ids:
            unstructured += 1
            continue
        if all((BY_ID.get(i) or {}).get('status') in TERMINAL for i in ids if i in BY_ID) and any(i in BY_ID for i in ids):
            met.append(c['id'])
    return res(2, 'board.parked_card_trigger_resolves', BOARD_OK, len(parked),
               {'parked': len(parked), 'waiting_on_met_still_parked': len(met), 'sample': met[:8], 'no_card_reference': unstructured},
               not met, 'a waiting_on with no card id is the prose trigger the change removes')


def p3():
    lanes = {s['name']: s for s in RUNNING}
    stale = []
    doing = [c for c in LIVE_CARDS if c.get('status') == 'doing' and c.get('session') in lanes]
    for c in doing:
        s = lanes[c['session']]
        quiet = NOW - max(s.get('last_activity') or 0, s.get('last_progress_at') or 0)
        if quiet > 2700 and ((s.get('board_drive') or {}).get('eligible_todos') or 0) > 0:
            stale.append(c['id'])
    return res(3, 'dispatch.doing_card_is_progressing', BOARD_OK and SESS_OK, len(doing), {'doing_quiet_45m_with_other_eligible': stale}, not stale)


def p4():
    n = log_count(r'fleet_cap|dispatch cap|cap_reached|AMUX_BOARD_DRIVE_MAX')
    return res(4, 'dispatch.no_cross_lane_slot_wait', n is not None, 1, {'cap_refusal_log_lines_recent': n}, n == 0,
               'counted from the last 40 MB of server-rs.log')


def p5():
    since = NOW - 86400
    rows = q("select id, delivery, delivered_at, card_id from cmd_history where type='user' and (origin='' or origin is null) and ts>?", (since * 1000,))
    rows += q("select id, delivery, delivered_at, card_id from cmd_history where type='user' and (origin='' or origin is null) and ts>? and ts<1e12", (since,))
    undelivered = [r[0] for r in rows if not r[2]]
    return res(5, 'message.owner_send_reaches_pane', len(rows) > 0, len(rows), {'owner_messages_24h': len(rows), 'no_delivered_at': len(undelivered), 'sample': undelivered[:8]},
               not undelivered)


def p6():
    return res(6, 'message.priority_send_latency', False, 0, None, False,
               'no priority flag exists on sends, so no priority latency can be measured; building the flag is the change')


def p7():
    since = NOW - 86400
    rows = q("select origin, session, text, ts from cmd_history where ts>? order by ts", (since * 1000,))
    seen, dups = {}, 0
    for origin, sess, text, ts in rows:
        t = ts / 1000 if ts > 1e12 else ts
        key = (origin or '', sess or '', hashlib.sha1(re.sub(r'\s+', ' ', (text or '')).strip().encode()).hexdigest())
        if key in seen and t - seen[key] < 6 * 3600 and (text or '').strip():
            dups += 1
        seen[key] = t
    return res(7, 'message.duplicate_suppressed', len(rows) > 0, len(rows), {'duplicates_within_6h': dups}, dups == 0)


def p8():
    since = (NOW - 7 * 86400) * 1000
    n = q("select count(*) from cmd_history where type='user' and origin like 'api (no worker identity)%' and ts>?", (since,))[0][0]
    total = q("select count(*) from cmd_history where type='user' and ts>?", (since,))[0][0]
    return res(8, 'message.owner_label_is_stamped', total > 0, total, {'user_typed_without_identity_7d': n}, n == 0,
               'type=user rows whose origin is api (no worker identity) render as the owner today')


def p9():
    since = (NOW - 86400) * 1000
    n = q("select count(*) from cmd_history where ts>? and text like '%no reply path%'", (since,))[0][0]
    total = q("select count(*) from cmd_history where ts>?", (since,))[0][0]
    return res(9, 'message.reply_path_exists', total > 0, total, {'messages_carrying_no_reply_path_24h': n}, n == 0,
               'proxy: the footer amux appends when the recipient cannot answer')


def p10():
    d = api('/api/usage/attribution', timeout=120) or {}
    if 'background_pct' not in d:
        return res(10, 'usage.background_share', False, 0, None, False, '/api/usage/attribution did not answer')
    pct = d.get('background_pct')
    return res(10, 'usage.background_share', True, d.get('rows_considered') or 0,
               {'background_pct': pct, 'total_cost_usd': d.get('total_cost_usd'), 'window_hours': d.get('window_hours'),
                'top_sources': [(s['source'], s['cost_usd']) for s in (d.get('by_source') or [])[:4]]},
               pct is not None and pct <= 50, 'target 50%; rate-card attribution, not an invoice')


def p11():
    scheds = api('/api/schedules') or []
    runs = api('/api/schedules/runs') or []
    tmux = {s['id']: s for s in scheds if (s.get('kind') or 'tmux') == 'tmux' and s.get('enabled') and not s.get('deleted')}
    since = NOW - 86400
    fires = {}
    for r in runs if isinstance(runs, list) else []:
        sid = r.get('schedule_id') or r.get('sched_id')
        ts = r.get('started_at') or r.get('ts') or r.get('fired_at') or 0
        ts = ts / 1000 if ts and ts > 1e12 else ts
        if sid in tmux and ts and ts > since:
            fires[sid] = fires.get(sid, 0) + 1
    heavy = {k: v for k, v in fires.items() if v > 5}
    return res(11, 'schedule.model_fire_changes_state', bool(runs), len(tmux), {'tmux_schedules_over_5_fires_24h': heavy},
               not heavy, 'no-change detection does not exist yet; this counts fires, the upper bound of no-op fires')


def p12():
    ver = [c for c in LIVE_CARDS if c.get('status') == 'verified']
    named = [c for c in ver if ((c.get('verification') or {}).get('check') or (c.get('verification') or {}).get('check_id'))]
    return res(12, 'board.verified_has_passing_check', BOARD_OK, len(ver),
               {'verified_cards': len(ver), 'naming_a_check': len(named)},
               len(named) == len(ver), 'verification records a gate snapshot and an actor, with no check whose latest result can be read')


def p13():
    since = NOW - 7 * 86400
    rows = q("select changed_by, changed_at from board_change_log where old_status='done' and new_status='discarded' and changed_at>?", (since,))
    per_hour = {}
    for who, t in rows:
        k = (who, int(t // 3600))
        per_hour[k] = per_hour.get(k, 0) + 1
    bulk = {f'{k[0]}@{time.strftime("%m-%d %H:00", time.gmtime(k[1]*3600))}': v for k, v in per_hour.items() if v > 10}
    return res(13, 'board.done_not_bulk_discarded', True, len(rows), {'done_to_discarded_7d': len(rows), 'bulk_hours': bulk}, not bulk)


def p14():
    mem = api('/api/memories?all=1') or api('/api/memory') or None
    return res(14, 'ruling.reissue_rate', False, 0, None, False,
               'owner rulings are not recorded as rulings anywhere amux reads, so a re-issue cannot be counted; that is the change')


def p15():
    since = (NOW - 7 * 86400) * 1000
    rows = q("select session, text from cmd_history where ts>? and (text like '%goal keeper%' or text like '%goal-keeper%' or text like '%/goal%not met%') order by session, ts", (since,))
    run, worst, cur = {}, 0, None
    for sess, text in rows:
        flat = 'not met' in (text or '').lower()
        run[sess] = run.get(sess, 0) + 1 if flat else 0
        worst = max(worst, run[sess])
    return res(15, 'goal.keeper_stops_on_flat', True, len(rows), {'longest_not_met_run_7d': worst}, worst < 7,
               'run length counts consecutive not-met prompts; whether the measured state changed between them is not recorded')


def p16():
    logs = [f for f in glob.glob(os.path.expanduser('~/.amux/logs/land/*.log'))
            if NOW - os.path.getmtime(f) < 7 * 86400 and not f.endswith('-timed.log') and 'precheck' not in f]
    no_outcome = []
    for f in logs:
        try:
            tail = open(f, errors='replace').read()[-4000:]
        except OSError:
            continue
        if NOW - os.path.getmtime(f) > 1800 and not re.search(r'landed [0-9a-f]{7,}|verdict=land_gate_(fail|error)|nothing queued|refus|rejected|did not pass', tail):
            no_outcome.append(os.path.basename(f))
    return res(16, 'land.request_has_outcome', len(logs) > 0, len(logs), {'land_logs_7d': len(logs), 'without_outcome_line': len(no_outcome), 'sample': no_outcome[:5]},
               not no_outcome, 'a land log is the request record today; no request id exists')


def p17():
    scheds = api('/api/schedules') or []
    expired = [s['id'] for s in scheds if s.get('until') and str(s.get('until')) < time.strftime('%Y-%m-%dT%H:%M', time.localtime())
               and not s.get('deleted')]
    refused = log_count(r'(?i)plan.reserve|reserve.*refus')
    return res(17, 'schedule.expiry_announced + schedule.critical_not_refused', bool(scheds) and refused is not None, len(scheds),
               {'schedules_past_until': len(expired), 'sample': expired[:6], 'plan_reserve_refusals_recent_log': refused},
               False if expired else refused == 0,
               'no expiry notice exists, so every expired schedule fails the first half')


def p18():
    return res(18, 'schedule.path_trigger_latency', False, 0, None, False, 'no on_change schedule kind exists to time')


def p19():
    n = log_count(r'spawn: |worker restarted|restart_worker')
    return res(19, 'worker.restart_restores_context', False, n, {'spawns_in_recent_log': n}, False,
               'no restart context message exists to look for; n_considered is the restarts it would have to cover')


def p20():
    names = {s['name'] for s in RUNNING if not s.get('isolated')}
    out = sh('ps', 'eww', '-ax', '-o', 'pid=,command=', timeout=60)
    # Isolated workers carry AMUX_ISOLATED_WORKER and no AMUX_SESSION by design.
    claude = [l for l in out.splitlines() if re.search(r'(^|/)claude( |$)', l.split(' ', 2)[-1] if ' ' in l else l)
              and 'AMUX_' in l and 'AMUX_ISOLATED_WORKER=' not in l]
    missing = [l.split()[0] for l in claude if 'AMUX_SESSION=' not in l or re.search(r'AMUX_SESSION=( |$)', l)]
    return res(20, 'worker.identity_env_present', bool(claude) and SESS_OK, len(claude),
               {'claude_processes_with_amux_env': len(claude), 'missing_or_empty_AMUX_SESSION': len(missing), 'non_isolated_running': len(names)},
               not missing, 'reads process environments with ps eww')


def p21():
    log = os.path.expanduser('~/.amux/logs/tmux-watchdog.log')
    pidfile = os.path.expanduser('~/.amux/tmux-watchdog-server-pid')
    have_pid = os.path.exists(pidfile)
    replaced = int((sh('bash', '-c', f"grep -ac 'was REPLACED' {log} 2>/dev/null").strip() or 0))
    return res(21, 'session.tmux_replacement_recovered', have_pid, 1, {'watchdog_records_server_pid': have_pid, 'replacements_logged': replaced}, have_pid,
               'the watchdog runs from the shared checkout; until it is current the pid file is never written')


def p22():
    hooks = glob.glob(os.path.expanduser('~/.amux/hooks/*guard*.py'))
    cases = {'pkill -f node': True, 'killall python3': True, 'rm -rf /Users/ethan/Dev/mixpeek/server': True, 'kill 12345': False}
    results = {}
    for cmd, should_block in cases.items():
        payload = json.dumps({'tool_name': 'Bash', 'tool_input': {'command': cmd}, 'cwd': '/Users/ethan/Dev/mixpeek', 'session_id': 'probe', 'hook_event_name': 'PreToolUse'})
        blocked = False
        for h in hooks:
            try:
                r = subprocess.run(['python3', h], input=payload, capture_output=True, text=True, timeout=20, env={**os.environ, 'AMUX_GUARD_DRY_RUN': '1'})
            except Exception:
                continue
            if r.returncode == 2 or '"deny"' in r.stdout or 'BLOCKED' in r.stdout + r.stderr:
                blocked = True
        results[cmd] = {'blocked': blocked, 'expected_block': should_block}
    wrong = [c for c, v in results.items() if v['blocked'] != v['expected_block']]
    return res(22, 'test: guard refuses destructive pattern commands', bool(hooks), len(cases), {'hooks': [os.path.basename(h) for h in hooks], 'results': results},
               not wrong, 'hooks fed a PreToolUse payload directly; nothing is executed')


def p23():
    out = sh('docker', 'ps', '--format', '{{.ID}} {{.Names}} {{.RunningFor}}', timeout=30)
    rows = [l.split(' ', 2) for l in out.splitlines() if l.strip()]
    if not rows:
        return res(23, 'host.container_has_owner_and_cap', False, 0, None, False, 'docker did not answer')
    bad = []
    for cid, name, age in rows:
        insp = sh('docker', 'inspect', '--format', '{{json .Config.Labels}}|{{.HostConfig.NanoCpus}}|{{.HostConfig.Memory}}', cid, timeout=30).strip()
        labels, cpus, memb = (insp.split('|') + ['', '', ''])[:3]
        owner = any(k in labels for k in ('amux.owner', 'amux.worker', 'mixpeek.com/owner'))
        if not owner or cpus in ('0', '') or memb in ('0', ''):
            bad.append({'name': name, 'owner_label': owner, 'cpu_limit': cpus != '0', 'mem_limit': memb != '0'})
    return res(23, 'host.container_has_owner_and_cap', True, len(rows), {'containers': len(rows), 'unowned_or_uncapped': bad}, not bad)


def p24():
    return res(24, 'transcript.no_vault_values', False, 0, None, False,
               'not measured here: a scan needs vault values, which this probe must not read; the server-side scan with hashes is the change')


def p25():
    log = sh('git', '-C', REPO, 'log', 'origin/main', '--since=7.days', '--format=%s', timeout=60).splitlines()
    machine = [s for s in log if re.search(r'^amux land: regenerate|ledger|^chore\(ledger\)|regenerate interaction registry', s, re.I)]
    share = round(100 * len(machine) / len(log), 1) if log else None
    old_logs = [f for f in glob.glob(os.path.expanduser('~/.amux/logs/land/*.log')) if NOW - os.path.getmtime(f) > 14 * 86400]
    return res(25, 'repo.machine_commit_share + land.log_retention', bool(log), len(log),
               {'amux_commits_7d': len(log), 'machine_commits': len(machine), 'share_pct': share, 'land_logs_older_14d': len(old_logs)},
               (share or 0) <= 5 and not old_logs, 'amux repo only; the audit measured mixpeek (24% ledger commits)')


def p26():
    d1 = api('/api/usage/attribution?session=mxp-gs12', timeout=120) or {}
    d7 = api('/api/usage/attribution?hours=168', timeout=120) or {}
    others = [o for o in (d1.get('top_origins') or []) if o.get('session') not in (None, '', 'mxp-gs12')]
    return res(26, 'test: usage attribution honours session filter and window', bool(d1), len(d1.get('top_origins') or []),
               {'rows_from_other_sessions_with_session_filter': len(others), 'window_hours_when_asked_168': d7.get('window_hours')},
               not others and d7.get('window_hours') == 168)


def p27():
    # The board switch (owner, 2026-10-31): a worker whose steering is explicitly
    # off must have no board use. Counts cards created for it and board-labelled
    # deliveries to it in the last 24 hours.
    off = []
    for f in glob.glob(os.path.expanduser('~/.amux/sessions/*.env')):
        t = open(f, errors='replace').read()
        m = re.search(r'^AMUX_BOARD_FORCE_ADHERENCE="?([^"\n]*)', t, re.M)
        if m and m.group(1).strip().lower() in ('0', 'false', 'no', 'off') and not re.search(r'^CC_ISOLATED="?1', t, re.M):
            off.append(os.path.basename(f)[:-4])
    since = NOW - 86400
    used = {}
    for n in off:
        cards = [c['id'] for c in BOARD if c.get('session') == n and (c.get('created') or 0) > since]
        msgs = q("select count(*) from cmd_history where session=? and ts>? and (type='pickup' or origin like 'board%')", (n, since * 1000))[0][0]
        if cards or msgs:
            used[n] = {'cards_24h': len(cards), 'board_deliveries_24h': msgs}
    return res(27, 'board.off_switch_means_unused', BOARD_OK, len(off), {'switched_off_workers': len(off), 'still_used': used}, not used)


PROBES = [p1, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15, p16, p17, p18, p19, p20, p21, p22, p23, p24, p25, p26, p27]


def main():
    out = []
    for p in PROBES:
        try:
            out.append(p())
        except Exception as e:
            out.append(res(int(p.__name__[1:]), p.__name__, False, 0, None, False, 'probe error: %s' % e))
    if '--json' in sys.argv:
        print(json.dumps({'measured_at': NOW, 'results': out}, indent=1, default=str))
        return
    for r in out:
        print(f"{r['item']:>2} {r['verdict']:<10} {r['invariant']}  n={r['n_considered']}  {json.dumps(r['value'], default=str)[:300]}")
        if r['note']:
            print('   ' + r['note'])
    c = {v: sum(1 for r in out if r['verdict'] == v) for v in ('pass', 'fail', 'unmeasured')}
    print(f"\n{len(out)} probes: {c['pass']} pass, {c['fail']} fail, {c['unmeasured']} unmeasured")


if __name__ == '__main__':
    main()
