// The `goal` verb end to end: a real browser route, a real page, and an agent
// endpoint that speaks OpAgent's protocol. The model is a STAND-IN (a local
// OpenAI-compatible server that answers in OpAgent's exact <tool_call> format)
// so the loop, the adapter, the coordinate mapping and the trajectory are
// proven without a 20 GB model; the real model is the same request and reply.
//
// Run against an isolated candidate server, like browser-routing-e2e.mjs:
//   AMUX_ROUTING_E2E_BASE=https://127.0.0.1:<port> AMUX_ROUTING_E2E_HOME=<home> node tests/browser-goal-e2e.mjs
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { run, api } from '../scripts/browser-route-driver.mjs';

const base = process.env.AMUX_ROUTING_E2E_BASE;
const home = process.env.AMUX_ROUTING_E2E_HOME;
assert(base && home, 'point at an isolated candidate server and its AMUX_HOME');
const here = dirname(fileURLToPath(import.meta.url));
const checks = [];
const prove = (label, cond, detail) => { assert(cond, label + (detail ? ' :: ' + JSON.stringify(detail).slice(0, 600) : '')); checks.push(label); console.log('PASS ' + label); };

// The page: one button at a fixed place (centre at 50% x, 55% y of the
// viewport). Clicking it is recorded server-side, so success is the site's
// observation, not the driver's claim.
const clicks = [];
const site = createServer((req, res) => {
  if (req.url === '/clicked') { clicks.push(Date.now()); res.end('ok'); return; }
  res.setHeader('Content-Type', 'text/html');
  res.end(`<!doctype html><title>Goal proof</title><style>html,body{margin:0;height:100%}#reveal{position:fixed;left:40%;top:50%;width:20%;height:10%;font-size:20px}</style>
<button id="reveal" onclick="fetch('/clicked');document.title='revealed';this.textContent='Code 4242'">Reveal the code</button>`);
});
await new Promise(r => site.listen(0, '127.0.0.1', r));
const siteUrl = `http://127.0.0.1:${site.address().port}/`;

// The stand-in agent: step 1 clicks the button on OpAgent's 0-1000 grid,
// step 2 answers. It checks every request has the screenshot and the task.
const seen = [];
const agentSrv = createServer(async (req, res) => {
  let body = ''; for await (const c of req) body += c;
  const j = JSON.parse(body);
  const parts = j.messages?.[0]?.content || [];
  seen.push({ model: j.model, hasImage: parts.some(p => p.type === 'image_url' && p.image_url.url.startsWith('data:image/png;base64,')), prompt: parts.find(p => p.type === 'text')?.text || '' });
  const reply = seen.length === 1
    ? '<think>The code is behind the button.</think><tool_call>{"name": "click", "arguments": {"coords": [500, 550]}}</tool_call>'
    : '<think>The button now shows the code.</think><tool_call>{"name": "answer", "arguments": {"content": "4242"}}</tool_call>';
  res.setHeader('Content-Type', 'application/json');
  res.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: reply } }] }));
});
await new Promise(r => agentSrv.listen(0, '127.0.0.1', r));
const endpoint = `http://127.0.0.1:${agentSrv.address().port}/v1/chat/completions`;

const profile = 'goal-proof';
mkdirSync(join(home, 'playwright-auth', 'profiles', profile), { recursive: true });
const agent = { id: 'stand-in', protocol: 'opagent-v1', endpoint, model: 'OpAgent-32B-Q4', enabled: true, max_steps: 5, settle_ms: 300 };
const ctx = { base, home, session: 'goal-e2e', config: { native_profile: profile, chrome_profile: '', cua_profile: '', allow_cua: false },
  methods: { ladder: ['amux', 'cdp', 'cua'], agents: [agent] },
  agent_modules: { 'opagent-v1': resolve(here, '../scripts/browser-agents/opagent-v1.mjs') } };
