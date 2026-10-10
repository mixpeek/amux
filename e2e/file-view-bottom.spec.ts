import {test, expect, Page, allowUnusedRoute} from './fixtures';
import {cleanup} from './teardown';
import {mkdtemp, writeFile, rm, copyFile, readFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {execFileSync} from 'node:child_process';
declare const _xlsxEd: any;

test.beforeEach(async ({page}) => {
  await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[{name:'file-bottom-worker',running:false,provider:'codex',dir:'/tmp'}]}));
});

const end = 'FILE_VIEW_FINAL_LINE_20261009';
const longText = Array.from({length:150}, (_, i) => (i + 1) + '. Read the complete artifact and verify its evidence.').join('\n\n') + '\n\n' + end;
const sizes = [{width:1280,height:800}, {width:1280,height:350}, {width:375,height:667}, {width:667,height:375}];

async function fits(page: Page, selector = '#file-body') {
  await expect.poll(() => page.locator(selector).evaluate(el => {
    const r = el.getBoundingClientRect(), vv = window.visualViewport;
    const top = vv?.offsetTop || 0, bottom = top + (vv?.height || innerHeight);
    return r.top >= top - 1 && r.bottom <= bottom + 1 && r.left >= -1 && r.right <= innerWidth + 1;
  }), 'the entire ' + selector + ' scrollport is inside the visible viewport').toBe(true);
}

// Measure the final glyph, including raw text without a DOM wrapper.
// scrollTop=max alone passes even when the scrollport extends off screen.
async function finalLine(page: Page, scroller = '#file-body', scroll = true, needle = end) {
  // Scroll on every poll: content that renders or grows after one scroll left
  // the last line below the fold on CI (desktop, mobile and ios-safari runs).
  await expect.poll(() => page.locator(scroller).evaluate((el, [needle, scroll]) => {
    if (scroll) el.scrollTop = el.scrollHeight;
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let node: Node | null, offset = 0;
    const text = el.textContent || '', at = text.indexOf(needle);
    if (at < 0) return false;
    const range = document.createRange();
    let started = false;
    while ((node = walker.nextNode())) {
      const length = (node.textContent || '').length;
      if (!started && at < offset+length) {range.setStart(node,at-offset); started=true;}
      if (started && at+needle.length <= offset+length) {
        range.setEnd(node,at+needle.length-offset);
      const r = range.getBoundingClientRect(), b = el.getBoundingClientRect();
      const hit = document.elementFromPoint(r.left+Math.min(10,r.width/2), r.bottom-2);
      return r.height > 0 && r.top >= b.top - 1 && r.bottom <= Math.min(b.bottom,innerHeight) + 1 && !!hit && el.contains(hit);
      }
      offset += length;
    }
    return false;
  }, [needle, scroll] as [string, boolean]), 'the complete final line is visible and hit-testable after scrolling to the end').toBe(true);
}

