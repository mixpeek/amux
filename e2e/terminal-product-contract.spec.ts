import { type Route } from '@playwright/test';
import { test, expect, type Page, allowUnusedRoute } from './fixtures';

const worker = 'terminal-contract';

async function boot(page: Page, options?: {
  transcript?: string;
  live?: string;
  historyRows?: Record<string, unknown>[];
  liveDelayMs?: number;
  willSend?: boolean;
  waitForBothFrames?: boolean;
  paneCols?: number;
}) {
  let live = options?.live || 'Idle terminal\n';
  let peekResponses = 0;
  const transcript = options?.transcript || '';
  const historyRows = options?.historyRows || [];
  let persistedLayout: string | null = null;

  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, route => route.fulfill({ json: [{
    name: worker, dir: '/tmp/terminal-contract', running: true, status: 'working',
  }] }));
  await page.route(`**/api/sessions/${worker}/subagents`, route => route.fulfill({json:{session:worker,subagents:[]}}));
  const tasksRoute = `**/api/sessions/${worker}/tasks`;
  await page.route(tasksRoute, route => route.fulfill({ json: { tasks: [], counts: {}, total: 0 } }));
  allowUnusedRoute(page, tasksRoute); // plan polling is throttled and optional to these terminal contracts
  await page.route(/\/api\/prefs(?:\?.*)?$/, async route => {
    if (route.request().method() === 'POST') {
      const body = route.request().postDataJSON() as { key?: string; value?: string };
      if (body.key === 'peek_tab_layout') persistedLayout = body.value || null;
      return route.fulfill({ json: { ok: true, key: body.key, value: body.value } });
    }
    const key = new URL(route.request().url()).searchParams.get('key') || '';
    return route.fulfill({ json: key === 'peek_tab_layout'
      ? { key, value: persistedLayout }
      : { key, value: null } });
  });
  await page.route(`**/api/history?limit=200&offset=0&session=${worker}`,
    route => route.fulfill({ json: historyRows }));
  const sendRoute = `**/api/sessions/${worker}/send`;
  await page.route(sendRoute, async (route: Route) => {
    const body = route.request().postDataJSON() as { text?: string };
    live = `\u276f ${body.text || ''}\nAssistant accepted the request\n`;
    await route.fulfill({ json: { ok: true, submitted: true, submission: 'verified' } });
  });
  if (!options?.willSend) allowUnusedRoute(page, sendRoute);
  await page.route(`**/api/sessions/${worker}/peek?*`, async route => {
    const liveOnly = new URL(route.request().url()).searchParams.has('live');
    if (liveOnly && options?.liveDelayMs) await new Promise(resolve => setTimeout(resolve, options.liveDelayMs));
    await route.fulfill({ json: liveOnly
      ? { name: worker, live, output: live, live_only: true, pane_cols: options?.paneCols }
      : { name: worker, history: transcript, live, output: live, output_is_viewport_only: true, pane_cols: options?.paneCols } });
    peekResponses += 1;
  });

  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(name => {
    eval("_sendMode='send'");
    (window as any).openPeek(name);
  }, worker);
  await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
  await page.waitForFunction(() => getComputedStyle(document.getElementById('peek-overlay')!).transform === 'none');
  await expect(page.locator('#peek-body')).not.toContainText('Loading latest');
  // openPeek intentionally starts the fast live frame and the complete frame
  // concurrently. Wait for both so a slower engine cannot race a synthetic
  // frame against the still-arriving initial response.
  if (options?.waitForBothFrames !== false) {
    await expect.poll(() => peekResponses).toBeGreaterThanOrEqual(2);
  }
  return {
    setLive(value: string) { live = value; },
    getPersistedLayout() { return persistedLayout; },
  };
}

// A scrollTop assignment is also used by the product's bottom-follow logic;
// it is not a reader gesture. Supply wheel/touch intent before positioning a
// specimen; the native Simulator test separately exercises an actual swipe.
async function readEarlier(page: Page, touch: boolean) {
  const body = page.locator('#peek-body');
  const before = await body.evaluate(el => el.scrollTop);
  if (touch) {
    // Playwright mobile WebKit has no wheel API. A real tap on the body's
    // padding supplies touch intent; the native Simulator suite owns swipes.
    await body.tap({ position: { x: 5, y: 5 } });
    await body.evaluate(el => { el.scrollTop -= 400; });
  } else {
    await body.hover();
    await page.mouse.wheel(0, -400);
  }
  await expect.poll(() => body.evaluate(el => el.scrollTop)).toBeLessThan(before - 10);
}

