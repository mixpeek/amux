// MDAI pages, end to end (Ethan, 2026-10-14: "re-create the mdai pages (list,
// details, create, edit) ... the ability to create this mdai and see it via
// browser", and "it also needs to be viewable/editable entirely in the ui").
//
// Everything below goes through the dashboard's own controls. The server is
// real: the doc API writes real .mdai files, runs are real detached runs with
// the real single-flight registry, history rows are real. ONE thing is stubbed,
// at the server's provider boundary and disclosed here: the model. The e2e home
// carries a non-secret API key, so the server's ApiModel answers each node, and
// the `mdai_api_base` pref points it at a loopback stand-in for the Messages API
// started by this spec. That keeps CI deterministic and free; the same journey
// was run once against a real model locally (see the PR).
import { createServer, type IncomingMessage, type Server } from 'node:http';
import { mkdtemp, readFile, rm, writeFile, mkdir, access } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { Page } from '@playwright/test';
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

type Mode = { delayMs: number; fail?: string };

async function fakeMessagesApi() {
  const mode: Mode = { delayMs: 0 };
  const prompts: string[] = [];
  // `mdai_api_base` is a server-wide pref, and other model helpers share the
  // server's ApiModel: a skills spec running beside this one on the same
  // project server saved a skill, and its example-inference calls landed here
  // and were counted as node runs ("ran it once" read 5 in run 38947919221,
  // "one model call" read 2 in run 38945632973; the server logged
  // skill_examples_unavailable with this stand-in's 181-byte answer). Only a
  // prompt carrying this spec's instruction is a node call. Anything else is
  // recorded apart and refused the way the real provider refuses the e2e
  // home's non-secret key.
  const foreign: string[] = [];
  const server: Server = createServer(async (req: IncomingMessage, res) => {
    let body = '';
    for await (const chunk of req) body += chunk;
    if (req.method !== 'POST' || !req.url?.endsWith('/v1/messages')) { res.writeHead(404).end(); return; }
    const prompt = JSON.parse(body).messages?.[0]?.content || '';
    if (!String(prompt).includes(NODE_MARK)) {
      foreign.push(String(prompt).slice(0, 200));
      res.writeHead(401, { 'content-type': 'application/json' }).end(JSON.stringify({ type: 'error', error: { type: 'authentication_error', message: 'invalid x-api-key (stand-in: not an MDAI node prompt)' } }));
      return;
    }
    prompts.push(prompt);
    const { delayMs, fail } = mode;
    setTimeout(() => {
      if (res.destroyed) return;
      if (fail) {
        res.writeHead(529, { 'content-type': 'application/json' }).end(JSON.stringify({ type: 'error', error: { type: 'overloaded_error', message: fail } }));
        return;
      }
      const text = '# Where I Left Off\n\n| Workstream | Last state | Next action |\n|---|---|---|\n| MDAI pages | rework in review | run the e2e |\n\n## Workstream registry\n\n- **MDAI pages**: ' + prompts.length + ' run(s) seen.';
      res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify({
        content: [{ type: 'text', text }], usage: { input_tokens: 1234, output_tokens: 567 },
      }));
    }, delayMs);
  });
  await new Promise<void>(r => server.listen(0, '127.0.0.1', () => r()));
  const port = (server.address() as { port: number }).port;
  return { url: `http://127.0.0.1:${port}`, mode, prompts, foreign, close: () => new Promise<void>(r => { server.closeAllConnections(); server.close(() => r()); }) };
}

// Every node prompt carries the instruction (the run assertions below check
// this phrase in it), so it marks a call as this spec's.
const NODE_MARK = '"Where I Left Off" dashboard';
const PROMPT = '# Amux Workstream Reconstruction & Resumption\n\nAnalyze my complete Amux message history and reconstruct all the workstreams.\n\n## Final instruction\n\nStart with a **"Where I Left Off" dashboard**.\n';

