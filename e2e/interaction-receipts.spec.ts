import { test, expect, Page } from './fixtures';

async function boot(page: Page): Promise<void> {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).__amuxState?.interactions?.recent === 'function');
}

async function seedReviewReceipts(page: Page, ids: string[], phase = 'applied'): Promise<void> {
  await page.addInitScript(({ids,phase}) => {
    if(sessionStorage.getItem('receipt-review-seeded'))return;
    sessionStorage.setItem('receipt-review-seeded','1');
    localStorage.setItem('amux_interactions_v2',JSON.stringify(ids.map(id=>({
      id,command:{id:'cmd_'+id,kind:'environment.post',target:{primitive:'environment',id:'prefs'}},
      request:{method:'POST',path:'/api/prefs'},phase,measured:true,n_considered:1,
      acknowledgement:{status:200,applied:true},effects:[],
      feedback:{required:true,message:phase==='applied'?'Completed':'Sending'},
      created_at:Date.now(),updated_at:Date.now(),
    }))));
  },{ids,phase});
}

test('review: completed command recovers effects after a failed read and reload without replay', async ({page},testInfo) => {
  const id='int_review_effect_retry';let probes=0;let mutations=0;
  page.on('request',request=>{if(request.method()==='POST' && new URL(request.url()).pathname==='/api/prefs')mutations++;});
  await page.route(`**/api/interactions/${id}/effects`,route=>{
    probes++;
    return probes===1 ? route.fulfill({status:503,json:{error:'Unavailable'}})
      : route.fulfill({json:{measured:true,n_considered:1,effects:[{id:'recovered',kind:'pref.updated'}]}});
  });
  await seedReviewReceipts(page,[id]);await boot(page);
  await expect.poll(()=>page.evaluate(id=>(window as any).__amuxInteractions.get(id)?.effect_sync?.phase,id),{timeout:15000}).toBe('failed');
  await page.locator('#interaction-feedback > summary').click();
  await expect(page.locator(`article[data-interaction-id="${id}"] .interaction-effects-status`)).toHaveText('Changes unavailable; retrying');
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  await page.screenshot({path:testInfo.outputPath('effect-retry.png')});
  await page.reload();
  await expect.poll(()=>page.evaluate(id=>(window as any).__amuxInteractions?.get(id)?.effect_sync?.phase,id),{timeout:15000}).toBe('synced');
  const receipt=await page.evaluate(id=>(window as any).__amuxInteractions.get(id),id);
  expect(receipt.phase).toBe('applied');expect(receipt.effects.map((e:any)=>e.id)).toEqual(['recovered']);
  expect(mutations).toBe(0);expect(probes).toBe(2);
});

test('review: an effects body that never finishes times out and another receipt still progresses', async ({page}) => {
  const stuck='int_review_hanging_body';const healthy='int_review_healthy';
  let timeoutBeacon=false;
  page.on('request',request=>{
    if(!request.url().includes('/api/client-debug'))return;
    const body=request.postDataJSON();
    if(body?.verdict==='interaction_reconcile_failed' && body?.interaction_id===stuck)timeoutBeacon=true;
  });
  await page.addInitScript(stuck=>{
    const original=window.fetch;
    window.fetch=(input,init)=>{
      if(String(input).endsWith('/api/interactions/'+stuck+'/effects')) {
        return Promise.resolve(new Response(new ReadableStream({start(controller){
          init?.signal?.addEventListener('abort',()=>controller.error(new Error('Aborted test response body')),{once:true});
        }}),{headers:{'Content-Type':'application/json'}}));
      }
      return original(input,init);
    };
  },stuck);
  await page.route(`**/api/interactions/${healthy}/effects`,route=>route.fulfill({json:{
    measured:true,n_considered:1,effects:[{id:'healthy-effect',kind:'pref.updated'}],
  }}));
  await seedReviewReceipts(page,[stuck,healthy]);await boot(page);
  await expect.poll(()=>page.evaluate(id=>(window as any).__amuxInteractions.get(id)?.effect_sync?.phase,stuck),{timeout:18000}).toBe('failed');
  await expect.poll(()=>timeoutBeacon).toBe(true);
  await expect.poll(()=>page.evaluate(id=>(window as any).__amuxInteractions.get(id)?.effects.length,healthy),{timeout:10000}).toBe(1);
});