test('real Markdown preview, raw and edit reach the end in dashboard and embedded chat', async ({page,browserName}, info) => {
  const dir = await mkdtemp(join(tmpdir(),'amux-file-bottom-'));
  const file = join(dir,'long.md'); await writeFile(file,longText);
  try {
    for (const route of ['/', '/?peekEmbed=file-bottom-worker&peekTab=chat']) {
      await page.goto(route);
      if (route !== '/') await expect(page.locator('#peek-overlay')).toBeVisible();
      for (const size of sizes) {
        await page.setViewportSize(size);
        await page.evaluate(p => (window as any).openFilePreview(p),file);
        await expect(page.locator('#file-body')).toContainText(end);
        await fits(page);
        await page.locator('#file-body').evaluate(el=>{el.scrollTop=0;});
        // Playwright cannot synthesize wheel input on mobile WebKit. Still
        // measure the actual scroll range and final glyph on that engine.
        if(browserName==='webkit'&&info.project.use.isMobile) await page.locator('#file-body').evaluate(el=>{el.scrollTop=el.scrollHeight;});
        else {await page.locator('#file-body').hover(); await page.mouse.wheel(0,100000);}
        await finalLine(page,'#file-body',false);
        await page.locator('#file-tab-raw').click(); await finalLine(page);
        await page.locator('#file-menu-btn').click(); await page.locator('#file-tab-edit').click();
        await expect(page.locator('#file-edit-ta')).toHaveValue(longText);
        await fits(page,'#file-edit-ta');
        await page.locator('#file-edit-ta').press('ControlOrMeta+End');
        // Mobile WebKit reveals the caret without the textarea's bottom padding;
        // scroll the real range there, as the preview check above does.
        if(browserName==='webkit'&&info.project.use.isMobile) await page.locator('#file-edit-ta').evaluate(el=>{el.scrollTop=el.scrollHeight;});
        const atEnd = await page.locator('#file-edit-ta').evaluate((el:HTMLTextAreaElement) => ({at:el.selectionEnd,length:el.value.length,remaining:el.scrollHeight-el.clientHeight-el.scrollTop}));
        expect(atEnd.at).toBe(atEnd.length); expect(atEnd.remaining).toBeLessThanOrEqual(2);
        await page.locator('#file-tab-preview').click(); await finalLine(page);
        if (size.height===350) await page.screenshot({path:info.outputPath(route==='/'?'markdown-dashboard.png':'markdown-embedded.png')});
        await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
      }
    }
  } finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

test('code, plain text, JSON, CSV and HTML reach their final content in every window size', async ({page,browserName}, info) => {
  const dir = await mkdtemp(join(tmpdir(),'amux-file-views-'));
  const content = {
    'long.txt':longText.replace(end,'More evidence follows.').repeat(15)+'\n\n'+end,
    'long.js':Array.from({length:150},(_,i)=>'const value'+i+' = '+i+';').join('\n')+'\n// '+end,
    'long.json':JSON.stringify({rows:Array.from({length:150},(_,i)=>({row:i,value:'Evidence'})),final:end}),
    'long.csv':'row,value\n'+Array.from({length:150},(_,i)=>i+',Evidence').join('\n')+'\n151,'+end,
    'long.html':'<!doctype html><html><body>'+Array.from({length:150},(_,i)=>'<p>Evidence row '+i+'</p>').join('')+'<p id="end">'+end+'</p></body></html>',
  };
  try {
    for (const [name,text] of Object.entries(content)) await writeFile(join(dir,name),text);
    await page.goto('/?peekEmbed=file-bottom-worker&peekTab=chat');
    await expect(page.locator('#peek-overlay')).toBeVisible();
    for (const size of sizes) {
      await page.setViewportSize(size);
      for (const name of Object.keys(content)) {
        await page.evaluate(p=>(window as any).openFilePreview(p),join(dir,name));
        await fits(page);
        if (name.endsWith('.html')) {
          const frame = page.frameLocator('#file-body iframe');
          await frame.locator('#end').waitFor();
          if(browserName==='webkit'&&info.project.use.isMobile) await frame.locator('html').evaluate(el=>{el.ownerDocument.scrollingElement!.scrollTop=el.ownerDocument.scrollingElement!.scrollHeight;});
          else {await page.locator('#file-body iframe').hover(); await page.mouse.wheel(0,100000);}
          await expect.poll(()=>frame.locator('#end').evaluate(el=>{const r=el.getBoundingClientRect();return r.top>=0&&r.bottom<=innerHeight;})).toBe(true);
          await fits(page,'#file-body iframe');
        } else {
          await expect(page.locator('#file-body')).toContainText(end);
          await finalLine(page,name.endsWith('.csv')?'#file-body .csv-wrap':'#file-body');
        }
        await page.locator('#file-tab-raw').click(); await finalLine(page);
        if (size.height===350) await page.screenshot({path:info.outputPath(name+'-short.png')});
        await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
      }
    }
  } finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

test('images, video, audio, binary downloads and unsupported ebooks fit their file view',async({page},info)=>{
  const dir=await mkdtemp(join(tmpdir(),'amux-file-media-'));
  try{
    await writeFile(join(dir,'tall.svg'),'<svg xmlns="http://www.w3.org/2000/svg" width="200" height="2000"><rect width="200" height="2000" fill="blue"/><rect y="1950" width="200" height="50" fill="red"/></svg>');
    await writeFile(join(dir,'data.bin'),Buffer.alloc(1024));
    await writeFile(join(dir,'book.azw3'),Buffer.alloc(1024));
    await writeFile(join(dir,'book.fb2'),'<FictionBook/>');
    const wav=Buffer.alloc(44+1600);wav.write('RIFF',0);wav.writeUInt32LE(wav.length-8,4);wav.write('WAVEfmt ',8);wav.writeUInt32LE(16,16);wav.writeUInt16LE(1,20);wav.writeUInt16LE(1,22);wav.writeUInt32LE(8000,24);wav.writeUInt32LE(16000,28);wav.writeUInt16LE(2,32);wav.writeUInt16LE(16,34);wav.write('data',36);wav.writeUInt32LE(1600,40);
    await writeFile(join(dir,'sound.wav'),wav);
    await copyFile(join(__dirname,'fixtures','file-view-tiny.mp4'),join(dir,'clip.mp4'));
    await page.goto('/');
    for(const size of sizes){
      await page.setViewportSize(size);
      for(const name of ['tall.svg','clip.mp4','sound.wav','data.bin','book.azw3','book.fb2']){
        await page.evaluate(p=>(window as any).openFilePreview(p),join(dir,name));await fits(page);
        if(name==='tall.svg'){
          await expect.poll(()=>page.locator('#file-body img').evaluate((el:HTMLImageElement)=>el.complete&&el.naturalHeight===2000)).toBe(true);
          await fits(page,'#file-body img');
        }else if(name==='clip.mp4'){
          // CI's Chromium and WebKit builds lack H.264 and can stall without an
          // error event, so decoding is not asserted; the layout below is.
          await expect(page.locator('#file-body video')).toBeAttached();
          await fits(page,'#file-body .file-video-meta');
        }else if(name==='book.fb2'){
          await expect(page.locator('#file-body')).toContainText('ebook rendering not implemented');
          await page.locator('#file-body').evaluate(el=>{el.scrollTop=el.scrollHeight;});
        }else{
          const child=page.locator('#file-body > div').first();
          expect(await child.evaluate(el=>el.getBoundingClientRect().top>=el.parentElement!.getBoundingClientRect().top)).toBe(true);
          await page.locator('#file-body').evaluate(el=>{el.scrollTop=el.scrollHeight;});
          const last=page.locator(name==='sound.wav'?'#file-body > div > div:last-child':'#file-body a[download]');
          await fits(page,name==='sound.wav'?'#file-body > div > div:last-child':'#file-body a[download]');
          await expect(last).toBeVisible();
        }
        if(size.height===350)await page.screenshot({path:info.outputPath(name+'-short.png')});
        await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
      }
    }
  }finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

// Minimal valid multipage PDF, with real xref offsets, consumed by the real
// server and pdf.js rather than mocked canvas dimensions.
function pdfBytes(count=3) {
  const objects = ['<< /Type /Catalog /Pages 2 0 R >>','<< /Type /Pages /Kids ['+Array.from({length:count},(_,i)=>(3+i*2)+' 0 R').join(' ')+'] /Count '+count+' >>'];
  for (let i=0;i<count;i++) {
    objects.push('<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /Resources << >> /Contents '+(4+i*2)+' 0 R >>');
    const stream='0.1 0.4 0.8 rg 10 10 280 380 re f\n';
    objects.push('<< /Length '+Buffer.byteLength(stream)+' >>\nstream\n'+stream+'endstream');
  }
  let text='%PDF-1.4\n'; const offsets=[0];
  objects.forEach((obj,i)=>{offsets.push(Buffer.byteLength(text));text+=(i+1)+' 0 obj\n'+obj+'\nendobj\n';});
  const xref=Buffer.byteLength(text);
  text+='xref\n0 '+offsets.length+'\n0000000000 65535 f \n'+offsets.slice(1).map(n=>String(n).padStart(10,'0')+' 00000 n \n').join('');
  text+='trailer\n<< /Size '+offsets.length+' /Root 1 0 R >>\nstartxref\n'+xref+'\n%%EOF\n';
  return Buffer.from(text);
}

test('PDF final page remains reachable; cached Markdown still reaches the last line', async ({page},info) => {
  const dir = await mkdtemp(join(tmpdir(),'amux-file-docs-'));
  try {
    await writeFile(join(dir,'report.pdf'),pdfBytes());
    await writeFile(join(dir,'cached.md'),longText);
    await page.goto('/');
    for (const size of sizes) {
      await page.setViewportSize(size);
      await page.evaluate(p=>(window as any).openFilePreview(p),join(dir,'report.pdf'));
      await expect(page.locator('#file-body .pdf-page')).toHaveCount(3,{timeout:30000});
      await fits(page); await page.locator('#file-body').evaluate(el=>{el.scrollTop=el.scrollHeight;});
      const last=page.locator('#file-body .pdf-page').last();
      expect(await last.evaluate(el=>{const r=el.getBoundingClientRect(),b=el.closest('#file-body')!.getBoundingClientRect();return r.bottom<=Math.min(innerHeight,b.bottom)&&r.bottom>b.top;})).toBe(true);
      await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
    }
    await page.setViewportSize({width:1280,height:350});
    const cached=join(dir,'cached.md');
    await page.evaluate(p=>(window as any).openFilePreview(p),cached);
    await page.evaluate(()=>(window as any).closeFilePreview());
    await page.route('**/api/file?**',r=>r.abort('internetdisconnected'));
    await page.evaluate(p=>(window as any).openFilePreview(p),cached);
    await expect(page.locator('#file-title')).toContainText('cached');
    await fits(page); await finalLine(page);
    await page.screenshot({path:info.outputPath('cached-markdown-final-line.png')});
  } finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

test('a clipped file scrollport emits the existing server diagnostic without file contents',async({page})=>{
  const dir=await mkdtemp(join(tmpdir(),'amux-file-diagnostic-'));
  try {
    const file=join(dir,'probe.md');await writeFile(file,longText);await page.goto('/');
    await page.evaluate(p=>(window as any).openFilePreview(p),file);
    await fits(page);
    const beacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.kind==='modal-layout-clipped'&&r.postDataJSON()?.clipped?.includes('file-overlay'));
    await page.locator('#file-overlay').evaluate(el=>{el.style.setProperty('height','1200px','important');el.style.maxHeight='none';});
    await page.evaluate(()=>window.dispatchEvent(new Event('resize')));
    const data=(await beacon).postDataJSON();
    expect(data.measured).toBe(true);expect(data.n_considered).toBeGreaterThan(0);
    expect(JSON.stringify(data)).not.toContain(end);expect(JSON.stringify(data)).not.toContain(file);
    await page.locator('#file-overlay').evaluate(el=>{el.style.removeProperty('height');el.style.removeProperty('max-height');});
    await page.evaluate(()=>(window as any).closeFilePreview());
    const binary=join(dir,'probe.bin');await writeFile(binary,Buffer.alloc(100));
    await page.evaluate(p=>(window as any).openFilePreview(p),binary);
    const mediaBeacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.clipped?.includes('file-overlay:start-content-unreachable'));
    await page.locator('#file-body').evaluate(el=>{el.style.alignItems='center';(el.firstElementChild as HTMLElement).style.minHeight='2000px';el.scrollTop=0;});
    await page.evaluate(()=>window.dispatchEvent(new Event('resize')));
    expect((await mediaBeacon).postDataJSON().measured).toBe(true);
    await expect.poll(()=>page.evaluate(async()=>{
      const result=await (await fetch('/api/client-debug?kind=modal-layout-clipped')).json();
      return result.beacons.some((b:any)=>b.body.clipped?.includes('file-overlay:start-content-unreachable'));
    }), 'the backend retained the diagnostic').toBe(true);
  } finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

test('spreadsheet read-only views expose the last row and XLSX editor keeps its canvas and footer on screen',async({page},info)=>{
  // Cold real CDN loading can exceed the canonical 30s desktop budget.
  test.setTimeout(90_000);
  const dir=await mkdtemp(join(tmpdir(),'amux-file-workbook-'));
  try {
    execFileSync('python3',[join(__dirname,'file-view-fixtures.py'),dir,end]);
    await page.goto('/');
    for(const size of sizes){
      await page.setViewportSize(size);
      await page.evaluate(p=>(window as any).openFilePreview(p),join(dir,'workbook.ods'));
      await expect(page.locator('#file-body')).toContainText(end);
      await fits(page);await finalLine(page);
      await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
    }
    // The editor is a canvas surface. Verify the real imported cell model and
    // the whole canvas/footer frame, rather than mistaking DOM text for cells.
    await page.setViewportSize({width:1280,height:350});
    await page.evaluate(p=>(window as any).openFilePreview(p),join(dir,'workbook.xlsx'));
    await expect(page.locator('#xlsx-univer canvas').first()).toBeVisible({timeout:45000});
    await fits(page,'#xlsx-univer');
    const tab=page.locator('#xlsx-univer').getByText('Evidence',{exact:true});
    await expect(tab).toBeVisible();
    await tab.click();
    await page.locator('#xlsx-univer canvas[id^="univer-sheet-main-canvas"]').click({position:{x:130,y:70},timeout:10000});
    const address=page.locator('#xlsx-univer [data-u-comp="defined-name"] input');
    await address.fill('A151');await address.press('Enter');
    await expect(address).toHaveValue('A151');
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    await info.attach('spreadsheet-inputs', {body:JSON.stringify(await page.locator('#xlsx-univer input').evaluateAll(els=>els.map((el:any)=>({value:el.value,placeholder:el.placeholder,label:el.getAttribute('aria-label')})))),contentType:'application/json'});
    expect(await tab.evaluate(el=>el.getBoundingClientRect().bottom<=innerHeight)).toBe(true);
    expect(await page.evaluate(()=>{const sheet=_xlsxEd.api.getActiveWorkbook().save();return Object.values(sheet.sheets as any).some((s:any)=>Object.values(s.cellData||{}).some((row:any)=>Object.values(row).some((c:any)=>c.v==='FILE_VIEW_FINAL_LINE_20261009')));})).toBe(true);
    await page.screenshot({path:info.outputPath('xlsx-editor-short-window.png')});
  }finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

test('Files listing and worker split previews expose their final row and complete scrollports',async({page},info)=>{
  const dir=await mkdtemp(join(tmpdir(),'amux-file-list-'));
  try{
    await Promise.all(Array.from({length:150},(_,i)=>writeFile(join(dir,String(i).padStart(3,'0')+'.md'),'Evidence '+i)));
    await writeFile(join(dir,'zzz-final.md'),longText);
    await writeFile(join(dir,'zzz-final.html'),'<html><body>'+Array.from({length:150},(_,i)=>'<p>Row '+i+'</p>').join('')+'<p id="end">'+end+'</p></body></html>');
    await writeFile(join(dir,'split.pdf'),pdfBytes());
    await page.goto('/');
    for(const size of sizes){
      await page.setViewportSize(size);
      await page.evaluate(p=>{(window as any).openExplore(p);return (window as any).loadFiles(p);},dir);
      const row=page.locator('#files-body .fe-row').filter({hasText:'zzz-final.md'});
      await row.scrollIntoViewIfNeeded();
      expect(await row.evaluate(el=>{const r=el.getBoundingClientRect();return r.top>=0&&r.bottom<=innerHeight;})).toBe(true);
      if(await row.getAttribute('title')==='Double-click to open')await row.dblclick();else await row.click();
      await expect(page.locator('#file-body')).toContainText(end);await finalLine(page);
      await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
    }
    await page.goto('/?peekEmbed=file-bottom-worker');
    for(const size of [sizes[0],sizes[1],sizes[3]]){
      await page.setViewportSize(size);
      await page.evaluate(p=>{(window as any)._peekSplitOpen('files');return (window as any)._psfLoad(p);},dir);
      await page.locator('#psf-body .fe-row').filter({hasText:'zzz-final.md'}).click();
      await fits(page,'#psf-body .file-overlay-body');await finalLine(page,'#psf-body .file-overlay-body');
      await page.evaluate(p=>(window as any)._psfViewFile(p),join(dir,'zzz-final.html'));
      const frame=page.frameLocator('#psf-body iframe');await frame.locator('#end').waitFor();
      await fits(page,'#psf-body iframe');
      await frame.locator('html').evaluate(el=>{el.ownerDocument.scrollingElement!.scrollTop=el.ownerDocument.scrollingElement!.scrollHeight;});
      expect(await frame.locator('#end').evaluate(el=>{const r=el.getBoundingClientRect();return r.top>=0&&r.bottom<=innerHeight;})).toBe(true);
      if(size.height===350)await page.screenshot({path:info.outputPath('worker-split-final-paragraph.png')});
      await page.evaluate(p=>(window as any)._psfViewFile(p),join(dir,'split.pdf'));
      const pdf=page.locator('#psf-body .file-pdf');
      await expect(pdf.locator('.pdf-page')).toHaveCount(3,{timeout:30000});await fits(page,'#psf-body .file-pdf');
      await pdf.evaluate(el=>{el.scrollTop=el.scrollHeight;});
      expect(await pdf.locator('.pdf-page').last().evaluate(el=>{const r=el.getBoundingClientRect();return r.bottom<=innerHeight&&r.bottom>0;})).toBe(true);
      await expect.poll(()=>page.evaluate(async()=>{
        const r=await (await fetch('/api/client-debug?kind=file-pdf-render')).json();
        return r.beacons.some((b:any)=>b.body.viewer==='split'&&b.body.verdict==='rendered'&&b.body.pages===3);
      })).toBe(true);
    }
  }finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

test('PDF CDN fallback notice and full download remain reachable in a short window',async({page},info)=>{
  const dir=await mkdtemp(join(tmpdir(),'amux-pdf-fallback-'));
  try{
    const bytes=pdfBytes(),file=join(dir,'fallback.pdf');await writeFile(file,bytes);
    await page.route('**/pdfjs-dist@*/build/pdf.min.js',r=>r.abort());
    await page.goto('/');await page.setViewportSize({width:1280,height:350});
    await page.evaluate(p=>(window as any).openFilePreview(p),file);
    const note='Inline rendering needs a connection — use Download to open offline.';
    await expect(page.locator('#file-body')).toContainText(note);await fits(page);await finalLine(page,'#file-body',true,note);
    await page.locator('#file-menu-btn').click();
    const downloading=page.waitForEvent('download');await page.locator('#file-download-btn').click();const download=await downloading;
    const downloaded=await readFile((await download.path())!);
    expect(createHash('sha256').update(downloaded).digest('hex')).toBe(createHash('sha256').update(bytes).digest('hex'));
    await download.delete();await page.screenshot({path:info.outputPath('pdf-fallback-final-notice.png')});
  }finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

test('computed Markdown node editor and raw view fit with its full instruction retained',async({page})=>{
  let runs=0;
  await page.route('**/api/files/mdai/run',r=>{runs++;return r.fulfill({status:503,json:{error:'Raw viewing must never run a model'}});});
  allowUnusedRoute(page,'**/api/files/mdai/run'); // Expected zero: raw viewing must not execute a node.
  const dir=await mkdtemp(join(tmpdir(),'amux-node-view-'));
  try{
    const file=join(dir,'node.mdai');await writeFile(file,longText);await page.goto('/');
    for(const size of sizes){
      await page.setViewportSize(size);
      await page.evaluate(p=>(window as any).openMdaiNode(p,{autorun:false}),file);
      await fits(page,'#mdai-body');
      await page.evaluate(()=>(window as any)._mdaiSetBottomTab('list'));
      await expect(page.locator('#mdai-body-ta')).toHaveValue(longText);
      await page.locator('#mdai-body').evaluate(el=>{el.scrollTop=el.scrollHeight;});
      await fits(page,'#mdai-body-ta');
      await page.locator('#mdai-menu-btn').click();
      await fits(page,'.explore-menu-popup');
      await page.getByRole('button',{name:'Open raw .mdai file',exact:true}).click();
      await expect(page.locator('#file-body')).toContainText(end);await fits(page);await finalLine(page);
      await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
    }
    expect(runs).toBe(0);
    await expect.poll(()=>page.evaluate(async()=>{const r=await (await fetch('/api/client-debug?kind=mdai-raw-view')).json();return r.beacons.some((b:any)=>b.body.verdict==='opened');})).toBe(true);
  }finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});

test('late directory and file responses cannot replace the worker current file',async({page})=>{
  const dir=await mkdtemp(join(tmpdir(),'amux-file-race-'));
  try{
    const oldFile=join(dir,'old.md'), currentFile=join(dir,'current.md');
    await writeFile(oldFile,'OLD_FILE_MUST_NOT_REPLACE_CURRENT');await writeFile(currentFile,longText);
    await page.goto('/?peekEmbed=file-bottom-worker');await page.setViewportSize(sizes[0]);
    await page.evaluate(()=>(window as any)._peekSplitOpen('files'));
    for(const [matcher,start] of [
      [/\/api\/ls\?path=/,() => page.evaluate(p=>{void (window as any)._psfLoad(p);},dir)],
      [/\/api\/file\?path=.*old\.md/,() => page.evaluate(p=>{void (window as any)._psfViewFile(p);},oldFile)],
    ] as const){
      let release!:()=>void, arrived!:()=>void, completed!:()=>void;
      const held=new Promise<void>(r=>release=r), seen=new Promise<void>(r=>arrived=r), done=new Promise<void>(r=>completed=r);
      await page.route(matcher,async route=>{
        const response=await route.fetch();arrived();await held;
        await route.fulfill({response});completed();
      });
      try{
        await start();await seen;
        await page.evaluate(p=>(window as any)._psfViewFile(p),currentFile);
        await expect(page.locator('#psf-body')).toContainText(end);
        const beacon=page.waitForRequest(r=>r.url().endsWith('/api/client-debug')&&r.postDataJSON()?.kind==='file-split-stale-response',{timeout:5000});
        release();await done;expect((await beacon).postDataJSON().verdict).toBe('discarded');
        await expect(page.locator('#psf-body')).toContainText(end);
        await expect(page.locator('#psf-body')).not.toContainText('OLD_FILE_MUST_NOT_REPLACE_CURRENT');
        await finalLine(page,'#psf-body .file-overlay-body');
      }finally{release();await cleanup('unroute held response', () => page.unroute(matcher));}
    }
  }finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});


test('capped PDF and large text disclose their limits and download the complete original',async({page})=>{
  const dir=await mkdtemp(join(tmpdir(),'amux-file-caps-'));
  try{
    const fixtures={'large.txt':Buffer.from('Large evidence.\n'.repeat(16000)+end),'many-pages.pdf':pdfBytes(31)};
    await page.goto('/');await page.setViewportSize(sizes[1]);
    for(const [name,bytes] of Object.entries(fixtures)){
      const file=join(dir,name);await writeFile(file,bytes);await page.evaluate(p=>(window as any).openFilePreview(p),file);
      const note=name.endsWith('.pdf')?'Showing 30 of 31 pages — use Download to open the full document.':'... (truncated at 200KB)';
      await expect(page.locator('#file-body')).toContainText(note,{timeout:30000});await fits(page);await finalLine(page,'#file-body',true,note);
      await page.locator('#file-menu-btn').click();
      const downloading=page.waitForEvent('download');await page.locator('#file-download-btn').click();const download=await downloading;
      expect(createHash('sha256').update(await readFile((await download.path())!)).digest('hex')).toBe(createHash('sha256').update(bytes).digest('hex'));
      await download.delete();await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
    }
  }finally { await cleanup('remove temp folder', () => rm(dir,{recursive:true,force:true})); }
});
