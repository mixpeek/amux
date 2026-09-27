// Component acceptance: shipped module/CSS, fixture API, never production or Slack.
import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'node:http';
import { existsSync, readFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { resolve } from 'node:path';
import { chromium } from 'playwright';
import { expect } from '@playwright/test';

const root = new URL('../crates/amux-dashboard/static/', import.meta.url);
const shots = process.env.WR_SCREENSHOT_DIR || '/Users/ezis/work/outputs/amux-workdesk-unification';
let server, browser, origin;
const html = `<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/app.css"></head><body>
<div id="wr-toolbar"><span id="wr-connection"></span><button id="wr-filter">Filter</button><button id="wr-sync">Sync</button></div>
<div id="board-detail-overlay" class="overlay active"><div class="overlay-header">Native board fixture</div><div class="board-detail-body"><section id="wr-detail" class="wr-detail" hidden></section></div></div>
<script>window.API='';window.apiCall=(url,options)=>fetch(url,options);window._authUrl=url=>url;window.fetchBoard=async()=>{};window.renderBoard=()=>{};</script><script src="/work-requests.js"></script></body></html>`;

before(async () => {
  server = createServer((req, res) => {
    if (req.url === '/') { res.setHeader('Content-Type', 'text/html; charset=utf-8'); res.end(html); return; }
    if (['/app.css', '/work-requests.js'].includes(req.url)) {
      res.setHeader('Content-Type', req.url.endsWith('.js') ? 'text/javascript; charset=utf-8' : 'text/css; charset=utf-8');
      res.end(readFileSync(new URL(req.url.slice(1), root))); return;
    }
    res.writeHead(404); res.end();
  });
  await new Promise((yes, no) => { server.once('error', no); server.listen(0, '127.0.0.1', yes); });
  origin = `http://127.0.0.1:${server.address().port}`;
  const cachedMacBrowser = resolve(homedir(), 'Library/Caches/ms-playwright/chromium-1234/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing');
  const executablePath = process.env.WR_CHROMIUM_PATH || (existsSync(cachedMacBrowser) ? cachedMacBrowser : undefined);
  browser = await chromium.launch({headless: true, ...(executablePath ? {executablePath} : {})});
}, {timeout: 20000});
after(async () => {
  if (browser) await browser.close();
  if (server) await new Promise(yes => server.close(yes));
});

function detail(id = 'WR-1') {
  return {task_id: id, candidate_id: 17, title: 'Fixture task ' + id, status: 'review', source_fingerprint: 'source-v1',
    allowed_actions: ['generate', 'revise', 'approve', 'hold'], detail: {
      issue: {text: 'Summarize the provided request.', requester_name: 'Fixture Owner', channel_id: 'C1', permalink: 'https://team.slack.com/archives/C1/p100'},
      artifact: {id: 4, version: 2, status: 'ready_for_review', content: 'This is the complete reviewable artifact. It has a clear source and a proposed action.', content_hash: 'a'.repeat(64), recipient_channel: 'C1', recipient_thread_ts: '100.001'},
      evidence: [{text: 'Captured supporting source.', author_id: 'U1', permalink: 'javascript:alert(1)'}], attachments: []}};
}
async function harness(t, {value = detail(), action, read, viewport} = {}) {
  const context = await browser.newContext({viewport: viewport || {width: 1280, height: 900}});
  const page = await context.newPage();
  page.setDefaultTimeout(5000);
  const calls = [], errors = [];
  t.after(async () => {
    if (process.env.WR_DEBUG && !page.isClosed()) {
      mkdirSync(shots, {recursive: true});
      const name = t.name.replace(/[^a-z0-9]+/gi, '-');
      const dom = await page.evaluate(() => ({text: document.body.innerText,
        feedback: document.querySelector('#wr-feedback')?.textContent,
        buttons: [...document.querySelectorAll('#wr-detail button')].map(b => ({text: b.textContent, disabled: b.disabled}))})).catch(error => ({error: error.message}));
      writeFileSync(resolve(shots, `component-${name}.json`), JSON.stringify({dom, calls, errors}, null, 2));
      await page.screenshot({path: resolve(shots, `component-${name}.png`)}).catch(() => {});
    }
    await context.close();
  });
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/*', async route => {
    const url = new URL(route.request().url());
    if (url.origin !== origin) { await route.abort(); return; }
    if (!url.pathname.startsWith('/api/')) { await route.continue(); return; }
    if (url.pathname === '/api/work-requests/status') { await route.fulfill({json: {configured: true, enabled: true}}); return; }
    if (url.pathname.endsWith('/source/actions')) {
      const body = route.request().postDataJSON(); calls.push({url: url.pathname, body});
      if (action) { await action(route, body, calls.length); return; }
      await route.fulfill({json: {...value, operation: {state: 'completed', operation_id: body.operation_id}}}); return;
    }
    if (url.pathname.endsWith('/source')) {
      if (read) { await read(route, url); return; }
      await route.fulfill({json: value}); return;
    }
    await route.fulfill({status: 404, json: {error: 'unexpected_fixture_api'}});
  });
  await page.goto(origin);
  await page.waitForFunction(() => !!window.AmuxWorkRequests);
  return {page, calls, errors, open: id => page.evaluate(id => window.AmuxWorkRequests.open({id, source: 'workdesk'}), id || value.task_id)};
}

// No browser mocks: the fixture API supplies domain data while real DOM events drive the shipped code.
test('native detail renders source and artifacts safely without source HTML execution', {timeout: 15000}, async t => {
  const value = detail(); value.detail.issue.text = '<img src=x onerror="window.injected=true">';
  value.detail.artifact.content = '<script>window.injected=true</script>' + 'A'.repeat(90);
  const h = await harness(t, {value}); await h.open();
  await expect(h.page.locator('#wr-detail')).toContainText('<img src=x');
  await expect(h.page.locator('.wr-artifact')).toContainText('<script>');
  assert.equal(await h.page.locator('#wr-detail img, #wr-detail script').count(), 0);
  assert.equal(await h.page.locator('#wr-detail a[href^="javascript:"]').count(), 0);
  assert.equal(await h.page.evaluate(() => window.injected), undefined);
  assert.deepEqual(h.errors, []);
});

test('generate and revise submit frozen source and artifact hashes', {timeout: 15000}, async t => {
  const h = await harness(t); await h.open();
  await h.page.getByRole('button', {name: '초안 다시 작성', exact: true}).click();
  await expect(h.page.locator('#wr-feedback')).toContainText('반영');
  await h.page.getByRole('textbox', {name: '초안 수정 요청'}).fill('Keep the cited source, shorten the conclusion.');
  await h.page.getByRole('button', {name: '수정 요청', exact: true}).click();
  await expect.poll(() => h.calls.length).toBe(2);
  assert.equal(h.calls[0].body.kind, 'generate');
  assert.equal(h.calls[0].body.expected_source_fingerprint, 'source-v1');
  assert.equal(h.calls[1].body.kind, 'revise');
  assert.equal(h.calls[1].body.artifact_id, 4);
  assert.equal(h.calls[1].body.content_hash, 'a'.repeat(64));
  assert.equal(h.calls[1].body.instruction, 'Keep the cited source, shorten the conclusion.');
});

test('approval requires visible confirmation and never implies delivery', {timeout: 15000}, async t => {
  const h = await harness(t); await h.open();
  const button = h.page.getByRole('button', {name: '이 버전 승인', exact: true});
  await expect(button).toBeDisabled();
  await expect(h.page.locator('.wr-destination')).toContainText('C1');
  await expect(h.page.locator('.wr-destination')).toContainText('100.001');
  await h.page.getByRole('checkbox').check(); await button.click();
  await expect.poll(() => h.calls.length).toBe(1);
  assert.equal(h.calls[0].body.kind, 'approve');
  assert.equal(h.calls[0].body.recipient_channel, 'C1');
  assert.equal(h.calls[0].body.recipient_thread_ts, '100.001');
  assert.equal(h.calls[0].body.content_hash, 'a'.repeat(64));
  assert.equal(h.calls.some(call => call.body.kind === 'deliver'), false);
});

test('delivery is a separate explicit action for the approved version', {timeout: 15000}, async t => {
  const value = detail(); value.detail.artifact.status = 'approved'; value.detail.approval = {state: 'approved'}; value.allowed_actions = ['deliver'];
  const h = await harness(t, {value}); await h.open();
  const button = h.page.getByRole('button', {name: 'Slack으로 전송', exact: true});
  await expect(button).toBeDisabled(); assert.equal(h.calls.length, 0);
  await h.page.getByRole('checkbox').check(); await button.click();
  await expect.poll(() => h.calls.length).toBe(1);
  assert.equal(h.calls[0].body.kind, 'deliver'); assert.equal(h.calls[0].body.artifact_id, 4);
  assert.equal(h.calls[0].body.content_hash, 'a'.repeat(64));
});

test('held request overrides stale advertised artifact actions and only offers restore', {timeout: 15000}, async t => {
  const value = detail(); value.status = 'held'; value.allowed_actions = ['restore', 'generate', 'revise', 'approve', 'deliver'];
  const h = await harness(t, {value, action: async (route, body) => {
    await route.fulfill({json: {...detail(), operation: {state: 'completed', operation_id: body.operation_id}}});
  }});
  await h.open();
  await expect(h.page.locator('.wr-state')).toHaveText('보류');
  await expect(h.page.locator('.wr-artifact')).toContainText('This is the complete reviewable artifact.');
  await expect(h.page.locator('.wr-actions button')).toHaveText(['다시 진행']);
  assert.equal(await h.page.getByRole('button', {name: '이 버전 승인', exact: true}).count(), 0);
  assert.equal(await h.page.getByRole('button', {name: '수정 요청', exact: true}).count(), 0);
  await h.page.getByRole('button', {name: '다시 진행', exact: true}).click();
  await expect(h.page.locator('.wr-state')).toHaveText('검토 필요');
  assert.equal(h.calls.length, 1);
  assert.equal(h.calls[0].body.kind, 'restore');
  assert.equal(h.calls[0].body.expected_source_fingerprint, 'source-v1');
});

test('stale source conflict is visible and refresh requires fresh confirmation', {timeout: 15000}, async t => {
  let value = detail();
  const h = await harness(t, {read: route => route.fulfill({json: value}), action: async route => {
    value = detail(); value.source_fingerprint = 'source-v2'; value.detail.artifact.status = 'stale'; value.allowed_actions = ['generate'];
    await route.fulfill({status: 409, json: {error: 'source_changed'}});
  }});
  await h.open(); await h.page.getByRole('checkbox').check();
  await h.page.getByRole('button', {name: '이 버전 승인', exact: true}).click();
  await expect(h.page.locator('#wr-feedback')).toContainText('source_changed');
  await h.page.getByRole('button', {name: '새로고침', exact: true}).click();
  await expect(h.page.locator('.wr-state')).toContainText('원문 변경');
  assert.equal(await h.page.getByRole('button', {name: '이 버전 승인', exact: true}).count(), 0);
  assert.equal(h.calls.length, 1);
});

for (const [state, label] of [['pending', '대기'], ['unknown', '결과 확인 필요'], ['failed', '실패']]) {
  test(`reopened card shows top-level ${state} operation without submitting another action`, {timeout: 15000}, async t => {
    const value = detail();
    value.operation = {operation_id: 'saved-operation-1', kind: 'revise', state, error: state === 'failed' ? 'source_changed' : null};
    value.allowed_actions = ['hold'];
    const h = await harness(t, {value}); await h.open();
    await expect(h.page.locator('#wr-feedback')).toContainText('요청 상태: ' + label);
    await expect(h.page.getByRole('button', {name: '보류', exact: true})).toBeEnabled();
    assert.equal(await h.page.getByRole('button', {name: '초안 다시 작성', exact: true}).count(), 0);
    await h.page.getByRole('button', {name: '새로고침', exact: true}).click();
    await expect(h.page.locator('#wr-feedback')).toContainText('요청 상태: ' + label);
    assert.equal(h.calls.length, 0);
  });
}

for (const state of ['pending', 'unknown', 'response-lost']) {
  test(`retry preserves operation identity after ${state}`, {timeout: 15000}, async t => {
    const value = detail();
    const h = await harness(t, {value, action: async (route, body, count) => {
      if (state === 'response-lost' && count === 1) { await route.abort('failed'); return; }
      await route.fulfill({json: {...value, operation: {state: count === 1 ? state : 'completed', operation_id: body.operation_id}}});
    }});
    await h.open(); const button = h.page.getByRole('button', {name: '초안 다시 작성', exact: true});
    await button.click(); await expect.poll(() => h.calls.length).toBe(1);
    if (state !== 'response-lost') {
      await expect(button).toHaveCount(0);
      await expect(h.page.locator('#wr-feedback')).not.toContainText('요청이 반영됐습니다.');
      await h.page.getByRole('button', {name: '새로고침', exact: true}).click();
    }
    await expect(button).toBeEnabled();
    await expect(h.page.locator('#wr-feedback')).not.toContainText('요청이 반영됐습니다.');
    await button.click(); await expect.poll(() => h.calls.length).toBe(2);
    assert.equal(h.calls[1].body.operation_id, h.calls[0].body.operation_id);
    assert.equal(h.calls[1].body.expected_source_fingerprint, 'source-v1');
  });
}

test('rapid card switch requests new identity immediately and ignores stale response', {timeout: 15000}, async t => {
  let release, requestedA;
  const waitA = new Promise(yes => { requestedA = yes; }); const gate = new Promise(yes => { release = yes; });
  t.after(() => release());
  const h = await harness(t, {read: async (route, url) => {
    if (url.pathname.includes('WR-A')) { requestedA(); await gate; await route.fulfill({json: detail('WR-A')}).catch(() => {}); }
    else await route.fulfill({json: detail('WR-B')});
  }});
  await h.open('WR-A'); await waitA; await h.open('WR-B');
  await expect(h.page.locator('#wr-detail')).toContainText('Fixture Owner', {timeout: 1500});
  release(); await h.page.getByRole('button', {name: '초안 다시 작성', exact: true}).click();
  await expect.poll(() => h.calls.length).toBe(1);
  assert.equal(h.calls[0].url, '/api/board/WR-B/source/actions');
});

test('375px native component wraps long content without horizontal overflow', {timeout: 15000}, async t => {
  const value = detail(); value.detail.issue.text = 'LongSource'.repeat(90); value.detail.artifact.content = 'LongArtifact'.repeat(150);
  const h = await harness(t, {value, viewport: {width: 375, height: 812}}); await h.open();
  await expect(h.page.locator('.wr-artifact')).toContainText('LongArtifact');
  const sizes = await h.page.evaluate(() => ({viewport: innerWidth, document: document.documentElement.scrollWidth,
    panelWidth: document.querySelector('#wr-detail').clientWidth, panelScroll: document.querySelector('#wr-detail').scrollWidth}));
  assert.ok(sizes.document <= sizes.viewport, JSON.stringify(sizes));
  assert.ok(sizes.panelScroll <= sizes.panelWidth + 1, JSON.stringify(sizes));
  mkdirSync(shots, {recursive: true}); await h.page.screenshot({path: resolve(shots, 'work-requests-component-mobile.png'), fullPage: true});
});
