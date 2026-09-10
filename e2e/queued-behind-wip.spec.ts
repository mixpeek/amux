import { test, expect, Page } from './fixtures';

const ready = [1295,1309,1468,1496,1516,1520].map((n, i) => ({
  id: 'MG-' + n, title: i === 0 ? 'Pricing harness needs an internal plan' : 'Queued task ' + (i + 1),
}));
const frontier = { session:'mixpeek-general', measured:true, claimable_now:0,
  ready, wip:{holding:['MG-1743'],cap:1} };

async function setup(page: Page, response = frontier) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, route => route.fulfill({json:[
    {name:'mixpeek-general',dir:'/tmp/queue-test',running:true,status:'idle',flags:'--model claude-opus-5'}]}));
  await page.route('**/api/board/ready?session=mixpeek-general', route => route.fulfill({json:response}));
  await page.route('**/api/sessions/mixpeek-general/peek?*', route => route.fulfill({json:{name:'mixpeek-general',output:'Worker ready.\n',history:''}}));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function' && eval("sessions.some(s => s.name === 'mixpeek-general' && s.status === 'idle')"));
  await page.evaluate(() => {
    (window as any).openPeek('mixpeek-general');
    (window as any)._stopPeekPoll();
  });
  await expect(page.locator('#peek-overlay')).toBeVisible();
}

test('queued badge opens every task and real editable board details above the worker', async ({page}, info) => {
  const writes: string[] = [];
  page.on('request', r => { if (/\/api\/(board|sessions)\//.test(r.url()) && ['PATCH','PUT','DELETE'].includes(r.method())) writes.push(r.url()); });
  await page.route('**/api/board/MG-*', route => {
    expect(route.request().method()).toBe('GET');
    const id = new URL(route.request().url()).pathname.split('/').pop();
    const card = ready.find(c=>c.id===id);
    return route.fulfill({json:{id,title:card?.title || 'Current worker task',status:id==='MG-1743'?'doing':'todo',
      session:'mixpeek-general',desc:'Task details',tags:[],gate:[],rev:1}});
  });
  await setup(page);
  await page.locator('#peek-cmd-input').fill('Keep my unsent message');
  const chip = page.locator('#peek-session-status .work-queued-chip');
  await expect(chip).toHaveAccessibleName('Manage task queue: MG-1295 +5 queued behind MG-1743');
  if (info.project.name === 'mobile') {
    await expect(chip.locator('.work-queued-short')).toHaveText('6 queued ▾');
    const title = await page.locator('#peek-title').boundingBox();
    expect(title!.width).toBeGreaterThan(30);
  }
  await chip.click();
  const dialog = page.getByRole('dialog',{name:'mixpeek-general · Task queue'});
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText('limit: 1');
  await expect(dialog.locator('[data-queue-card]')).toHaveCount(7);
  await expect(dialog).toContainText('Pricing harness needs an internal plan');
  await expect(dialog).toContainText('Ready tasks · 6');
  const overflow = await dialog.evaluate(el => el.scrollWidth > el.clientWidth);
  expect(overflow).toBe(false);
  await page.screenshot({path:info.outputPath('task-queue.png')});
  // Test the actual editor, not just a changed hash or stubbed open callback.
  await dialog.locator('[data-queue-card="MG-1743"]').click();
  await expect(page.locator('#bd-key')).toHaveText('MG-1743');
  await expect(page.locator('#bd-session')).toBeVisible();
  await expect(page.locator('#bd-title')).toHaveValue('Current worker task');
  await expect(page.locator('#bd-status-row')).toBeVisible();
  await page.locator('#board-detail-overlay').getByRole('button',{name:'← Back',exact:true}).click();
  await expect(page.locator('#peek-cmd-input')).toHaveValue('Keep my unsent message');
  await chip.click();
  await dialog.locator('[data-queue-card="MG-1520"]').click();
  await expect(page.locator('#bd-key')).toHaveText('MG-1520');
  await expect(page.locator('#bd-session')).toBeVisible();
  await expect(page.locator('#bd-title')).toHaveValue('Queued task 6');
  await expect(page.locator('#bd-session')).toHaveValue('mixpeek-general');
  await expect(page.locator('#board-detail-overlay')).toHaveCSS('opacity','1');
  await page.screenshot({path:info.outputPath('queued-task-controls.png')});
  expect(writes).toEqual([]);
});

test('failed queue fetch is visible, retryable, and does not claim an empty queue', async ({page}) => {
  await setup(page);
  const beacons:any[]=[];
  await page.route('**/api/client-debug', route => {beacons.push(route.request().postDataJSON());return route.fulfill({json:{ok:true}});});
  await expect(page.locator('.work-queued-chip')).toBeVisible();
  let fail = true;
  await page.route('**/api/board/ready?session=mixpeek-general', route => fail
    ? route.fulfill({status:503,json:{error:'unavailable'}}) : route.fulfill({json:frontier}));
  await page.locator('.work-queued-chip').click();
  const dialog=page.getByRole('dialog',{name:'mixpeek-general · Task queue'});
  await expect(dialog.getByRole('alert')).toContainText('Could not load');
  await expect(dialog.locator('[data-queue-card]')).toHaveCount(0);
  fail=false;
  await dialog.getByRole('button',{name:'Try again'}).click();
  await expect(dialog.locator('[data-queue-card]')).toHaveCount(7);
  expect(beacons.some(b=>b.kind==='worker-queue' && b.verdict==='load-failed' && b.measured===false)).toBe(true);
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  await expect(page.locator('#peek-overlay')).toBeVisible();
});

test('ready but unclaimable work with no holding card still says stalled', async ({page}) => {
  await setup(page,{...frontier,wip:{holding:[],cap:1}});
  await expect(page.locator('#peek-session-status')).toContainText('stalled · 6 ready');
  await expect(page.locator('.work-queued-chip')).toHaveCount(0);
});

test('task queue stays accessible from Worker actions while the worker is active', async ({page}) => {
  await setup(page);
  await page.evaluate(() => { eval("sessions.find(s=>s.name==='mixpeek-general').status = 'active'"); (window as any).updatePeekStatus(); });
  await expect(page.locator('.work-queued-chip')).toHaveCount(0);
  await page.locator('#peek-overlay').getByRole('button',{name:'Worker actions',exact:true}).click();
  await page.getByRole('menuitem',{name:'Task queue',exact:false}).click();
  const dialog=page.getByRole('dialog',{name:'mixpeek-general · Task queue'});
  await expect(dialog.locator('[data-queue-card]')).toHaveCount(7);
  await dialog.getByRole('button',{name:'Close task queue'}).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.locator('#peek-overlay')).toBeVisible();
});
