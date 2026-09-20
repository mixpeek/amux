// Actual Chromium executes shipped Projects and file preview functions over fixture HTTP.
import { chromium } from 'playwright';
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
const source=readFileSync('crates/amux-dashboard/static/app.js','utf8');
const fn=(start,end)=>source.slice(source.indexOf(start),source.indexOf(end,source.indexOf(start)));
const browser=await chromium.launch({headless:true});
try {
 const page=await browser.newPage();
 await page.route('https://project.test/',r=>r.fulfill({contentType:'text/html',body:'<div id="projects-view"></div><div id="file-overlay"><div id="file-title"></div><div id="file-subpath"></div><div id="file-body"></div><div id="file-view-tabs"></div><div id="file-tab-preview"></div><div id="file-tab-raw"></div><div id="file-tab-edit"></div><div id="file-tab-teleprompter"></div><div id="file-tab-search"></div><div id="file-tab-readaloud"></div><div id="file-save-btn"></div><div id="file-edit-wrap"></div><button id="file-download-btn"></button></div>'}));
 await page.route('https://project.test/api/file?**',r=>r.fulfill({contentType:'application/json',body:JSON.stringify({path:new URL(r.request().url()).searchParams.get('path'),content:'# Verified report\n<script>window.compromised=true</script>',is_markdown:true})}));
 await page.goto('https://project.test/');
 await page.addScriptTag({content:`
 const API='';let activeView='projects',peekSessionDir='',_fileData=null,_fileViewMode='preview';const _FILE_CACHE_MAX=0;
 const esc=s=>String(s).replaceAll('&','&amp;').replaceAll('"','&quot;').replaceAll('<','&lt;');const escJs=s=>String(s);const _idb={getFile:async()=>null};
 ${fn('async function openFilePreview(', 'function closeFilePreview(')}
 ${fn('function setFileViewMode(', '// ---------- Markdown in-page search')}
 ${source.slice(source.indexOf('// Project-owned outcomes.'))}
 ${fn('function _renderFileBody(', 'function _fileBindAnchors(')}
 let _mdSearchHits=[];const _readPosDetach=()=>{},_bindReadPosDiv=()=>{};
 const data={project:{name:'sample',revision:1,policy:{enabled:true,paused:false,coordinator:{provider:'codex',model:'astra'},executor:{provider:'codex',model:'astra'}}},commands:[],migrations:[],usage:{},cards:[{id:'A',title:'Report output',phase:'verified',evidence:'Do not link /tmp/guessed.mdai',execution_plan:{execution:{stage:'verified',retained_assets:[{source:{path:'report.md',sha256:'a'.repeat(64)},head:'b'.repeat(40),path:'/private/artifacts/project-reports/'+ 'a'.repeat(64)+'.md'},{source:{path:'attack.mdai',sha256:'a'.repeat(64)},path:'/tmp/attack.mdai'}]}}}]};
 _projectStorage=(key)=>key==='selected'?'sample':'';_projectRequest=async(path)=>path?data:{projects:[data.project]};
 `});
 await page.evaluate(()=>_projectsLoad());
 assert.equal(await page.locator('.project-report-asset').count(),1);
 await page.getByRole('button',{name:'report.md',exact:true}).click();
 await page.waitForFunction(()=>document.getElementById('file-body').textContent.includes('# Verified report'));
 assert.equal(await page.evaluate(()=>_fileData.readOnly),true);
 assert.equal(await page.locator('#file-view-tabs').evaluate(e=>e.style.display),'none');
 await page.evaluate(()=>setFileViewMode('edit'));
 assert.equal(await page.evaluate(()=>_fileViewMode),'raw');
 assert.equal(await page.evaluate(()=>window.compromised),undefined);
 assert.equal(await page.locator('#file-subpath').evaluate(e=>e.onclick),null);
 console.log('project assets UI: PASS (explicit link, retained preview, inert prose/HTML/mdai, read-only)');
} finally {await browser.close();}
