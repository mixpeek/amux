#!/usr/bin/env node
// Real cron dispatcher, independent shell children, durable claims, SIGKILL.
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_RS_SCHEDULER: '0', AMUX_ISOLATED: '0',
  AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
}});
const checks = [];
const check = (name, ok, detail) => {checks.push({name, ok: !!ok, detail}); if (!ok) throw Error(name + ': ' + JSON.stringify(detail));};
const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
const db = (sql, args = []) => JSON.parse(execFileSync('python3', ['-c',
  'import json,sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=10); c.row_factory=sqlite3.Row; r=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps([dict(x) for x in r.fetchall()])); c.commit()',
  path.join(amux.home, 'amux.db'), sql, JSON.stringify(args)], {encoding: 'utf8'}));
const runs = id => db('select * from schedule_runs where schedule_id=? order by id', [id]);
const create = async (title, command) => {
  const result = await amux.req('POST', '/api/schedules', {title, kind: 'shell', command,
    schedule_expr: 'daily at 3am', enabled: 0});
  check('private schedule created: ' + title, result.status < 300 && result.body.id, result.status);
  return result.body.id;
};
const due = ids => {for (const id of ids) db("update schedules set enabled=1, sched_type='once', next_run='2020-01-01T00:00' where id=?", [id]);};
try {
  const slowStarted = path.join(amux.root, 'slow-started');
  const unsafe = path.join(amux.root, 'unsafe-after-interruption');
  const fastDone = path.join(amux.root, 'fast-done');
  const lateDone = path.join(amux.root, 'late-done');
  const backgroundPid = path.join(amux.root, 'background.pid');
  const slow = await create('slow first', `echo started > ${quote(slowStarted)}; sleep 120; echo unsafe > ${quote(unsafe)}`);
  const fast = await create('independent fast', `echo completed >> ${quote(fastDone)}`);
  const pipe = await create('detached pipe holder', `sleep 120 & echo $! > ${quote(backgroundPid)}; echo parent-exited`);
  const disabled = await create('disabled remains disabled', `echo unsafe > ${quote(unsafe)}`);
  await amux.down(); due([slow, fast, pipe]);
  amux.env.AMUX_RS_SCHEDULER = '1'; await amux.up();
  await waitFor('actual long shell child started', () => fs.existsSync(slowStarted), 15000);
  await waitFor('independent scheduled shell completes despite long first job', () => runs(fast).some(x => x.status === 'ok'), 8000);
  check('slow job cannot block unrelated schedule', runs(slow).some(x => x.status === 'running')
    && fs.readFileSync(fastDone, 'utf8').trim() === 'completed' && !fs.existsSync(unsafe));
  await waitFor('direct shell exit completes despite descendant-held pipes', () => runs(pipe).some(x => x.status === 'ok'), 5000);
  const childPid = Number(fs.readFileSync(backgroundPid, 'utf8').trim());
  check('pipe cutoff uses actual parent exit while descendant remains alive', (() => {try {process.kill(childPid, 0); return true;} catch {return false;}})()
    && runs(pipe)[0].exit_code === 0 && runs(pipe)[0].output_tail.includes('output capture incomplete')
    && fs.readFileSync(amux.serverLog, 'utf8').includes('shell_descendant_pipes_closed'));
  const late = await create('next tick independent', `echo completed >> ${quote(lateDone)}`);
  due([late]);
  await waitFor('next scheduler tick runs while earlier slow job remains active', () => runs(late).some(x => x.status === 'ok'), 40000);
  check('future ticks keep progressing', runs(slow).some(x => x.status === 'running') && !fs.existsSync(unsafe));
  check('disabled schedule did not acquire a claim', runs(disabled).length === 0);
  await amux.down(); await amux.up();
  await waitFor('interrupted cron claim is reconciled on restart', () => runs(slow).some(x => x.status === 'error'), 10000);
  check('interrupted result is explicit and never success', runs(slow).length === 1 && runs(slow)[0].status === 'error'
    && !fs.existsSync(unsafe));
  check('completed receipts adopted without duplicate external effects', [fast, late, pipe].every(id => runs(id).length === 1 && runs(id)[0].status === 'ok')
    && fs.readFileSync(fastDone, 'utf8').trim() === 'completed'
    && fs.readFileSync(lateDone, 'utf8').trim() === 'completed');
  check('dispatch has a named measured signal', fs.readFileSync(amux.serverLog, 'utf8').includes('schedule_dispatched'));
} catch (e) {checks.push({name: 'scenario completed', ok: false, detail: String(e.stack || e)});}
finally {await amux.stop();}
const receipt = {measured: true, n_considered: checks.length, failed: checks.filter(x => !x.ok).length,
  artifacts: amux.root, fixture_boundary: 'real cron dispatch and shell commands; only private due times seeded; long first job, detached inherited pipes, a later natural tick, actual controller SIGKILL/restart; durable interrupted result and no duplicate completed effects', checks};
fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
console.log(JSON.stringify(receipt, null, 2)); process.exit(receipt.failed ? 1 : 0);
