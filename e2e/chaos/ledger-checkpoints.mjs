#!/usr/bin/env node
// END-TO-END (governance PR 10): signed ledger checkpoints, exported where the
// host cannot quietly rewrite them.
//
// A private amux with the S3 sink pointed at a local S3 stand-in (path-style
// PUT/GET/list and `?retention`, the calls the sink makes) and the webhook
// sink at the same stand-in. Checks:
//   a. activity, then a forced checkpoint: signed, exported to the file, the
//      S3 stand-in and the webhook; `worm` is MEASURED: true when the store
//      answers with a retention mode, false when it says it has none;
//   b. GET /api/ledger/verify: chain, checkpoints, export file and the S3 copy
//      all ok; GET /api/ledger/checkpoints/key gives the key;
//   c. `amux ledger verify --against <file> --pubkey` (an auditor with only the
//      exported file and key) passes, against the server and against a copy
//      of the database (--db);
//   d. a sink that fails is counted, and the request that forced it succeeds;
//   e. the newest checkpoint deleted here (row and file line, triggers
//      dropped) is caught only by the S3 copy: verify names the newer object;
//   f. a row rewritten and every row after it re-hashed (triggers dropped):
//      the chain alone verifies, verify fails naming the checkpoint, and the
//      auditor's `amux ledger verify` fails from the file plus key, against
//      the server and against a copy of the database.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/ledger-checkpoints.mjs
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import http from 'node:http';
import {execFileSync, spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {startAmux} from './harness.mjs';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');

// ── S3 stand-in ────────────────────────────────────────────────────────────
const objects = new Map(); // key -> {body, lock}
const hooks = [];
const stub = {lock: true, fail: false, puts: 0, unsigned: 0};
const server = http.createServer((req, res) => {
  let body = [];
  req.on('data', c => body.push(c));
  req.on('end', () => {
    body = Buffer.concat(body);
    const u = new URL(req.url, 'http://x');
    if (u.pathname === '/hook') {
      hooks.push(JSON.parse(body.toString()));
      res.writeHead(200); return res.end('ok');
    }
    if (!String(req.headers.authorization || '').startsWith('AWS4-HMAC-SHA256 Credential=TESTKEY/')) stub.unsigned++;
    const [, bucket, ...rest] = u.pathname.split('/');
    const key = decodeURIComponent(rest.join('/'));
    if (bucket !== 'ckpt') { res.writeHead(404); return res.end('<Error><Code>NoSuchBucket</Code></Error>'); }
    if (req.method === 'PUT') {
      if (stub.fail) { res.writeHead(500); return res.end('<Error><Code>InternalError</Code></Error>'); }
      stub.puts++;
      objects.set(key, {body, lock: stub.lock ? req.headers['x-amz-object-lock-mode'] : undefined});
      res.writeHead(200); return res.end();
    }
    if (req.method === 'GET' && !key) {
      const prefix = u.searchParams.get('prefix') || '';
      const after = u.searchParams.get('start-after') || '';
      const keys = [...objects.keys()].filter(k => k.startsWith(prefix) && k > after).sort();
      res.writeHead(200, {'content-type': 'application/xml'});
      return res.end('<ListBucketResult><IsTruncated>false</IsTruncated>' + keys.map(k => `<Contents><Key>${k}</Key></Contents>`).join('') + '</ListBucketResult>');
    }
    if (req.method === 'GET' && u.searchParams.has('retention')) {
      const o = objects.get(key);
      if (o && o.lock) { res.writeHead(200); return res.end(`<Retention><Mode>${o.lock}</Mode></Retention>`); }
      res.writeHead(404); return res.end('<Error><Code>NoSuchObjectLockConfiguration</Code></Error>');
    }
    if (req.method === 'GET') {
      const o = objects.get(key);
      if (!o) { res.writeHead(404); return res.end('<Error><Code>NoSuchKey</Code></Error>'); }
      res.writeHead(200); return res.end(o.body);
    }
    res.writeHead(405); res.end();
  });
});
await new Promise(r => server.listen(0, '127.0.0.1', r));
const stubUrl = `http://127.0.0.1:${server.address().port}`;

const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_BOARD_DRIVE_SECS: '0', AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', AMUX_VAULT_KEYSTORE: 'file',
  AMUX_LEDGER_CHECKPOINT_S3_ENDPOINT: stubUrl, AMUX_LEDGER_CHECKPOINT_S3_BUCKET: 'ckpt',
  AMUX_LEDGER_CHECKPOINT_S3_ACCESS_KEY_ID: 'TESTKEY', AMUX_LEDGER_CHECKPOINT_S3_SECRET_ACCESS_KEY: 'testsecret',
  AMUX_LEDGER_CHECKPOINT_S3_LOCK_DAYS: '30', AMUX_LEDGER_CHECKPOINT_WEBHOOK: stubUrl + '/hook',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
const ownerToken = () => fs.readFileSync(path.join(amux.home, ['auth', 'token'].join('_')), 'utf8').trim();
const OWNER = () => ({Authorization: 'Bearer ' + ownerToken()});
const get = async p => (await amux.req('GET', p, undefined, 30000, OWNER())).body;
const force = async () => amux.req('POST', '/api/ledger/checkpoints', {}, 60000, OWNER());
const dbPath = path.join(amux.home, 'amux.db');
const jsonl = path.join(amux.home, 'ledger-checkpoints.jsonl');
const py = (code, ...args) => execFileSync('python3', ['-I', '-c', code, dbPath, ...args], {encoding: 'utf8'}).trim();
const cli = (...args) => spawnSync('bash', [path.join(repo, 'amux'), ...args], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_API: amux.base, CC_HOME: amux.home},
  encoding: 'utf8', timeout: 120000,
});
const dbCopy = () => {
  const dst = path.join(amux.root, 'audit-copy-' + crypto.randomBytes(4).toString('hex') + '.db');
  py('import sqlite3,sys; s=sqlite3.connect(sys.argv[1],timeout=30); d=sqlite3.connect(sys.argv[2]); s.backup(d); d.close()', dst);
  return dst;
};
const activity = async n => {
  for (let i = 0; i < n; i++) {
    const r = await amux.req('POST', '/api/board', {title: 'checkpoint e2e ' + crypto.randomBytes(3).toString('hex'), status: 'todo'}, 10000, OWNER());
    if (r.status >= 300) throw Error('board post failed: ' + JSON.stringify(r));
  }
};