// The MDAI list is a directory view like Files: open the node's folder, then
// open the node the way Files does (tap on touch, double-click with a mouse).
async function openNodeFromList(page: Page, folder = 'team') {
  await page.locator('#mdai-fe-body .fe-row.fe-dir', { hasText: folder }).click();
  await expect(page.locator('#mdai-breadcrumb')).toContainText(folder);
  const row = page.locator('#mdai-fe-body .fe-row:not(.fe-dir)');
  await expect(row).toHaveCount(1);
  if (await page.evaluate(() => (window as any)._feTouchDevice())) await row.click(); else await row.dblclick();
  await expect(page.locator('#mdai-overlay')).toHaveClass(/active/);
}
const tab = (page: Page, t: string) => page.locator(`#mdai-overlay .mdai-tab[data-tab=${t}]`).click();
// An unchanged node is served from its cache with no model call, which is the
// engine working. A run that must reach the model changes the instruction
// first; Run saves the unsaved edit before it starts.
let edits = 0;
async function editInstruction(page: Page) {
  await tab(page, 'edit');
  await page.locator('#mdai-instr-ta').fill(PROMPT + '\nKeep it short.\nEdit ' + (++edits) + '.\n');
}
const fileText = (p: string) => readFile(p, 'utf8');

test('MDAI: create and configure a node entirely in the UI, every field round-trips, runs are single-flight, honest and recoverable', async ({ page, request }, info) => {
  test.setTimeout(300_000);
  const narrow = page.viewportSize()!.width <= 600;
  const api = await fakeMessagesApi();
  const dir = await mkdtemp(join(tmpdir(), 'amux-mdai-pages-'));
  await mkdir(join(dir, 'team'), { recursive: true });
  await writeFile(join(dir, 'team', 'notes.md'), 'Ship the MDAI pages.\n');
  page.on('dialog', d => d.accept());
  await page.goto('/');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as any)._AMUX_AUTH_TOKEN }));
  const pref = (key: string, value: string) => request.post('/api/prefs', { headers, data: { key, value } });
  const prior = await Promise.all(['mdai_root', 'mdai_api_base'].map(async k => (await (await request.get('/api/prefs?key=' + k, { headers })).json()).value || ''));
  try {
    expect((await pref('mdai_root', dir)).ok()).toBeTruthy();
    expect((await pref('mdai_api_base', api.url)).ok()).toBeTruthy();
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).openMdaiNode === 'function');
    await page.evaluate(() => (window as any).switchView('mdai'));

    // ---- List: empty state, then the create page --------------------------
    await expect(page.locator('#mdai-list')).toContainText('No MDAI nodes yet');
    await page.locator('#mdai-new-btn').click();
    await expect(page.locator('#mdai-create-page')).toBeVisible();
    // An empty instruction is refused before anything is written.
    await page.locator('#mdai-new-name').fill('Workstreams');
    await page.locator('#mdai-create-page .mdai-create-go:not(.primary)').click();
    await expect(page.locator('#mdai-new-error')).toContainText('Write the instruction');

    // ---- Create: every field through a control ------------------------------
    await page.locator('#mdai-new-folder').fill('team');
    await page.locator('#mdai-new-title').fill('Where I left off');
    await page.getByRole('button', { name: '+ Amux message history' }).click();
    await page.locator('#new-days-0').selectOption('all');
    await page.locator('#new-limit-0').fill('300');
    await page.locator('#new-offset-0').fill('10');
    await page.locator('#new-kb-0').fill('200');
    await page.locator('#new-prompt-0').fill('Owner messages, oldest first.');
    await page.getByRole('button', { name: '+ File or folder' }).click();
    await page.locator('#new-path-1').fill('notes.md');
    await page.locator('#mdai-new-model').selectOption('__custom');
    await page.locator('#mdai-new-model-custom').fill('claude-sonnet-4-6');
    await page.locator('#mdai-new-timeout').fill('600');
    await page.locator('#mdai-new-body').fill(PROMPT);
    await page.locator('#mdai-create-page .mdai-seg button[data-mode=preview]').click();
    await expect(page.locator('#mdai-new-preview')).toContainText('Where I Left Off');
    if (narrow) {
      for (const sel of ['#mdai-create-page .mdai-seg button', '#mdai-create-page .mdai-create-go', '#new-sources .mdai-add-row .btn']) {
        for (const b of await page.locator(sel).all()) expect((await b.boundingBox())!.height, sel).toBeGreaterThanOrEqual(44);
      }
    }
    expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(1);
    await page.screenshot({ path: info.outputPath('mdai-create.png'), fullPage: true });
    await page.locator('#mdai-create-page .mdai-create-go:not(.primary)').click();
    await expect(page.locator('#mdai-overlay')).toHaveClass(/active/);
    await expect(page.locator('#mdai-title')).toHaveText('Workstreams.mdai');

    // The file on disk holds exactly what the form said.
    const file = join(dir, 'team', 'Workstreams.mdai');
    let text = await fileText(file);
    for (const want of ['title: Where I left off', 'model: claude-sonnet-4-6', 'refresh: manual', 'timeout: 600',
      'amux:messages?days=all&limit=300&offset=10&kb=200', 'Owner messages, oldest first.', 'notes.md', '"Where I Left Off" dashboard']) {
      expect(text, want).toContain(want);
    }
    expect(api.prompts.length, 'creating does not run the node').toBe(0);

    // ---- Reload: every field reads back into its control ---------------------
    await page.reload();
    await page.evaluate(() => (window as any).switchView('mdai'));
    // The list shows the folder, and inside it the node file, with the same
    // columns as Files.
    await expect(page.locator('#mdai-fe-body .fe-row.fe-dir', { hasText: 'team' })).toHaveCount(1);
    await expect(page.locator('#mdai-view .fe-col-headers')).toContainText('Modified');
    await openNodeFromList(page);
    await expect(page.locator('#mdai-title')).toHaveText('Workstreams.mdai');
    await tab(page, 'settings');
    await expect(page.locator('#mdai-set-title')).toHaveValue('Where I left off');
    await expect(page.locator('#mdai-set-model')).toHaveValue('__custom');
    await expect(page.locator('#mdai-set-model-custom')).toHaveValue('claude-sonnet-4-6');
    await expect(page.locator('#mdai-set-timeout')).toHaveValue('600');
    await expect(page.locator('#mdai-set-refresh')).not.toBeChecked();
    await tab(page, 'sources');
    await expect(page.locator('#src-days-0')).toHaveValue('all');
    await expect(page.locator('#src-limit-0')).toHaveValue('300');
    await expect(page.locator('#src-offset-0')).toHaveValue('10');
    await expect(page.locator('#src-kb-0')).toHaveValue('200');
    await expect(page.locator('#src-prompt-0')).toHaveValue('Owner messages, oldest first.');
    await expect(page.locator('#src-path-1')).toHaveValue('notes.md');
    await tab(page, 'edit');
    await expect(page.locator('#mdai-instr-ta')).toHaveValue(PROMPT);

    // ---- Edit: settings, sources, the API call and the instruction ---------------
    await tab(page, 'settings');
    await page.locator('#mdai-set-title').fill('Resume here');
    await page.locator('#mdai-set-model').selectOption('haiku');
    await page.locator('#mdai-set-timeout').fill('5');
    await page.locator('#mdai-set-save').click();
    await expect(page.locator('#mdai-body .mdai-field-err')).toContainText('between 30 and 3600');
    await page.locator('#mdai-set-timeout').fill('120');
    await page.locator('#mdai-set-refresh').check();
    await page.locator('#mdai-set-save').click();
    await expect.poll(() => fileText(file)).toContain('title: Resume here');
    text = await fileText(file);
    expect(text).toContain('model: haiku');
    expect(text).toContain('timeout: 120');
    expect(text).toContain('refresh: open');
    await tab(page, 'sources');
    await page.locator('#src-limit-0').fill('150');
    await page.locator('#src-days-0').selectOption('30');
    await page.locator('.mdai-src[data-kind=path] button', { hasText: 'Remove' }).click();
    await page.locator('#mdai-body details.mdai-fetch summary').click();
    await page.locator('#mdai-fetch-url').fill(api.url + '/data');
    await page.locator('#mdai-fetch-headers').fill('Accept: application/json');
    await page.locator('#mdai-fetch-select').fill('data.items');
    await page.locator('#mdai-src-save').click();
    await expect.poll(() => fileText(file)).toContain('select: data.items');
    text = await fileText(file);
    expect(text).toContain('amux:messages?days=30&limit=150&offset=10&kb=200');
    expect(text).not.toContain('notes.md');
    expect(text).toContain('Accept: application/json');
    // Clearing the URL removes the API call again. The edit starts once the
    // save has finished rendering; a render that lands while the field is
    // being edited must not put the saved URL back.
    await expect(page.locator('#mdai-fetch-url')).toHaveValue(api.url + '/data');
    await page.locator('#mdai-fetch-url').fill('');
    await expect(page.locator('#mdai-fetch-url')).toHaveValue('');
    // Any render (graph reload, run poll) can land here; force one so the
    // race is checked every run instead of when the timing happens to hit.
    await page.evaluate(() => (window as any)._mdaiRender());
    await expect(page.locator('#mdai-fetch-url')).toHaveValue('');
    await page.locator('#mdai-src-save').click();
    await expect.poll(() => fileText(file)).not.toContain('fetch:');
    await tab(page, 'edit');
    await page.locator('#mdai-instr-ta').fill(PROMPT + '\nKeep it short.\n');
    await expect(page.locator('#mdai-save-btn')).toHaveText('Save');
    await page.locator('#mdai-save-btn').click();
    await expect.poll(() => fileText(file)).toContain('Keep it short.');
    // Raw file mode edits the whole text; the server refuses a file that does not parse.
    await page.locator('#mdai-body .mdai-seg button[data-mode=raw]').click();
    await expect(page.locator('#mdai-body-ta')).toHaveValue(await fileText(file));
    await page.reload();
    await page.evaluate(() => (window as any).switchView('mdai'));
    await openNodeFromList(page);
    // refresh: open now runs on open; let that first run land.
    await expect.poll(() => api.prompts.length, { timeout: 15_000 }).toBe(1);
    await expect(page.locator('#mdai-runbanner.mdai-runbanner')).toHaveCount(0, { timeout: 20_000 });
    await tab(page, 'settings');
    await expect(page.locator('#mdai-set-title')).toHaveValue('Resume here');
    await expect(page.locator('#mdai-set-model')).toHaveValue('haiku');
    await page.locator('#mdai-set-refresh').uncheck();
    await page.locator('#mdai-set-save').click();
    await expect.poll(() => fileText(file)).toContain('refresh: manual');
    const afterOpen = api.prompts.length;
    expect(afterOpen, 'opening a refresh:open node ran it once').toBe(1);

    // ---- Run: double-click starts ONE run; the running state is the server's ---------
    api.mode.delayMs = 2500;
    await editInstruction(page);
    await page.locator('#mdai-run-btn').dblclick();
    await expect(page.locator('#mdai-runbanner.mdai-runbanner')).toBeVisible();
    await expect(page.locator('#mdai-run-btn')).toBeDisabled();
    // A second client asking for the same node joins the run in flight.
    await expect.poll(async () => (await (await request.get('/api/files/mdai/active?path=' + encodeURIComponent(file), { headers })).json()).running).toBe(true);
    const second = await request.post('/api/files/mdai/run', { headers, data: { path: file, wait: false } });
    expect(second.status()).toBe(202);
    expect((await second.json()).joined).toBe(true);
    await tab(page, 'runs');
    await expect(page.locator('#mdai-body .mdai-run-live')).toContainText('Running');
    await expect(page.locator('#mdai-runbanner.mdai-runbanner')).toHaveCount(0, { timeout: 20_000 });
    expect(api.prompts.length - afterOpen, 'double-click plus a second client = one model call').toBe(1);
    const row = page.locator('#mdai-body .mdai-run:not(.mdai-run-h)').first();
    await expect(row).toContainText('Completed');
    await expect(row).toContainText('haiku');
    await expect(row.locator('[data-l=In]')).toHaveText('1.2K');
    await expect(row.locator('[data-l=Out]')).toHaveText('567');
    await row.click();
    await expect(page.locator('#mdai-body .mdai-output')).toContainText('Where I Left Off');
    expect(api.prompts.at(-1)).toContain('"Where I Left Off" dashboard');
    expect(api.prompts.at(-1)).toContain('[source: amux:messages?days=30&limit=150&offset=10&kb=200]');
    await page.screenshot({ path: info.outputPath('mdai-preview.png') });

    // ---- Reload mid-run: the run is found again, then finishes ------------------
    // Long enough to outlast a reload plus reopening the node on a slow iOS
    // runner (4 s did not, 2026-10-16 run on 941fd8044); completion is awaited
    // with a 20 s budget below.
    api.mode.delayMs = 9000;
    await editInstruction(page);
    await page.locator('#mdai-run-btn').click();
    await expect(page.locator('#mdai-runbanner.mdai-runbanner')).toBeVisible();
    await page.reload();
    await page.evaluate(() => (window as any).switchView('mdai'));
    await openNodeFromList(page);
    await expect(page.locator('#mdai-runbanner.mdai-runbanner')).toBeVisible();
    await expect(page.locator('#mdai-runbanner.mdai-runbanner')).toHaveCount(0, { timeout: 20_000 });
    await tab(page, 'runs');
    await expect(page.locator('#mdai-body .mdai-run:not(.mdai-run-h)').first()).toContainText('Completed');

    // ---- Stop: the call is abandoned and the run says Cancelled ------------------
    api.mode.delayMs = 30_000;
    await editInstruction(page);
    await page.locator('#mdai-run-btn').click();
    await expect(page.locator('#mdai-stop-btn')).toBeVisible();
    await page.locator('#mdai-stop-btn').click();
    await expect(page.locator('#mdai-runbanner.mdai-runbanner')).toHaveCount(0, { timeout: 10_000 });
    await tab(page, 'runs');
    await expect(page.locator('#mdai-body .mdai-run:not(.mdai-run-h)').first()).toContainText('Cancelled');

    // ---- A failing provider: the run says why -----------------------------------
    api.mode.delayMs = 0;
    api.mode.fail = 'Overloaded (stand-in)';
    await editInstruction(page);
    await page.locator('#mdai-run-btn').click();
    await expect(page.locator('#mdai-body .mdai-err')).toHaveCount(1, { timeout: 15_000 });
    await expect(page.locator('#mdai-body .mdai-err')).toContainText('Overloaded (stand-in)', { timeout: 15_000 });
    await tab(page, 'runs');
    await expect(page.locator('#mdai-body .mdai-run:not(.mdai-run-h)').first()).toContainText('Failed');
    api.mode.fail = undefined;
    if (narrow) {
      expect((await page.locator('#mdai-run-btn').boundingBox())!.height).toBeGreaterThanOrEqual(44);
      expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(1);
    }
    await page.screenshot({ path: info.outputPath('mdai-runs.png') });

    // ---- Rename keeps the history; delete removes the node ------------------------
    await tab(page, 'settings');
    await page.locator('#mdai-set-name').fill('Resume');
    await page.locator('#mdai-set-save').click();
    await expect(page.locator('#mdai-title')).toHaveText('Resume.mdai');
    await tab(page, 'runs');
    await expect(page.locator('#mdai-overlay .mdai-tab[data-tab=runs] .mdai-tab-n')).toHaveText('5');
    await tab(page, 'settings');
    await page.getByRole('button', { name: 'Delete node' }).click();
    await expect(page.locator('#mdai-overlay')).not.toHaveClass(/active/);
    await expect(page.locator('#mdai-list')).toContainText('No MDAI nodes yet');
    await expect(access(join(dir, 'team', 'Resume.mdai'))).rejects.toThrow();
  } finally {
    if (api.foreign.length) {
      console.log(`[mdai-pages] ${api.foreign.length} non-node model call(s) reached the stand-in and were not counted: ${JSON.stringify(api.foreign.map(p => p.slice(0, 80)))}`);
      info.annotations.push({ type: 'foreign-model-calls', description: String(api.foreign.length) });
    }
    await cleanup('restore MDAI prefs', () => Promise.all([pref('mdai_root', prior[0]), pref('mdai_api_base', prior[1])]));
    await cleanup('stop the stand-in Messages API', () => api.close());
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }));
  }
});

