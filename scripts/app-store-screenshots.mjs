// App Store screenshots for ios/fastlane/screenshots/en-US, rendered from a
// FAKE fleet (Acme workers, cards, events, schedules). Every /api/* request is
// answered from the fixtures below or with an empty body, so no real worker,
// customer, email or card can appear on the public store page. The live
// server only serves the dashboard's static files.
//
//   U=$(amux url) OUT=/tmp/shots DEVICE=iphone node scripts/app-store-screenshots.mjs   # 1320x2868
//   U=$(amux url) OUT=/tmp/shots DEVICE=ipad   node scripts/app-store-screenshots.mjs   # 2064x2752
//
// Then copy into ios/fastlane/screenshots/en-US as iphone_<name>.png / ipad_<name>.png
// and review every image before committing: the release lane overwrites the store set.
import { chromium } from 'playwright';
import fs from 'node:fs';
const base = process.env.U, out = process.env.OUT, device = process.env.DEVICE || 'iphone';
const now = Math.floor(Date.now() / 1000);
const E = '\x1b[';
const W = (o) => ({
  archived: false, isolated: false, ephemeral: false, pinned: false, orchestrator: false,
  backend: 'tmux', managed_by: 'tmux', lifecycle: 'active', provider: 'claude', yolo: true,
  auto_pickup: true, auto_continue: true, standing_orders: true, board_decompose: true,
  board_force_adherence: false, branch: 'main', composer_preview: '', composer_stuck_since: 0,
  creator: 'MacBook Pro', credit_limited: false, api_error: false, rate_limit_banner: false,
  rate_limited_until: 0, review_held: false, steering: [], steering_queue: [], subagent_live_ids: [],
  subagents_live: 0, agents_working: false, spans_groups: true, waiting_since: 0,
  worktree: '', worktree_active: false, worktree_integration: null, worktree_path: '',
  worktree_repo: '', mcp: '', model: '', model_source: 'self-report', task_source: 'board',
  task_override: '', task_board_age: 0, task_time: 0, board_drive: null,
  tokens_source: 'transcript', last_human_ts: (now - 600) * 1000, session_created: now - 86400 * 3,
  preview: '', preview_lines: [], ...o });
