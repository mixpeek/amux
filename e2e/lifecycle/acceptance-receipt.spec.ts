import { test, expect } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers } from './evidence';

test('LC-RECEIPT: accepted native message clears pending before the original HTTP response', async ({page, request}, info) => {
  await boot(page);const headers=await auth(page);
  const name=`lc-receipt-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions',{headers,data:{name,dir:'/tmp'}})).status()).toBe(201);
  let release!:()=>void;const held=new Promise<void>(r=>release=r);
  let payload:any;let posts=0;let confirmed=false;
  await page.route(`**/api/sessions/${name}/send`,async r=>{posts++;payload=r.request().postDataJSON();await held;await r.fulfill({json:{ok:true,submitted:true}});});
  await page.route(`**/api/sessions/${name}/send?msg_id=*`,async r=>{
    const id=new URL(r.request().url()).searchParams.get('msg_id');
    await r.fulfill({status:confirmed?200:202,json:{ok:true,accepted:confirmed,msg_id:id,...(confirmed?{id:'durable-receipt'}:{})}});
  });
  const entries=()=>page.evaluate(()=>JSON.parse(localStorage.getItem('amux_offline_queue')||'[]'));
  try {
    await page.reload();
    await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await page.locator('#peek-cmd-input').fill('Receipt regression: already accepted by the worker');
    await page.locator('#peek-overlay .send-split-main').click();
    await expect(page.locator('#peek-cmd-input')).toHaveValue('');
    await expect.poll(()=>posts).toBe(1);
    await page.locator('#peek-tab-messages').click();
    await expect(page.locator('#peek-messages-list')).toContainText('Awaiting confirmation');
    await expect(page.locator('#peek-messages-list')).not.toContainText('not yet delivered');
    await expect(page.locator('#peek-messages-list button[title="Remove this unattempted local message"]')).toBeHidden();
    await checkpoint(page,info,'receipt-pending-truthful');
    confirmed=true;
    await expect.poll(async()=> (await entries()).length).toBe(0);
    await expect(page.locator('#peek-messages-list')).not.toContainText('Awaiting confirmation');
    expect(posts).toBe(1);expect(payload.msg_id).toBeTruthy();
    await checkpoint(page,info,'receipt-confirmed-before-post-finished');
    release();
    await page.reload();expect(await entries()).toHaveLength(0);
  } finally {release();await deleteOwnedWorkers(page,request,headers,[name]);}
});
