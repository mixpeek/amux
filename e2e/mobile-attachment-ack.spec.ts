import {test, expect} from './fixtures';
declare const _idb: any;
for (const status of [200, 503]) {
  test(`mobile attachment is released after local message acceptance with HTTP ${status}`, async ({page}, info) => {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[{name:'mobile-ack',running:true,status:'idle',dir:'/tmp'}]}));
    await page.route(/\/api\/sessions\/mobile-ack\/peek\?/, r => r.fulfill({json:{name:'mobile-ack',live:'Inspect attached evidence',history:''}}));
    await page.route('**/api/sessions/mobile-ack/subagents', r => r.fulfill({json:{session:'mobile-ack',subagents:[]}}));
    const messages: any[] = [];
    await page.route('**/api/sessions/mobile-ack/send', r => {
      messages.push(r.request().postDataJSON());
      return r.fulfill({status,json:status===200 ? {ok:true,submitted:true} : {error:'temporary outage'}});
    });
    await page.goto('/');
    await page.evaluate(() => (window as any).openPeek('mobile-ack'));
    await page.locator('#peek-cmd-input').fill('Review this evidence');
    await page.locator('#peek-file-input').setInputFiles({name:'accepted-evidence.txt',mimeType:'text/plain',buffer:Buffer.from('retained server artifact')});
    await expect(page.locator('#peek-attach-bar')).toContainText('✓');
    const stored = await page.evaluate(async () => (await _idb.getUploads())[0]);
    expect(stored.path).toBeTruthy();
    await page.locator('#peek-overlay .send-split-main').click();
    await expect(page.locator('#peek-cmd-input')).toHaveValue('');
    await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(0);
    await expect.poll(() => page.evaluate(async () => (await _idb.getUploads()).length)).toBe(0);
    await expect.poll(() => messages.length).toBeGreaterThan(0);
    expect(JSON.stringify(messages[0])).toContain('@'+stored.path);
    await page.reload();
    await page.evaluate(() => (window as any).openPeek('mobile-ack'));
    await expect(page.locator('#peek-attach-bar .peek-attach-chip')).toHaveCount(0);
    await page.screenshot({path:info.outputPath('acknowledged-attachment-cleared.png')});
  });
}

test('mobile Workers deep link dismisses an open terminal', async ({page}) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[{name:'mobile-route',running:true,status:'idle',dir:'/tmp'}]}));
  await page.route(/\/api\/sessions\/mobile-route\/peek\?/, r => r.fulfill({json:{name:'mobile-route',live:'Return to Workers',history:''}}));
  await page.route('**/api/sessions/mobile-route/subagents', r => r.fulfill({json:{session:'mobile-route',subagents:[]}}));
  await page.goto('/');
  await page.evaluate(() => (window as any).openPeek('mobile-route'));
  await expect(page.locator('#peek-overlay')).toHaveClass(/active/);
  await page.goto('/#view=sessions');
  await expect(page.locator('#peek-overlay')).not.toHaveClass(/active/);
  await expect(page.locator('#tab-sessions')).toHaveClass(/active/);
});
