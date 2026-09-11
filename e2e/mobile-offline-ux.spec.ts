import { test, expect, Page } from './fixtures';
declare const _idb: any;
declare function _upqAdd(file: File, dir: string, kind: string): Promise<number>;
declare function _upqList(): Promise<any[]>;
async function boot(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._upqAdd === 'function');
}
test('mobile outbox retains every simultaneous file addition in real IndexedDB', async ({page}) => {
  await boot(page);
  const names = await page.evaluate(async () => {
    await Promise.all(Array.from({length: 8}, (_, i) => _upqAdd(new File(['bytes-' + i], 'mobile-' + i + '.txt'), '', 'file')));
    return (await _upqList()).map(x => x.name).sort();
  });
  expect(names).toEqual(Array.from({length: 8}, (_, i) => 'mobile-' + i + '.txt'));
  await page.reload();
  expect(await page.evaluate(async () => (await _upqList()).length)).toBe(8);
});
test('mobile outbox rejects an aborted storage transaction instead of claiming queued', async ({page}) => {
  await boot(page);
  const result = await page.evaluate(async () => {
    const put = IDBObjectStore.prototype.put;
    IDBObjectStore.prototype.put = function(...args: any[]) {
      const request = put.apply(this, args as any);
      this.transaction.abort();
      return request;
    };
    try {
      await _upqAdd(new File(['do not lose me'], 'not-saved.txt'), '', 'file');
      return 'falsely accepted';
    } catch { return 'rejected'; }
    finally { IDBObjectStore.prototype.put = put; }
  });
  expect(result).toBe('rejected');
  expect(await page.evaluate(async () => (await _upqList()).length)).toBe(0);
});
test('mobile pending file resumes on foreground without an online transition', async ({page}) => {
  await boot(page);
  await page.evaluate(() => _upqAdd(new File(['resume bytes'], 'resume.txt'), '', 'file'));
  let delivered = 0;
  await page.route(/\/api\/upload\//, r => {
    const path = new URL(r.request().url()).pathname;
    if (path.endsWith('/finish')) delivered++;
    return r.fulfill({json: path.endsWith('/start') ? {id:'foreground'} :
      path.endsWith('/finish') ? {path:'/uploads/resume.txt',url:'/api/uploads/resume.txt'} : {ok:true}});
  });
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await expect.poll(() => delivered).toBe(1);
  await expect.poll(() => page.evaluate(async () => (await _upqList()).length)).toBe(0);
});
async function composer(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[{name:'mobile-upload',running:true,status:'idle',dir:'/tmp/mobile-upload'}]}));
  await page.route(/\/api\/sessions\/mobile-upload\/peek\?/, r => r.fulfill({json:{name:'mobile-upload',live:'Mobile worker terminal',history:''}}));
  await page.route('**/api/sessions/mobile-upload/subagents', r => r.fulfill({json:{session:'mobile-upload',subagents:[]}}));
  await page.goto('/');
  await page.evaluate(() => (window as any).openPeek('mobile-upload'));
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity','1');
}
test('mobile composer restores attachment bytes after reload and can remove them permanently', async ({page}, info) => {
  let offline = true;
  await page.route(/\/api\/upload\//, r => offline ? r.abort('internetdisconnected') : r.fulfill({json:
    r.request().url().endsWith('/start') ? {id:'restored'} :
    r.request().url().endsWith('/finish') ? {path:'/uploads/restored.txt',url:'/api/uploads/restored.txt'} : {ok:true}}));
  await composer(page);
  await page.locator('#peek-cmd-input').fill('Review the attached output');
  await page.locator('#peek-file-input').setInputFiles({name:'persist-me.txt',mimeType:'text/plain',buffer:Buffer.from('durable attachment bytes')});
  const chips = page.locator('#peek-attach-bar .peek-attach-chip');
  await expect(chips).toContainText('persist-me.txt');
  await expect(chips.locator('.chip-retry')).toBeVisible({timeout:15000});
  await page.reload();
  await page.evaluate(() => (window as any).openPeek('mobile-upload'));
  await expect(chips).toContainText('persist-me.txt');
  await expect(page.locator('#peek-cmd-input')).toHaveValue('Review the attached output');
  offline = false;
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await expect(chips).toContainText('✓',{timeout:15000});
  expect(await page.evaluate(async () => {
    const rows = await _idb.getUploads();
    const row = rows.find((r:any) => r.name === 'persist-me.txt');
    return new TextDecoder().decode(await _idb.uploadChunk(row.id, 0));
  })).toBe('durable attachment bytes');
  await page.screenshot({path:info.outputPath('mobile-restored-attachment.png')});
  // Force the crash window: cancellation is accepted, but the old page's
  // IndexedDB deletion never completes before navigation.
  await page.evaluate(() => { _idb.deleteUpload = () => new Promise(() => {}); });
  await chips.locator('.chip-remove').click();
  await page.reload();
  await page.evaluate(() => (window as any).openPeek('mobile-upload'));
  await expect(chips).toHaveCount(0);
  await expect.poll(() => page.evaluate(async () => (await _idb.getUploads()).length)).toBe(0);
});

