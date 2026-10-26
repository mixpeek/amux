import {test,expect} from './fixtures';

test('production component guide previews every theme and restores focus and theme',async({page},info)=>{
 test.setTimeout(120_000);   // every control's tap size, under each of the built-in themes
 await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
 await page.goto('/');await page.evaluate(()=>(window as any).themeActivate('amux-dark',{local:true}));
 await page.locator('#settings-btn').click();await page.locator('.settings-tab-btn[data-stab="device"]').click();await page.locator('#style-guide-btn').click();
 const guide=page.locator('#ui-guide-overlay');await expect(guide).toBeVisible();
 await expect(guide.getByRole('dialog')).toHaveAttribute('aria-modal','true');
 const first=guide.getByRole('button',{name:'Close style guide'}),last=guide.locator('.modal-footer button');
 await first.focus();await page.keyboard.press('Shift+Tab');await expect(last).toBeFocused();
 await page.keyboard.press('Tab');await expect(first).toBeFocused();
 await guide.getByRole('button',{name:'Primary action',exact:true}).click();
 await expect(guide.locator('#ui-guide-feedback')).toContainText('completed. Example only');
 await guide.getByLabel('Name',{exact:false}).first().fill('Unsubmitted guide draft');
 await expect(guide.getByRole('button',{name:'Unavailable',exact:true})).toBeDisabled();
 await guide.getByRole('button',{name:'Try confirmation dialog'}).click();
 const covered=await page.locator('#modal-backdrop').evaluate(e=>{const previous=e.style.zIndex;e.style.zIndex='1';const result=(window as any)._modalLayoutCheck();e.style.zIndex=previous;return result;});
 expect(covered.clipped).toContain('modal-backdrop:covered-actions');
 await page.locator('#modal-btns button').filter({hasText:'Cancel'}).click();
 await expect(guide.locator('#ui-guide-feedback')).toHaveText('Example cancelled.');
 // Every built-in theme, through the theme engine the guide's own button uses.
 const themes:string[]=await page.evaluate(()=>(window as any).themeAll().filter((t:any)=>t.builtin).map((t:any)=>t.id));
 expect(themes.length).toBeGreaterThanOrEqual(9);
 for(const theme of themes){
  await page.evaluate(id=>(window as any).themePreview((window as any).themeById(id)),theme);
  const light=await page.evaluate(()=>document.body.classList.contains('light'));
  await expect.poll(()=>page.evaluate(()=>(window as any)._uiComponentCheck(document.querySelector('#ui-guide-overlay')))).toMatchObject({measured:true,issues:[]});
  await expect.poll(()=>page.evaluate(()=>(window as any)._modalLayoutCheck().clipped)).toEqual([]);
  expect(await guide.locator('h3').evaluate(e=>{const r=e.getBoundingClientRect();return e.contains(document.elementFromPoint(r.left+r.width/2,r.top+2));})).toBe(true);
  const shape=await guide.locator('.modal').boundingBox();expect(shape!.x).toBeGreaterThanOrEqual(0);expect(shape!.x+shape!.width).toBeLessThanOrEqual(page.viewportSize()!.width);
  if(page.viewportSize()!.width<=600){
   for(const control of await guide.locator('.btn,.input,.modal-close').all())expect((await control.boundingBox())!.height,await control.evaluate(e=>e.tagName+'#'+e.id)).toBeGreaterThanOrEqual(44);
   expect(await guide.locator('.input').first().evaluate(e=>getComputedStyle(e).fontSize)).toBe('16px');
  }
  await guide.locator('.modal-body').evaluate(e=>{e.scrollTop=0;});await page.screenshot({path:info.outputPath('guide-'+theme+(light?'-light':'')+'.png')});
 }
 await guide.locator('.modal-body').evaluate(e=>{e.scrollTop=e.scrollHeight;});
 await last.click();await expect(guide).toHaveCount(0);await expect(page.locator('#settings-btn')).toBeFocused();
 expect(await page.evaluate(()=>document.body.classList.contains('light'))).toBe(false);
 expect(await page.evaluate(()=>document.body.dataset.theme)).toBe('amux-dark');
 await page.locator('#settings-btn').click();await page.locator('.settings-tab-btn[data-stab="device"]').click();await page.locator('#style-guide-btn').click();await page.keyboard.press('Escape');await expect(guide).toHaveCount(0);await expect(page.locator('#settings-btn')).toBeFocused();
});

test('a broken primary color produces a measured component drift beacon',async({page})=>{
 await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
 await page.goto('/');await page.evaluate(()=>{document.body.classList.remove('light');(window as any).openStyleGuide();});
 await expect(page.locator('#ui-guide-overlay')).toBeVisible();
 const request=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.kind==='ui-component-drift');
 await page.setViewportSize({width:375,height:800});
 await page.locator('#ui-guide-primary').evaluate(e=>{e.style.color=getComputedStyle(e).backgroundColor;e.style.minHeight='20px';e.style.height='20px';e.style.padding='0';window.dispatchEvent(new Event('resize'));});
 const data=(await request).postDataJSON();expect(data.measured).toBe(true);expect(data.n_considered).toBeGreaterThan(0);expect(data.issues).toContain('ui-guide-primary:low-contrast');expect(data.issues).toContain('ui-guide-primary:small-control');
});
