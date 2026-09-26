// A fully private amux for chaos and end-to-end proofs.
//
// Own HOME, AMUX_HOME, port and tmux socket, and a fake `claude` first on PATH
// (e2e/chaos/bin/claude) that logs what it actually received and where it
// actually runs. Nothing here can reach the live fleet: the tmux socket lives
// under a fresh short /tmp dir, and HOME is private so trust seeding cannot
// write into the real ~/.claude.json.
import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import https from 'node:https';

export const CHAOS_DIR = path.dirname(new URL(import.meta.url).pathname);

export async function freePort() {
  return new Promise(res => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => res(p)); }); });
}

export function request(base, method, p, body, timeoutMs = 30000, extraHeaders = {}) {
  return new Promise((resolve, reject) => {
    const data = body === undefined ? undefined : Buffer.from(JSON.stringify(body));
    const r = https.request(base + p, { method, rejectUnauthorized: false, timeout: timeoutMs,
      headers: { 'Content-Type': 'application/json', ...(data ? { 'Content-Length': data.length } : {}), ...extraHeaders } }, res => {
      let buf = ''; res.on('data', c => buf += c);
      res.on('end', () => { let j = {}; try { j = JSON.parse(buf || '{}'); } catch { j = { raw: buf }; } resolve({ status: res.statusCode, body: j }); });
    });
    r.on('timeout', () => r.destroy(new Error('timeout')));
    r.on('error', reject);
    if (data) r.write(data);
    r.end();
  });
}

export async function startAmux({ binary, env: extra = {}, root } = {}) {
  if (!binary || !fs.existsSync(binary)) throw new Error('pass the amux-server binary (AMUX_CHAOS_BINARY)');
  root = root || fs.mkdtempSync(path.join(os.tmpdir(), 'amux-chaos-'));
  const home = path.join(root, 'amux-home'); const userHome = path.join(root, 'home');
  // tmux appends to TMUX_TMPDIR; macOS's long TMPDIR overflows the socket path.
  const tmuxDir = fs.mkdtempSync('/tmp/amux-chaos-tmux-');
  fs.mkdirSync(home, { recursive: true }); fs.mkdirSync(userHome, { recursive: true });
  let port = await freePort();
  const log = path.join(root, 'fake-claude.log');
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('AMUX_') && !['TMUX', 'TMUX_PANE'].includes(k)));
  Object.assign(env, {
    HOME: userHome, AMUX_HOME: home, AMUX_RS_PORT: String(port), TMUX_TMPDIR: tmuxDir,
    AMUX_ISOLATED: '1', AMUX_NO_SELF_ADOPT: '1',
    // AMUX-4724 refuses spawns from a temp AMUX_HOME because a test usually
    // shares the REAL tmux server. This one does not: TMUX_TMPDIR above is a
    // fresh private socket dir, and the agent is the fake on PATH. So the
    // spawn is genuinely intended, which is what the override is for.
    AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME: '1',
    PATH: path.join(CHAOS_DIR, 'bin') + path.delimiter + process.env.PATH,
    FAKE_CLAUDE_LOG: log, FAKE_CLAUDE_RAW_LOG: path.join(root, 'fake-claude.raw'),
    GIT_AUTHOR_NAME: 'chaos', GIT_AUTHOR_EMAIL: 'chaos@example.invalid',
    GIT_COMMITTER_NAME: 'chaos', GIT_COMMITTER_EMAIL: 'chaos@example.invalid',
  }, extra);
  let base = `https://localhost:${port}`;
  const serverLog = path.join(root, 'server.log');
  let proc = null;
  const up = async () => {
    const out = fs.openSync(serverLog, 'a');
    proc = spawn(binary, [], { env, stdio: ['ignore', out, out], detached: true });
    let exited = false; proc.on('exit', () => { exited = true; });
    for (let i = 0; i < 240; i++) {
      if (exited) throw new Error('server exited during boot; see ' + serverLog);
      try { if ((await request(base, 'GET', '/health', undefined, 2000)).status === 200) return; } catch {}
      await new Promise(r => setTimeout(r, 250));
    }
    throw new Error('server never became healthy; see ' + serverLog);
  };
  const down = async () => {
    if (!proc) return;
    const p = proc; proc = null;
    try { process.kill(-p.pid, 'SIGKILL'); } catch {}
    // Wait for the port to be released before any restart binds it again.
    for (let i = 0; i < 80; i++) {
      const busy = await new Promise(res => { const s = net.connect(port, '127.0.0.1'); s.on('connect', () => { s.destroy(); res(true); }); s.on('error', () => res(false)); });
      if (!busy) return;
      await new Promise(r => setTimeout(r, 100));
    }
  };
  const tmux = (...args) => execFileSync('tmux', args, { env, encoding: 'utf8' });
  const fakeLog = () => fs.existsSync(log)
    ? fs.readFileSync(log, 'utf8').split('\n').filter(Boolean).map(l => JSON.parse(l)) : [];
  const stop = async () => { await down(); try { tmux('kill-server'); } catch {} };
  // The free-port probe races every other process on a busy box: a port can
  // be taken between probe and bind (seen: AddrInUse). Retry the FIRST boot on
  // a fresh port; a restart keeps its port, since clients already hold it.
  for (let attempt = 1; ; attempt++) {
    try { await up(); break; } catch (e) {
      await down();
      if (attempt >= 3) throw e;
      port = await freePort(); base = `https://localhost:${port}`;
      env.AMUX_RS_PORT = String(port);
    }
  }
  return { get base() { return base; }, root, home, userHome, env, get port() { return port; }, serverLog, up, down, stop, tmux, fakeLog,
    req: (m, p, b, t, h) => request(base, m, p, b, t, h) };
}

export function git(cwd, ...args) {
  return execFileSync('git', ['-C', cwd, ...args], { encoding: 'utf8',
    env: { ...process.env, GIT_AUTHOR_NAME: 'chaos', GIT_AUTHOR_EMAIL: 'chaos@example.invalid',
      GIT_COMMITTER_NAME: 'chaos', GIT_COMMITTER_EMAIL: 'chaos@example.invalid' } }).trim();
}

export async function waitFor(what, fn, timeoutMs = 30000, stepMs = 200) {
  const end = Date.now() + timeoutMs; let last;
  while (Date.now() < end) {
    try { last = await fn(); if (last) return last; } catch (e) { last = e; }
    await new Promise(r => setTimeout(r, stepMs));
  }
  throw new Error(`timed out waiting for ${what}; last=${last instanceof Error ? last.message : JSON.stringify(last)}`);
}
