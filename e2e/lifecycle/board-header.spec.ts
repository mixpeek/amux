import {test,expect} from '../fixtures';
import {boot,checkpoint} from './evidence';
test('LC-BOARD-HEADER: Verified and its terminal badge remain readable on a phone',async({page},info)=>{
 await page.setViewportSize({width:375,height:812});await boot(page);await page.locator('#tab-board').click();
 const col=page.locator('#board-view .board-col[data-col="verified"]');await col.scrollIntoViewIfNeeded();
 if(await col.evaluate(el=>el.classList.contains('col-collapsed')))await col.locator('.board-col-collapse').click();
 const label=col.locator('.board-col-label'),badge=col.locator('.col-terminal-chip');
 await expect(label).toHaveText(/verified/i);await expect(badge).toHaveText('terminal');
 for(const item of [label,badge]) {
  await expect(item).toBeVisible();expect(await item.evaluate(el=>el.scrollWidth<=el.clientWidth+1&&el.scrollHeight<=el.clientHeight+1)).toBe(true);
 }
 const bounds=await col.locator('.board-col-header').boundingBox();
 for(const item of [label,badge,col.locator('.col-gate-btn'),col.locator('.col-more-btn')]) {
  const b=await item.boundingBox();expect(b!.x).toBeGreaterThanOrEqual(bounds!.x-1);expect(b!.x+b!.width).toBeLessThanOrEqual(bounds!.x+bounds!.width+1);
 }
 await checkpoint(page,info,'verified-header-phone');
});
