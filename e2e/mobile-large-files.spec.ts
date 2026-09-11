import {test, expect, Page} from './fixtures';
import {mkdtemp, open, rm, stat} from 'node:fs/promises';
import {createReadStream} from 'node:fs';
import {createHash} from 'node:crypto';
import {tmpdir} from 'node:os';
import path from 'node:path';
import https from 'node:https';
declare const _idb: any;
async function hashFile(file: string) {
  const hash = createHash('sha256');
  for await (const bytes of createReadStream(file)) hash.update(bytes);
  return hash.digest('hex');
}
async function setup(page: Page) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[{name:'large-mobile',running:true,status:'idle',dir:'/tmp'}]}));
  await page.route(/\/api\/sessions\/large-mobile\/peek\?/, r => r.fulfill({json:{name:'large-mobile',live:'Verify the uploaded artifact',history:''}}));
  await page.route('**/api/sessions/large-mobile/subagents', r => r.fulfill({json:{session:'large-mobile',subagents:[]}}));
  await page.goto('/');
  await page.evaluate(() => (window as any).openPeek('large-mobile'));
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity','1');
}
test('large mobile file survives interrupted upload and reload with bounded storage chunks', async ({page}, info) => {
  test.setTimeout(240000);
  const dir = await mkdtemp(path.join(tmpdir(), 'amux-large-mobile-'));
  const file = path.join(dir, 'large-evidence.bin');
  const size = 256 * 1024 * 1024 + 123;
  const handle = await open(file, 'w');
  const block = Buffer.alloc(1024 * 1024);
  for(let i=0;i<block.length;i++) block[i]=(i*31+17)%251;
  for(let i=0;i<256;i++) await handle.write(block);
  await handle.write(block.subarray(0,123));await handle.close();
  const expected = await hashFile(file);
  let offline = true, interrupted = 0, uploaded: any;
  await page.route(/\/api\/upload\/[^/]+\/chunk\//, async r => {
    const n = Number(r.request().url().split('/').pop());
    if(offline && n>=3) {interrupted++;return r.abort('internetdisconnected');}
    await r.continue();
  });
  page.on('response',async r => {if(r.url().endsWith('/finish') && r.ok()) uploaded = await r.json();});
  try {
    await setup(page);
    // A whole-file read would throw. Production must persist at most one 5 MiB slice at a time.
    await page.evaluate(() => {
      const original = Blob.prototype.arrayBuffer;
      Blob.prototype.arrayBuffer = function() {
        if(this.size > 5*1024*1024) throw new Error('Unbounded file read');
        return original.call(this);
      };
    });
    await page.locator('#peek-cmd-input').fill('Check the large evidence file');
    await page.locator('#peek-file-input').setInputFiles(file);
    const chip = page.locator('#peek-attach-bar .peek-attach-chip');
    await expect(chip.locator('.chip-retry')).toBeVisible({timeout:120000});
    expect(interrupted).toBeGreaterThan(0);
    const stored = await page.evaluate(async () => {
      const row = (await _idb.getUploads()).find((x:any) => x.name==='large-evidence.bin');
      const bytes = await _idb.uploadChunk(row.id, row.totalChunks-1);
      return {size:row.size,chunks:row.totalChunks,lastBytes:bytes.byteLength};
    });
    expect(stored).toEqual({size,chunks:52,lastBytes:1024*1024+123});
    await page.reload();
    await page.evaluate(() => (window as any).openPeek('large-mobile'));
    await expect(chip).toContainText('large-evidence.bin');
    offline = false;
    await page.evaluate(() => window.dispatchEvent(new Event('focus')));
    await expect(chip).toContainText('✓',{timeout:120000});
    await expect.poll(() => !!uploaded).toBe(true);
    expect((await stat(uploaded.path)).size).toBe(size);
    expect(await hashFile(uploaded.path)).toBe(expected);
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
    const url = new URL(uploaded.url, page.url());
    const downloadHash = await new Promise<string>((resolve,reject) => {
      https.get(url,{rejectUnauthorized:false,headers:{Authorization:'Bearer '+token}},res=>{
        if(res.statusCode!==200) {res.resume();return reject(new Error('Download status '+res.statusCode));}
        const hash=createHash('sha256');res.on('data',b=>hash.update(b));res.on('end',()=>resolve(hash.digest('hex')));res.on('error',reject);
      }).on('error',reject);
    });
    expect(downloadHash).toBe(expected);
    await page.screenshot({path:info.outputPath('large-upload-recovered.png')});
    await info.attach('large-upload-proof',{body:JSON.stringify({size,sha256:expected,interrupted,stored,downloadHash}),contentType:'application/json'});
    await chip.locator('.chip-remove').click();
    await expect.poll(() => page.evaluate(async () => (await _idb.getUploads()).length)).toBe(0);
  } finally {await rm(dir,{recursive:true,force:true});}
});
test('mobile composer remains reachable in landscape and a reduced keyboard viewport', async ({page},info) => {
  await setup(page);
  for(const size of [{width:320,height:568},{width:430,height:932},{width:667,height:375},{width:393,height:330}]) {
    await page.setViewportSize(size);
    await page.locator('#peek-cmd-input').fill('Acceptance criteria and evidence\n'.repeat(10));
    const send = page.locator('#peek-overlay .send-split-main');
    const rect = await send.boundingBox();expect(rect).not.toBeNull();
    expect(rect!.y+rect!.height,JSON.stringify(size)).toBeLessThanOrEqual(size.height);
    expect(rect!.height).toBeGreaterThanOrEqual(44);
    await page.screenshot({path:info.outputPath(`composer-${size.width}-${size.height}.png`)});
  }
});
test('chunked Files publish preserves names, refuses dangerous paths and never overwrites', async ({page,request}) => {
  const dir = await mkdtemp(path.join(tmpdir(), 'amux-large-destination-'));
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
  const headers = {Authorization:'Bearer '+token};
  async function publish(name:string) {
    const response = await request.post('/api/upload/start',{headers,data:{name,size:3,chunks:1}});
    expect(response.ok()).toBeTruthy();const {id} = await response.json();
    expect((await request.put(`/api/upload/${id}/chunk/0`,{headers,data:Buffer.from('abc')})).ok()).toBeTruthy();
    return request.post(`/api/upload/${id}/finish?dir=${encodeURIComponent(dir)}&name=${encodeURIComponent(name)}`,{headers});
  }
  try {
    const first = await publish('résumé file.bin');expect(first.ok()).toBeTruthy();
    const a = await first.json();expect(path.basename(a.path)).toBe('résumé file.bin');
    const replay = await request.post(first.url(),{headers});expect(replay.ok()).toBeTruthy();expect(await replay.json()).toEqual(a);
    const conflict = new URL(first.url());conflict.searchParams.set('name','changed.bin');
    expect((await request.post(conflict.toString(),{headers})).status()).toBe(409);
    const second = await publish('résumé file.bin');expect(second.ok()).toBeTruthy();
    const b = await second.json();expect(path.basename(b.path)).toBe('résumé file_1.bin');
    expect(await hashFile(a.path)).toBe(await hashFile(b.path));
    expect((await publish('.bashrc')).status()).toBe(403);
  } finally {await rm(dir,{recursive:true,force:true});}
});
test('large file uploaded from the Files page appears with exact bytes', async ({page,request},info) => {
  test.setTimeout(180000);
  const dir = await mkdtemp(path.join(tmpdir(),'amux-files-large-'));
  const sourceDir = await mkdtemp(path.join(tmpdir(),'amux-files-source-'));
  const source = path.join(sourceDir,'large-folder-file.bin');
  const h = await open(source,'w');await h.truncate(128*1024*1024+17);await h.close();
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done','1'));
  await page.goto('/');
  const headers = await page.evaluate(() => ({Authorization:'Bearer '+(window as any)._AMUX_AUTH_TOKEN}));
  const prior = await (await request.get('/api/prefs?key=files_cwd',{headers})).json();
  try {
    expect((await request.post('/api/prefs',{headers,data:{key:'files_cwd',value:dir}})).ok()).toBeTruthy();
    await page.reload();await page.locator('#tab-files').click();
    const choose = page.waitForEvent('filechooser');
    const button = page.getByTitle('Upload files into this folder',{exact:true});
    if(await button.isVisible()) await button.click();
    else {await page.locator('#files-overflow-btn').click();await page.locator('#files-overflow-menu').getByRole('button',{name:/Upload files/}).click();}
    await (await choose).setFiles(source);
    await expect(page.locator('#files-body .fe-row').filter({hasText:'large-folder-file.bin'})).toBeVisible({timeout:120000});
    const destination = path.join(dir,'large-folder-file.bin');
    expect((await stat(destination)).size).toBe(128*1024*1024+17);
    expect(await hashFile(destination)).toBe(await hashFile(source));
    await page.screenshot({path:info.outputPath('large-files-page.png')});
  } finally {
    await request.post('/api/prefs',{headers,data:{key:'files_cwd',value:prior.value || ''}});
    await rm(dir,{recursive:true,force:true});await rm(sourceDir,{recursive:true,force:true});
  }
});
