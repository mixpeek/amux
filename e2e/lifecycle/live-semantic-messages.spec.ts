import { test, expect } from '@playwright/test';
import { mkdir } from 'node:fs/promises';
import path from 'node:path';
import { boot, auth, checkpoint, expectDelivered, getSessionsResilient } from './evidence';
import { lifecyclePrefix, selectLifecycleProvider, createLifecycleWorker, expectLifecycleWorker, expectLifecycleTerminal } from './provider';

// No board/history writes or classifier stubs: every tested task must originate
// in a real message sent through the composer and accepted by a native worker.
test('LC-SEMANTIC-MESSAGES: sent messages append and merge into one task while distinct outcomes stay separate', async ({page,request}, info) => {
  test.setTimeout(900_000);
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  const workspace=process.env.AMUX_LIFECYCLE_LAB_WORKSPACE;
  expect(workspace, 'dedicated scratch workspace is required').toBeTruthy();
  const healthResponse=await request.get('/api/health');
  const health=await healthResponse.json();
  await info.attach('semantic-message-prerequisites',{body:JSON.stringify(health),contentType:'application/json'});
  expect(healthResponse.ok(), 'lab health must be available').toBe(true);
  expect(health.admission, 'worker admission denied: live semantic coverage is unavailable, not passed').not.toBe('deny');
  const name=`${lifecyclePrefix}semantic-${Date.now()}`;
  const dir=path.join(workspace!,name);await mkdir(dir,{recursive:true});
  await boot(page);const headers=await auth(page);
  const receipts:any[]=[];let created=false;
  const suffix=` Scope ${name}. Planning only: wait for approval before implementation. Acknowledge briefly; do not create, edit or complete board records yourself during this intake check.`;
  const history=async()=>{
    const r=await request.get(`/api/history?session=${encodeURIComponent(name)}&limit=250`,{headers});
    expect(r.ok()).toBe(true);return r.json();
  };
  const board=async()=>{
    const r=await request.get(`/api/board?session=${encodeURIComponent(name)}&done_limit=0`,{headers});
    expect(r.ok()).toBe(true);return r.json();
  };
  const send=async(text:string,action:string,survivor?:string)=>{
    text+=suffix;
    await page.locator('#peek-cmd-input').fill(text);
    const sent=page.waitForResponse(r=>r.url().endsWith(`/api/sessions/${name}/send`) && r.request().method()==='POST',{timeout:180_000});
    await page.locator('#peek-overlay .send-split-main').click();
    await expect(page.locator('#peek-cmd-input')).toHaveValue('');
    const response=await sent;await expectDelivered(page,response);
    let matches:any[]=[];
    await expect.poll(async()=>{
      matches=(await history()).filter((row:any)=>String(row.text).includes(text));
      return matches.length===1 && !!matches[0].card_id;
    },{timeout:150_000,intervals:[1000,2000,5000],message:'one delivered source message must link to its automatically captured task'}).toBe(true);
    const message=matches[0];
    const r=await request.get(`/api/board/${message.card_id}`,{headers});expect(r.ok()).toBe(true);
    const card=await r.json();
    receipts.push({text,request:response.request().postDataJSON(),receipt:await response.json(),message,card});
    expect(card.owner_type).toBe('agent');expect(card.session).toBe(name);
    if(survivor) expect(card.id,'paraphrase/refinement must retain the original task identity').toBe(survivor);
    expect(card.log).toContain(`semantic intake: action=${action}`);
    const decisions=String(card.log).split('\n').filter(line=>line.includes('semantic intake:'));
    expect(decisions.at(-1),'a disabled/unavailable helper cannot pass semantic intake').toContain('measured=true');
    expect(decisions.at(-1)).toContain(`action=${action}`);
    expect(card.messages.some((m:any)=>String(m.id).replace(/^MSG-/,'')===String(message.id).replace(/^MSG-/,'') && m.card_id===card.id && m.text.includes(text))).toBe(true);
    return card;
  };
  try {
    await page.locator('#tab-sessions').click();
    await page.locator('[onclick*="toggleAddMenu"]').click();
    await page.locator('.card-menu-item',{hasText:'New worker'}).click();
    await page.locator('#create-name').fill(name);await page.locator('#create-dir').fill(dir);
    await selectLifecycleProvider(page);await page.locator('#create-prompt').fill('');
    created=true;await createLifecycleWorker(page);
    const roster=await getSessionsResilient(request,headers);expect(roster.ok()).toBe(true);
    expectLifecycleWorker((await roster.json()).find((row:any)=>row.name===name));
    const worker=page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
    await worker.locator('.card-menu-btn').click();await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await expectLifecycleTerminal(page);
    const a=await send('Build the invoice CSV normalizer. Store USD amounts as integer cents and reject malformed money. Produce invoices.json. USD only.','create');
    const b=await send('Convert billing invoice dollar values from CSV into whole cents in invoices.json, rejecting invalid monetary strings. This restates the same normalization deliverable.','append',a.id);
    expect(b.rev).toBeGreaterThan(a.rev);
    const c=await send('Additional context for the same invoice CSV normalizer: customer exports sometimes contain surrounding whitespace. Keep the original CSV sample as input evidence for the existing invoices.json work.','append',a.id);
    const d=await send('Update the same invoice normalizer requirements: replace the USD-only restriction with support for USD and EUR. Preserve integer cents and malformed-input rejection, producing the same invoices.json.','update',a.id);
    expect(d.rev).toBeGreaterThan(c.rev);
    expect(d.desc).toContain('USD only');expect(d.desc).toContain('surrounding whitespace');expect(d.desc).toContain('support for USD and EUR');
    const e=await send('Build persistent dark and light theme controls for the invoice dashboard. This independent UI deliverable must persist across reload and be keyboard accessible; it makes no changes to invoices.json.','create');
    const f=await send('Build a payroll CSV normalizer for employee salaries. Convert salary amounts into integer cents in payroll.json. This separate payroll dataset and output must not change invoice work.','create');
    expect(new Set([a.id,b.id,c.id,d.id,e.id,f.id]).size).toBe(3);
    const rows=await board();
    expect(rows.map((row:any)=>row.id).sort(),'six requests must produce three tasks, without orphan near-duplicates').toEqual([a.id,e.id,f.id].sort());
    const final=await (await request.get(`/api/board/${a.id}`,{headers})).json();
    for(const proof of receipts.slice(0,4)) expect(final.messages.some((m:any)=>m.text.includes(proof.text) && m.card_id===a.id)).toBe(true);
    // Verify preserved source links in the actual task UI at desktop and phone widths.
    await page.locator('#peek-overlay').getByRole('button',{name:'Close worker',exact:true}).click();
    for(const width of [1280,375]) {
      await page.setViewportSize({width,height:900});await page.goto('/#issue='+a.id);
      await expect(page.locator('#bd-key')).toHaveText(a.id);
      await expect(page.locator('#bd-preview')).toContainText('support for USD and EUR');
      await checkpoint(page,info,`semantic-messages-merged-${width}`);
      for(const proof of receipts.slice(0,4)) {
        const id=String(proof.message.id).replace(/^MSG-/,'');
        await expect(page.getByRole('button',{name:'MSG-'+id,exact:true})).toBeVisible();
      }
      const refinedId=String(receipts[3].message.id).replace(/^MSG-/,'');
      await page.getByRole('button',{name:'MSG-'+refinedId,exact:true}).click();
      await expect(page.locator('#messages-view')).toContainText('replace the USD-only restriction');
      await checkpoint(page,info,`semantic-messages-source-${width}`);
    }
  } finally {
    await info.attach('semantic-message-proof',{body:JSON.stringify({name,dir,receipts},null,2),contentType:'application/json'});
    if(created) {
      const roster=await getSessionsResilient(request,headers);
      expect(roster.ok(),'read cleanup ownership before stopping a worker').toBe(true);
      if((await roster.json()).some((row:any)=>row.name===name)) {
        const stopped=await request.post(`/api/sessions/${name}/stop`,{headers});
        expect(stopped.ok(),'stop only the run-owned worker; retain its tasks/messages for diagnosis').toBe(true);
      }
    }
  }
});
