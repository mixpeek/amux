#!/usr/bin/env node
// An owner-configured receive boundary fences both new and queued peer input.
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_ISOLATED: '0', AMUX_GROUP_SEND_ENFORCE: '0',
  AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
}});
const checks = [];
const check = (name, ok, detail) => {checks.push({name, ok: !!ok, detail}); if (!ok) throw Error(name + ': ' + JSON.stringify(detail));};
const db = (sql, args = []) => JSON.parse(execFileSync('python3', ['-c',
  'import json,sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=10); c.row_factory=sqlite3.Row; r=c.execute(sys.argv[2],json.loads(sys.argv[3])); print(json.dumps([dict(x) for x in r.fetchall()])); c.commit()',
  path.join(amux.home, 'amux.db'), sql, JSON.stringify(args)], {encoding: 'utf8'}));
const peer = 'product-peer', target = 'harness-peer', hub = 'project-hub';
const received = text => amux.fakeLog().filter(x => x.text?.includes(text)).length;
try {
  for (const name of [peer, target, hub]) {
    const dir = path.join(amux.root, name); fs.mkdirSync(dir);
    const r = await amux.req('POST', '/api/sessions', {name, dir, start: false});
    check('private worker created ' + name, r.status === 201, r.status);
    fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'), `\nCC_TAGS=product\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\nCC_RECEIVE_ANY=1\nCC_SEND_ALLOW=*\n`);
    const start = await amux.req('POST', `/api/sessions/${name}/start`);
    check('private provider started ' + name, start.status < 300, start.status);
    // A 202 is start acceptance, not a completed tmux/provider launch.
    await waitFor('actual provider launch ' + name, () => amux.fakeLog().some(x => x.event === 'launch' && x.argv.includes(name)), 30000);
  }
  await waitFor('all real private providers launch', () => amux.fakeLog().filter(x => x.event === 'launch').length === 3, 30000);
  // Seed only a private, not-yet-attempted queued input from before the owner
  // changed policy, then kill before the delivery tick can consume it.
  await amux.down();
  db('insert into steering_queue(id,session,text,queued_at,guard,sender) values(?,?,?,?,?,?)',
    ['old-peer-report', target, 'old-peer-injection', Date.now()/1000, 'message', peer]);
  fs.appendFileSync(path.join(amux.home, 'sessions', target + '.env'), '\nCC_RECEIVE_DENY=product\n');
  await amux.up();
  await waitFor('older queued input receives durable refusal after restart', () => db("select outcome from steering_history where id='old-peer-report'").length > 0, 15000);
  check('policy fences queued input without erasing its audit trail', received('old-peer-injection') === 0
    && db("select outcome,text from steering_history where id='old-peer-report'")[0].outcome === 'refused:peer_receive_denied'
    && db("select count(*) n from steering_queue where id='old-peer-report'")[0].n === 0);
  const blocked = await amux.req('POST', `/api/workers/${target}/send`,
    {text: 'new-peer-injection', source_session: target}, 10000, {'X-Amux-Session': peer});
  check('peer blocked despite receiver-open, sender-wildcard and body spoof', blocked.status === 403
    && blocked.body.code === 'peer_receive_denied' && !blocked.body.grant_id && received('new-peer-injection') === 0, blocked.body);
  const owner = await amux.req('POST', `/api/workers/${target}/send`, {text: 'direct-owner-input'}, 30000);
  check('owner input still accepted', owner.status < 300, owner.status);
  await waitFor('owner input reaches actual provider', () => received('direct-owner-input') === 1, 30000);
  const ownHub = await amux.req('POST', `/api/workers/${hub}/send`, {text: 'own-project-report'}, 30000, {'X-Amux-Session': peer});
  check('project peer can still notify its own hub', ownHub.status < 300, ownHub.status);
  await waitFor('own hub input reaches actual provider', () => received('own-project-report') === 1, 30000);
  await amux.down(); await amux.up();
  const after = await amux.req('POST', `/api/workers/${target}/send`, {text: 'after-restart-injection'}, 10000, {'X-Amux-Session': peer});
  check('receive boundary survives another controller crash', after.status === 403 && after.body.code === 'peer_receive_denied');
  check('accepted inputs were not replayed after crash', received('direct-owner-input') === 1 && received('own-project-report') === 1
    && received('after-restart-injection') === 0 && db("select count(*) n from steering_history where id='old-peer-report'")[0].n === 1);
  check('receive refusal has a named measured server signal', fs.readFileSync(amux.serverLog, 'utf8').includes('peer_receive_denied'));
} catch (e) {checks.push({name: 'scenario completed', ok: false, detail: String(e.stack || e)});}
finally {await amux.stop();}
const receipt = {measured: true, n_considered: checks.length, failed: checks.filter(x => !x.ok).length,
  artifacts: amux.root, fixture_boundary: 'real API, tmux providers, private owner receive policy, pre-policy queue seeded with server down, actual SIGKILL/restarts; peer refusal, durable audit, owner and own-project delivery', checks};
fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
console.log(JSON.stringify(receipt, null, 2)); process.exit(receipt.failed ? 1 : 0);