let ok = false;
try {
  // a. Sign and export.
  await activity(3);
  let r = await force();
  check('a. a forced checkpoint is signed', r.status === 200 && r.body.ok && r.body.created && r.body.created.signature.length === 128, r);
  const cp1 = r.body.created;
  check('a. ...exported to the file', fs.readFileSync(jsonl, 'utf8').trim().split('\n').some(l => JSON.parse(l).checkpoint_hash === cp1.checkpoint_hash), null);
  check('a. ...to the S3 sink, signed with SigV4', r.body.exports.s3 && r.body.exports.s3.ok && objects.has(r.body.exports.s3.key) && stub.unsigned === 0, r.body.exports);
  check('a. ...write-once is measured: the store reported a retention mode', r.body.exports.s3.worm === 'true', r.body.exports.s3);
  check('a. ...and to the webhook', hooks.some(h => h.checkpoint_hash === cp1.checkpoint_hash && h.signature === cp1.signature), hooks);
  stub.lock = false;
  await activity(1);
  r = await force();
  check('a. a store with no object lock reads worm=false, never assumed true', r.body.exports.s3.ok && r.body.exports.s3.worm === 'false', r.body.exports.s3);

  // b. The server's verify.
  let v = await get('/api/ledger/verify');
  check('b. verify: ledger and checkpoints ok', v.ok === true && v.chain_ok === true && v.checkpoints.ok === true && v.checkpoints.verified === 2 && v.checkpoints.measured === true, v);
  check('b. ...export file matches', v.checkpoints.export_file.ok === true && v.checkpoints.export_file.lines === 2, v.checkpoints.export_file);
  check('b. ...the S3 copy of the newest matches and nothing is newer', v.checkpoints.external.checked === true && v.checkpoints.external.ok === true && v.checkpoints.external.newer_in_sink.length === 0, v.checkpoints.external);
  const key = await get('/api/ledger/checkpoints/key');
  check('b. the public key is published', key.algorithm === 'ed25519' && key.public_key === cp1.public_key && /BEGIN PUBLIC KEY/.test(key.public_key_pem), key);
  const counters = (await get('/api/contract/counters')).ledger_checkpoints;
  check('b. counters: created, sinks configured, measured', counters && counters.created >= 2 && counters.sinks.s3 && counters.sinks.webhook && counters.measured === true, counters);

  // c. The auditor's check, from the exported file and the key only.
  let a = cli('ledger', 'verify', '--against', jsonl, '--pubkey', key.public_key);
  check('c. amux ledger verify --against <file> passes against the server', a.status === 0 && /ledger compared: yes, from server/.test(a.stdout) && /\bOK\b/.test(a.stdout), a);
  a = cli('ledger', 'verify', '--against', jsonl, '--pubkey', key.public_key, '--db', dbCopy());
  check('c. ...and against a copy of the database, every row re-hashed', a.status === 0 && /from db/.test(a.stdout), a);
  a = cli('ledger', 'verify', '--against', jsonl, '--pubkey', 'ab'.repeat(32));
  check('c. a different pinned key fails', a.status === 1 && /different key/.test(a.stdout), a);

  // d. A failing sink is counted; the request still answers.
  stub.fail = true;
  await activity(1);
  const before = (await get('/api/contract/counters')).ledger_checkpoints.ledger_checkpoint_sink_failed;
  r = await force();
  const after = (await get('/api/contract/counters')).ledger_checkpoints.ledger_checkpoint_sink_failed;
  check('d. a failing S3 sink: the checkpoint is still made and the failure counted', r.status === 200 && r.body.created && r.body.exports.s3.ok === false && after >= before + 1, {r: r.body, before, after});
  stub.fail = false;
  r = await amux.req('POST', '/api/ledger/checkpoints', {}, 60000, OWNER());
  check('d. ...the next one exports again', r.body.exports.s3 && r.body.exports.s3.ok === true, r.body);

  // e. Delete the newest checkpoint here (row and file line). Only the S3 copy knows.
  const newest = r.body.created;
  py(`import sqlite3,sys
c=sqlite3.connect(sys.argv[1],timeout=30)
c.execute("DROP TRIGGER amux_ledger_checkpoints_no_delete")
c.execute("DELETE FROM _amux_ledger_checkpoints WHERE checkpoint_hash=?", (sys.argv[2],))
c.commit()`, newest.checkpoint_hash);
  const lines = fs.readFileSync(jsonl, 'utf8').trim().split('\n').filter(l => JSON.parse(l).checkpoint_hash !== newest.checkpoint_hash);
  fs.writeFileSync(jsonl, lines.join('\n') + '\n');
  v = await get('/api/ledger/verify');
  check('e. locally everything still links', v.checkpoints.ok === true && v.checkpoints.export_file.ok === true, v.checkpoints);
  check('e. ...the S3 sink names the newer checkpoint removed here', v.ok === false && v.checkpoints.external.ok === false && v.checkpoints.external.newer_in_sink.some(k => k.includes(newest.checkpoint_hash.slice(0, 16))), v.checkpoints.external);

  // f. Rewrite a row and re-hash every row after it, as someone holding the file would.
  const target = py(`import sqlite3,sys
c=sqlite3.connect(sys.argv[1],timeout=30)
print(c.execute("SELECT MIN(seq)+1 FROM _amux_ledger").fetchone()[0])`);
  py(`import sqlite3,sys,hashlib
c=sqlite3.connect(sys.argv[1],timeout=30)
seq=int(sys.argv[2])
for t in ("amux_ledger_no_update","amux_ledger_seal_once"): c.execute("DROP TRIGGER IF EXISTS "+t)
c.execute("UPDATE _amux_ledger SET resource='worker:not-who-it-was' WHERE seq=?", (seq,))
cols="seq, event_id, ts, kind, trace_id, parent_event_id, actor, claimed, on_behalf_of, authority, policy_version, action, resource, input_hash, output_hash, decision, outcome, cost_microusd, source_table, source_id, span_id, parent_span_id, trace_source"
prev=c.execute("SELECT hash FROM _amux_ledger WHERE seq<? AND hash IS NOT NULL ORDER BY seq DESC LIMIT 1",(seq,)).fetchone()
prev=prev[0] if prev else c.execute("SELECT anchor_hash FROM _amux_ledger_anchor").fetchone()[0]
for row in c.execute("SELECT "+cols+" FROM _amux_ledger WHERE seq>=? AND hash IS NOT NULL ORDER BY seq",(seq,)).fetchall():
    f=[None if v is None else str(v) for v in row]
    if all(v is None for v in f[20:]): f=f[:20]
    h=hashlib.sha256((prev+"".join("\\x1e"+(v if v is not None else "\\x00") for v in f)).encode()).hexdigest()
    c.execute("UPDATE _amux_ledger SET prev_hash=?, hash=? WHERE seq=?",(prev,h,row[0])); prev=h
c.commit()`, target);
  v = await get('/api/ledger/verify');
  check('f. the re-hashed chain verifies on its own', v.chain_ok === true, v);
  check('f. ...but not against what was signed: verify names the checkpoint', v.ok === false && v.checkpoints.ok === false
    && /no longer holds the hash this checkpoint signed/.test(v.checkpoints.first_bad.reason) && v.checkpoints.first_bad.checkpoint_seq >= Number(target), v.checkpoints);
  a = cli('ledger', 'verify', '--against', jsonl, '--pubkey', key.public_key);
  check('f. the auditor (file + key, against the server) sees the rewrite', a.status === 1 && /rewritten/.test(a.stdout), a);
  a = cli('ledger', 'verify', '--against', jsonl, '--pubkey', key.public_key, '--db', dbCopy());
  check('f. ...and against a copy of the database', a.status === 1 && /rewritten/.test(a.stdout), a);
  ok = true;
} finally {
  console.log(JSON.stringify({ok, passed: checks.filter(c => c.ok).length}));
  await amux.stop();
  server.close();
}
process.exit(ok ? 0 : 1);