test('history/live seam renders every submitted prompt exactly once', async ({ page }) => {
  const oldOne = '[05:35 PM] first submitted request';
  const oldTwo = '[05:38 PM] second submitted request';
  const current = '[05:52 PM] newest submitted request';
  const transcript = `\u276f ${oldOne}\nAssistant first answer\n\u276f ${oldTwo}\nAssistant second answer\n`;
  const live = `\u276f ${oldOne}\nAssistant first answer\n\u276f ${oldTwo}\nAssistant second answer\n\u276f ${current}\nWorking now\n`;
  await boot(page, { transcript, live, historyRows: [
    { id: 1, session: worker, type: 'user', kind: 'human', text: oldOne, ts: Date.now() - 2 },
    { id: 2, session: worker, type: 'user', kind: 'human', text: oldTwo, ts: Date.now() - 1 },
    { id: 3, session: worker, type: 'user', kind: 'human', text: current, ts: Date.now() },
  ] });

  for (const text of [oldOne, oldTwo, current]) {
    await expect(page.locator('#peek-body .peek-prompt').filter({ hasText: text })).toHaveCount(1);
  }
  await expect(page.locator('#peek-body .peek-prompt-human')).toHaveCount(3);
});

test('a prompt sent while terminal is open is immediately attributed to the human', async ({ page }) => {
  await boot(page, { historyRows: [], willSend: true });
  const text = '[05:52 PM] sent from this open terminal';
  await page.locator('#peek-cmd-input').fill(text);
  await page.locator('.peek-cmd-bar .send-split-main').click();

  const prompt = page.locator('#peek-body .peek-prompt').filter({ hasText: text });
  await expect(prompt).toHaveCount(1);
  await expect(prompt).toHaveAttribute('data-msg-kind', 'human');
  await expect(prompt).toHaveAttribute('data-msg-label', 'Human');
  await expect(prompt).not.toContainText('Unclassified');
});

test('a delayed nonempty history snapshot preserves newly queued human attribution', async ({page}) => {
  let release!:()=>void;
  const held=new Promise<void>(resolve=>{release=resolve;});
  await page.route('**/api/history?limit=500',async route=>{
    await held;
    await route.fulfill({json:[{id:800,text:'Older server history',type:'direct',session:'another-worker',ts:1}]});
  });
  try {
    await boot(page,{historyRows:[],willSend:true});
    const text='New human message must survive the older global snapshot';
    await page.locator('#peek-cmd-input').fill(text);
    await page.locator('.peek-cmd-bar .send-split-main').click();
    await expect(page.locator('#peek-cmd-input')).toHaveValue('');
    const response=page.waitForResponse(r=>r.url().endsWith('/api/history?limit=500'));
    release();await response;
    await expect.poll(()=>page.evaluate(()=>eval('_cmdHistory').some((r:any)=>r.id===800))).toBe(true);
    await expect.poll(()=>page.evaluate(text=>eval('_cmdHistory').some((r:any)=>r.text===text&&r.session==='terminal-contract'),text)).toBe(true);
    await expect(page.locator('#peek-body .peek-prompt').filter({hasText:text})).toHaveAttribute('data-msg-kind','human');
    const peers=await page.evaluate(()=>{
      (window as any).cmdHistoryAdd('Same message',{session:'peer-one',type:'direct'});
      (window as any).cmdHistoryAdd('Same message',{session:'peer-two',type:'direct'});
      return eval('_cmdHistory').filter((r:any)=>r.text==='Same message').map((r:any)=>r.session);
    });
    expect(peers).toEqual(['peer-one','peer-two']);
  } finally {release();}
});

