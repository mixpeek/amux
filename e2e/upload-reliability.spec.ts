import {test,expect,Page} from './fixtures';
const png=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=','base64');
const file=(name='image.png')=>({name,mimeType:'image/png',buffer:png});
async function setup(page:Page) {
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:['upload-a','upload-b'].map(name=>({name,running:true,status:'idle',dir:'/tmp/'+name}))}));
  await page.route(/\/api\/sessions\/upload-[ab]\/peek\?/,r=>r.fulfill({json:{name:r.request().url().includes('upload-a')?'upload-a':'upload-b',live:'Worker output',history:''}}));
  await page.route(/\/api\/sessions\/upload-[ab]\/subagents$/,r=>r.fulfill({json:{session:r.request().url().includes('upload-a')?'upload-a':'upload-b',subagents:[]}}));
  await page.goto('/');
  await page.waitForFunction(()=>typeof (window as any).openPeek==='function');
  await page.evaluate(()=>(window as any).openPeek('upload-a'));
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity','1');
}
const chips=(page:Page)=>page.locator('#peek-attach-bar .peek-attach-chip');
test('a stalled start times out, retries and preserves both image chips',async({page},info)=>{
  let starts=0;
  await page.route(/\/api\/upload\//,async r=>{
    if(r.request().url().endsWith('/start')) {
      starts++;
      if(starts<=2)return; // intentionally unanswered requests, until the client's deadline
      return r.fulfill({json:{id:'recovered-'+starts}});
    }
    if(r.request().url().endsWith('/finish'))return r.fulfill({json:{path:'/uploads/image.png',url:'/api/uploads/image.png'}});
    return r.fulfill({json:{ok:true}});
  });
  await setup(page); await page.clock.install();
  await page.locator('#peek-cmd-input').fill('Keep my message');
  await page.locator('#peek-file-input').setInputFiles([file(),file('second.png')]);
  await expect(chips(page)).toHaveCount(2);
  await expect(chips(page).first()).toContainText('Starting');
  await page.screenshot({path:info.outputPath('upload-starting.png')});
  await page.clock.fastForward(45001);
  await expect(chips(page).first()).toContainText('Retrying');
  await page.clock.fastForward(1001);
  await expect(chips(page).filter({hasText:'✓'})).toHaveCount(2);
  await expect(page.locator('#peek-cmd-input')).toHaveValue('Keep my message');
  expect(starts).toBe(4);
});
test('persistent failure is bounded and the same chip can be retried',async({page},info)=>{
  let starts=0,fail=true;
  await page.route(/\/api\/upload\//,r=>{
    if(r.request().url().endsWith('/start')){starts++;return r.fulfill({status:fail?503:200,json:fail?{error:'Temporarily unavailable'}:{id:'retry-ok'}});}
    return r.fulfill({json:r.request().url().endsWith('/finish')?{path:'/uploads/x.png',url:'/api/uploads/x.png'}:{ok:true}});
  });
  await setup(page);await page.locator('#peek-file-input').setInputFiles(file());
  await expect(chips(page).locator('.chip-retry')).toBeVisible({timeout:10000});
  expect(starts).toBe(3);
  await page.screenshot({path:info.outputPath('upload-retry.png')});
  fail=false;await chips(page).locator('.chip-retry').click();
  await expect(chips(page)).toContainText('✓');expect(starts).toBe(4);
});
test('queued files stay with their worker and can be cancelled before starting',async({page})=>{
  let release!:()=>void;const held=new Promise<void>(r=>release=r);let starts=0;
  await page.route(/\/api\/upload\//,async r=>{
    if(r.request().url().endsWith('/start')){const id='q'+(++starts);await held;return r.fulfill({json:{id}});}
    return r.fulfill({json:r.request().url().endsWith('/finish')?{path:'/uploads/x.png',url:'/api/uploads/x.png'}:{ok:true}});
  });
  await setup(page);await page.locator('#peek-file-input').setInputFiles(Array.from({length:6},(_,i)=>file(i+'.png')));
  await expect(chips(page)).toHaveCount(6);await expect(chips(page).nth(5)).toContainText('Queued');
  await chips(page).nth(5).locator('.chip-remove').click();
  await page.evaluate(()=>(window as any).openPeek('upload-b'));
  await expect(chips(page)).toHaveCount(0);
  release();await expect.poll(()=>starts).toBe(5);
  await page.evaluate(()=>(window as any).openPeek('upload-a'));
  await expect(chips(page).filter({hasText:'✓'})).toHaveCount(5);
});
test('removing a stalled upload releases its slot without retrying or losing other chips',async({page})=>{
  let starts=0;
  await page.route(/\/api\/upload\//,r=>{
    if(r.request().url().endsWith('/start')) {starts++;return;}
    return r.fulfill({json:{ok:true}});
  });
  await setup(page);await page.locator('#peek-file-input').setInputFiles(Array.from({length:5},(_,i)=>file(i+'.png')));
  await expect.poll(()=>starts).toBe(4);
  await chips(page).first().locator('.chip-remove').click();
  await expect.poll(()=>starts).toBe(5);
  await expect(chips(page)).toHaveCount(4);await expect(chips(page).locator('.chip-err')).toHaveCount(0);
});
test('a server restart during chunk upload recovers with a fresh upload ID',async({page})=>{
  let starts=0,chunks=0;
  await page.route(/\/api\/upload\//,r=>{
    const u=r.request().url();
    if(u.endsWith('/start'))return r.fulfill({json:{id:'restart-'+(++starts)}});
    if(u.includes('/chunk/')){chunks++;return r.fulfill({status:chunks===1?404:200,json:chunks===1?{error:'unknown upload'}:{ok:true}});}
    return r.fulfill({json:{path:'/uploads/x.png',url:'/api/uploads/x.png'}});
  });
  await setup(page);await page.locator('#peek-file-input').setInputFiles(file());
  await expect(chips(page)).toContainText('✓');expect(starts).toBe(2);expect(chunks).toBe(2);
});
test('the timeout also covers a response body that never finishes',async({page})=>{
  await setup(page);await page.clock.install();
  await page.evaluate(()=>{
    const orig=window.fetch;let first=true;
    window.fetch=async (url:any,options:any)=>{
      if(String(url).includes('/api/upload/')) {
        if(String(url).endsWith('/start')) {
          if(first){first=false;return new Response(new ReadableStream({start(){}}),{status:200});}
          return new Response(JSON.stringify({id:'body-retry'}));
        }
        return new Response(JSON.stringify(String(url).endsWith('/finish')?{path:'/uploads/x.png',url:'/api/uploads/x.png'}:{ok:true}));
      }
      return orig(url,options);
    };
  });
  await page.locator('#peek-file-input').setInputFiles(file());
  await expect(chips(page)).toContainText('Starting');
  await page.clock.fastForward(45001);await expect(chips(page)).toContainText('Retrying');
  await page.clock.fastForward(1001);await expect(chips(page)).toContainText('✓');
});

test('two images upload through the real API and download byte-for-byte',async({page},info)=>{
  await setup(page);
  const completed:any[]=[];
  page.on('response',async r=>{if(r.url().includes('/api/upload/') && r.url().endsWith('/finish') && r.ok())completed.push(await r.json());});
  await page.locator('#peek-file-input').setInputFiles([file('roundtrip-one.png'),file('roundtrip-two.png')]);
  await expect(chips(page).filter({hasText:'✓'})).toHaveCount(2,{timeout:20000});
  await expect.poll(()=>completed.length).toBe(2);
  for(const result of completed){const r=await page.request.get(result.url);expect(r.ok()).toBe(true);expect(await r.body()).toEqual(png);}
  await page.screenshot({path:info.outputPath('upload-complete.png')});
});