const fleet = [
  W({ name: 'api-server', status: 'active', running: true, dir: '/Users/dev/code/acme/api', tags: ['backend'],
      desc: 'REST API and background jobs for the Acme app.', task_name: 'Add rate limiting to the upload endpoint',
      task_board_id: 'ACME-142', flags: '--model claude-opus-5-5', active_model: 'claude-opus-5-5',
      last_activity: now - 20, tokens: { input: 842000, output: 0, total: 842000 }, sched_on: 2, sched_off: 0,
      runtime_board: { measured: true, status: 'linked', card_count: 4, card_id: 'ACME-142', card_live: true, runtime_status: 'active' },
      self_report: { source: 'claude-hook', state: 'active', ts: now } }),
  W({ name: 'web-app', status: 'active', running: true, dir: '/Users/dev/code/acme/web', tags: ['frontend'],
      desc: 'Next.js web client.', task_name: 'Redesign the checkout flow for mobile', task_board_id: 'ACME-138',
      flags: '--model claude-sonnet-5', active_model: 'claude-sonnet-5', last_activity: now - 45,
      tokens: { input: 516000, output: 0, total: 516000 }, sched_on: 1, sched_off: 0,
      runtime_board: { measured: true, status: 'linked', card_count: 3, card_id: 'ACME-138', card_live: true, runtime_status: 'active' },
      self_report: { source: 'claude-hook', state: 'active', ts: now } }),
  W({ name: 'ios-app', status: 'waiting', running: true, dir: '/Users/dev/code/acme/ios', tags: ['mobile'],
      desc: 'SwiftUI app.', task_name: 'Fix push token refresh after reinstall', task_board_id: 'ACME-131',
      flags: '--model claude-opus-5-5', active_model: 'claude-opus-5-5', last_activity: now - 240, waiting_since: now - 200,
      tokens: { input: 301000, output: 0, total: 301000 }, sched_on: 0, sched_off: 0,
      runtime_board: { measured: true, status: 'linked', card_count: 2, card_id: 'ACME-131', card_live: true, runtime_status: 'waiting' },
      self_report: { source: 'claude-hook', state: 'blocked', ts: now } }),
  W({ name: 'docs', status: 'active', running: true, provider: 'codex', dir: '/Users/dev/code/acme/docs', tags: ['docs'],
      desc: 'Public developer docs.', task_name: 'Write the v2 API migration guide', task_board_id: 'ACME-127',
      flags: '--model gpt-6', active_model: 'gpt-6', last_activity: now - 90,
      tokens: { input: 204000, output: 0, total: 204000 }, sched_on: 0, sched_off: 0,
      runtime_board: { measured: true, status: 'linked', card_count: 1, card_id: 'ACME-127', card_live: true, runtime_status: 'active' },
      self_report: { source: 'codex', state: 'active', ts: now } }),
  W({ name: 'infra', status: 'idle', running: true, dir: '/Users/dev/code/acme/infra', tags: ['backend'],
      desc: 'Terraform and CI.', task_name: 'Add a read replica to staging', task_board_id: 'ACME-120',
      flags: '--model claude-sonnet-5', active_model: 'claude-sonnet-5', last_activity: now - 1800,
      tokens: { input: 98000, output: 0, total: 98000 }, sched_on: 1, sched_off: 1,
      runtime_board: { measured: true, status: 'linked', card_count: 1, card_id: 'ACME-120', card_live: true, runtime_status: 'idle' },
      self_report: { source: 'claude-hook', state: 'idle', ts: now } }),
  W({ name: 'qa', status: 'idle', running: true, dir: '/Users/dev/code/acme/e2e', tags: ['qa'],
      desc: 'End-to-end and visual regression tests.', task_name: 'Nightly e2e suite', task_board_id: '',
      flags: '--model claude-haiku-4-5', active_model: 'claude-haiku-4-5', last_activity: now - 5400,
      tokens: { input: 61000, output: 0, total: 61000 }, sched_on: 2, sched_off: 0,
      runtime_board: { measured: true, status: 'idle', card_count: 0, card_id: '', card_live: false, runtime_status: 'idle' },
      self_report: { source: 'claude-hook', state: 'idle', ts: now } }),
];
const card = (id, title, status, session, extra = {}) => ({ id, title, status, session, type: 'code', tags: [],
  owner_type: 'human', created: now - 86400 * 2, updated: now - 3600, entered_state_at: now - 3600, pos: 0, rev: 1,
  creator: session, source: 'owner', slim: [], desc_len: 40, desc_head: '', archived: false, live: status === 'doing',
  depends_on: [], acceptance_criteria: [], evidence: [], log_n: 3, version: 1, ...extra });
const board = [
  card('ACME-142', 'Add rate limiting to the upload endpoint', 'doing', 'api-server', { desc_head: 'Token bucket per API key, 429 with Retry-After.' }),
  card('ACME-138', 'Redesign the checkout flow for mobile', 'doing', 'web-app', { desc_head: 'One-page checkout, Apple Pay first.' }),
  card('ACME-131', 'Fix push token refresh after reinstall', 'doing', 'ios-app', { desc_head: 'Tokens go stale after a reinstall.' }),
  card('ACME-127', 'Write the v2 API migration guide', 'doing', 'docs'),
  card('ACME-145', 'Cache product images at the edge', 'todo', 'web-app'),
  card('ACME-144', 'Paginate the orders endpoint', 'todo', 'api-server'),
  card('ACME-143', 'Dark mode for the settings screen', 'todo', 'ios-app'),
  card('ACME-120', 'Add a read replica to staging', 'review', 'infra', { reviewer: 'api-server' }),
  card('ACME-118', 'Flaky login test on Safari', 'review', 'qa'),
  card('ACME-115', 'Upgrade to Postgres 17', 'done', 'infra'),
  card('ACME-112', 'Onboarding email sequence', 'done', 'web-app'),
  card('ACME-150', 'Offline mode for the order history', 'backlog', 'ios-app'),
  card('ACME-149', 'Usage-based billing research', 'backlog', 'api-server'),
];
const day = (d, h, m = 0) => { const t = new Date(); t.setDate(t.getDate() + d); t.setHours(h, m, 0, 0);
  const p = (n) => String(n).padStart(2, '0'); return `${t.getFullYear()}-${p(t.getMonth()+1)}-${p(t.getDate())}T${p(h)}:${p(m)}:00`; };
const ev = (id, title, d, h, dur = 30) => ({ id: 'EVT-' + id, title, start: day(d, h), end: day(d, h, dur % 60), all_day: 0,
  description: '', location: '', rrule: '', created: now, updated: now, deleted: 0 });
