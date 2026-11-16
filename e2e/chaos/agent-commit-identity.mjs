#!/usr/bin/env node
// END-TO-END (governance PR 15): an agent's commit names the worker that made
// it, the amux server it ran on, and the person it acted for.
//
// On 2026-11-10 a sandbox's deploy-wake told its `amux-helper` three commits
// "of yours" were live; a same-named lane on another Mac made them, authored
// as the same human. A private amux (own home, port, tmux socket, fake CLIs):
//   a. a launched worker's environment carries a bot author and committer,
//      `<worker> (amux) <worker+<host8>@agents.noreply.amux.io>`, and
//      AMUX_HOST_ID equal to the server's ledger host id;
//   b. after the owner's send, a commit made with that environment through the
//      shipped prepare-commit-msg hook is authored by the bot and carries
//      Amux-Session, Amux-Host and On-behalf-of: owner;
//   c. after another worker's send, On-behalf-of is the receiving worker's
//      sponsor (a member), not the sending worker;
//   d. a worker whose own scope file sets GIT_AUTHOR_NAME keeps it (no bot
//      identity is pushed over a person's choice);
//   e. the counters report the launches, measured;
//   f. AMUX_AGENT_COMMIT_IDENTITY=human: a relaunched worker gets no GIT_*
//      identity, and still gets AMUX_HOST_ID.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/agent-commit-identity.mjs
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor} from './harness.mjs';

const repo = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../..');
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_ISOLATED: '0', AMUX_RS_SCHEDULER: '0', AMUX_BOARD_DRIVE_SECS: '0',
  AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0', AMUX_MODEL_CATALOG_REFRESH_SECS: '0',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
  console.log('ok -', name);
};
// tmux exits non-zero for an unset variable; that is "absent", not an error.
const tenv = (lane, key) => {
  let out = '';
  try { out = amux.tmux('show-environment', '-t', 'amux-' + lane, key).trim(); } catch { return null; }
  return out.startsWith(key + '=') ? out.slice(key.length + 1) : null;
};
const cli = (lane, tok, ...args) => execFileSync('bash', [path.join(repo, 'amux'), ...args], {
  env: {...amux.env, AMUX_URL: amux.base, AMUX_API: amux.base, AMUX_SESSION: lane, AMUX_WORKER_TOKEN: tok, CC_HOME: amux.home},
  encoding: 'utf8', timeout: 30000,
});
const mkWorker = async (name, extra = '') => {
  const dir = path.join(amux.root, name); fs.mkdirSync(dir, {recursive: true});
  const r = await amux.req('POST', '/api/sessions', {name, dir, start: false});
  check('worker created ' + name, r.status === 201, r);
  fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'),
    '\nCC_TAGS=product\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\nAMUX_BOARD_FORCE_ADHERENCE=1\n' + extra);
};
const start = async name => {
  const s = await amux.req('POST', `/api/sessions/${name}/start`);
  check('worker started ' + name, s.status < 300, s);
  await waitFor('provider launch ' + name, () => amux.fakeLog().some(x => x.event === 'launch' && (x.argv || []).includes(name)), 30000);
};
// A commit made the way a lane makes one: its own environment, the shipped hook.
const commitAs = (lane, gitDir, subject) => {
  const env = {...amux.env, AMUX_SESSION: lane, AMUX_HOME: amux.home};
  for (const k of ['GIT_AUTHOR_NAME', 'GIT_AUTHOR_EMAIL', 'GIT_COMMITTER_NAME', 'GIT_COMMITTER_EMAIL', 'AMUX_HOST_ID']) {
    const v = tenv(lane, k);
    if (v !== null) env[k] = v; else delete env[k];
  }
  const g = (...args) => execFileSync('git', args, {cwd: gitDir, encoding: 'utf8', env});
  fs.appendFileSync(path.join(gitDir, 'f.txt'), subject + '\n');
  g('add', 'f.txt');
  g('-c', 'user.name=Ethan', '-c', 'user.email=ethan@example.com', 'commit', '-q', '-m', subject);
  return {author: g('log', '-1', '--format=%an <%ae>').trim(), committer: g('log', '-1', '--format=%cn <%ce>').trim(),
    body: g('log', '-1', '--format=%B')};
};
const deliveredTo = (lane, text) => waitFor(lane + ' receives ' + text,
  () => amux.fakeLog().some(x => x.text?.includes(text)), 30000);

