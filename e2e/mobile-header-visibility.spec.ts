import {test,expect} from './fixtures';

test('loaded fleet header controls stay visible and operable at phone widths',async({page},info)=>{
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:Array.from({length:52},(_,i)=>({
    name:'header-fixture-'+i,running:true,status:'working',dir:'/workspace',provider:'codex',
    rate_limited_until:i<18?Date.now()/1000+3600:null,
  }))}));
  await page.goto('/');
  await expect(page.locator('#active-count')).toHaveText('52');
  await expect(page.locator('#active-btn')).toBeHidden();
  for(const width of [320,375,402,480,481,600]){
    await page.setViewportSize({width,height:800});
    await expect(page.locator('#rate-limit-pill-count')).toHaveText('18');
    await expect(page.locator('#rate-limit-pill-count')).toBeVisible();
    expect(await page.locator('#brand-name-header').evaluate(e=>getComputedStyle(e,'::after').content)).toBe('"a"');
    expect(await page.locator('#conn-status').evaluate(e=>getComputedStyle(e).fontSize)).toBe('0px');
    await expect.poll(()=>page.evaluate(()=>(window as any)._headerLayoutCheck())).toEqual([]);
    for(const id of ['brand-header','conn-status','notif-btn','rate-limit-pill','add-btn','settings-btn']){
      const box=await page.locator('#'+id).boundingBox();
      expect(box).not.toBeNull();expect(box!.width).toBeGreaterThanOrEqual(44);expect(box!.height).toBeGreaterThanOrEqual(44);
      expect(box!.x).toBeGreaterThanOrEqual(0);expect(box!.x+box!.width).toBeLessThanOrEqual(width);
      expect(await page.locator('#'+id).evaluate(e=>{const r=e.getBoundingClientRect();return e.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));})).toBe(true);
    }
    expect(await page.locator('.header-row').evaluate(e=>e.getBoundingClientRect().height)).toBeLessThanOrEqual(57);
    const tops=await page.locator('#brand-header,#conn-status,#notif-btn,#rate-limit-pill,#add-btn,#settings-btn').evaluateAll(els=>els.map(e=>e.getBoundingClientRect().top));
    expect(Math.max(...tops)-Math.min(...tops)).toBeLessThanOrEqual(1);
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

test('header badge, controls and text tabs fit both themes from phone through desktop',async({page},info)=>{
 // Four text tabs fit at 375px; icon-plus-label is intentionally horizontally scrollable.
 // Pin both the initial cache and async preference so live owner settings cannot race this assertion.
 await page.addInitScript(()=>{localStorage.setItem('amux_walkthrough_done','1');localStorage.setItem('amux_tabs_display','text');});
 await page.route(/\/api\/prefs\?key=tabs_display$/,r=>r.fulfill({json:{value:'text'}}));
 await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:Array.from({length:52},(_,i)=>({name:'header-test-'+i,running:true,dir:'/workspace',provider:'codex',rate_limited_until:i<23?Date.now()/1000+3600:null}))}));
 await page.goto('/');await expect(page.locator('#active-count')).toHaveText('52');
  await expect(page.locator('#active-btn')).toBeHidden();
 for(const light of [true,false])for(const width of [375,600,601,768,1440]){
  await page.setViewportSize({width,height:900});
  await page.evaluate(light=>{document.body.classList.toggle('light',light);const badge=document.querySelector<HTMLElement>('#notif-badge')!;badge.textContent='99+';badge.style.display='flex';},light);
  // The app's own badge refresh (_notifUpdateBadge, once Needs You loads) sets
  // the real count and hides a zero badge, which collapsed its box and failed the
  // fit check for the whole poll (ios-safari CI f2bb59c75). Each poll re-applies
  // the widest badge before it measures.
  const widest=()=>{const badge=document.querySelector<HTMLElement>('#notif-badge')!;badge.textContent='99+';badge.style.display='flex';};
  await expect.poll(()=>page.evaluate(`(${widest})(),_headerLayoutCheck()`)).toEqual([]);
  await expect.poll(()=>page.evaluate(`(${widest})(),(()=>{const r=document.querySelector('#notif-btn').getBoundingClientRect(),b=document.querySelector('#notif-badge').getBoundingClientRect();return b.left>=r.left-0.1&&b.right<=r.right+0.1;})()`)).toBe(true);
  if(width===375){
   // The FOURTH visible tab, whichever it is: the strip's order changes over
   // time (the Projects tab led it from cbf4d384 until it was removed).
   const fit=await page.evaluate(()=>{const strip=document.querySelector('.tab-bar')!.getBoundingClientRect();const tabs=[...document.querySelectorAll<HTMLElement>('.tab-bar [id^="tab-"]')].filter(t=>t.offsetParent!==null&&getComputedStyle(t).display!=='none');const t=tabs[3];return t?{id:t.id,right:t.getBoundingClientRect().right,limit:strip.right}:null;});
   expect(fit,'at least four tabs are visible').not.toBeNull();
   expect(fit!.right,`${fit!.id} must fit inside the strip`).toBeLessThanOrEqual(fit!.limit);
  }
  if(width===375||width===1440)await page.screenshot({path:info.outputPath('header-'+width+'-'+(light?'light':'dark')+'.png'),clip:{x:0,y:0,width,height:200}});
 }
 // A badge escaping its button must reach the diagnostic on desktop too.
 const beacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.clipped?.includes('notif-badge'));
 await page.locator('#notif-badge').evaluate(e=>{e.style.right='-25px';});
 await page.setViewportSize({width:1399,height:900});
 const data=(await beacon).postDataJSON();expect(data.measured).toBe(true);expect(data.n_considered).toBeGreaterThan(0);expect(data.surface).toBe('desktop');
});

// A second row is a fit regression even when no element overflows the page.
test('wrapped mobile header self-announces in client diagnostics',async({page})=>{
 await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
 await page.goto('/');
 await page.setViewportSize({width:375,height:800});
 await expect(page.locator('#settings-btn')).toBeVisible();
 await expect.poll(()=>page.evaluate(()=>(window as any)._headerLayoutCheck())).toEqual([]);
 const beacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.clipped?.includes('header-row-wrapped'));
 await page.locator('.header-row').evaluate(e=>{e.style.width='200px';e.style.flexWrap='wrap';});
 const data=(await beacon).postDataJSON();
 expect(data.measured).toBe(true);expect(data.n_considered).toBeGreaterThanOrEqual(5);expect(data.surface).toBe('mobile');
});
