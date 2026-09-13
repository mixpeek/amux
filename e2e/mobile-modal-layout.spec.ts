import {test,expect} from './fixtures';

test.beforeEach(async({page})=>{
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:Array.from({length:52},(_,i)=>({
    name:'modal-fixture-'+i,running:false,dir:'/workspace',provider:'codex',
    rate_limited_until:i<30?Date.now()/1000+3600:null,rate_limit_banner:i<30,credit_limited:i>=30,
  }))}));
  await page.goto('/');await expect(page.locator('#rate-limit-pill-count')).not.toHaveText('0');
});

test('long dialogs keep their actions visible and content scrollable at phone and keyboard heights',async({page},info)=>{
 for(const size of [{width:375,height:667},{width:320,height:568},{width:402,height:350},{width:667,height:375}]){
  await page.setViewportSize(size);
  for(const kind of ['limited','confirm','alert']){
   await page.evaluate(kind=>{
    if(kind==='limited')(window as any).openBulkActions();
    else if(kind==='confirm')(window as any).showConfirm('Long confirmation with evidence. '.repeat(100));
    else (window as any).showAlert('Long alert with details. '.repeat(100));
   },kind);
   const root=page.locator(kind==='limited'?'#bulk-actions-overlay':'#modal-backdrop');
   const close=root.locator(kind==='limited'?'button[onclick="closeBulkActions()"]':kind==='confirm'?'button[onclick="_modalClose(false)"]':'button');
   await expect.poll(()=>page.evaluate(()=>(window as any)._modalLayoutCheck().clipped)).toEqual([]);
   const box=(await close.boundingBox())!;expect(box.y).toBeGreaterThanOrEqual(0);expect(box.y+box.height).toBeLessThanOrEqual(size.height);
   const body=root.locator(kind==='limited'?'#bulk-actions-body':'#modal-msg');
   expect(await body.evaluate(e=>e.scrollHeight>e.clientHeight)).toBe(true);
   const motion=await body.evaluate(e=>{e.scrollTop=e.scrollHeight;const end=e.scrollTop;e.scrollTop=end-70;return {end,up:e.scrollTop};});
   expect(motion.up).toBeLessThan(motion.end);
   if(size.width===375)await page.screenshot({path:info.outputPath(kind+'-mobile.png')});
   await close.click();await expect(root).not.toBeVisible();
  }
 }
});

test('orchestrator and connection history have visible close controls; proxy opens interactively',async({page})=>{
 await page.setViewportSize({width:375,height:667});
 for(const [open,root,close] of [
  ['_orchOpen()','#orch-overlay','button[aria-label="Close orchestrator"]'],
  ['showConnHistory()','#conn-hist-modal','#conn-modal-close'],
  ["switchView('proxies');_proxyOpenForm()",'#proxy-form-overlay','button[onclick="_proxyCloseForm()"]'],
 ]){
  await page.evaluate(code=>(0,eval)(code),open);
  const control=page.locator(root).locator(close);await expect(control).toBeVisible();
  expect(await control.evaluate(e=>{const r=e.getBoundingClientRect();return e.contains(document.elementFromPoint(r.x+r.width/2,r.y+r.height/2));})).toBe(true);
  await control.click();
  await expect.poll(()=>page.evaluate(sel=>{const e=document.querySelector(sel);return e?getComputedStyle(e).display:'none';},root)).toBe('none');
 }
});

test('vocabulary correction fits and a clipped dialog emits a measurable diagnostic',async({page})=>{
 await page.setViewportSize({width:375,height:667});
 await page.evaluate(()=>{(window as any)._dictAddWord({id:1,word:'amuks',correct:'amux'});});
 for(const id of ['dictm-mis','dictm-cor']){const b=(await page.locator('#'+id).boundingBox())!;expect(b.x).toBeGreaterThanOrEqual(0);expect(b.x+b.width).toBeLessThanOrEqual(375);}
 const beacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.kind==='modal-layout-clipped');
 await page.locator('#modal-backdrop .modal-box').evaluate(e=>{e.style.setProperty('min-height','1000px');});
 await page.evaluate(()=>window.dispatchEvent(new Event('resize')));
 const data=(await beacon).postDataJSON();expect(data.measured).toBe(true);expect(data.n_considered).toBeGreaterThan(0);expect(data.clipped).toContain('modal-backdrop');
});