// The MDAI tab lists its folder through the Files tab's file view, not a
// parallel table (Ethan, 2026-10-16: "this should be the same file directory /
// file view as any other file view"). The same folder is opened in both tabs
// and the markup is compared: breadcrumb, header cells, row structure, parent
// row and status bar come from the same builders, sorting is one state, and a
// .mdai row opens its node page.
// Same file as the lifecycle test above, so the two run one after the other in
// one worker: both point the server-wide mdai_root pref at their own folder.
// The class list of every cell in the first row matching `sel`: the shape
// _feBuildRow produces, without names, sizes or dates.
const rowShape = (page: Page, sel: string) => page.locator(sel).first().evaluate(row =>
  [row.className.split(' ').filter(c => c !== 'fe-selected').sort().join(' ')]
    .concat([...row.children].map(c => c.className)).join(' | '));

test('MDAI: the folder list is the Files file view (same breadcrumb, rows, headers, parent row, status)', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  const dir = await mkdtemp(join(tmpdir(), 'amux-mdai-fv-'));
  await mkdir(join(dir, 'team'), { recursive: true });
  await writeFile(join(dir, 'Top.mdai'), '---\ntitle: Top\n---\nSay hello.\n');
  await writeFile(join(dir, 'team', 'Inner.mdai'), '---\ntitle: Inner\n---\nSay hello.\n');
  await page.goto('/');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as any)._AMUX_AUTH_TOKEN }));
  const pref = (key: string, value: string) => request.post('/api/prefs', { headers, data: { key, value } });
  const prior = (await (await request.get('/api/prefs?key=mdai_root', { headers })).json()).value || '';
  try {
    expect((await pref('mdai_root', dir)).ok()).toBeTruthy();
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).openMdaiNode === 'function');
    await page.evaluate(() => (window as any).switchView('mdai'));

    // Breadcrumb: the Files breadcrumb, the whole path, root segment first.
    const crumb = page.locator('#mdai-breadcrumb');
    await expect(crumb).toHaveClass(/fe-breadcrumb/);
    await expect(crumb.locator('.fe-crumb').first()).toHaveText('/');
    await expect(crumb.locator('.fe-crumb').last()).toHaveText(dir.split('/').pop()!);
    await expect(page.locator('#mdai-view .mdai-rootline'), 'the separate "Folder <path>" line is gone').toHaveCount(0);

    // Rows: the folder and the node, built by the shared row builder.
    const mdaiDir = '#mdai-fe-body .fe-row.fe-dir';
    await expect(page.locator(mdaiDir, { hasText: 'team' })).toHaveCount(1);
    await expect(page.locator('#mdai-fe-body .fe-row:not(.fe-dir)', { hasText: 'Top.mdai' })).toHaveCount(1);
    await expect(page.locator('#mdai-fe-status')).toHaveText('1 folder, 1 file');
    const mdaiShape = await rowShape(page, mdaiDir);
    const mdaiFileShape = await rowShape(page, '#mdai-fe-body .fe-row:not(.fe-dir)');
    const mdaiHeaders = await page.locator('#mdai-view .fe-col-headers > div').evaluateAll(els => els.map(e => e.className.replace(/\s*sorted/, '') + ':' + (e as HTMLElement).dataset.sortCol));

    // Its own sort, most recently opened first by default (Ethan, 2026-10-16);
    // a header click here sorts this list and leaves the Files tab's sort alone.
    await expect(page.locator('#mdai-view [data-sort-col=opened]')).toHaveClass(/sorted/);
    await page.locator('#mdai-view [data-sort-col=name]').click();
    await expect(page.locator('#mdai-view [data-sort-col=name]')).toHaveClass(/sorted/);
    await expect(page.locator('#fe-hdr-name')).not.toHaveClass(/sorted/);

    // Into the subfolder and back through the shared parent row.
    await page.locator(mdaiDir, { hasText: 'team' }).click();
    await expect(crumb.locator('.fe-crumb').last()).toHaveText('team');
    await expect(page.locator('#mdai-fe-body .fe-back-row svg')).toHaveCount(1);
    await expect(page.locator('#mdai-fe-body .fe-row', { hasText: 'Inner.mdai' })).toHaveCount(1);
    await page.locator('#mdai-fe-body .fe-back-row').click();
    await expect(page.locator(mdaiDir, { hasText: 'team' })).toHaveCount(1);
    await page.screenshot({ path: info.outputPath('mdai-file-view.png') });

    // The same folder in the Files tab: identical row and header structure.
    await page.evaluate(d => { (window as any).switchView('files'); (window as any).loadFiles(d); }, dir);
    const filesDir = '#files-body .fe-row.fe-dir';
    await expect(page.locator(filesDir, { hasText: 'team' })).toHaveCount(1);
    expect(await rowShape(page, filesDir), 'folder row').toBe(mdaiShape);
    expect(await rowShape(page, '#files-body .fe-row:not(.fe-dir)'), 'file row').toBe(mdaiFileShape);
    const filesHeaders = await page.locator('#fe-col-headers > div').evaluateAll(els => els.map(e => e.className.replace(/\s*sorted/, '') + ':' + (e as HTMLElement).dataset.sortCol));
    expect(filesHeaders, 'header cells').toEqual(mdaiHeaders);
    await page.screenshot({ path: info.outputPath('files-same-folder.png') });

    // A .mdai row opens its node page, the way Files opens it.
    await page.evaluate(() => (window as any).switchView('mdai'));
    const node = page.locator('#mdai-fe-body .fe-row:not(.fe-dir)', { hasText: 'Top.mdai' });
    if (await page.evaluate(() => (window as any)._feTouchDevice())) await node.click(); else await node.dblclick();
    await expect(page.locator('#mdai-overlay')).toHaveClass(/active/);
    await expect(page.locator('#mdai-title')).toHaveText('Top.mdai');
  } finally {
    await cleanup('restore the MDAI folder pref', () => pref('mdai_root', prior), info);
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});