const events = [ev(1, 'Standup', 0, 9), ev(2, 'Release review', 0, 14), ev(3, 'Standup', 1, 9), ev(4, 'Design sync', 1, 11),
  ev(5, 'Deploy window', 2, 16), ev(6, 'Standup', 3, 9), ev(7, 'Customer demo', 3, 13), ev(8, 'Sprint planning', 6, 10),
  ev(9, 'Standup', -1, 9), ev(10, 'Retro', -2, 15), ev(11, 'On-call handoff', 4, 17), ev(12, 'Standup', 7, 9)];
const sched = (id, title, session, expr, command, extra = {}) => ({ id, title, session, schedule_expr: expr, command,
  enabled: 1, kind: 'tmux', created: now - 86400 * 10, updated: now - 86400, next_run: day(1, 2).slice(0, 16),
  computed_next_run: day(1, 2).slice(0, 16), last_run: day(0, 2).slice(0, 16), run_at: '', run_count: 14, deleted: 0,
  sched_type: 'recurring', recurrence: expr, version: 1, ...extra });
const schedules = [
  sched('SCHED-1', 'Nightly e2e suite', 'qa', 'daily at 2am', 'Run the full e2e suite and file a card for each failure'),
  sched('SCHED-2', 'Dependency updates', 'api-server', 'weekly on Monday at 9am', 'Check for dependency updates and open a PR'),
  sched('SCHED-3', 'Morning triage', 'web-app', 'every weekday at 8:30am', 'Triage new bug reports into the board'),
  sched('SCHED-4', 'Uptime check', 'infra', 'every 15m', 'curl -fsS https://status.example.com/health', { kind: 'shell' }),
  sched('SCHED-5', 'Docs link check', 'docs', 'daily at 6am', 'Find and fix broken links in the docs'),
];
const hist = [
  `${E}38;2;153;153;153m❯ ${E}39madd rate limiting to the upload endpoint: 60 requests a minute per API key`,
  ``, `${E}38;2;255;255;255m●${E}39m I'll add a token-bucket limiter keyed on the API key and wire it into the upload route.`,
  ``, `${E}38;2;255;255;255m●${E}39m ${E}1mUpdate${E}22m(src/middleware/rateLimit.ts)`,
  `  ⎿  Added 14 lines`,
  `      ${E}48;2;2;40;0m 12 +${E}49m${E}48;2;2;40;0m export function rateLimit(perMinute: number) {           ${E}49m`,
  `      ${E}48;2;2;40;0m 13 +${E}49m${E}48;2;2;40;0m   const buckets = new Map<string, Bucket>();          ${E}49m`,
  `      ${E}48;2;2;40;0m 14 +${E}49m${E}48;2;2;40;0m   return (req, res, next) => {                        ${E}49m`,
  `      ${E}48;2;2;40;0m 15 +${E}49m${E}48;2;2;40;0m     const b = take(buckets, req.apiKey, perMinute);   ${E}49m`,
  `      ${E}48;2;2;40;0m 16 +${E}49m${E}48;2;2;40;0m     if (!b.ok) return res.status(429)                 ${E}49m`,
  `      ${E}48;2;2;40;0m 17 +${E}49m${E}48;2;2;40;0m       .set('Retry-After', b.retryAfter).end();        ${E}49m`,
  `      ${E}48;2;2;40;0m 18 +${E}49m${E}48;2;2;40;0m     next();                                           ${E}49m`,
  `      ${E}48;2;2;40;0m 19 +${E}49m${E}48;2;2;40;0m   };                                                  ${E}49m`,
  `      ${E}48;2;2;40;0m 20 +${E}49m${E}48;2;2;40;0m }                                                    ${E}49m`,
  ``, `${E}38;2;255;255;255m●${E}39m ${E}1mBash${E}22m(npm test -- rateLimit)`,
  `  ⎿  ${E}32m✓${E}39m allows 60 requests in a minute`, `     ${E}32m✓${E}39m returns 429 with Retry-After on the 61st`,
  `     ${E}32m✓${E}39m keeps separate buckets per API key`, `     Tests: 3 passed, 3 total`,
  ``, `${E}38;2;255;255;255m●${E}39m The limiter is in and tested. Uploads over 60 a minute now get a 429 with Retry-After.`,
  `  Moving ACME-142 to review.`, ``,
  `${E}38;2;215;119;87m✻${E}39m Wrapping up… ${E}2m(42s · ↓ 3.1k tokens)${E}22m`,
].join('\n');
const peek = { name: 'api-server', history: hist, output: hist, live: '', history_lines: 30, output_lines: 30,
  output_is_viewport_only: false, pane_cols: 90, pane_rows: 40, hint: '' };
