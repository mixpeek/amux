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
  // Status refreshes must preserve the live menu item. Feedback decorates it
  // with data attributes, which used to make an innerHTML comparison rebuild
  // the open menu on every refresh and lose an iOS tap during DOM replacement.
  const resumeNode=await resume.elementHandle();
  await page.evaluate(()=>{(window as any).updatePeekStatus();(window as any).updatePeekStatus();});
  expect(await resume.evaluate((node, original)=>node===original,resumeNode)).toBe(true);
  const resumed=page.waitForResponse(r=>r.url().endsWith('/api/workers/pause-probe/resume') && r.request().method()==='POST');
  await resume.click();
  expect(await (await resumed).json()).toMatchObject({lifecycle:'active',running:true});
  await expect.poll(()=>calls).toBe(2);
  await page.waitForFunction(()=>eval('sessions.find(s=>s.name==="pause-probe").lifecycle')==='active');
  await expect(page.locator('#peek-session-status')).not.toContainText('resuming');
  await page.locator('#peek-worker-menu-btn').click();
  await expect(page.locator('#peek-more-dropdown [data-worker-action="pause"]')).toHaveAttribute('aria-disabled','false');
});

// The service-worker bar these cases measured was retired (f154d030): offline
// mode off is now a dot on the badge, and e2e/sw-offline-notice.spec.ts pins
// that Send and Create stay tappable with it.