test('terminal controls stay compact and the bottom affordance distinguishes navigation from new output', async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  const transcript = Array.from({ length: 220 }, (_, i) => `terminal output row ${i}`).join('\n');
  const state = await boot(page, { transcript, live: 'latest output\n' });

  const layout = await page.evaluate(() => {
    const controls = document.querySelector('.peek-output-controls')!.getBoundingClientRect();
    const body = document.getElementById('peek-body')!.getBoundingClientRect();
    return { controlsWidth: controls.width, bodyWidth: body.width, controlsRight: controls.right, bodyRight: body.right };
  });
  expect(layout.controlsWidth).toBeLessThan(260);
  expect(Math.abs(layout.controlsRight - layout.bodyRight)).toBeLessThan(12);

  await readEarlier(page, !!testInfo.project.use.hasTouch);
  await page.evaluate(() => {
    const body = document.getElementById('peek-body')!;
    body.scrollTop = 0;
    body.dispatchEvent(new Event('scroll'));
  });
  const notice = page.locator('.scroll-lock-badge');
  await expect(notice).toBeVisible();
  await expect(notice).toHaveText('Jump to bottom \u2193');
  expect(await notice.evaluate(el => el.closest('#peek-body') !== null)).toBe(true);

  state.setLive('new buffered output\n');
  await page.evaluate(() => (window as any).refreshPeek(true));
  await expect(notice).toBeVisible();
  await expect(notice).toHaveText('New output \u2193');
  expect(await notice.evaluate(el => el.closest('#peek-body') !== null)).toBe(true);
  expect(await notice.evaluate(el => el.getBoundingClientRect().width)).toBeLessThan(180);
  await page.screenshot({ path: testInfo.outputPath('terminal-product-contract.png') });
});

test('terminal chrome cannot inject navigation or slash-picker keys', async ({ page }) => {
  const keyRequests: string[] = [];
  page.on('request', request => {
    if (/\/api\/sessions\/[^/]+\/(?:keys|send)/.test(request.url())) keyRequests.push(request.url());
  });
  await boot(page);

  const controls = page.locator('.peek-output-controls');
  await expect(controls.locator('button:visible')).toHaveCount(1);
  await expect(controls.locator('[onclick*="peekQuickKeys"]')).toHaveCount(0);
  await controls.locator('#peek-copy-btn').click();
  await page.waitForTimeout(100);
  expect(keyRequests).toEqual([]);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('');
});

test('worker tab choices restore from the server after browser storage is lost and the page reloads', async ({ page }) => {
  const state = await boot(page);
  await page.locator('#peek-tab-customize').click();
  const translate = page.locator('#peek-tab-customizer-menu .tab-customizer-item').filter({ hasText: 'Translate' });
  await expect(translate).toBeVisible();
  await translate.locator('input').uncheck();
  await expect(page.locator('#peek-tab-simple')).toBeHidden();
  await expect.poll(() => {
    const saved = state.getPersistedLayout();
    return saved ? JSON.parse(saved).hidden : [];
  }).toContain('simple');

  // Reproduce the failure mode more aggressively than a normal refresh: even
  // if browser storage has been purged, the server-backed layout must win.
  await page.evaluate(() => {
    localStorage.removeItem('amux_peek_hidden_tabs');
    localStorage.removeItem('amux_peek_tab_order');
  });
  await page.reload();
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(name => (window as any).openPeek(name), worker);
  await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
  await expect(page.locator('#peek-tab-simple')).toBeHidden();
  await expect(page.locator('#peek-tab-steering')).toBeVisible();
});

test('terminal scroll geometry remains stable through repeated live frames', async ({ page }, testInfo) => {
  const transcript = Array.from({ length: 900 }, (_, i) => `stable terminal row ${i}`).join('\n');
  const state = await boot(page, { transcript, live: 'live frame zero\n' });
  await readEarlier(page, !!testInfo.project.use.hasTouch);
  const before = await page.evaluate(() => {
    const body = document.getElementById('peek-body')!;
    body.scrollTop = Math.floor((body.scrollHeight - body.clientHeight) * 0.45);
    const history = body.querySelector('#pk-hist')!;
    return { top: body.scrollTop, contentVisibility: getComputedStyle(history).contentVisibility };
  });
  expect(before.contentVisibility).toBe('visible');

  for (let i = 1; i <= 8; i++) {
    state.setLive(`live frame ${i}\n`);
    await page.evaluate(async () => {
      await (window as any).refreshPeek(true);
      await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    });
  }
  const after = await page.locator('#peek-body').evaluate(body => (body as HTMLElement).scrollTop);
  expect(Math.abs(after - before.top)).toBeLessThanOrEqual(1);
});