test('attachment removal keeps the file when cancellation intent cannot be persisted', async ({page}) => {
  await page.route(/\/api\/upload\//, r => r.abort('internetdisconnected'));
  await composer(page);
  await page.locator('#peek-file-input').setInputFiles({name:'keep-me.txt',mimeType:'text/plain',buffer:Buffer.from('keep the only copy')});
  const chip = page.locator('#peek-attach-bar .peek-attach-chip');
  await expect(chip.locator('.chip-retry')).toBeVisible();
  await page.evaluate(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function(key, value) {
      if (key.startsWith('amux_attachment_cancel:')) throw new DOMException('storage full', 'QuotaExceededError');
      return original.call(this, key, value);
    };
  });
  await chip.locator('.chip-remove').click();
  await expect(chip).toContainText('keep-me.txt');
  expect(await page.evaluate(async () => (await _idb.getUploads()).length)).toBe(1);
});

test('text outbox reclaims disposable cache space without losing drafts or pending messages', async ({page, context}) => {
  await composer(page);
  await context.setOffline(true);
  const result = await page.evaluate(async () => {
    const w = window as any;
    const sentinel = JSON.stringify({t:'another worker unsent draft',ts:Date.now()});
    localStorage.setItem('amux_draft_other-worker', sentinel);
    const existing = {id:'earlier-send',url:'/api/sessions/other-worker/send',timestamp:Date.now(),state:'pending',
      options:{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text:'earlier pending message',msg_id:'earlier-message'})}};
    localStorage.setItem('amux_offline_queue', JSON.stringify([existing]));
    // Fill the real browser quota, rather than mocking setItem to always fail.
    // This is the observed Safari state: reproducible caches consume the room
    // that the newly mandatory local send needs.
    let low = 0, high = 12 * 1024 * 1024;
    while (high - low > 512) {
      const size = Math.floor((low + high) / 2);
      try { localStorage.setItem('amux_app_html', 'x'.repeat(size)); low = size; }
      catch (error) { if ((error as DOMException).name !== 'QuotaExceededError') throw error; high = size; }
    }
    const text = 'large mobile message '.repeat(3500);
    const accepted = await w.doSend('mobile-upload', text);
    const queue = JSON.parse(localStorage.getItem('amux_offline_queue') || '[]');
    return {accepted,cacheBytes:low,queueLength:queue.length,
      oldPreserved:JSON.stringify(queue[0]) === JSON.stringify(existing),
      draftPreserved:localStorage.getItem('amux_draft_other-worker') === sentinel,
      newText:queue[1] ? JSON.parse(queue[1].options.body).text : null,
      newId:queue[1] ? JSON.parse(queue[1].options.body).msg_id : null};
  });
  expect(result.cacheBytes).toBeGreaterThan(1000000);
  expect(result.accepted).toBe('queued');
  expect(result.queueLength).toBe(2);
  expect(result.oldPreserved).toBe(true);
  expect(result.draftPreserved).toBe(true);
  expect(result.newText).toContain('large mobile message '.repeat(3500));
  expect(result.newId).toBeTruthy();
});