test('review: a real replay acknowledgement removes the obsolete reload warning', async ({page}) => {
  const id='int_review_replay_measurement';
  await seedReviewReceipts(page,[id],'sending');await boot(page);
  expect(await page.evaluate(id=>(window as any).__amuxInteractions.get(id)?.measured,id)).toBe(false);
  await page.evaluate(async id=>{
    const response=await fetch('/api/prefs',{method:'POST',headers:{'Content-Type':'application/json','X-Amux-Interaction-Id':id},
      body:JSON.stringify({key:'review-replay-measured',value:'1'})});
    if(!response.ok)throw new Error('Replay failed: '+response.status);
  },id);
  const receipt=await page.evaluate(id=>(window as any).__amuxInteractions.get(id),id);
  expect(receipt.phase).toBe('applied');expect(receipt.measured).toBe(true);expect(receipt.why_unmeasured).toBeUndefined();
  await page.locator('#interaction-feedback > summary').click();
  await expect(page.locator(`article[data-interaction-id="${id}"]`)).not.toContainText('Page reloaded before completion');
});

test('status polling reaches older receipts and server queues while newer commands remain running', async ({page}) => {
  await page.route('**/api/interactions/int_poll_**', async route => {
    const path=new URL(route.request().url()).pathname;
    if(path.endsWith('/effects')) return route.fulfill({json:{measured:true,n_considered:0,effects:[]}});
    const id=path.split('/').at(-1);
    return route.fulfill({json:{phase:id==='int_poll_0'?'applied':'running',measured:true}});
  });
  await page.addInitScript(() => {
    const receipts=Array.from({length:12},(_,n)=>({
      id:'int_poll_'+n,command:{id:'cmd_'+n,kind:'board.patch',target:{primitive:'board',id:'poll-'+n}},
      request:{method:'PATCH',path:'/api/board/poll-'+n},
      phase:n===0?'queued':'unknown',acknowledgement:{locally_queued:false},effects:[],
      feedback:{required:true,message:'Awaiting confirmation'},created_at:n,updated_at:Date.now(),
    }));
    localStorage.setItem('amux_interactions_v2',JSON.stringify(receipts));
  });
  await boot(page);
  await expect.poll(()=>page.evaluate(()=>(window as any).__amuxInteractions.get('int_poll_0')?.phase),
    {timeout:20000}).toBe('applied');
  await expect.poll(()=>page.evaluate(()=>(window as any).__amuxInteractions.get('int_poll_11')?.phase),
    {timeout:20000}).toBe('running');
  await page.locator('#interaction-feedback > summary').click();
  await expect(page.locator('article[data-interaction-id="int_poll_0"] .interaction-status')).toHaveText('Completed');
});

test('mutation fetch creates an inspectable interaction receipt and linked effect', async ({ page }) => {
  await boot(page);
  let interactionHeader = '';
  let commandHeader = '';
  await page.route('**/api/prefs', async route => {
    if (route.request().method() !== 'POST') return route.continue();
    const headers = route.request().headers();
    interactionHeader = headers['x-amux-interaction-id'] || '';
    commandHeader = headers['x-amux-command-kind'] || '';
    return route.fulfill({
      json: { ok: true, applied: true, rev: 123, entity_id: 'pref:receipt-smoke', version: 1,
        effects:[{id:'pref-effect', kind:'pref.updated', entity:{primitive:'environment',id:'pref:receipt-smoke'}, rev:123}] },
    });
  });

  const id = await page.evaluate(async () => {
    const response = await fetch('/api/prefs', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ key: 'receipt-smoke', value: '1' }),
    });
    await response.json();
    return (window as any).__amuxState.interactions.recent(1)[0].id;
  });

  expect(interactionHeader).toBe(id);
  expect(commandHeader).toBe('environment.post');
  await expect.poll(async () => page.evaluate((receiptId) => {
    const receipt = (window as any).__amuxState.interactions.get(receiptId);
    return {
      phase: receipt?.phase,
      command: receipt?.command?.kind,
      target: receipt?.command?.target?.primitive,
      status: receipt?.acknowledgement?.status,
      rev: receipt?.acknowledgement?.rev,
      effects: receipt?.effects?.length || 0,
      effectKind: receipt?.effects?.[0]?.kind || '',
    };
  }, id)).toEqual({
    phase: 'applied',
    command: 'environment.post',
    target: 'environment',
    status: 200,
    rev: 123,
    effects: 1,
    effectKind: 'pref.updated',
  });
});