for (const width of [390, 1280]) {
  test(`terminal documents retain markup boundaries and literal output at ${width}px`, async ({ page }, testInfo) => {
    await page.setViewportSize({ width, height: 844 });
    const rendererSignals: unknown[] = [];
    page.on('request', request => {
      if (request.url().endsWith('/api/client-debug') && request.method() === 'POST') {
        const data = request.postDataJSON();
        if (data?.kind === 'peek-poll') rendererSignals.push(data.input_chunk_parser_present);
      }
    });
    const numbered = '38-    background: #1c2128;\n39-    color: white;';
    const history = [
      ...Array.from({ length: 63 }, (_, i) => `ordinary output ${i}`),
      '\u276f a submitted prompt at the old chunk boundary',
      '  its indented continuation',
      'OUTSIDE_PROMPT first answer',
      '\u001b[38;2;144;200;240m' + Array.from({ length: 140 }, (_, i) => `colored answer ${i}`).join('\n') + '\u001b[0m',
      numbered,
      '\u001b]8;;https://example.com/reference\u0007reference\u001b]8;;\u0007',
      'OUTSIDE_PROMPT final answer',
    ].join('\n');
    await boot(page, { transcript: history, live: 'current output\n' });
    const body = page.locator('#peek-body');
    await expect(body).toContainText('OUTSIDE_PROMPT final answer');
    await expect(body.locator('.peek-render-chunk, .peek-code-row, .peek-code-split')).toHaveCount(0);
    await expect.poll(() => rendererSignals.length).toBeGreaterThan(0);
    expect(rendererSignals.every(value => value === false)).toBe(true);
    await expect(body.locator('.peek-prompt')).toHaveCount(1);
    await expect(body.locator('.peek-prompt')).not.toContainText('OUTSIDE_PROMPT');
    await expect(body.locator('.peek-prompt .peek-prompt')).toHaveCount(0);
    await expect(body).toContainText(numbered);
    await expect(body.locator('a[href="https://example.com/reference"]')).toHaveText('reference');
    const bounds = await body.boundingBox();
    expect(bounds!.width).toBeLessThanOrEqual(width);
    await body.evaluate(el => { el.scrollTop = el.scrollHeight; });
    await page.screenshot({ path: testInfo.outputPath(`document-boundaries-${width}.png`) });
  });
}

test('a delayed live-frame request cannot hold the terminal loading screen open', async ({ page }) => {
  const started = Date.now();
  await boot(page, {
    transcript: 'full response paints without waiting for live\n',
    live: 'current terminal frame\n',
    liveDelayMs: 3000,
    waitForBothFrames: false,
  });
  await expect(page.locator('#peek-body')).toContainText('current terminal frame', { timeout: 700 });
  expect(Date.now() - started).toBeLessThan(2500);
  await expect(page.locator('#peek-body .peek-loading')).toHaveCount(0);
});

test('the HTTP frame retains the worker column cap after renderer reconciliation', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 844 });
  await boot(page, { paneCols: 80, transcript: 'Ordinary prose in the worker terminal. '.repeat(160) });
  const size = await page.evaluate(() => {
    const body = document.getElementById('peek-body')!;
    const history = document.getElementById('pk-hist')!;
    return { cols: body.style.getPropertyValue('--peek-cols'), body: body.clientWidth,
      history: history.getBoundingClientRect().width, max: parseFloat(getComputedStyle(history).maxWidth),
      overflow: history.scrollWidth - history.clientWidth };
  });
  expect(size.cols).toBe('80');
  expect(size.history).toBeCloseTo(size.max, 0);
  expect(size.history).toBeLessThan(size.body);
  expect(size.overflow).toBeLessThanOrEqual(1);
});

