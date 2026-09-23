import {test, expect} from './fixtures';

for (const provider of ['claude', 'codex', 'gemini']) {
  test(`${provider}: pause/resume keeps terminal state, actions and drafts consistent`, async ({page}, info) => {
    const worker = {name:'pause-probe',provider,model:'test-model',running:true,status:'active',lifecycle:'active',dir:'/tmp'};
    let actionCount = 0;
    let release: (() => void) | undefined;
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done','1'));
    await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[worker]}));
    await page.route('**/api/sessions/pause-probe/peek?*', r => r.fulfill({json:{name:worker.name,live:'Saved conversation',pane_cols:80}}));
    await page.route(/\/api\/workers\/pause-probe\/(pause|resume)$/, async r => {
      actionCount++;
      await new Promise<void>(resolve => { release = resolve; });
      const paused = r.request().url().endsWith('/pause');
      worker.lifecycle = paused ? 'paused' : 'active';
      worker.running = !paused;
      worker.status = paused ? 'idle' : 'starting';
      await r.fulfill({json:{applied:true,name:worker.name,lifecycle:worker.lifecycle,running:worker.running,session:paused?'stopped':'started'}});
    });
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
    await page.evaluate(worker => {
      eval('sessions=['+JSON.stringify(worker)+']; render();');
      (window as any).openPeek(worker.name); (window as any)._stopPeekPoll();
    }, worker);
    await page.locator('#peek-cmd-input').fill('keep this unsent text');
    await page.locator('#peek-worker-menu-btn').click();
    await page.locator('#peek-more-dropdown [data-worker-action="pause"]').click();
    await expect.poll(() => actionCount).toBe(1);
    await expect(page.locator('#peek-session-status')).toContainText('pausing');
    await page.evaluate(() => { void (window as any).pauseWorker('pause-probe'); });
    expect(actionCount).toBe(1);
    release!();
    await expect(page.locator('#peek-session-status')).toHaveText('paused');
    await expect(page.locator('#peek-cmd-input')).toHaveValue('keep this unsent text');
    await page.screenshot({path:info.outputPath('paused-terminal.png')});
    await page.locator('#peek-worker-menu-btn').click();
    await page.locator('#peek-more-dropdown [data-worker-action="resume"]').click();
    await expect.poll(() => actionCount).toBe(2);
    await expect(page.locator('#peek-session-status')).toContainText('resuming');
    release!();
    await expect(page.locator('#peek-session-status')).toHaveText('starting');
    await expect(page.locator('#peek-cmd-input')).toHaveValue('keep this unsent text');
    await page.evaluate(() => (window as any).closePeek());
    await expect(page.locator('#cards .card[data-session="pause-probe"]')).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({path:info.outputPath('resumed-card.png')});
  });
}

test('a failed pause exposes remaining work and offers Retry Pause', async ({page}) => {
  const worker = {name:'pause-probe',running:true,status:'active',lifecycle:'paused',dir:'/tmp'};
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[worker]}));
  await page.route('**/api/sessions/pause-probe/peek?*', r => r.fulfill({json:{name:worker.name,live:'Still running',pane_cols:80}}));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
  await page.evaluate(worker => {
    eval('sessions=['+JSON.stringify(worker)+']; pausedExpanded=true; render();');
    (window as any).openPeek(worker.name); (window as any)._stopPeekPoll();
  },worker);
  await expect(page.locator('#peek-session-status')).toHaveText('pause incomplete');
  await page.locator('#peek-worker-menu-btn').click();
  await expect(page.locator('#peek-more-dropdown [data-worker-action="pause"]')).toBeVisible();
  await page.evaluate(() => (window as any).closePeek());
  await expect(page.locator('.paused-resume-btn')).toHaveText('Retry Pause');
});

