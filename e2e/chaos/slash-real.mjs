#!/usr/bin/env node
// REAL CLI: slash commands sent through amux reach the real provider CLI and
// their panels stay open, because amux presses Enter exactly once for them.
// Uses the owner's real sign-in (HOME is NOT private) and a cheap model;
// AMUX_HOME, port and tmux socket are private. Costs a few Haiku calls.
// Usage: AMUX_CHAOS_BINARY=<bin> PROVIDER=claude node e2e/chaos/slash-real.mjs
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, execFileSync } from 'node:child_process';
import { request, freePort, waitFor } from './harness.mjs';

const provider = process.env.PROVIDER || 'claude';
const CASES = {
  claude: { model: 'haiku',
    idle: [['/btw what is 2+2', /Esc to close/, /\b4\b/], ['/status', /Status|Version|Model/i], ['/context', /Context|tokens/i], ['/help', /Shortcuts|commands|help/i]],
    busy: 'Count from 1 to 80, one number per line.', midturn: ['/btw what is 3+3', /Esc to close/, /\b6\b/] },
  codex: { model: '', idle: [['/status', /Model|Session|Token|Account/i]] },
  gemini: { model: '', idle: [['/about', /Version|Model|CLI/i], ['/stats', /Session|Stats|Tokens|Duration/i]] },
};
const cfg = CASES[provider];
const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'amux-slash-'));
const home = path.join(root, 'amux-home'); fs.mkdirSync(home, { recursive: true });
const work = path.join(root, 'work'); fs.mkdirSync(work);
const tmuxDir = fs.mkdtempSync('/tmp/amux-slash-tmux-');
const port = await freePort(); const base = `https://localhost:${port}`;
const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('AMUX_') && !['TMUX', 'TMUX_PANE'].includes(k)));
Object.assign(env, { AMUX_HOME: home, AMUX_RS_PORT: String(port), TMUX_TMPDIR: tmuxDir, AMUX_ISOLATED: '1', AMUX_NO_SELF_ADOPT: '1', AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME: '1' });
const logf = fs.openSync(path.join(root, 'server.log'), 'a');
const srv = spawn(process.env.AMUX_CHAOS_BINARY, [], { env, stdio: ['ignore', logf, logf], detached: true });
const req = (m, p, b, t) => request(base, m, p, b, t);
const pane = () => { try { return execFileSync('tmux', ['capture-pane', '-p', '-t', 'amux-slashy'], { env, encoding: 'utf8' }); } catch { return ''; } };
const tail = (p, n = 8) => p.split('\n').filter(l => l.trim()).slice(-n).join('\n');
try {
  await waitFor('health', async () => (await req('GET', '/health', undefined, 2000).catch(() => ({}))).status === 200, 60000, 500);
  const body = { name: 'slashy', dir: work, provider }; if (cfg.model) body.model = cfg.model;
  const c = await req('POST', '/api/sessions', body);
  check('worker created', c.status === 201, c.body);
  // Startup modals are the CLI's, not amux's: skip codex's update offer and
  // trust only this test's temp work folder for gemini.
  const keys = (...k) => { try { execFileSync('tmux', ['send-keys', '-t', 'amux-slashy', ...k], { env }); } catch {} };
  await waitFor('cli ready', () => {
    const p = pane(); fs.writeFileSync(path.join(root, 'last-pane.txt'), p);
    if (/Update available/.test(p) && /Skip until next version/.test(p)) { keys('2'); keys('Enter'); return false; }
    if (/Do you trust the files in this folder/.test(p)) { keys('Enter'); return false; }
    return /❯|›|>/.test(p) && /shortcuts|bypass|manual mode|for help|Type your message|context left|esc to|Ask Codex/i.test(p);
  }, 120000, 1000);
  fs.writeFileSync(path.join(root, 'ready-pane.txt'), pane());
  await new Promise(r => setTimeout(r, 5000));
  let direct = 0;
  const run = async (cmd, want, answer, label) => {
    const r = await req('POST', '/api/workers/slashy/send', { text: cmd }, 60000);
    if (r.body && r.body.submission !== 'deferred') direct++;
    const seen = await waitFor(label, () => { const p = pane(); return want.test(p) && (!answer || answer.test(p)) ? p : null; }, 45000, 500).catch(() => null);
    await new Promise(r => setTimeout(r, 3000));
    const after = pane();
    check(`${label}: accepted by amux`, r.status === 200 && r.body.ok !== false, r.body);
    check(`${label}: the CLI shows its result`, !!seen, tail(after));
    check(`${label}: still on screen after the send finished (no second Enter)`, want.test(after) && (!answer || answer.test(after)), tail(after, 6));
    execFileSync('tmux', ['send-keys', '-t', 'amux-slashy', 'Escape'], { env });
    await new Promise(r => setTimeout(r, 1500));
  };
  for (const [cmd, want, answer] of cfg.idle) await run(cmd, want, answer, `${provider} idle ${cmd.split(' ')[0]}`);
  if (cfg.busy) {
    await req('POST', '/api/workers/slashy/send', { text: cfg.busy }, 60000);
    const busy = await waitFor('busy', () => /esc to interrupt/i.test(pane()), 30000, 300).catch(() => false);
    check('the worker is mid-turn before the mid-turn /btw', !!busy);
    const [cmd, want, answer] = cfg.midturn;
    await run(cmd, want, answer, `${provider} MID-TURN ${cmd.split(' ')[0]}`);
  }
  const log = fs.readFileSync(path.join(root, 'server.log'), 'utf8');
  check('amux never pressed a retry Enter on a slash command', !/submission_enter_retry|mid-turn Enter was not accepted/.test(log), (log.match(/.*(submission_enter_retry|mid-turn Enter was not accepted).*/g) || []).slice(0, 3));
  check('each slash command was logged as a single Enter', (log.match(/slash_command_single_enter/g) || []).length >= direct, { direct, logged: (log.match(/slash_command_single_enter/g) || []).length });
} catch (e) { check('harness ran to completion', false, String(e && e.stack || e)); }
finally { try { process.kill(-srv.pid, 'SIGKILL'); } catch {} try { execFileSync('tmux', ['kill-server'], { env }); } catch {} }
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ provider, measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