const statuses = [['backlog','Backlog'],['todo','To Do'],['doing','In Progress'],['review','In Review'],['done','Done'],['verified','Verified']]
  .map(([id,label]) => ({ id, label, gate: [], mode: 'implicit', terminal: id === 'verified' }));
const unknown = new Set();
const reply = (r, body) => r.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) });
const b = await chromium.launch();
const dims = device === 'ipad'
  ? { viewport: { width: 1032, height: 1376 }, deviceScaleFactor: 2,
      userAgent: 'Mozilla/5.0 (iPad; CPU OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1' }
  : { viewport: { width: 440, height: 956 }, deviceScaleFactor: 3,
      userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1' };
const ctx = await b.newContext({ ...dims, isMobile: true, hasTouch: true, ignoreHTTPSErrors: true });
await ctx.route('**/api/**', (r) => {
  const u = new URL(r.request().url()), p = u.pathname;
  if (r.request().method() !== 'GET') return reply(r, { ok: true });
  if (p === '/api/sessions') return reply(r, fleet);
  if (p === '/api/board') return reply(r, board);
  if (p === '/api/board/statuses') return reply(r, statuses);
  if (p === '/api/cal-events') return reply(r, events);
  if (p === '/api/schedules') return reply(r, schedules);
  if (p === '/api/schedules/runs') return reply(r, []);
  if (p === '/api/sync') return reply(r, { rev: 1, events: [], full_sync_required: false, more: false });
  if (p === '/api/system-jobs') return reply(r, { count: 0, jobs: [], note: '', now, stall_rule: '', unhealthy: 0 });
  if (p === '/api/events') return r.fulfill({ status: 200, contentType: 'text/event-stream',
    headers: { 'cache-control': 'no-cache' }, body: 'data: {"type":"ping"}\n\n' });
  if (p === '/api/identity') return reply(r, { access_scope: null, email: '', has_api_key: true, has_oauth: true,
    is_cloud: false, is_local_member: false, key_error: '', key_valid: null, managed_upstream: false, team: null });
  if (/^\/api\/sessions\/[^/]+\/peek$/.test(p)) return reply(r, peek);
  if (p === '/api/prefs' || p === '/api/branding' || p === '/api/board/session-gates' || p.startsWith('/api/config/')) return reply(r, {});
  unknown.add(p);
  return reply(r, p.endsWith('s') || p.includes('history') ? [] : {});
});
await ctx.route('**/proxy/**', (r) => r.abort());
const p = await ctx.newPage();
await p.addInitScript(() => { try { localStorage.clear(); } catch (e) {} });
await p.goto(base + '/', { waitUntil: 'load' });
await p.addStyleTag({ content: '#chrome-tabs-bar,.toast,#toast,.toast-container,#toast-container{display:none!important}' });
await p.waitForTimeout(3500);
const shots = [['01_workers', 'sessions'], ['02_terminal', null], ['03_board', 'board'], ['04_calendar', 'calendar'],
  ['05_scheduler', 'scheduler'], ['06_groups', 'groups']];
fs.mkdirSync(out, { recursive: true });
for (const [file, v] of shots) {
  await p.evaluate((v) => { try { closePeek(); } catch (e) {} ; if (v) switchView(v); else { switchView('sessions'); openPeek('api-server'); } }, v);
  await p.waitForTimeout(3000);
  if (v === 'calendar') await p.waitForSelector('.fc-daygrid-day, .fc-timegrid-slot', { timeout: 20000 }).then(() => p.waitForTimeout(1500)).catch(() => console.log('calendar grid did not render'));
  await p.addStyleTag({ content: '#chrome-tabs-bar,.toast,#toast,.toast-container,#toast-container{display:none!important}' });
  await p.evaluate(() => { try { _liveSSE = true; updateConnectionStatus(); } catch (e) {}
    document.querySelectorAll('#conn-status').forEach(el => { el.className = 'conn-status online'; el.textContent = 'Live'; }); });
  await p.screenshot({ path: `${out}/${file}.png` });
}
console.log('unknown endpoints answered empty:', [...unknown].sort().join(' '));
await b.close();