test('pause refresh keeps shared Resume disabled until lifecycle operation settles', async ({page}) => {
  const worker={name:'pause-probe',provider:'codex',running:true,status:'active',lifecycle:'active',dir:'/tmp'};
  let held=false, calls=0;
  let release!:()=>void;
  const refresh=new Promise<void>(resolve=>{release=resolve;});
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/,async r=>{if(held)await refresh;await r.fulfill({json:[worker]});});
  await page.route('**/api/sessions/pause-probe/peek?*',r=>r.fulfill({json:{name:worker.name,live:'Saved conversation',pane_cols:80}}));
  await page.route('**/api/workers/pause-probe/resume',async r=>{calls++;worker.lifecycle='active';worker.running=true;worker.status='active';await r.fulfill({json:{lifecycle:'active',running:true}});});
  await page.route('**/api/workers/pause-probe/pause',async r=>{calls++;worker.lifecycle='paused';worker.running=false;held=true;await r.fulfill({json:{lifecycle:'paused',running:false}});});
  await page.goto('/');
  await page.waitForFunction(()=>typeof (window as any).openPeek==='function');
  await page.evaluate(worker=>{eval('sessions=['+JSON.stringify(worker)+']; render();');(window as any).openPeek(worker.name);(window as any)._stopPeekPoll();},worker);
  await page.evaluate(()=>{void (window as any).pauseWorker('pause-probe');});
  await expect.poll(()=>calls).toBe(1);
  await page.waitForFunction(()=>eval('sessions.find(s=>s.name==="pause-probe").lifecycle')==='paused');
  await page.locator('#peek-worker-menu-btn').click();
  const resume=page.locator('#peek-more-dropdown [data-worker-action="resume"]');
  await expect(resume).toHaveAttribute('aria-disabled','true');
  await resume.dispatchEvent('click');
  expect(calls).toBe(1);
  release();
  await expect(resume).toHaveAttribute('aria-disabled','false');
  await expect(resume).toHaveText(/Resume/);
  const resumed=page.waitForResponse(r=>r.url().endsWith('/api/workers/pause-probe/resume') && r.request().method()==='POST');
  await resume.click();
  expect(await (await resumed).json()).toMatchObject({lifecycle:'active',running:true});
  await expect.poll(()=>calls).toBe(2);
  await page.waitForFunction(()=>eval('sessions.find(s=>s.name==="pause-probe").lifecycle')==='active');
  await expect(page.locator('#peek-session-status')).not.toContainText('resuming');
  await page.locator('#peek-worker-menu-btn').click();
  await expect(page.locator('#peek-more-dropdown [data-worker-action="pause"]')).toHaveAttribute('aria-disabled','false');
});

for(const width of [390,1280]) test(`service-worker warning leaves terminal Send reachable at ${width}`,async({page})=>{
  await page.setViewportSize({width,height:800});
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  const worker={name:'pause-probe',provider:'codex',running:true,status:'idle',dir:'/tmp'};
  await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:[worker]}));
  await page.route('**/api/sessions/pause-probe/peek?*',r=>r.fulfill({json:{name:worker.name,live:'Saved conversation',pane_cols:80}}));
  await page.route('**/api/offline-origin',r=>r.fulfill({json:{why:'self-signed certificate',proxied:false}}));
  await page.goto('/');await page.waitForFunction(()=>typeof (window as any).openPeek==='function');
  await page.evaluate(async worker=>{eval('sessions=['+JSON.stringify(worker)+']; render();');(window as any).openPeek(worker.name);(window as any)._stopPeekPoll();await (window as any)._swOfferGoodOrigin();},worker);
  await expect(page.locator('#sw-fail-bar')).toBeVisible();
  const send=page.locator('#peek-overlay .send-split-main');
  await expect(send).toBeVisible();
  await expect.poll(async()=>{const a=(await send.boundingBox())!,b=(await page.locator('#sw-fail-bar').boundingBox())!;return a.y+a.height<=b.y;}).toBe(true);
  await send.click({trial:true});
});
