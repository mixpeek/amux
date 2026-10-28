---
description: Audit one or more amux workers - are they turning time, tokens, compute and human attention into validated progress, and if not, why. Read-only. e.g. /worker-audit workers=mxp-gs12,tubescience-parity since=72h depth=deep compare=true
argument-hint: workers=<w1,w2,...> [since=72h] [depth=quick|deep] [compare=true|false]
allowed-tools: Bash, Read, Write
amux-managed: true
---

# /worker-audit: objective worker performance audit

Decide whether the named workers are making meaningful, validated progress at
a reasonable rate, and when they are not, find the highest-impact causes and
the interventions that would change the outcome. Evidence first: every claim in
the report points at a log line, message, commit, timestamp or metric.

## Parameters

Parse `$ARGUMENTS` as `key=value` pairs (order free, commas separate list items):

| Parameter | Type | Default | Meaning |
|---|---|---|---|
| `workers` | list | required | Worker names, e.g. `mxp-gs12,tubescience-parity` |
| `since` | duration | `72h` | Lookback window: `24h`, `7d`, `30d` |
| `depth` | `quick` or `deep` | `deep` | `quick` reads the collected summary only; `deep` also reads transcripts, commits and diffs |
| `compare` | bool | `true` | With 2+ workers, add the cross-worker synthesis |

A bare list with no `workers=` (`/worker-audit mxp-gs12 amux-helper`) means
`workers=`. No workers given: ask for them and stop. Never substitute a
hardcoded list.

## Rules

- READ-ONLY. Never send to, steer, stop, restart or edit a worker, its board
  or its files. Use only GET requests and read-only commands.
- Never fabricate. A source that could not be read is reported as
  `unmeasured` with the reason, never as zero or as "no problems".
- Activity is not progress. Progress is a change that is validated: landed
  on the repo's main, a passing test or benchmark, a closed owner request, a
  deliverable the owner accepted.
- Separate symptoms from root causes, and unavoidable complexity from
  preventable waste.

## 1. Collect (one pass per worker, before any judgement)

Run the collector. It resolves each name against the live fleet, reports
unresolved workers explicitly, and writes one JSON evidence file per worker
plus a manifest under `~/.amux/audits/worker-audit/<UTC stamp>/`.

```bash
python3 - "$ARGUMENTS" <<'PY'
import json, os, re, ssl, subprocess, sys, time, urllib.request, urllib.parse
args = sys.argv[1] if len(sys.argv) > 1 else ''
kv = dict(re.findall(r'(\w+)=(\S+)', args))
bare = [w for w in re.split(r'[\s,]+', re.sub(r'\w+=\S+', '', args)) if w]
workers = [w for w in re.split(r'[,\s]+', kv.get('workers', '')) if w] or bare
if not workers: sys.exit('worker-audit: no workers given (workers=a,b)')
since = kv.get('since', '72h'); m = re.fullmatch(r'(\d+)([hd])', since)
if not m: sys.exit(f'worker-audit: since={since} is not like 24h or 7d')
hours = int(m.group(1)) * (24 if m.group(2) == 'd' else 1)
t0 = time.time() - hours * 3600
url = subprocess.run(['amux', 'url'], capture_output=True, text=True).stdout.strip() or os.environ.get('AMUX_URL', '')
ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
def get(path):
    try:
        with urllib.request.urlopen(url + path, timeout=30, context=ctx) as r:
            return {'measured': True, 'status': r.status, 'data': json.loads(r.read() or b'null')}
    except urllib.error.HTTPError as e:
        return {'measured': False, 'status': e.code, 'why_unmeasured': f'HTTP {e.code}'}
    except Exception as e:
        return {'measured': False, 'why_unmeasured': str(e)[:200]}
def sh(cmd, cwd=None):
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=60, cwd=cwd)
        return {'measured': p.returncode == 0, 'out': p.stdout[-20000:], 'err': p.stderr[-500:]}
    except Exception as e:
        return {'measured': False, 'why_unmeasured': str(e)[:200]}
stamp = time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())
out = os.path.expanduser(f'~/.amux/audits/worker-audit/{stamp}')
os.makedirs(out, exist_ok=True)
fleet = get('/api/sessions')
names = {s.get('name') for s in (fleet.get('data') or [])} if fleet.get('measured') else set()
attribution = get(f'/api/usage/attribution?since_h={hours}')
manifest = {'stamp': stamp, 'since': since, 'hours': hours, 'depth': kv.get('depth', 'deep'),
            'compare': kv.get('compare', 'true'), 'workers': [], 'unresolved': []}
for w in workers:
    if fleet.get('measured') and w not in names:
        manifest['unresolved'].append(w); continue
    ev = {'worker': w, 'window_start': t0, 'hours': hours}
    ev['session'] = get('/api/sessions/' + urllib.parse.quote(w))
    ev['quota'] = get('/api/sessions/' + urllib.parse.quote(w) + '/quota')
    ev['task_history'] = get('/api/sessions/' + urllib.parse.quote(w) + '/task-history')
    hist = get('/api/history?' + urllib.parse.urlencode({'session': w, 'limit': 1000}))
    if hist.get('measured'):
        rows = [r for r in (hist['data'] or []) if (r.get('ts') or 0) / 1000 >= t0]
        hist = {'measured': True, 'n_in_window': len(rows),
                'by_kind': {k: sum(1 for r in rows if (r.get('kind') or r.get('type')) == k) for k in {(r.get('kind') or r.get('type')) for r in rows}},
                'rows': [{'id': r.get('id'), 'ts': r.get('ts'), 'kind': r.get('kind') or r.get('type'),
                          'sender': r.get('sender'), 'text': (r.get('text') or '')[:600]} for r in rows]}
    ev['messages'] = hist
    ev['board'] = get('/api/board?' + urllib.parse.urlencode({'session': w, 'all': 1, 'slim': 0}))
    if attribution.get('measured'):
        d = attribution['data'] or {}
        ev['usage'] = {'measured': True, 'note': 'rows of /api/usage/attribution whose session is this worker',
                       'rows': [r for r in (d.get('top_origins') or []) + (d.get('by_source') or []) if r.get('session') == w]}
    else:
        ev['usage'] = attribution
    ev['requests'] = sh(['amux', 'requests', w])
    ev['resources'] = sh(['amux', 'resources', '--worker', w])
    peek = get('/api/sessions/' + urllib.parse.quote(w) + '/peek?lines=1500')
    if peek.get('measured'):
        text = (peek['data'] or {}).get('history') or (peek['data'] or {}).get('output') or ''
        open(f'{out}/{w}.peek.txt', 'w').write(text)
        peek = {'measured': True, 'lines': text.count('\n'), 'file': f'{out}/{w}.peek.txt'}
    ev['peek'] = peek
    commits = {}
    work_dir = ((ev['session'].get('data') or {}).get('dir') or '') if ev['session'].get('measured') else ''
    for repo in {os.path.expanduser('~/Dev/amux'), os.path.expanduser('~/Dev/mixpeek'), work_dir}:
        if repo and os.path.isdir(os.path.join(repo, '.git')) or (repo and os.path.isfile(os.path.join(repo, '.git'))):
            r = sh(['git', 'log', 'origin/main', f'--since=@{int(t0)}', f'--grep=Amux-Session: {w}$',
                    '--format=%h %cI %s', '--shortstat'], cwd=repo)
            commits[repo] = r
    ev['commits_on_main'] = commits
    path = f'{out}/{w}.json'
    json.dump(ev, open(path, 'w'), indent=1, default=str)
    manifest['workers'].append({'worker': w, 'evidence': path,
        'unmeasured': [k for k, v in ev.items() if isinstance(v, dict) and v.get('measured') is False]})
json.dump(manifest, open(f'{out}/manifest.json', 'w'), indent=1)
print(json.dumps(manifest, indent=1))
PY
```

