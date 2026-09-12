#!/usr/bin/env node
// Real Safari Simulator tests through amux's shipped browser API. No fleet writes.
import assert from 'node:assert/strict';
import http from 'node:http';
import https from 'node:https';
import fs from 'node:fs/promises';
import path from 'node:path';
import { chromium } from 'playwright';
import { messageScenarios } from '../e2e/ios-message-scenarios.mjs';
const messages=messageScenarios();
const base = new URL(process.env.AMUX_IOS_TEST_URL || 'https://localhost:18854');
assert(['localhost','127.0.0.1','[::1]'].includes(base.hostname) && Number(base.port) >= 18000,
  'Use an isolated local test server on a port >=18000; production is refused');
const output = path.resolve(process.env.AMUX_IOS_TEST_OUTPUT || 'scratch/ios-simulator-review');
await fs.mkdir(output,{recursive:true});
function request(url, method='GET', body, headers={}) {
  return new Promise((resolve,reject)=>{
    const bytes=body===undefined ? undefined : Buffer.from(JSON.stringify(body));
    const req=(url.protocol==='https:' ? https : http).request(url,{method,rejectUnauthorized:false,
      headers:{...(bytes?{'content-type':'application/json','content-length':bytes.length}:{}),...headers}},res=>{
      const chunks=[];res.on('data',b=>chunks.push(b));res.on('end',()=>resolve({status:res.statusCode,bytes:Buffer.concat(chunks),headers:res.headers}));res.on('error',reject);
    });
    const timer=setTimeout(()=>req.destroy(new Error('Test HTTP operation exceeded 200s')),200000);
    req.on('close',()=>clearTimeout(timer));req.on('error',reject);req.end(bytes);
  });
}
const runId=Date.now().toString(36);
const session='ios-test';let udid,started=false,checks=0;
async function api(verb,body,expected=200,worker=session,device=udid) {
  const result=await request(new URL('/api/browser/ios/'+verb+(body===undefined?'?session='+worker:''),base),body===undefined?'GET':'POST',body===undefined?undefined:{...body,session:worker},device?{'X-Amux-Simulator':device}:{});
  const data=JSON.parse(result.bytes);assert.equal(result.status,expected,JSON.stringify(data));return data;
}
async function evaluate(script){return (await api('action',{action:'eval',script})).data.result;}
async function until(script,predicate,timeout=20000){
  const deadline=Date.now()+timeout;let value;
  do {try{value=await evaluate(script);if(predicate(value))return value;}catch(error){value={readError:String(error)};}await new Promise(r=>setTimeout(r,200));}while(Date.now()<deadline);
  throw new Error('Timed out: '+script+'; last '+JSON.stringify(value)+'; page '+JSON.stringify(await evaluate('({url:location.href,scenario:window.__iosTestScenario,head:document.head.innerHTML.slice(0,300)})')));
}
function pass(name){checks++;console.log('PASS '+name);}
async function shot(name){const data=await api('screenshot');const png=await request(new URL(data.serve,base));assert.equal(png.status,200);await fs.writeFile(path.join(output,name+'.png'),png.bytes);}
let mode='plain',effectReads=0,prefPosts=0,beacons=[],hangingAborts=0;
const receipts=()=> (mode==='hang'?['ios_hang','ios_healthy']:[mode==='recovery'?'ios_recovery':'ios_replay']).map(id=>({
 id,command:{id:'cmd_'+id,kind:'environment.post',target:{primitive:'environment',id:'prefs'}},request:{method:'POST',path:'/api/prefs'},
 phase:mode==='replay'?'sending':'applied',measured:true,n_considered:1,acknowledgement:{status:200,applied:true},effects:[],
 feedback:{required:true,message:mode==='replay'?'Sending':'Completed'},created_at:Date.now(),updated_at:Date.now()}));