// "Opened" column (Ethan, 2026-10-16): every file table shows when a file was
// last opened, the MDAI list sorts by it by default, and Escape goes back from
// an MDAI page the way it does from any file.
test('MDAI: last opened sorts the list, shows in Files, and Escape goes back', async ({ page, request }, info) => {
  test.setTimeout(120_000);
  const dir = await mkdtemp(join(tmpdir(), 'amux-mdai-opened-'));
  await writeFile(join(dir, 'Alpha.mdai'), '---\ntitle: Alpha\nrefresh: manual\n---\nSay hello.\n');
  await writeFile(join(dir, 'Beta.mdai'), '---\ntitle: Beta\nrefresh: manual\n---\nSay hello.\n');
  await page.goto('/');
  const headers = await page.evaluate(() => ({ Authorization: 'Bearer ' + (window as any)._AMUX_AUTH_TOKEN }));
  const pref = (key: string, value: string) => request.post('/api/prefs', { headers, data: { key, value } });
  const prior = (await (await request.get('/api/prefs?key=mdai_root', { headers })).json()).value || '';
  try {
    expect((await pref('mdai_root', dir)).ok()).toBeTruthy();
    // Beta was opened, Alpha never: Beta leads although Alpha sorts first by name.
    const rec = await request.post('/api/fs/opened', { headers, data: { path: join(dir, 'Beta.mdai') } });
    expect(rec.ok(), await rec.text()).toBeTruthy();
    await page.reload();
    await page.waitForFunction(() => typeof (window as any).openMdaiNode === 'function');
    await page.evaluate(() => (window as any).switchView('mdai'));
    const files = page.locator('#mdai-fe-body .fe-row:not(.fe-dir)');
    await expect(files).toHaveCount(2);
    await expect(files.nth(0)).toContainText('Beta.mdai');
    await expect(files.nth(0).locator('.fe-cell-opened')).not.toHaveText('');
    await expect(files.nth(1).locator('.fe-cell-opened')).toHaveText('');
    await expect(page.locator('#mdai-view [data-sort-col=opened]')).toHaveClass(/sorted/);

    // Opening Alpha records it; Escape closes the node page back to the list,
    // which now leads with Alpha.
    await page.evaluate(p => (window as any).openMdaiNode(p, { autorun: false }), join(dir, 'Alpha.mdai'));
    await expect(page.locator('#mdai-overlay')).toHaveClass(/active/);
    await page.keyboard.press('Escape');
    await expect(page.locator('#mdai-overlay')).not.toHaveClass(/active/);
    await expect(files.nth(0)).toContainText('Alpha.mdai');
    await expect(files.nth(0).locator('.fe-cell-opened')).not.toHaveText('');

    // Escape on the New MDAI page goes back to the list too.
    await page.evaluate(() => (window as any)._mdaiCreatePage());
    await expect(page.locator('#mdai-create-page')).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(page.locator('#mdai-create-page')).toBeHidden();
    await expect(page.locator('#mdai-list-page')).toBeVisible();

    // Files shows the same column and keeps its own default sort (Modified).
    await page.evaluate(d => { (window as any).switchView('files'); (window as any).loadFiles(d); }, dir);
    await expect(page.locator('#fe-hdr-opened')).toHaveText(/Opened/);
    await expect(page.locator('#fe-hdr-modified')).toHaveClass(/sorted/);
    const alpha = page.locator('#files-body .fe-row', { hasText: 'Alpha.mdai' });
    await expect(alpha.locator('.fe-cell-opened')).not.toHaveText('');
    // Header and row tracks agree, and nothing spills past the viewport sideways
    // except the table's own scroller.
    const fit = await page.evaluate(() => {
      const h = document.querySelector('#fe-col-headers') as HTMLElement, r = document.querySelector('#files-body .fe-row') as HTMLElement;
      return { cols: getComputedStyle(h).gridTemplateColumns === getComputedStyle(r).gridTemplateColumns,
               cells: r.children.length, headers: h.children.length,
               overflow: document.documentElement.scrollWidth - window.innerWidth };
    });
    expect(fit.cols, 'header and row columns match').toBeTruthy();
    expect(fit.cells).toBe(fit.headers);
    expect(fit.overflow, 'no page-level horizontal overflow').toBeLessThanOrEqual(1);
    await page.screenshot({ path: info.outputPath('files-opened-column.png') });
  } finally {
    await cleanup('restore the MDAI folder pref', () => pref('mdai_root', prior), info);
    await cleanup('remove temp folder', () => rm(dir, { recursive: true, force: true }), info);
  }
});
