import {test,expect} from '@playwright/test';
import {mkdir,readFile} from 'node:fs/promises';
import path from 'node:path';
import {boot,auth,checkpoint,getSessionsResilient} from './evidence';
import {lifecyclePrefix,selectLifecycleProvider,createLifecycleWorker,expectLifecycleTerminal,expectLifecycleWorker} from './provider';
test('LC-STEERING-AUTO: queued file assignment is automatically picked up and completed by a real worker',async({page,request},info)=>{
 test.setTimeout(900_000);expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
 const health=await(await request.get('/api/health')).json();await info.attach('steering-prerequisites',{body:JSON.stringify(health),contentType:'application/json'});
 expect(health.admission,'worker admission denied; automatic native pickup is not verified').not.toBe('deny');
 const name=`${lifecyclePrefix}steering-${Date.now()}`;const dir=path.join(process.env.AMUX_LIFECYCLE_LAB_WORKSPACE!,name);await mkdir(dir,{recursive:true});
 await boot(page);const headers=await auth(page);let created=false;
 try{
  await page.locator('#tab-sessions').click();await page.locator('[onclick*="toggleAddMenu"]').click();await page.locator('.card-menu-item',{hasText:'New worker'}).click();
  await page.locator('#create-name').fill(name);await page.locator('#create-dir').fill(dir);await selectLifecycleProvider(page);await page.locator('#create-prompt').fill('');created=true;await createLifecycleWorker(page);
  const roster=await(await getSessionsResilient(request,headers)).json();expectLifecycleWorker(roster.find((r:any)=>r.name===name));
  await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();await expectLifecycleTerminal(page);
  await page.locator('#peek-composer-more-btn').click();const chooser=page.waitForEvent('filechooser');await page.locator('#peek-more-menu').getByRole('button',{name:'Attach file',exact:false}).click();
  await(await chooser).setFiles({name:'steering-input.json',mimeType:'application/json',buffer:Buffer.from('{"values":[7,11,13]}')});await expect(page.locator('#peek-attach-bar')).toContainText('steering-input.json');await expect(page.locator('#peek-attach-bar .uploading')).toHaveCount(0);await expect(page.locator('#peek-attach-bar .failed')).toHaveCount(0);
  const main=page.locator('#peek-overlay .send-split-main');if(await main.innerText()!=='Queue')await page.locator('#peek-overlay .send-split-arrow').click();
  const message=`Authorized lifecycle assignment ${name}. Work only in ${dir}. Read the exact uploaded JSON file attached here; sum its values with a real command. Write steering-receipt.json with worker (your name), total, input_path and task_id. Use your actual source-message board task or create one linked to this message if missing; complete that original task through normal gates with the real command result and receipt file as evidence. Do not request another acknowledgement, send peer messages or create replacement tasks. Finish all real work then idle.`;
  await page.locator('#peek-cmd-input').fill(message);const response=page.waitForResponse(r=>r.url().endsWith('/'+name+'/steer')&&r.request().method()==='POST');await main.click();const accepted=await(await response).json();expect(typeof accepted.id).toBe('string');
  await page.locator('#peek-tab-steering').click();await checkpoint(page,info,'native-steering-queued');
  let history:any[]=[];await expect.poll(async()=>{history=await(await request.get(`/api/sessions/${name}/steer?history=1`,{headers})).json();return history.some(r=>r.id===accepted.id&&r.delivered_at>0&&['confirmed','retried'].includes(r.submit_verdict));},{timeout:240_000,intervals:[1000,5000]}).toBe(true);
  let receipt:any;await expect.poll(async()=>{try{receipt=JSON.parse(await readFile(path.join(dir,'steering-receipt.json'),'utf8'));return receipt.total;}catch{return null;}},{timeout:360_000,intervals:[5000]}).toBe(31);
  expect(receipt.worker).toBe(name);expect(receipt.input_path).toContain('steering-input');expect(receipt.task_id).toBeTruthy();
  let card:any;await expect.poll(async()=>{card=await(await request.get('/api/board/'+receipt.task_id,{headers})).json();return ['done','verified'].includes(card.status)&&Boolean(card.evidence);},{timeout:120_000,intervals:[5000]}).toBe(true);
  expect(card.messages.some((m:any)=>m.card_id===card.id&&String(m.text).includes(message))).toBe(true);
  expect((await(await request.get('/api/health')).json()).build).toBe(health.build);
  await page.locator('#peek-tab-terminal').click();await expect(page.locator('#peek-body')).toContainText('steering-receipt');await checkpoint(page,info,'native-steering-terminal-proof');
  await info.attach('native-steering-proof',{body:JSON.stringify({name,accepted,history,receipt,card}),contentType:'application/json'});
 }finally{if(created)await request.post(`/api/sessions/${name}/stop`,{headers});}
});