let ok = false;
try {
  await mkWorker('ci-a');
  await mkWorker('ci-b');
  await mkWorker('ci-c', 'GIT_AUTHOR_NAME=Chosen Person\n');
  for (const n of ['ci-a', 'ci-b', 'ci-c']) await start(n);

  // a. The launch environment.
  const host = fs.readFileSync(path.join(amux.home, 'ledger', 'host-id'), 'utf8').trim();
  check('a. the server has a host id', /^[0-9a-f]{16}$/.test(host), host);
  const email = `ci-a+${host.slice(0, 8)}@agents.noreply.amux.io`;
  check('a. the worker launches as a bot author and committer', tenv('ci-a', 'GIT_AUTHOR_NAME') === 'ci-a (amux)'
    && tenv('ci-a', 'GIT_AUTHOR_EMAIL') === email && tenv('ci-a', 'GIT_COMMITTER_EMAIL') === email
    && tenv('ci-a', 'GIT_COMMITTER_NAME') === 'ci-a (amux)', {n: tenv('ci-a', 'GIT_AUTHOR_NAME'), e: tenv('ci-a', 'GIT_AUTHOR_EMAIL')});
  check('a. ...with the host id as AMUX_HOST_ID', tenv('ci-a', 'AMUX_HOST_ID') === host, tenv('ci-a', 'AMUX_HOST_ID'));

  const gitDir = path.join(amux.root, 'ci-repo'); fs.mkdirSync(gitDir);
  execFileSync('git', ['init', '-q'], {cwd: gitDir});
  fs.copyFileSync(path.join(repo, 'scripts/git-hooks/prepare-commit-msg'), path.join(gitDir, '.git/hooks/prepare-commit-msg'));
  fs.chmodSync(path.join(gitDir, '.git/hooks/prepare-commit-msg'), 0o755);

  // b. The owner asks; the commit names the bot, the host and the owner.
  const send = await amux.req('POST', '/api/workers/ci-a/send', {text: 'identity e2e: owner asks A'}, 30000);
  check('b. the owner sends to A', send.status < 300, send);
  await deliveredTo('ci-a', 'owner asks A');
  const obo = path.join(amux.home, 'trace', 'ci-a.on-behalf-of');
  await waitFor('on-behalf-of written', () => fs.existsSync(obo), 10000);
  check('b. the delivery records the owner as the one A acts for', fs.readFileSync(obo, 'utf8') === 'owner\n', fs.readFileSync(obo, 'utf8'));
  const c1 = commitAs('ci-a', gitDir, 'identity e2e: a commit from A');
  check('b. the commit is authored and committed by the bot, not the repo config', c1.author === `ci-a (amux) <${email}>`
    && c1.committer === `ci-a (amux) <${email}>`, c1);
  check('b. it carries Amux-Session, Amux-Host and On-behalf-of', c1.body.includes('Amux-Session: ci-a')
    && c1.body.includes(`Amux-Host: ${host}`) && c1.body.includes('On-behalf-of: owner'), c1.body);

  // c. A worker asks; A acts for its sponsor.
  const sp = await amux.req('PUT', '/api/workers/ci-a/sponsor', {sponsor: 'member:sponsor@example.com'});
  check('c. the owner sets A sponsor', sp.status < 300, sp);
  const tokB = tenv('ci-b', 'AMUX_WORKER_TOKEN');
  cli('ci-b', tokB, 'send', 'ci-a', 'identity e2e: B asks A');
  await deliveredTo('ci-a', 'B asks A');
  await waitFor('on-behalf-of moves to the sponsor', () => fs.readFileSync(obo, 'utf8') === 'member:sponsor@example.com\n', 10000);
  const c2 = commitAs('ci-a', gitDir, 'identity e2e: second commit from A');
  check("c. a worker's ask: On-behalf-of is A's sponsor, not worker B", c2.body.includes('On-behalf-of: member:sponsor@example.com')
    && !c2.body.includes('On-behalf-of: ci-b'), c2.body);

  // d. A person's own choice is kept.
  check("d. a worker whose scope sets GIT_AUTHOR_NAME gets no bot identity", tenv('ci-c', 'GIT_AUTHOR_EMAIL') === null
    && tenv('ci-c', 'GIT_COMMITTER_NAME') === null && tenv('ci-c', 'AMUX_HOST_ID') === host,
    {e: tenv('ci-c', 'GIT_AUTHOR_EMAIL'), h: tenv('ci-c', 'AMUX_HOST_ID')});

  // e. Counters.
  const k = (await amux.req('GET', '/api/contract/counters')).body.agent_commit_identity || {};
  check('e. the counters report the launches, measured', k.measured === true && k.mode === 'bot'
    && k.launches_bot >= 2 && k.launches_kept_explicit >= 1 && k.on_behalf_written >= 2 && k.on_behalf_failed === 0, k);

  // f. Human mode.
  await amux.down();
  amux.env.AMUX_AGENT_COMMIT_IDENTITY = 'human';
  await amux.up();
  await amux.req('POST', '/api/sessions/ci-a/stop');
  await waitFor('ci-a stopped', async () => !(await amux.req('GET', '/api/sessions/ci-a')).body.running, 30000).catch(() => {});
  const before = amux.fakeLog().filter(x => x.event === 'launch' && (x.argv || []).includes('ci-a')).length;
  const s2 = await amux.req('POST', '/api/sessions/ci-a/start');
  check('f. A restarts in human mode', s2.status < 300, s2);
  await waitFor('relaunch ci-a', () => amux.fakeLog().filter(x => x.event === 'launch' && (x.argv || []).includes('ci-a')).length > before, 30000);
  check('f. human mode: no bot identity, the host id still set', tenv('ci-a', 'GIT_AUTHOR_EMAIL') === null
    && tenv('ci-a', 'GIT_AUTHOR_NAME') === null && tenv('ci-a', 'AMUX_HOST_ID') === host,
    {n: tenv('ci-a', 'GIT_AUTHOR_NAME'), h: tenv('ci-a', 'AMUX_HOST_ID')});
  ok = true;
} finally {
  const receipt = {scenario: 'agent-commit-identity', ok, checks};
  fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
  console.log(JSON.stringify({ok, passed: checks.filter(x => x.ok).length, root: amux.root}));
  await amux.stop();
}
if (!ok) process.exit(1);
