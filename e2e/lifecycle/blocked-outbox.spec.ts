import { test, expect } from '../fixtures';
import { boot, auth, checkpoint } from './evidence';

test('LC-BLOCKED-OUTBOX: offline failed changes stay distinct from retryable edits', async ({page, request, context}, info) => {
  test.setTimeout(90_000);
  page.setDefaultTimeout(10_000);
  await boot(page); const headers = await auth(page);
  const cards: any[] = [];
  const queue = () => page.evaluate(() => JSON.parse(localStorage.getItem('amux_offline_queue') || '[]'));
  const enqueue = async (id: string, title: string, rev: number) => page.evaluate(async ({id, title, rev}) => {
    return (window as any)._queueOp('/api/board/' + id, {method:'PATCH', headers:{'Content-Type':'application/json'},
      body:JSON.stringify({title, expect_rev:rev})});
  }, {id, title, rev});
  try {
    for (const title of ['Conflicting edit', 'Offline edit']) {
      const made = await request.post('/api/board', {headers, data:{title:`${title} ${info.project.name}`, type:'chore'}});
      expect(made.ok()).toBe(true); cards.push(await made.json());
    }
    const peer = await request.patch('/api/board/' + cards[0].id, {headers,
      data:{title:'Newer peer edit must survive', expect_rev:cards[0].rev}});
    expect(peer.ok()).toBe(true);
    expect(await enqueue(cards[0].id, 'Stale edit', cards[0].rev)).toBe(true);
    await page.evaluate(() => (window as any).runSyncBanner({quiet:true}));
    await expect.poll(async () => (await queue())[0]?.state).toBe('blocked');
    await context.setOffline(true);
    await page.evaluate(() => window.dispatchEvent(new Event('offline')));
    const title = page.locator('#offline-banner-title');
    await expect(title).toContainText('Offline');
    await expect(title).toContainText('1 failed op');
    await expect(title).not.toContainText('will send on reconnect');
    await checkpoint(page, info, 'offline-conflict-needs-review');
    await title.getByRole('link', {name:'review', exact:true}).click();
    await expect(page.locator('#queue-list')).toContainText('409');
    await page.locator('#queue-overlay [onclick="closeQueueModal()"]').click();
    expect(await enqueue(cards[1].id, 'Resumed offline edit', cards[1].rev)).toBe(true);
    await expect(title).toContainText('1 queued, will send on reconnect');
    await expect(title).toContainText('1 failed');
    await checkpoint(page, info, 'offline-mixed-queue');
    const dismiss = page.locator('#offline-ops .blocked').getByRole('button', {name:'Dismiss failed change', exact:true});
    await expect.poll(async()=>{const hit=await dismiss.boundingBox();return hit?Math.min(hit.width,hit.height):0;}).toBeGreaterThanOrEqual(44);
    if (info.project.use.hasTouch) await dismiss.tap(); else await dismiss.click();
    await expect.poll(async () => (await queue()).length).toBe(1);
    expect((await queue())[0].url).toBe('/api/board/' + cards[1].id);
    await expect(title).not.toContainText('failed');
    await context.setOffline(false);
    await page.evaluate(() => window.dispatchEvent(new Event('online')));
    await expect.poll(async () => (await queue()).length, {timeout:15_000}).toBe(0);
    await expect(page.locator('#offline-banner')).not.toHaveClass(/active/);
    const read = async (id: string) => (await request.get('/api/board/' + id, {headers})).json();
    expect((await read(cards[0].id)).title).toBe('Newer peer edit must survive');
    expect((await read(cards[1].id)).title).toBe('Resumed offline edit');
    await checkpoint(page, info, 'offline-edit-reconciled');
  } finally {
    if (!page.isClosed()) {
      await context.setOffline(false);
      await page.evaluate(async ids => (window as any)._mutateQueue((entries: any[]) => {
        for (let i=entries.length-1; i>=0; i--) if (ids.some(id => entries[i].url === '/api/board/' + id)) entries.splice(i,1);
      }), cards.map(c=>c.id));
    }
    for (const card of cards) await request.delete('/api/board/' + card.id, {headers});
  }
});

test('LC-BLOCKED-OUTBOX: an old uncertain message survives reload with its identity and file references', async ({page},info)=>{
  await boot(page);
  const row={id:'lc-uncertain-reload',url:'/api/sessions/lc-uncertain/send',state:'blocked',
    error:'409: previous message acceptance is uncertain; inspect the worker terminal',
    timestamp:Date.now()-18*60*60*1000,
    options:{method:'POST',body:JSON.stringify({msg_id:'lc-uncertain-reload',text:'Read @/tmp/lc-evidence.txt and report the result'})}};
  await page.evaluate(async row=>(window as any)._mutateQueue((q:any[])=>q.push(row)),row);
  await page.reload(); await page.waitForFunction(()=>typeof (window as any).runSyncBanner==='function');
  await page.evaluate(()=>(window as any).runSyncBanner(true));
  const saved=await page.evaluate(()=>JSON.parse(localStorage.getItem('amux_offline_queue')||'[]'));
  expect(saved).toHaveLength(1);expect(saved[0].id).toBe(row.id);expect(saved[0].options.body).toBe(row.options.body);
  expect(saved[0].state).toBe('blocked');
  await page.evaluate(()=>(window as any).showQueueModal());
  await expect(page.locator('#queue-list')).toContainText('acceptance is uncertain');
  await expect(page.locator('#sync-items .done')).toHaveCount(0);
  await checkpoint(page,info,'uncertain-message-retained-after-reload');
});