const proxy=http.createServer(async(req,res)=>{
  try {
    const fixture=messages.route(mode,req);
    if(fixture){res.writeHead(fixture.status,{'content-type':'application/json','cache-control':'no-store'});res.end(JSON.stringify(fixture.body));return;}
    if(req.url==='/sw.js'){res.writeHead(404);res.end('Service worker disabled for isolated fault injection');return;}
    if(['/app.js','/app.css','/state/kernel.js'].includes(req.url)) {res.setHeader('cache-control','no-store');res.setHeader('content-type',req.url.endsWith('.css')?'text/css':'text/javascript');res.end(await fs.readFile('crates/amux-dashboard/static'+req.url));return;}
    if(req.url.startsWith('/form')) {
      res.setHeader('content-type','text/html');res.end('<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><title>iOS browser test</title><style>button,input{font-size:20px;margin:20px}body{height:2400px}</style><input id="text" aria-label="Test text"><button id="button" onclick="this.textContent=\'Clicked\'">Click me</button><a href="/form?next">Next page</a>');return;
    }
    if(req.url.startsWith('/api/interactions/ios_') && req.url.endsWith('/effects')) {
      effectReads++;
      if(req.url.includes('ios_hang')){res.writeHead(200,{'content-type':'application/json'});res.write('{"effects":');req.on('close',()=>hangingAborts++);return;}
      if(mode==='recovery' && effectReads===1){res.writeHead(503,{'content-type':'application/json'});res.end('{"error":"test effects unavailable"}');return;}
      res.setHeader('content-type','application/json');res.end(JSON.stringify({measured:true,n_considered:1,effects:[{id:'recovered-effect',kind:'pref.updated'}]}));return;
    }
    if(req.method==='POST'&&req.url==='/api/prefs')prefPosts++;
    const chunks=[];for await(const chunk of req)chunks.push(chunk);const body=Buffer.concat(chunks);
    if(req.url==='/api/client-debug'){try{beacons.push(JSON.parse(body))}catch{}}
    const upstream=https.request(new URL(req.url,base),{method:req.method,rejectUnauthorized:false,headers:{...req.headers,host:base.host,'accept-encoding':'identity'}},up=>{
      if((req.url==='/' || req.url.startsWith('/?')) && up.statusCode===200){

        const parts=[];up.on('data',b=>parts.push(b));up.on('end',async()=>{
          let html=await fs.readFile('crates/amux-dashboard/static/index.html','utf8');
          const bootstrap=Buffer.concat(parts).toString().match(/<!-- AMUX-BOOTSTRAP-BEGIN[\s\S]*?<!-- AMUX-BOOTSTRAP-END -->/);
          assert(bootstrap, 'The isolated server must supply its real auth bootstrap');
          html=html.replace(/<!-- AMUX-BOOTSTRAP-BEGIN[\s\S]*?<!-- AMUX-BOOTSTRAP-END -->/,bootstrap[0]);
          const init=`window.__iosTestScenario=${JSON.stringify(mode)};localStorage.setItem('amux_walkthrough_done','1');delete Object.getPrototypeOf(navigator).serviceWorker;`+
            (mode==='plain'||messages.active(mode)?'':`if(!sessionStorage.getItem(${JSON.stringify('ios-seeded-'+runId+'-'+mode)})){sessionStorage.setItem(${JSON.stringify('ios-seeded-'+runId+'-'+mode)},'1');localStorage.setItem('amux_interactions_v2',${JSON.stringify(JSON.stringify(receipts()))});}`)+messages.seed(mode,runId);
          html=html.replace('<head>','<head><script>'+init+'</script>');
          res.writeHead(200,{'content-type':'text/html','cache-control':'no-store'});res.end(html);
        });
      }else{res.writeHead(up.statusCode,up.headers);up.pipe(res);}
    });
    upstream.on('error',e=>{if(!res.headersSent)res.writeHead(502);res.end(String(e));});
    res.on('close',()=>upstream.destroy());upstream.end(body);
  } catch(e){res.writeHead(500);res.end(String(e));}
});
await new Promise(r=>proxy.listen(0,'127.0.0.1',r));const origin='http://127.0.0.1:'+proxy.address().port;
let browser;
const before=JSON.parse((await request(new URL('/health',base))).bytes);
console.log('SOURCE before:',JSON.stringify({commit:before.commit,build:before.build,origin:base.origin}));
try {
  const inventory=await api('targets');assert.equal(inventory.measured,true);const target=inventory.targets.find(t=>t.state==='Booted');assert(target,'Boot an iOS device in Simulator first');udid=target.udid;
  const result=await api('start',{udid,url:origin+'/form'});started=true;
  const native=result.capabilities.automationName==='XCUITest';
  assert.equal(native?result.capabilities.udid:result.capabilities['safari:deviceUDID'],udid);
  console.log('TARGET:',JSON.stringify(result.capabilities));pass('dynamic discovery and real iOS Safari launch');
  assert.equal(await evaluate('Promise.resolve(42)'),42,'eval must await browser promises');
  await api('action',{action:'eval',script:'location.href'},409,'other-worker');
  await api('stop',{},409,'other-worker');
  await api('action',{action:'eval',script:'location.href'},409,session,'wrong-device');
  await api('action',{action:'viewport',device:'iphone'},501);pass('owner/device guards and unsupported action refusal');
  await api('action',{action:'click',selector:'#text'});await until('document.activeElement?.id',v=>v==='text');const typed=await api('action',{action:'type',text:'Simulator input'});assert.equal(typed.data.result.input_method,native?'xcuitest':'webkit-editor');
  assert.equal(await evaluate('document.querySelector("#text").value'),'Simulator input');
  await api('action',{action:'key',key:'Tab'});
  const rect=await evaluate('(()=>{const r=document.querySelector("#button").getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()');
  await api('action',native?{action:'click',selector:'#button'}:{action:'click',...rect});assert.equal(await evaluate('document.querySelector("#button").textContent'),'Clicked');
  const state=await api('state');assert(state.elements.some(e=>e.label==='Clicked'));
  const scrolled=await api('action',{action:'scroll',dy:300});if(native)assert.equal(scrolled.data.result.input_method,'xcuitest');await until('scrollY',v=>v>0);
  await api('start',{udid,url:origin+'/form?next'});await api('action',{action:'back'});await until('location.pathname',v=>v==='/form');
  for(let i=0;i<5 && await evaluate('scrollY')>0;i++)await api('action',{action:'scroll',dy:-600});await shot('simulator-form');pass((native?'Native':'WebKit')+' text editing, key dispatch, button activation, element state, scroll, back and PNG');

  // Exercise the actual desktop Browser controls driving the separate simulator.
  browser=await chromium.launch({headless:true});
  const page=await browser.newPage({ignoreHTTPSErrors:true,viewport:{width:1280,height:800},serviceWorkers:'block'});
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  for(const asset of ['app.js','app.css','state/kernel.js'])await page.route('**/'+asset,r=>r.fulfill({path:'crates/amux-dashboard/static/'+asset,contentType:asset.endsWith('.css')?'text/css':'text/javascript'}));
  await page.route(base.origin+'/',r=>r.fulfill({path:'crates/amux-dashboard/static/index.html',contentType:'text/html'}));
  await page.goto(base.origin);await page.waitForFunction(()=>!!window.__amuxState);
  await page.evaluate(()=>{(0,eval)('_bwSession="ios-test"');window.switchView('browser');});
  await page.waitForFunction(id=>Array.from(document.querySelector('#bw-backend').options).some(o=>o.value==='ios:'+id),udid);
  await page.selectOption('#bw-backend','ios:'+udid);assert.equal(await page.locator('#bw-profile').isDisabled(),true);
  await page.fill('#bw-url',origin+'/form');await page.locator('#browser-view button.primary').first().click();
  await page.waitForFunction(()=>{const img=document.querySelector('#bw-img');return img.complete&&img.naturalWidth>0&&img.style.display!=='none';});
  await page.evaluate(()=>window._bwStopLive());await page.screenshot({path:path.join(output,'browser-picker-desktop.png')});
  await page.setViewportSize({width:375,height:750});await page.screenshot({path:path.join(output,'browser-picker-mobile.png')});
  assert(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
  await page.selectOption('#bw-backend','');assert.equal(await page.locator('#bw-profile').isDisabled(),false);
  assert.equal(await page.locator('#bw-img').isVisible(),false);await browser.close();browser=null;
  pass('Browser picker, Go, remote simulator frame, mobile layout and target switching');

  for(const scenario of ['recovery','hang','replay']) {
    mode=scenario;effectReads=0;prefPosts=0;beacons=[];
    await api('start',{udid,url:origin+'/?iosCase='+mode+'-'+runId});await until('window.__iosTestScenario+":"+!!window.__amuxState',v=>v===mode+':true');
    if(mode==='recovery'){
      await until('window.__amuxInteractions.get("ios_recovery")?.effect_sync?.phase',v=>v==='failed');
      await api('action',{action:'click',selector:'#interaction-feedback > summary'});await shot('simulator-effects-retry');
      assert((await evaluate('document.querySelector("article[data-interaction-id=ios_recovery]").innerText')).includes('Changes unavailable; retrying'));
      await api('start',{udid,url:origin+'/?iosCase='+mode+'-'+runId});
      await until('window.__amuxInteractions?.get("ios_recovery")?.effect_sync?.phase',v=>v==='synced');
      assert.equal(prefPosts,0);assert.equal(effectReads,2);pass('settled receipt reload recovery, two effects reads and zero command POSTs');
    }else if(mode==='hang'){
      await until('window.__amuxInteractions.get("ios_hang")?.effect_sync?.phase',v=>v==='failed');
      await until('window.__amuxInteractions.get("ios_healthy")?.effects.length',v=>v===1);
      assert(beacons.some(b=>b.verdict==='interaction_reconcile_failed'&&b.interaction_id==='ios_hang'));assert(hangingAborts>0);
      pass('hung effects body abort, receipt-specific failure beacon and healthy peer progress');
    }else{
      await until('window.__amuxInteractions.get("ios_replay")?.measured',v=>v===false);
      await evaluate(`fetch('/api/prefs',{method:'POST',headers:{'Content-Type':'application/json','X-Amux-Interaction-Id':'ios_replay'},body:JSON.stringify({key:'ios-replay-measured',value:'1'})}).then(r=>{if(!r.ok)throw Error(r.status);return true})`);
      const receipt=await until('window.__amuxInteractions.get("ios_replay")',v=>v?.measured===true&&v.phase==='applied');
      assert.equal(receipt.why_unmeasured,undefined);
      await api('action',{action:'click',selector:'#interaction-feedback > summary'});await shot('simulator-replay-recovered');
      assert(!(await evaluate('document.querySelector("article[data-interaction-id=ios_replay]").innerText')).includes('Page reloaded before completion'));
      pass('real replay acknowledgement removes obsolete uncertainty and warning');
    }
    assert(await evaluate('document.documentElement.scrollWidth<=innerWidth'));
  }
  await messages.run({setMode:value=>{mode=value;},navigate:suffix=>api('start',{udid,url:origin+suffix}),until,evaluate,api,shot,pass,runId});
  { // Board editing is a baseline native regression, including keyboard occlusion.
    mode='plain';
    const coverage=[];
    const visible=selector=>`(()=>{const e=document.querySelector(${JSON.stringify(selector)});return !!e&&!!e.getClientRects().length&&getComputedStyle(e).visibility!=='hidden'})()`;
    for (const surface of (process.env.AMUX_IOS_LIFECYCLE === '1' ? ['sessions','board','groups','calendar','scheduler','files','mdai','proxies','email','connectors','logs','messages','skills','sql','map','metrics','cost','torrents','terminal','browser'] : [])) {
      try {
        await api('start',{udid,url:origin+'/#view=sessions'});await until('!!window.__amuxState && !document.querySelector("#peek-overlay").classList.contains("active")',v=>v);
        const tab='#tab-'+surface;
        if (!await evaluate(visible(tab))) {
          await api('action',{action:'click',selector:'.tab-customize-wrap > .tab-customize-btn'});
          await api('action',{action:'click',selector:`#tab-customizer-menu [data-tab-id="${surface}"] input[type=checkbox]`});
          await api('action',{action:'click',selector:'.tab-customize-wrap > .tab-customize-btn'});
        }
        await api('action',{action:'click',selector:tab});
        const view=surface==='sessions'?'#session-view':'#'+surface+'-view';
        await until(visible(view),v=>v);
        const inventory=await evaluate(`(()=>{const root=document.querySelector(${JSON.stringify(view)});return {width:innerWidth,scrollWidth:document.documentElement.scrollWidth,headerClipped:_headerLayoutCheck(),controls:Array.from(root.querySelectorAll('button,input,select,textarea,a')).filter(e=>e.getClientRects().length).map(e=>{const r=e.getBoundingClientRect();return {tag:e.tagName,id:e.id,label:(e.getAttribute('aria-label')||e.title||e.innerText||e.placeholder||'').slice(0,120),width:r.width,height:r.height,disabled:!!e.disabled}})}})()`);
        await shot('lifecycle-view-'+surface);
        assert(inventory.scrollWidth<=inventory.width,'horizontal page overflow');assert.deepEqual(inventory.headerClipped,[]);
        coverage.push({case:'LC-VIEW:'+surface,verdict:'passed',scope:'native navigation, visible panel, control inventory, overflow; aesthetics requires screenshot review',...inventory});
        pass('native lifecycle view '+surface);
      }catch(error){await shot('lifecycle-failed-'+surface).catch(()=>{});console.log('HIT TARGET',await evaluate(`(()=>{const e=document.querySelector('#tab-${surface}');if(!e)return null;const r=e.getBoundingClientRect();const h=document.elementFromPoint(r.x+r.width/2,r.y+r.height/2);return {rect:r.toJSON(),hit:h?.outerHTML.slice(0,200)}})()`));coverage.push({case:'LC-VIEW:'+surface,verdict:'failed',error:String(error)});console.log('FAIL native lifecycle view '+surface+': '+error);}
      await fs.writeFile(path.join(output,'native-lifecycle.json'),JSON.stringify(coverage,null,2));
    }
    try {
      await api('start',{udid,url:origin+'/#view=sessions'});await until('!!window.__amuxState && !document.querySelector("#peek-overlay").classList.contains("active")',v=>v);
      await api('action',{action:'click',selector:'#tab-board'});await api('action',{action:'click',selector:'.board-new-btn'});
      const title='iOS lifecycle '+Date.now()+' — preserve this complete title across creation, reload and search';
      await api('action',{action:'input',selector:'#be-title',text:title});
      await api('action',{action:'input',selector:'#be-desc',text:'Acceptance: keep this exact simulator lifecycle note.'});
      await shot('lifecycle-board-new');
      const saved=await api('action',{action:'click',selector:'.be-save'});
      if(native)assert.equal(saved.data.result.keyboard_dismissed,true,'The real keyboard must be dismissed before tapping below it');
      const rows=await until(`fetch('/api/board?done_limit=0').then(r=>r.json()).then(rows=>rows.filter(r=>r.title===${JSON.stringify(title)}))`,v=>v?.length===1);
      const card=rows[0];
      const full=await evaluate(`fetch('/api/board/'+${JSON.stringify(card.id)}).then(r=>r.json())`);
      assert.equal(full.desc,'Acceptance: keep this exact simulator lifecycle note.','A page tap must not append a native keyboard character');
      await api('start',{udid,url:origin+'/#issue='+encodeURIComponent(card.id)});await until('document.querySelector("#bd-key")?.textContent',v=>v===card.id);
      assert((await evaluate('document.querySelector("#bd-preview").innerText')).includes('exact simulator lifecycle note'));
      await shot('lifecycle-board-persisted');await api('start',{udid,url:origin+'/#issue='+encodeURIComponent(card.id)});
      await until('document.querySelector("#bd-key")?.textContent',v=>v===card.id);
      coverage.push({case:'LC-BOARD',verdict:'passed',id:card.id,scope:'native form editing, UI save, exact one server card, detail and reload persistence'});pass('native board create/detail/reload');
    }catch(error){coverage.push({case:'LC-BOARD',verdict:'failed',error:String(error)});console.log('FAIL native board journey: '+error);}
    await fs.writeFile(path.join(output,'native-lifecycle.json'),JSON.stringify(coverage,null,2));
    assert(!coverage.some(r=>r.verdict==='failed'),'Native lifecycle has failures; see native-lifecycle.json');
  }
  const after=JSON.parse((await request(new URL('/health',base))).bytes);assert.equal(after.build,before.build);
  console.log('SOURCE after:',JSON.stringify({commit:after.commit,build:after.build}));
  console.log(`RESULT: ${checks} passed, 0 failed; native input covered; background/physical-device behavior NOT covered`);
} finally {
  if(browser)await browser.close();
  if(started)await api('stop',{});
  proxy.closeAllConnections();await new Promise(r=>proxy.close(r));
}
