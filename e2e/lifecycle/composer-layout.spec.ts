import { test, expect } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers } from './evidence';

test('LC-COMPOSER-LAYOUT: long drafts retain compact 44px send controls', async ({page,request},info) => {
  await boot(page); const headers=await auth(page);
  const name=`lc-compose-layout-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions',{headers,data:{name,dir:'/tmp'}})).status()).toBe(201);
  try {
    await page.reload();
    const card=page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
    await card.locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await page.locator('#peek-cmd-input').fill('A long draft with task links, output files, and acceptance evidence.\n'.repeat(12));
    const dimensions=await page.locator('#peek-overlay .send-split').evaluate(el=>({
      height:el.getBoundingClientRect().height,
      buttons:[...el.querySelectorAll('button')].map(b=>({width:b.getBoundingClientRect().width,height:b.getBoundingClientRect().height})),
    }));
    expect(dimensions.height).toBe(44);
    for(const button of dimensions.buttons) {expect(button.width).toBeGreaterThanOrEqual(44);expect(button.height).toBe(44);}
    await info.attach('composer-control-dimensions',{body:JSON.stringify(dimensions),contentType:'application/json'});
    await checkpoint(page,info,'compact-send-with-long-draft');
    await page.locator('#peek-cmd-input').fill('');
  } finally { await deleteOwnedWorkers(page,request,headers,[name]); }
});