test('scope discard cancellation preserves edits and explicit discard closes both dialogs',async({page})=>{
 await page.route('**/api/scope?**',r=>r.fulfill({json:{capabilities:[{key:'memory',supported:true,value:{text:'Original memory'}}]}}));
 await page.evaluate(()=>{(window as any)._scopeEditOpen('worker','modal-fixture-1','memory');});
 const editor=page.locator('#scope-edit-input');await expect(editor).toHaveValue('Original memory');await editor.fill('Unsaved revision');
 await page.locator('#scope-edit-backdrop button[onclick="_scopeEditClose()"]').click();
 await expect(page.locator('#modal-backdrop')).toBeVisible();await page.locator('#modal-btns button[onclick="_modalClose(false)"]').click();
 await expect(editor).toHaveValue('Unsaved revision');await expect(page.locator('#scope-edit-backdrop')).toBeVisible();
 await page.locator('#scope-edit-backdrop button[onclick="_scopeEditClose()"]').click();await page.locator('#modal-btns button[onclick="_modalClose(true)"]').click();
 await expect(page.locator('#scope-edit-backdrop')).not.toBeVisible();
});

test('calendar subscription remains dismissible while status hangs and reports the timeout',async({page})=>{
 const held: import('@playwright/test').Route[]=[];
 await page.route('**/api/tunnel/status',r=>{held.push(r);});
 const beacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.kind==='tunnel-status-unavailable');
 await page.evaluate(()=>{(window as any).showIcalInfo();});
 const close=page.locator('[data-ical-modal] button[onclick*="remove()"]');await expect(close).toBeVisible();
 const data=(await beacon).postDataJSON();expect(data.reason).toBe('timeout');expect(data.measured).toBe(true);expect(data.n_considered).toBe(1);
 await expect(page.locator('[data-ical-box]')).toContainText('Tunnel status is unavailable');
 await close.click();await expect(page.locator('[data-ical-modal]')).toHaveCount(0);
 for(const r of held)await r.abort().catch(()=>{});
});

test('dialog surfaces are opaque and readable in both themes and report theme regressions',async({page})=>{
 for(const light of [true,false]){
  await page.evaluate(light=>document.body.classList.toggle('light',light),light);
  for(const [open,root] of [
   ["_connScopePicker('github','worker',true)",'.conn-picker-overlay'],
   ['_jrnlShowConfig()','#jrnl-config-overlay'],
   ["_teamScopeDialog('Member scope','test@example.invalid','global','','Save')",'#team-scope-modal'],
   ["_workspaceAccessModal('<h3>Invite</h3>')",'.amux-workspace-dialog'],
   ["_upgradeModalShown=false;_showUpgradeModal({error:'trial_expired'})",'#upgrade-modal'],
  ]){
   await page.evaluate(code=>{(0,eval)(code);},open);
   await expect.poll(()=>page.evaluate(()=>(window as any)._modalLayoutCheck().clipped)).toEqual([]);
   await page.locator(root).evaluate(e=>e.remove());
  }
 }
 await page.evaluate(()=>{(window as any)._jrnlShowConfig();});
 const beacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.clipped?.includes('jrnl-config-overlay:transparent'));
 await page.locator('#jrnl-config-overlay > div').evaluate(e=>{e.style.background='transparent';});await page.evaluate(()=>window.dispatchEvent(new Event('resize')));
 expect((await beacon).postDataJSON().measured).toBe(true);
});

test('video close remains visible when playback controls hide',async({page})=>{
 await page.evaluate(()=>{document.querySelector<HTMLElement>('#video-overlay')!.style.display='flex';document.querySelector<HTMLElement>('#vp-controls')!.style.opacity='0';});
 const close=page.getByRole('button',{name:'Close video',exact:true});
 await expect(close).toBeVisible();const b=(await close.boundingBox())!;expect(b.height).toBeGreaterThanOrEqual(44);
 await close.click();await expect(page.locator('#video-overlay')).not.toBeVisible();
});
