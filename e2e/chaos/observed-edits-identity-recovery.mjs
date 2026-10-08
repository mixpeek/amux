#!/usr/bin/env node
// The installed Python hook talks to real authenticated middleware and a real
// durable store. Private provider credentials stay in memory/environment only.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {startAmux, waitFor, git} from './harness.mjs';

const repo = path.resolve(path.dirname(new URL(import.meta.url).pathname), '../..');
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG: 'info', AMUX_ISOLATED: '0', AMUX_BOARD_DRIVE_SECS: '0',
  AMUX_AUTOFIX_SECS: '0', AMUX_GHOST_RESCUE_SECS: '0',
  AMUX_MODEL_CATALOG_REFRESH_SECS: '0', FAKE_CLAUDE_ENV_KEYS: 'AMUX_WORKER_TOKEN',
}});
const checks = [];
const check = (name, ok, detail) => {
  checks.push({name, ok: !!ok, detail});
  if (!ok) throw Error(name + ': ' + JSON.stringify(detail));
};
const name = 'hook-identity';
const dir = path.join(amux.root, 'edits');
const hooks = path.join(amux.home, 'hooks');
let token = '';
const readReports = () => JSON.parse(execFileSync('python3', ['-c',
  'import json,sqlite3,sys; c=sqlite3.connect("file:"+sys.argv[1]+"?mode=ro",uri=True); r=c.execute("select value from prefs where key=?",(sys.argv[2],)).fetchone(); print(r[0] if r else "{}")',
  path.join(amux.home, 'amux.db'), 'observed_edits:' + name], {encoding: 'utf8'}));