test('a command control is immediately busy and its durable feedback survives reload', async ({ page }, testInfo) => {
  await boot(page);
  let release!: () => void;
  const pending = new Promise<void>(resolve => { release = resolve; });
  await page.route('**/api/prefs', async route => {
    if (route.request().method() !== 'POST') return route.continue();
    await pending;
    return route.fulfill({json:{ok:true,applied:true}});
  });
  await page.evaluate(() => {
    const button = document.createElement('button');
    button.id='receipt-test-save'; button.textContent='Save';
    button.onclick=() => { void fetch('/api/prefs',{method:'POST',headers:{'Content-Type':'application/json'},body:'{"key":"state-test","value":"1"}'}); };
    document.querySelector('.header-row')!.append(button);
  });
  const save=page.locator('#receipt-test-save');
  await save.click();
  await expect(save).toHaveAttribute('aria-busy','true');
  await expect(save).toHaveAttribute('data-interaction-kind','environment.post');
  const id=await save.getAttribute('data-interaction-id');
  await page.locator('#interaction-feedback > summary').click();
  const row=page.locator(`[data-interaction-id="${id}"]`).filter({has:page.locator('.interaction-status')});
  await expect(row.locator('progress')).toBeVisible();
  await expect(row.locator('.interaction-status')).toHaveText('Sending');
  expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
  await page.screenshot({path:testInfo.outputPath('action-pending.png')});
  release();
  await expect(save).toHaveAttribute('aria-busy','false');
  await expect(row.locator('.interaction-status')).toHaveText('Completed');
  await page.reload();
  await page.waitForFunction(id=>(window as any).__amuxInteractions?.get(id)?.phase==='applied',id);
  await page.locator('#interaction-feedback > summary').click();
  await expect(page.locator(`article[data-interaction-id="${id}"] .interaction-status`)).toHaveText('Completed');
});

test('nonqueueable command reports server failure and sends a diagnostic', async ({page})=>{
  await boot(page);
  let failed=false;
  await page.route('**/api/client-debug',async route=>{
    if(route.request().postDataJSON()?.verdict==='failed') failed=true;
    return route.fulfill({json:{ok:true}});
  });
  await page.route('**/api/files/mdai/run',route=>route.fulfill({status:500,json:{error:'Execution failed'}}));
  const receipt=await page.evaluate(async()=>{
    await fetch('/api/files/mdai/run',{method:'POST',body:'{}'});
    return (window as any).__amuxInteractions.recent(200).find((r:any)=>r.request.path==='/api/files/mdai/run');
  });
  expect(receipt.phase).toBe('failed');
  await expect.poll(()=>failed).toBe(true);
  await page.locator('#interaction-feedback > summary').click();
  await expect(page.locator(`article[data-interaction-id="${receipt.id}"]`)).toContainText('Execution failed');
});

test('409 and ignored fields retain the remedy instead of displaying success',async({page})=>{
  await boot(page);
  await page.route('**/api/prefs',route=>route.fulfill({status:409,json:{error:'Evidence required',fix:'Provide the command and result'}}));
  const result=await page.evaluate(async()=>{
    await fetch('/api/prefs',{method:'POST',body:'{}'});
    return (window as any).__amuxInteractions.recent(1)[0];
  });
  expect(result.phase).toBe('refused');
  expect(result.acknowledgement.remedy).toBe('Provide the command and result');
  await page.locator('#interaction-feedback > summary').click();
  await expect(page.locator(`article[data-interaction-id="${result.id}"]`)).toContainText('Evidence required');
});

test('offline replay keeps the original receipt id and settles after acknowledgement',async({page})=>{
  await boot(page);
  const id=await page.evaluate(async()=>{
    (window as any).setOnline(false);
    await fetch('/api/prefs',{method:'POST',headers:{'Content-Type':'application/json'},body:'{"key":"replay-state","value":"1"}'});
    return (window as any).__amuxInteractions.recent(1)[0].id;
  });
  await page.evaluate(async()=>{(window as any).setOnline(true);await (window as any).runSyncBanner(true);});
  await expect.poll(()=>page.evaluate(id=>(window as any).__amuxInteractions.get(id)?.phase,id)).toBe('applied');
  expect(await page.evaluate(id=>(window as any).__amuxInteractions.recent(200).filter((r:any)=>r.id===id).length,id)).toBe(1);
  const server=await page.evaluate(async id=>{
    const response=await fetch('/api/interactions/'+id);
    return {status:response.status,receipt:await response.json()};
  },id);
  expect(server.status).toBe(200);
  expect(server.receipt.id).toBe(id);
  expect(server.receipt.phase).toBe('applied');
});

test('Request and URL inputs receive correlation without losing response bodies',async({page})=>{
  await boot(page);
  const ids:string[]=[];
  await page.route('**/api/prefs',route=>{
    ids.push(route.request().headers()['x-amux-interaction-id']);
    return route.fulfill({json:{ok:true}});
  });
  const bodies=await page.evaluate(async()=>{
    const a=await fetch(new Request(location.origin+'/api/prefs',{method:'POST',body:'{}'}));
    const b=await fetch(new URL('/api/prefs',location.origin),{method:'POST',body:'{}'});
    return [await a.json(),await b.json()];
  });
  expect(ids.length).toBe(2); expect(ids.every(Boolean)).toBe(true);
  expect(bodies).toEqual([{ok:true},{ok:true}]);
});