Read the manifest it prints. Every worker in `unresolved` is named in the
report as not found. Every source in a worker's `unmeasured` list is named as
unmeasured with its reason.

## 2. Analyse each worker on its own

For `depth=deep`, read the worker's evidence JSON, its `.peek.txt` transcript,
and the commits and diffs it lists (`git show <sha> --stat`, then the diff when
it matters). For `depth=quick`, use the evidence JSON alone and say so.

Score each dimension 0 to 5 with one line of evidence, or `n/a (unmeasured: why)`:

1. **Progress:** the objective (its description, goal, first owner request in
   the window) against what is demonstrably done and validated.
2. **Efficiency:** validated progress per hour, per dollar of usage, per owner
   intervention. Background cost (peer messages, schedules) counts.
3. **Debugging:** instrumentation, measurement, hypotheses tested, root cause
   reached versus symptoms patched.
4. **Reasoning:** decomposition and technical decisions; reversals and why.
5. **Models:** whether the model, its limits (credit limits, rate limits, API
   errors) or a missing escalation constrained the outcome.
6. **Context:** compactions, lost instructions, repeated re-discovery, handoff
   failures.
7. **Guidance:** unclear or shifting requirements, distractions, interventions
   that did not help.
8. **Infrastructure:** machine pressure, disk, dependencies, environment.
9. **Harness:** amux orchestration, schedules, retries, steering, gates,
   observability gaps.
10. **Iteration:** time from failure to detection, diagnosis, fix and
    validation.

Build a timeline from timestamps: productive stretches (validated output),
stalls (no progress with activity), repeated failures, wasted iterations.

## 3. Cross-worker synthesis (`compare=true`, 2+ workers)

Only after every worker is analysed: recurring failure modes, shared
bottlenecks (for example peer-message loops visible in usage attribution, one
lane's resource use slowing others), and whether each problem originates in
the worker, the model, the task design, infrastructure, human direction or the
harness itself.

## 4. Report

Write the report to `~/.amux/audits/worker-audit/<stamp>/report.md` and print it:

1. **Executive verdict:** are they making meaningful progress at a reasonable rate.
2. **Scorecard:** one row per worker: progress, efficiency, debugging,
   top bottleneck, confidence (high, medium, low by evidence coverage),
   recommended action.
3. **Timeline analysis.**
4. **Root causes:** each with evidence and the counterfactual that would have
   changed the outcome.
5. **Harness recommendations:** ranked by impact, effort, frequency, confidence.
6. **Action plan:** immediate, short term, structural.
7. **Evidence appendix:** the log lines, message ids, commit shas and metrics cited.

Also write `scores.json` beside it (worker, dimension scores, verdict,
confidence) so the next audit can compare. When an earlier audit of the same
worker exists under `~/.amux/audits/worker-audit/`, compare against the most
recent one and say what moved.