const request = (verb, body = {}, c = ctx) => run({ context: c, verb, body });

try {
  const started = await request('start', { url: siteUrl, profile });
  prove('route started on the amux backend', started.route?.backend === 'amux', started);

  const g = await request('goal', { task: 'Reveal the code and tell me what it is', agent: 'stand-in' });
  prove('the goal finished with the agent\'s answer', g.ok === true && g.done === true && g.answer === '4242', g);
  prove('it took the expected two model steps', g.steps === 2 && seen.length === 2, { g, seen: seen.length });
  prove('the page itself recorded the agent\'s click', clicks.length === 1, clicks);
  prove('every model call carried the screenshot and the task', seen.every(s => s.hasImage && s.prompt.includes("Instruction: 'Reveal the code and tell me what it is'.")));
  prove('the second call carried the first action as history', seen[1].prompt.includes('"name": "click"'));
  const traj = JSON.parse(readFileSync(g.trajectory, 'utf8'));
  prove('the trajectory names the agent, backend and each step', traj.agent === 'stand-in' && traj.backend === 'amux' && traj.steps.length === 2 && traj.steps[0].action?.kind === 'click' && traj.steps[1].done?.answer === '4242', traj);
  prove('each step keeps its screenshot', traj.steps.every(s => existsSync(s.screenshot)));

  // A disabled agent is refused with a reason, and nothing is clicked.
  const off = await request('goal', { task: 'x', agent: 'off' }, { ...ctx, methods: { ...ctx.methods, agents: [{ ...agent, id: 'off', enabled: false }] } });
  prove('a disabled agent is refused, not run', off.status === 409 && /disabled/.test(off.error || ''), off);

  // An agent that never answers usefully stops after two empty replies.
  seen.length = 0;
  const junk = createServer((req, res) => { req.resume(); req.on('end', () => { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify({ choices: [{ message: { content: 'I am not sure.' } }] })); }); });
  await new Promise(r => junk.listen(0, '127.0.0.1', r));
  const confused = await request('goal', { task: 'x', agent: 'junk' }, { ...ctx, methods: { ...ctx.methods, agents: [{ ...agent, id: 'junk', endpoint: `http://127.0.0.1:${junk.address().port}/v1/chat/completions` }] } });
  prove('an agent with no usable action stops after two tries and says so', confused.ok === false && /no usable action/.test(confused.reason || ''), confused);
  junk.close();

  // Through the shipped endpoint: the owner's ~/.amux/browser-methods.toml
  // enables an agent, and the server's registry and adapters reach the driver.
  writeFileSync(join(home, 'browser-methods.toml'), `[[agent]]\nid = "opagent"\nendpoint = "${endpoint}"\nenabled = true\nsettle_ms = 300\n`);
  seen.length = 0; clicks.length = 0;
  const viaApi = (verb, body) => api(base, '/api/browser/routing/request', 'POST', { verb, session: 'goal-e2e-api', body }, '', process.env.AMUX_ROUTING_E2E_TOKEN || '');
  const cfg = await api(base, '/api/browser/routing/config');
  prove('the config shows the merged methods registry', cfg.methods?.agents?.some(a => a.id === 'opagent' && a.enabled === true && a.endpoint === endpoint), cfg.methods);
  await viaApi('start', { url: siteUrl, profile });
  const g2 = await viaApi('goal', { task: 'Reveal the code and tell me what it is' });
  prove('the shipped endpoint runs the owner-enabled OpAgent method', g2.ok === true && g2.answer === '4242' && g2.agent === 'opagent' && clicks.length === 1, {clicks: clicks.length, seen: seen.length, traj: JSON.parse(readFileSync(g2.trajectory, 'utf8')).steps.map(s => ({a: s.action, scale: s.scale, image: s.image, url: s.url}))});
  await viaApi('stop', {});
  await request('stop');
  console.log(`${checks.length} checks passed`);
} finally {
  site.close(); agentSrv.close();
}
