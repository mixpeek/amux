import {test,expect} from '../fixtures';
import {boot,auth,deleteOwnedWorkers} from './evidence';

test('LC-WORKER-MENU: unrelated panel scrolling preserves actions; anchor scrolling dismisses them',async({page,request},info)=>{
  await boot(page);const headers=await auth(page);
  const name=`lc-menu-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions',{headers,data:{name,dir:'/tmp'}})).status()).toBe(201);
  try {
    await page.goto('/#view=sessions');
    await page.reload();
    const card=page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
    await card.locator('.card-menu-btn').click();
    const action=page.locator('.card-menu.open [data-worker-action="delete"]');
    await expect(action).toBeVisible();
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    await page.locator('#peek-body').dispatchEvent('scroll');
    await expect(action).toBeVisible();
    await page.locator('.card-menu.open').dispatchEvent('scroll');
    await expect(action).toBeVisible();
    await page.evaluate(()=>document.dispatchEvent(new Event('scroll')));
    await expect(action).toHaveCount(0);
  } finally {await deleteOwnedWorkers(page,request,headers,[name]);}
});