test('a small upward scroll at the log bottom relinquishes following through live updates', async ({ page }) => {
  const fixture = await boot(page, { transcript: Array.from({length: 160}, (_, i) => `Saved line ${i}`).join('\n') });
  const body = page.locator('#peek-body');
  await expect.poll(() => body.evaluate(el => el.scrollHeight - el.clientHeight - el.scrollTop)).toBeLessThan(2);
  // Model the first five pixels of a slow reader gesture. The native Simulator
  // probe separately dispatches actual touch swipes; this isolates the 40px seam.
  await body.dispatchEvent('wheel', { deltaY: -5 });
  await body.evaluate(el => { el.scrollTop -= 5; });
  await expect.poll(() => page.evaluate(() => eval('_peekFollowBottom'))).toBe(false);
  const top = await body.evaluate(el => el.scrollTop);
  fixture.setLive('New live output while reading the preceding line\n');
  await page.evaluate(() => (window as any).refreshPeek(true));
  await expect.poll(() => body.evaluate(el => el.scrollTop)).toBeCloseTo(top, 0);
  await expect.poll(() => page.evaluate(() => eval('_peekScrollLocked'))).toBe(true);
  // Reaching the actual bottom again must still resume live following.
  await body.dispatchEvent('wheel', { deltaY: 100 });
  await body.evaluate(el => { el.scrollTop = el.scrollHeight; });
  await expect.poll(() => page.evaluate(() => eval('_peekFollowBottom'))).toBe(true);
  fixture.setLive('Following resumed after the reader returned to the end\n');
  await page.evaluate(() => (window as any).refreshPeek(true));
  await expect.poll(() => body.evaluate(el => el.scrollHeight - el.clientHeight - el.scrollTop)).toBeLessThan(2);
});

test('the compact phone composer keeps its empty prompt readable beside the actions', async ({ page }, testInfo) => {
  await page.setViewportSize({width:375,height:812});
  await boot(page);
  for (const status of ['active', 'idle']) {
    await page.evaluate(status => { eval(`sessions.find(s=>s.name==='terminal-contract').status=${JSON.stringify(status)};updatePeekStatus()`); }, status);
    const g = await page.locator('#peek-cmd-input').evaluate((el: HTMLTextAreaElement) => {
      const style=getComputedStyle(el), canvas=document.createElement('canvas'),ctx=canvas.getContext('2d')!;
      ctx.font=`${style.fontSize} ${style.fontFamily}`;
      const input=el.getBoundingClientRect(), more=document.querySelector('#peek-composer-more-btn')!.getBoundingClientRect(),send=document.querySelector('#peek-cmd-row > .send-split')!.getBoundingClientRect();
      return {promptWidth:ctx.measureText(el.placeholder).width,available:el.clientWidth-parseFloat(style.paddingLeft)-parseFloat(style.paddingRight),height:input.height,sameRow:Math.abs(input.bottom-more.bottom)<2&&Math.abs(input.bottom-send.bottom)<2,overflow:document.documentElement.scrollWidth>innerWidth};
    });
    expect(g.promptWidth,JSON.stringify(g)).toBeLessThanOrEqual(g.available);
    expect(g.sameRow).toBe(true);expect(g.overflow).toBe(false);
  }
  await page.locator('#peek-cmd-input').fill('Preserve this mobile draft.');
  await page.locator('#peek-composer-more-btn')[testInfo.project.use.hasTouch ? 'tap' : 'click']();
  await expect(page.locator('#peek-more-menu')).toBeVisible();
  await page.locator('#peek-composer-more-btn')[testInfo.project.use.hasTouch ? 'tap' : 'click']();
  await expect(page.locator('#peek-cmd-input')).toHaveValue('Preserve this mobile draft.');
  const failure = page.waitForRequest(request => request.url().endsWith('/api/client-debug') && request.postDataJSON()?.verdict === 'composer_placeholder_clipped');
  await page.evaluate(() => {
    const input=document.querySelector('#peek-cmd-input') as HTMLTextAreaElement;
    input.value=''; input.placeholder='This deliberately long placeholder cannot fit the phone composer';
    eval('_geoBeaconSent=false;_peekGeoBeacon()');
  });
  expect((await failure).postDataJSON()).toMatchObject({measured:true,n_considered:1,placeholder_fits:false});
});
