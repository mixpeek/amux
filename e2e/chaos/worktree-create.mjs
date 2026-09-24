#!/usr/bin/env node
// END-TO-END: "Use worktree" in the Create Worker modal puts the worker in a
// git worktree of the directory the owner SELECTED, on the requested branch.
//
// Driven through the real dashboard in Chromium against a private amux, with
// a fake `claude` that records its own cwd at launch. Every claim is checked
// against an independent source:
//   - the agent process itself (its launch record's cwd)
//   - tmux (the pane's current path)
//   - git, run in the ORIGINAL repo (worktree list, common dir, branch)
//   - the amux API (what the dashboard will show)
// and a control asserts the original checkout was not switched or dirtied.
//
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/worktree-create.mjs
// Exit 0 only if every check passes. Prints a JSON verdict either way.
import fs from 'node:fs';
import path from 'node:path';
import { chromium } from 'playwright';
import { startAmux, git, waitFor } from './harness.mjs';

const checks = [];
const check = (name, ok, detail) => { checks.push({ name, ok: !!ok, detail }); };
const real = p => fs.realpathSync(p);

const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
let browser;
try {
  // ---- a real repo with an origin, and a local main AHEAD of origin/main ----
  const originDir = path.join(amux.root, 'origin.git');
  const repo = path.join(amux.root, 'project');
  git(amux.root, 'init', '-q', '--bare', '-b', 'main', originDir);
  git(amux.root, 'init', '-q', '-b', 'main', repo);
  fs.writeFileSync(path.join(repo, 'README.md'), 'hello\n');
  git(repo, 'add', '.'); git(repo, 'commit', '-q', '-m', 'first');
  git(repo, 'remote', 'add', 'origin', originDir);
  git(repo, 'push', '-q', 'origin', 'main'); git(repo, 'fetch', '-q', 'origin');
  const originMain = git(repo, 'rev-parse', 'origin/main');
  fs.writeFileSync(path.join(repo, 'local-only.txt'), 'not pushed\n');
  git(repo, 'add', '.'); git(repo, 'commit', '-q', '-m', 'local only');
  const localHead = git(repo, 'rev-parse', 'HEAD');
  const repoBranchBefore = git(repo, 'rev-parse', '--abbrev-ref', 'HEAD');

  const name = 'wt-proof';
  const branch = 'worker/wt-proof';

  // ---- the owner's journey, in the real UI ----
  browser = await chromium.launch();
  const page = await (await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } })).newPage();
  const consoleErrors = [];
  const requests = [];
  page.on('pageerror', e => consoleErrors.push(String(e)));
  page.on('request', r => { if (r.url().includes('/api/')) requests.push(r.method() + ' ' + new URL(r.url()).pathname); });
  await page.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => typeof openCreate === 'function');
  await page.evaluate(() => openCreate());
  await page.locator('#create-name').fill(name);
  await page.locator('#create-dir').fill(repo);
  // The worktree box appears only once the server confirms the dir is a git repo.
  await page.locator('#create-worktree-field').waitFor({ state: 'visible', timeout: 10000 });
  await page.locator('#create-worktree-enabled').check();
  if (!(await page.locator('#create-branch-enabled').isChecked())) await page.locator('#create-branch-enabled').check();
  await page.locator('#create-branch').fill(branch);
  await page.screenshot({ path: path.join(amux.root, '1-modal.png') });
  const created = page.waitForResponse(r => new URL(r.url()).pathname === '/api/sessions' && r.request().method() === 'POST').catch(e => e);
  const started = page.waitForResponse(r => r.url().includes(`/api/sessions/${name}/start`), { timeout: 60000 }).catch(e => e);
  await page.locator('#create-overlay button.btn.primary, button[onclick="submitCreate()"]').first().click();
  const createRes = await created;
  if (createRes instanceof Error) throw new Error('no create POST; page requests: ' + JSON.stringify(requests.slice(-15)));
  const createReq = createRes.request().postDataJSON();
  const startRes = await started;
  if (startRes instanceof Error) throw new Error('no start POST; page requests: ' + JSON.stringify(requests.slice(-15)));
  check('modal sent worktree:true with the selected dir', createReq.worktree === true && createReq.dir === repo, createReq);
  check('start succeeded', startRes.ok(), { status: startRes.status(), body: await startRes.text().catch(() => '') });

  // ---- 1. the agent itself says where it is running ----
  const launch = await waitFor('fake claude launch record', () => amux.fakeLog().find(e => e.event === 'launch'), 30000);
  const wt = path.join(amux.home, 'worktrees', name);
  check('agent process cwd is the worktree', fs.existsSync(wt) && real(launch.cwd) === real(wt), { agent_cwd: launch.cwd, expected: wt });
  check('agent process cwd is NOT the original checkout', real(launch.cwd) !== real(repo), { agent_cwd: launch.cwd, repo });

  // ---- 2. tmux agrees ----
  const panePath = amux.tmux('display-message', '-p', '-t', `amux-${name}`, '#{pane_current_path}').trim();
  check('tmux pane path is the worktree', real(panePath) === real(wt), { panePath });

  // ---- 3. git, asked in the ORIGINAL repo ----
  const list = git(repo, 'worktree', 'list', '--porcelain');
  const entry = list.split('\n\n').find(b => b.split('\n')[0] === 'worktree ' + real(wt) || b.split('\n')[0] === 'worktree ' + wt);
  check('the selected repo registers the worktree', !!entry, { list });
  check('worktree is on the requested branch', !!entry && entry.includes(`branch refs/heads/${branch}`), { entry });
  const common = path.resolve(wt, git(wt, 'rev-parse', '--git-common-dir'));
  check('worktree shares the selected repo\'s object store', real(common) === real(path.join(repo, '.git')), { common });
  const wtHead = git(wt, 'rev-parse', 'HEAD');
  check('worktree is cut from origin/main, not the stale local HEAD', wtHead === originMain && wtHead !== localHead, { wtHead, originMain, localHead });
  check('worktree branch exists in the selected repo', git(repo, 'rev-parse', `refs/heads/${branch}`) === wtHead);

  // ---- 4. the control: the original checkout was not touched ----
  check('original checkout still on its branch', git(repo, 'rev-parse', '--abbrev-ref', 'HEAD') === repoBranchBefore, { before: repoBranchBefore });
  check('original checkout HEAD unchanged', git(repo, 'rev-parse', 'HEAD') === localHead);
  check('original checkout clean', git(repo, 'status', '--porcelain') === '', { status: git(repo, 'status', '--porcelain') });

  // ---- 5. what the dashboard is told ----
  const sessions = (await amux.req('GET', '/api/sessions')).body;
  const s = (Array.isArray(sessions) ? sessions : sessions.sessions || []).find(x => x.name === name) || {};
  check('API reports the worker running', s.running === true, { running: s.running, status: s.status });
  check('API names the selected dir as the worker dir', s.dir && real(s.dir) === real(repo), { dir: s.dir });
  check('API marks the worktree active at the worktree path',
    s.worktree_active === true && s.worktree_path && real(s.worktree_path) === real(wt),
    { worktree_active: s.worktree_active, worktree_path: s.worktree_path });

  // ---- 6. a message typed in the UI reaches the agent in the worktree ----
  const text = 'hello from the worktree proof';
  await waitFor('composer painted', () => {
    const pane = amux.tmux('capture-pane', '-p', '-t', `amux-${name}`);
    return pane.includes('\u276f') && pane.includes('bypass permissions');
  }, 20000);
  const sent = await amux.req('POST', `/api/workers/${name}/send`, { text }, 60000);
  const got = await waitFor('message at agent', () => amux.fakeLog().find(e => e.text === text), 30000).catch(() => null);
  check('a send reaches the agent running in the worktree', got && got.pid === launch.pid, { status: sent.status, body: sent.body, got });

  await page.screenshot({ path: path.join(amux.root, '2-after.png') });
  check('no uncaught page errors', consoleErrors.length === 0, consoleErrors.slice(0, 5));
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close().catch(() => {});
  await amux.stop();
}
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