const hookLog = () => fs.readFileSync(path.join(hooks, 'state', 'observed-edits.log'), 'utf8');
const hookEnv = credential => {
  const env = {...amux.env, AMUX_URL: amux.base, AMUX_SESSION: name};
  delete env.AMUX_WORKER_TOKEN;
  if (credential !== undefined) env.AMUX_WORKER_TOKEN = credential;
  return env;
};
const post = credential => execFileSync('python3', [path.join(hooks, 'observed-edits-post.py')], {
  env: hookEnv(credential), input: JSON.stringify({cwd: dir, tool_input: {command: 'cp source changed.txt'}}),
  encoding: 'utf8', timeout: 10000,
});
const markAndWrite = async (file, credential) => {
  execFileSync('python3', [path.join(hooks, 'observed-edits-pre.py')], {env: hookEnv(credential), timeout: 10000});
  await new Promise(r => setTimeout(r, 50));
  fs.writeFileSync(path.join(dir, file), 'actual fixture edit\n');
};
try {
  fs.mkdirSync(dir); fs.mkdirSync(hooks);
  git(dir, 'init', '-q');
  // Exercise the actual builder sync function on an exact committed snapshot.
  // The default fixture source is the current checkout; the predecessor hook
  // override is a negative control and is recorded by its content hash.
  const source = path.join(amux.root, 'hook-source');
  fs.mkdirSync(path.join(source, 'scripts', 'claude-hooks'), {recursive: true});
  const commands = path.join(amux.root, 'private-commands');
  fs.mkdirSync(commands);
  fs.mkdirSync(path.join(source, '.claude', 'commands'), {recursive: true});
  fs.copyFileSync(path.join(repo, '.claude', 'commands', 'orchestrate.md'), path.join(source, '.claude', 'commands', 'orchestrate.md'));
  const command = path.join(commands, 'orchestrate.md');
  fs.writeFileSync(command, 'stale orchestration command\n');
  const commandBefore = fs.statSync(command).ino;
  for (const half of ['pre', 'post']) {
    const src = half === 'post' && process.env.AMUX_CHAOS_OBSERVED_HOOK
      ? process.env.AMUX_CHAOS_OBSERVED_HOOK : path.join(repo, 'scripts', 'claude-hooks', `observed-edits-${half}.py`);
    fs.copyFileSync(src, path.join(source, 'scripts', 'claude-hooks', `observed-edits-${half}.py`));
    fs.writeFileSync(path.join(hooks, `observed-edits-${half}.py`), '# stale hook\n');
  }
  git(source, 'init', '-q'); git(source, 'add', '.');
  git(source, '-c', 'core.hooksPath=/dev/null', 'commit', '-qm', 'exact hook fixture source');
  const sha = git(source, 'rev-parse', 'HEAD');
  const builder = fs.readFileSync(path.join(repo, 'scripts', 'rust-auto-build.sh'), 'utf8');
  const sync = builder.match(/^sync_observed_edits_hooks\(\) \{[\s\S]*?^\}/m)?.[0];
  check('actual builder hook consumer exists', !!sync);
  const before = fs.statSync(path.join(hooks, 'observed-edits-post.py')).ino;
  const deployLog = path.join(amux.root, 'hook-sync.log');
  const syncEnv = {...amux.env, REPO: source, LOG: deployLog, AMUX_OBSERVED_HOOK_DIR: hooks, AMUX_CLAUDE_COMMAND_DIR: commands};
  execFileSync('bash', ['-euo', 'pipefail', '-c', sync + '\nsync_observed_edits_hooks "$1"', 'fixture', sha], {env: syncEnv});
  check('committed hooks installed by rename and verified', ['pre', 'post'].every(half =>
    fs.readFileSync(path.join(source, 'scripts', 'claude-hooks', `observed-edits-${half}.py`)).equals(fs.readFileSync(path.join(hooks, `observed-edits-${half}.py`))))
    && fs.statSync(path.join(hooks, 'observed-edits-post.py')).ino !== before
    && fs.readFileSync(deployLog, 'utf8').includes('verdict=observed_hook_synced'));
  const installedInode = fs.statSync(path.join(hooks, 'observed-edits-post.py')).ino;
  check('existing orchestrate command adopts exact committed source atomically',
    fs.readFileSync(command).equals(fs.readFileSync(path.join(source, '.claude', 'commands', 'orchestrate.md')))
    && fs.statSync(command).ino !== commandBefore
    && fs.readFileSync(deployLog, 'utf8').includes('verdict=orchestration_command_synced'));
  const commandInode = fs.statSync(command).ino;
  execFileSync('bash', ['-euo', 'pipefail', '-c', sync + '\nsync_observed_edits_hooks "$1"', 'fixture', sha], {env: syncEnv});
  check('unchanged committed hook needs no replacement', fs.statSync(path.join(hooks, 'observed-edits-post.py')).ino === installedInode);
  check('unchanged command needs no replacement', fs.statSync(command).ino === commandInode);
  const protectedFile = path.join(amux.root, 'protected-command');
  fs.writeFileSync(protectedFile, 'owner-managed symlink target\n');
  fs.unlinkSync(command); fs.symlinkSync(protectedFile, command);
  execFileSync('bash', ['-euo', 'pipefail', '-c', sync + '\nsync_observed_edits_hooks "$1"', 'fixture', sha], {env: syncEnv});
  check('builder preserves owner-managed command symlinks', fs.lstatSync(command).isSymbolicLink()
    && fs.readFileSync(protectedFile, 'utf8') === 'owner-managed symlink target\n');
  const created = await amux.req('POST', '/api/sessions', {name, dir, start: false});
  check('private worker created', created.status === 201, created.status);
  fs.appendFileSync(path.join(amux.home, 'sessions', name + '.env'), '\nAMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_RULES_OFF="1,2,3,4,5,6,7,8,9"\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\n');
  const started = await amux.req('POST', `/api/sessions/${name}/start`);
  check('real provider launch accepted', started.status < 300, started.status);
  await waitFor('actual provider credential inheritance', () => amux.fakeLog().some(x => x.event === 'launch' && x.env_sha256?.AMUX_WORKER_TOKEN), 30000);
  token = amux.tmux('show-environment', '-t', 'amux-' + name, 'AMUX_WORKER_TOKEN').trim().split('=')[1];
  const digest = crypto.createHash('sha256').update(token).digest('hex');
  const launch = amux.fakeLog().find(x => x.event === 'launch');
  check('launch and durable credential agree by digest', token.length === 64
    && launch.env_sha256.AMUX_WORKER_TOKEN === digest
    && fs.readFileSync(path.join(amux.home, 'worker-tokens', name + '.sha256'), 'utf8').trim() === digest);
  await markAndWrite('changed.txt');
  check('missing credential remains fail-open but is refused', post(undefined) === ''
    && hookLog().includes('send-failed:HTTPError') && Object.keys(readReports()).length === 0
    && fs.readFileSync(amux.serverLog, 'utf8').includes('worker_identity_refused'));
  const logBeforeWrong = hookLog().length;
  check('wrong credential cannot mint attribution', post('wrong-fixture-credential') === ''
    && hookLog().slice(logBeforeWrong).includes('send-failed:HTTPError') && Object.keys(readReports()).length === 0);
  post(token);
  check('valid inherited credential stores actual edited file', Object.hasOwn(readReports(), fs.realpathSync(path.join(dir, 'changed.txt')))
    && hookLog().split('\n').filter(Boolean).at(-1).includes(' sent'));
  const stored = readReports();
  post(token);
  check('repeated report does not invent a newer edit', JSON.stringify(readReports()) === JSON.stringify(stored));
  await amux.down();
  check('provider survives actual controller SIGKILL', (() => {try {process.kill(launch.pid, 0); return true;} catch {return false;}})());
  await amux.up();
  check('stored edit survives restart', JSON.stringify(readReports()) === JSON.stringify(stored));
  await markAndWrite('after-restart.txt', token); post(token);
  check('existing provider credential works after restart', Object.hasOwn(readReports(), fs.realpathSync(path.join(dir, 'after-restart.txt')))
    && amux.fakeLog().filter(x => x.event === 'launch').length === 1);
  check('authentication refusal has a named server signal', fs.readFileSync(amux.serverLog, 'utf8').includes('worker_identity_refused'));
  check('hook names the numeric authentication refusal', hookLog().includes('send-failed:HTTPError status=403'));
  check('credential never appears in retained logs', [amux.serverLog, deployLog,
    path.join(hooks, 'state', 'observed-edits.log'), path.join(amux.root, 'fake-claude.log'), path.join(amux.root, 'fake-claude.raw')]
    .every(file => !fs.existsSync(file) || !fs.readFileSync(file, 'utf8').includes(token)));
} catch (e) {
  checks.push({name: 'scenario completed', ok: false, detail: String(e.stack || e)});
} finally {await amux.stop();}
const receipt = {measured: true, n_considered: checks.length, failed: checks.filter(x => !x.ok).length,
  artifacts: amux.root, fixture_boundary: 'committed hook installed by actual builder function; real authenticated HTTPS middleware, tmux provider environment and SQLite store; actual controller SIGKILL/restart; missing and wrong credentials are negative controls',
  hook_sha256: crypto.createHash('sha256').update(fs.readFileSync(path.join(hooks, 'observed-edits-post.py'))).digest('hex'), checks};
fs.writeFileSync(path.join(amux.root, 'recovery-receipt.json'), JSON.stringify(receipt, null, 2));
console.log(JSON.stringify(receipt, null, 2)); process.exit(receipt.failed ? 1 : 0);
