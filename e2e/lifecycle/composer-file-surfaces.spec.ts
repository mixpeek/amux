import {test,expect} from '../fixtures';
import {boot,auth,checkpoint,getSessionsResilient,deleteOwnedWorkers} from './evidence';

for(const surface of ['card','details'] as const) for(const mode of ['Send','Queue'] as const) {
 test(`LC-COMPOSER-FILES: ${surface} ${mode} preserves uploaded bytes through offline recovery and newer drafts across reload`,async({page,request,context},info)=>{
  test.setTimeout(90_000); page.setDefaultTimeout(15_000); await boot(page); const headers=await auth(page);
  const name=`lc-files-${surface}-${mode.toLowerCase()}-${info.project.name}-${Date.now()}`;
  expect((await request.post('/api/sessions',{headers,data:{name,dir:'/tmp'}})).status()).toBe(201);
  const rows=await(await getSessionsResilient(request,headers)).json();
  Object.assign(rows.find((s:any)=>s.name===name),{running:true,status:'active'});
  // Only runtime and final send transport are controlled. Upload/storage/UI and
  // downloaded bytes are real; this case does not certify native execution.
  await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:rows}));
  const payloads:any[]=[]; let offline=false; let release!:()=>void;
  const held=new Promise<void>(r=>release=r); const verb=mode==='Queue'?'steer':'send';
  await page.route(`**/api/sessions/${name}/${verb}`,async r=>{if(offline)return r.abort('internetdisconnected');payloads.push(r.request().postDataJSON());await held;await r.fulfill({json:{ok:true,submitted:true,id:'accepted-'+name}})});
  const open=async()=>{
    await page.goto('/');if(await page.locator('#peek-overlay.active').isVisible())await page.getByRole('button',{name:'Close worker',exact:true}).click();const card=page.locator(`#cards .card[data-session="${name}"]`).locator('visible=true').first();
    if(surface==='card'){await card.locator('.card-name').click();return card;}
    await card.locator('.card-menu-btn').click();await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();return page.locator('#peek-overlay');
  };
  const queue=()=>page.evaluate(name=>JSON.parse(localStorage.getItem('amux_offline_queue')||'[]').filter((q:any)=>q.url.includes('/'+name+'/')),name);
  try {
    let view=await open();
    const bytes=Buffer.from(`Exact ${surface} ${mode} attachment\n`.repeat(400));
    if(surface==='card') {
      // The owner removed the card's Attach button. Exercise its supported paste
      // event with actual File bytes; details still uses the native file picker.
      await view.locator('textarea.send-input').evaluate((input,text)=>{
        const data=new DataTransfer();data.items.add(new File([text],'lifecycle-evidence.txt',{type:'text/plain'}));
        input.dispatchEvent(new ClipboardEvent('paste',{clipboardData:data,bubbles:true,cancelable:true}));
      },bytes.toString());
    } else {
      await page.locator('#peek-composer-more-btn').click();
      const picker=page.locator('#peek-more-menu').getByRole('button',{name:'Attach file',exact:false});
      await expect(picker).toBeVisible();
      const choice=page.waitForEvent('filechooser');await picker.click();
      await(await choice).setFiles({name:'lifecycle-evidence.txt',mimeType:'text/plain',buffer:bytes});
    }
    await expect(view.locator('.peek-attach-chip')).toHaveCount(1);await expect(view.locator('.uploading')).toHaveCount(0);
    const file=await page.evaluate(({name,surface})=>surface==='card'?eval('_cardFiles')[name][0]:eval('peekFiles')[0],{name,surface});
    expect(file.path).toBeTruthy();expect(await(await request.get(file.url,{headers})).body()).toEqual(bytes);
    const input=surface==='card'?view.locator('textarea.send-input'):page.locator('#peek-cmd-input');
    const send=view.locator('.send-split-main');if(await send.innerText()!==mode)await view.locator('.send-split-arrow').click();
    offline=true;await context.setOffline(true);await page.evaluate(()=>window.dispatchEvent(new Event('offline')));
    await input.fill('Read the uploaded evidence');await send.click();await expect(input).toHaveValue('');
    await expect.poll(async()=>(await queue()).length).toBe(1);
    const original=(await queue())[0];expect(JSON.parse(original.options.body).text).toContain('@'+file.path);expect(payloads).toHaveLength(0);
    await input.fill('Next draft must survive syncing');await checkpoint(page,info,`${surface}-${mode}-saved-offline`);
    offline=false;await context.setOffline(false);await page.evaluate(()=>window.dispatchEvent(new Event('online')));
    await expect.poll(()=>payloads.length).toBe(1);expect(payloads[0]).toEqual(JSON.parse(original.options.body));
    release();await expect.poll(async()=>(await queue()).length).toBe(0);
    await page.reload();view=await open();
    const restored=surface==='card'?view.locator('textarea.send-input'):page.locator('#peek-cmd-input');
    await expect(restored).toHaveValue('Next draft must survive syncing');
    await expect(view.locator('.peek-attach-chip'),'sent attachments must not return from durable storage').toHaveCount(0);
    expect(payloads).toHaveLength(1);await checkpoint(page,info,`${surface}-${mode}-synced-draft-retained`);
  }finally{release();offline=false;await context.setOffline(false);await page.evaluate(async name=>(window as any)._mutateQueue((q:any[])=>{for(let i=q.length-1;i>=0;i--)if(q[i].url.includes('/'+name+'/'))q.splice(i,1)}),name);await page.unroute(/\/api\/sessions(?:\?.*)?$/);await deleteOwnedWorkers(page,request,headers,[name]);}
 });
}
