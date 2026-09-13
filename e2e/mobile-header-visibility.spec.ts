import {test,expect} from './fixtures';

test('loaded fleet header controls stay visible and operable at phone widths',async({page},info)=>{
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:Array.from({length:52},(_,i)=>({
    name:'header-fixture-'+i,running:true,status:'working',dir:'/workspace',provider:'codex',
    rate_limited_until:i<18?Date.now()/1000+3600:null,
  }))}));
  await page.goto('/');
  await expect(page.locator('#active-count')).toHaveText('52');
  for(const width of [320,375,402]){
    await page.setViewportSize({width,height:800});
    await expect(page.locator('#rate-limit-pill-count')).toHaveText('18');
    await expect(page.locator('#rate-limit-pill-count')).toBeVisible();
    expect(await page.locator('#brand-name-header').evaluate(e=>getComputedStyle(e,'::after').content)).toBe('"a"');
    expect(await page.locator('#conn-status').evaluate(e=>getComputedStyle(e).fontSize)).toBe('0px');
    await expect.poll(()=>page.evaluate(()=>(window as any)._headerLayoutCheck())).toEqual([]);
    for(const id of ['brand-header','conn-status','notif-btn','rate-limit-pill','add-btn','settings-btn','active-btn']){
      const box=await page.locator('#'+id).boundingBox();
      expect(box).not.toBeNull();expect(box!.width).toBeGreaterThanOrEqual(44);expect(box!.height).toBeGreaterThanOrEqual(44);
      expect(box!.x).toBeGreaterThanOrEqual(0);expect(box!.x+box!.width).toBeLessThanOrEqual(width);
    }
    if(width>=375)expect(await page.locator('.header-row').evaluate(e=>e.getBoundingClientRect().height)).toBeLessThanOrEqual(64);
    await page.locator('#settings-btn').click();await expect(page.locator('#settings-menu')).toBeVisible();
    expect(await page.locator('#settings-menu').evaluate(e=>e.getBoundingClientRect().top)).toBeGreaterThanOrEqual((await page.locator('#settings-btn').boundingBox())!.y+44);
    await page.screenshot({path:info.outputPath('header-settings-'+width+'.png')});
    await page.locator('#settings-btn').click();
    await page.locator('#add-btn').click();await expect(page.locator('#add-menu')).toBeVisible();
    await page.locator('#add-btn').click();
  }
  // Positive control: total page width alone cannot detect ancestor clipping.
  expect(await page.evaluate(()=>{
    const parent=document.querySelector('#settings-btn')!.parentElement!.parentElement!;
    parent.style.cssText='display:flex!important;width:10px;overflow:hidden;flex-wrap:nowrap';
    return (window as any)._headerLayoutCheck().includes('settings-btn');
  })).toBe(true);
});

test('header badge and controls fit both themes from phone through desktop',async({page},info)=>{
 await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
 await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:Array.from({length:52},(_,i)=>({name:'header-test-'+i,running:true,dir:'/workspace',provider:'codex',rate_limited_until:i<23?Date.now()/1000+3600:null}))}));
 await page.goto('/');await expect(page.locator('#active-count')).toHaveText('52');
 for(const light of [true,false])for(const width of [375,600,601,768,1440]){
  await page.setViewportSize({width,height:900});
  await page.evaluate(light=>{document.body.classList.toggle('light',light);const badge=document.querySelector<HTMLElement>('#notif-badge')!;badge.textContent='99+';badge.style.display='flex';},light);
  await expect.poll(()=>page.evaluate(()=>(window as any)._headerLayoutCheck())).toEqual([]);
  await expect.poll(()=>page.evaluate(()=>{const r=document.querySelector('#notif-btn')!.getBoundingClientRect(),b=document.querySelector('#notif-badge')!.getBoundingClientRect();return b.left>=r.left-0.1&&b.right<=r.right+0.1;})).toBe(true);
  if(width===375){
   const tab=await page.locator('#tab-calendar').boundingBox(),strip=await page.locator('.tab-bar').boundingBox();
   expect(tab!.x+tab!.width).toBeLessThanOrEqual(strip!.x+strip!.width);
  }
  if(width===375||width===1440)await page.screenshot({path:info.outputPath('header-'+width+'-'+(light?'light':'dark')+'.png'),clip:{x:0,y:0,width,height:200}});
 }
 // A badge escaping its button must reach the diagnostic on desktop too.
 const beacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.clipped?.includes('notif-badge'));
 await page.locator('#notif-badge').evaluate(e=>{e.style.right='-25px';});
 await page.setViewportSize({width:1399,height:900});
 const data=(await beacon).postDataJSON();expect(data.measured).toBe(true);expect(data.n_considered).toBeGreaterThan(0);expect(data.surface).toBe('desktop');
});