test('reachable controls have feedback declarations and missing declarations fail the inventory',async({page},testInfo)=>{
  await boot(page);
  const coverage=await page.evaluate(()=>(window as any).__amuxState.coverage());
  expect(coverage.measured).toBe(true);
  expect(coverage.n_considered).toBeGreaterThan(10);
  expect(coverage.declared_command_controls).toBeGreaterThan(0);
  expect(coverage.missing).toEqual([]);
  await testInfo.attach('interaction-coverage',{body:JSON.stringify(coverage,null,2),contentType:'application/json'});
  await page.evaluate(()=>document.querySelector('[data-interaction-kind]')!.removeAttribute('data-feedback-required'));
  expect((await page.evaluate(()=>(window as any).__amuxState.coverage())).missing.length).toBeGreaterThan(0);
  await page.evaluate(()=>{
    const control=document.querySelector('[data-interaction-kind]')!;
    control.setAttribute('data-feedback-required','true');
    control.removeAttribute('data-interaction-kind');
  });
  expect((await page.evaluate(()=>(window as any).__amuxState.coverage())).missing.length).toBeGreaterThan(0);
});

test('correlation headers preserve real authentication through repeated wrappers',async({page})=>{
  await boot(page);
  const result=await page.evaluate(async()=>{
    const headers=(window as any)._authHeaders({'Content-Type':'application/json'});
    const response=await fetch('/api/prefs',{method:'POST',headers,body:'{"key":"receipt-auth","value":"ok"}'});
    return {status:response.status,queued:response.headers.get('X-Amux-Outbox'),authorizationFields:Object.keys(headers).filter(k=>k.toLowerCase()==='authorization').length};
  });
  expect(result.status).toBe(200);
  expect(result.queued).toBeNull();
  expect(result.authorizationFields).toBe(1);
});

test('offline mutation receipt resolves to queued, not applied', async ({ page }) => {
  await boot(page);
  const result = await page.evaluate(async () => {
    (window as any).setOnline(false);
    const response = await fetch('/api/prefs', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ key: 'receipt-offline', value: '1' }),
    });
    const receipt = (window as any).__amuxState.interactions.recent(1)[0];
    return {
      responseStatus: response.status,
      outbox: response.headers.get('X-Amux-Outbox'),
      phase: receipt.phase,
      command: receipt.command.kind,
      queued: receipt.acknowledgement.queued === true,
      severity: receipt.feedback.severity,
    };
  });

  expect(result).toEqual({
    responseStatus: 202,
    outbox: 'queued',
    phase: 'queued',
    command: 'environment.post',
    queued: true,
    severity: 'info',
  });
});

test('bare success and ignored fields cannot masquerade as applied',async({page})=>{
  await boot(page);
  let body:Record<string,unknown>={};
  await page.route('**/api/prefs',route=>route.fulfill({json:body}));
  for(const [ack,phase] of [[{},'unknown'],[{ignored_fields:['value']},'refused']] as const) {
    body=ack;
    const receipt=await page.evaluate(async()=>{
      await fetch('/api/prefs',{method:'POST',body:'{}'});
      return (window as any).__amuxInteractions.recent(1)[0];
    });
    expect(receipt.phase).toBe(phase);
    expect(receipt.effects).toEqual([]);
  }
});

test('replay mismatch retains a refused receipt instead of reporting success',async({page})=>{
  await boot(page);
  await page.route('**/api/board/RECEIPT-1',route=>route.fulfill({json:{id:'OTHER-1',applied:true}}));
  const id=await page.evaluate(async()=>{
    (window as any).setOnline(false);
    await fetch('/api/board/RECEIPT-1',{method:'PATCH',headers:{'Content-Type':'application/json'},body:'{"title":"Mismatch test"}'});
    return (window as any).__amuxInteractions.recent(1)[0].id;
  });
  await page.evaluate(async()=>{(window as any).setOnline(true);await (window as any).runSyncBanner(true);});
  await expect.poll(()=>page.evaluate(id=>(window as any).__amuxInteractions.get(id)?.phase,id)).toBe('refused');
  const receipt=await page.evaluate(id=>(window as any).__amuxInteractions.get(id),id);
  expect(receipt.feedback.message).toContain('exact card');
  expect(receipt.effects).toEqual([]);
});
