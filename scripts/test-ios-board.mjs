#!/usr/bin/env node
// Real Safari Simulator board regression. Requires an isolated amux API at
// AMUX_IOS_TEST_URL (default https://localhost:18854), booted Simulator and driver.
// Seeds 36 persistent test cards ONLY on that local high-port server; no worker
// commands. Run from repo root: node scripts/test-ios-board.mjs.
// AMUX_IOS_TEST_OUTPUT selects evidence; AMUX_IOS_CASE selects a case substring.
import assert from 'node:assert/strict';
import http from 'node:http';
import https from 'node:https';
import fs from 'node:fs/promises';
import path from 'node:path';
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
let frozenPeek;
const staticRoot=new URL('../crates/amux-dashboard/static/',import.meta.url);
const beacons=[];
const proxy=http.createServer(async(req,res)=>{
  try {
    if(req.url==='/sw.js'){res.writeHead(404);res.end('Service worker disabled so the test reads the candidate assets');return;}
    if(['/app.js','/app.css','/state/kernel.js'].includes(req.url)) {res.setHeader('cache-control','no-store');res.setHeader('content-type',req.url.endsWith('.css')?'text/css':'text/javascript');res.end(await fs.readFile(new URL(req.url.slice(1),staticRoot)));return;}
    const chunks=[];for await(const chunk of req)chunks.push(chunk);const body=Buffer.concat(chunks);
    if(req.url==='/api/client-debug'){try{beacons.push(JSON.parse(body))}catch{}}
    const upstream=https.request(new URL(req.url,base),{method:req.method,rejectUnauthorized:false,headers:{...req.headers,host:base.host,'accept-encoding':'identity'}},up=>{
      if((req.url==='/' || req.url.startsWith('/?')) && up.statusCode===200){

        const parts=[];up.on('data',b=>parts.push(b));up.on('end',async()=>{
          let html=await fs.readFile(new URL('index.html',staticRoot),'utf8');
          const bootstrap=Buffer.concat(parts).toString().match(/<!-- AMUX-BOOTSTRAP-BEGIN[\s\S]*?<!-- AMUX-BOOTSTRAP-END -->/);
          assert(bootstrap, 'The isolated server must supply its real auth bootstrap');
          html=html.replace(/<!-- AMUX-BOOTSTRAP-BEGIN[\s\S]*?<!-- AMUX-BOOTSTRAP-END -->/,bootstrap[0]);
          const init="localStorage.setItem('amux_walkthrough_done','1');delete Object.getPrototypeOf(navigator).serviceWorker;";
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
const before=JSON.parse((await request(new URL('/health',base))).bytes);
const results=[];let target;
async function record(name,fn){if(process.env.AMUX_IOS_CASE && !name.includes(process.env.AMUX_IOS_CASE))return;try{const evidence=await fn();results.push({name,passed:true,evidence});pass(name);}catch(e){results.push({name,passed:false,error:String(e)});console.log('FAIL '+name+': '+e);await shot('failed-'+results.length).catch(()=>{});}await fs.writeFile(path.join(output,'mobile-board-results.json'),JSON.stringify({measured:true,n_considered:results.length,build:before.build,results},null,2));}
const tap=selector=>api('action',{action:'click',selector});
const scroll=(dy,x=200,y=550,dx=0)=>api('action',{action:'scroll',dy,dx,x,y});
const geom=()=>evaluate(`({y:scrollY,w:innerWidth,h:innerHeight,v:visualViewport.height,sw:document.documentElement.scrollWidth,colsX:document.querySelector('#board-columns').scrollLeft,cols:document.querySelector('#board-columns').getBoundingClientRect().toJSON(),detail:document.querySelector('.board-detail-body').scrollTop,overlay:document.querySelector('#board-detail-overlay').classList.contains('active'),mode:boardViewMode})`);
const close=async()=>{if(await evaluate(`document.querySelector('#board-detail-overlay').classList.contains('active')`))await tap('#board-detail-overlay .overlay-header button');};
try{
 const rows=JSON.parse((await request(new URL('/api/board?done_limit=0',base))).bytes);
 for(let i=0;i<36;i++){
  if(rows.some(r=>r.title.startsWith('MobileBoardQA '+String(i).padStart(2,'0'))))continue;
  const data={title:'MobileBoardQA '+String(i).padStart(2,'0')+' — readable mobile task with a full title',status:i<24?'todo':'backlog',owner_type:'human',desc:Array.from({length:24},(_,j)=>'Paragraph '+j+': inspect this complete board description in Simulator Safari. Tap, scroll, and return without losing your place.').join('\n\n'),tags:['mobile-board-qa']};
  if(i===0)data.title='MobileBoardQA 00';
  const r=await request(new URL('/api/board',base),'POST',data);assert([200,201].includes(r.status),r.bytes.toString());
 }
 target=(await api('targets')).targets.find(t=>t.state==='Booted');assert(target);udid=target.udid;
 await api('start',{udid,url:origin+'/#view=board'});started=true;
 await until(`document.querySelector('#board-view').style.display!=='none' && boardItems.length>=36`,v=>v);
 await record('board initial visibility and viewport',async()=>{await shot('board-initial');const g=await geom();assert(g.sw<=g.w,JSON.stringify(g));assert.deepEqual(await evaluate('_headerLayoutCheck()'),[]);return g;});
 await record('list view native vertical scroll and exact card tap',async()=>{
  await tap('#bv-list');await scroll(350);await scroll(350);const g=await geom();assert(g.y>200,JSON.stringify(g));
  const row=await evaluate(`(()=>{const e=[...document.querySelectorAll('#board-columns .peek-issue-item')].find(e=>{const r=e.getBoundingClientRect();return r.top>100&&r.bottom<visualViewport.height-50});if(!e)return null;return {onclick:e.getAttribute('onclick'),text:e.innerText}})()`);assert(row,'visible row');
  const selector='#board-columns .peek-issue-item[onclick='+JSON.stringify(row.onclick)+']';await shot('board-list-scrolled');await tap(selector);
  const id=row.onclick.match(/openBoardDetail\('([^']+)'\)/)[1];await until('document.querySelector("#bd-key").textContent',v=>v===id);return {g,id,row};
 });
 await record('detail scroll, tabs, and Back retain list position',async()=>{
  assert((await geom()).overlay);const under=await evaluate('scrollY');await shot('board-detail');await scroll(330);const detail=await geom();assert(detail.detail>100,JSON.stringify(detail));await shot('board-detail-scrolled');
  await tap('#bd-tab-files');assert(await evaluate('document.querySelector("#bd-tab-files").classList.contains("active")'));
  await tap('#bd-tab-preview');await close();const after=await geom();assert(Math.abs(after.y-under)<3,JSON.stringify({under,after}));return {under,detail,after};
 });
 await record('search and clear with real keyboard',async()=>{
  await close();await tap('#board-search');await api('action',{action:'input',selector:'#board-search',text:'MobileBoardQA 00'});
  await until('_boardLastVisible.length',v=>v===1);await tap('#board-columns .peek-issue-item');assert((await evaluate('document.querySelector("#bd-title").value'))==='MobileBoardQA 00');await close();await tap('.board-search-wrap .search-clear');await until('_boardLastVisible.length',v=>v>=36);return {count:await evaluate('_boardLastVisible.length')};
 });
 await record('status columns native horizontal and vertical scrolling',async()=>{
  await close();await tap('#bv-status');await shot('board-columns-initial');
  await tap('#board-columns [data-col="backlog"] .board-col-collapse');
  await scroll(300);const start=await geom();await scroll(0,330,550,260);const horizontal=await geom();assert(horizontal.colsX>start.colsX+80,JSON.stringify({start,horizontal}));
  await scroll(0,70,550,-260);await scroll(350);const vertical=await geom();assert(vertical.y>horizontal.y+50,JSON.stringify({horizontal,vertical}));await shot('board-columns-scrolled');
  const card=await evaluate(`(()=>{const e=[...document.querySelectorAll('#board-columns .board-card')].find(e=>{const r=e.getBoundingClientRect();return r.left>=0&&r.right<=innerWidth&&r.top>80&&r.bottom<visualViewport.height-30});return e?.dataset.id})()`);assert(card,'visible card after scrolling');
  const position=await geom();
  await evaluate(`window.__boardTapEvents=[];['touchstart','touchend','mouseover','mousemove','click'].forEach(kind=>document.querySelector('#board-columns').addEventListener(kind,e=>window.__boardTapEvents.push({kind,target:e.target.className,card:e.target.closest('.board-card')?.dataset.id}),true));true`);
  await tap('#board-columns .board-card[data-id="'+card+'"]');
  const firstTap=await geom();const firstEvents=await evaluate('window.__boardTapEvents');
  if(!firstTap.overlay){await tap('#board-columns .board-card[data-id="'+card+'"]');const secondTap=await geom();const events=await evaluate('window.__boardTapEvents');await fs.writeFile(path.join(output,'tap-probe.json'),JSON.stringify({card,firstTap,firstEvents,secondTap,events},null,2));assert(firstTap.overlay,'First tap did not open the card; second tap overlay='+secondTap.overlay);}
  await until('document.querySelector("#bd-key").textContent',v=>v===card);await close();const returned=await geom();assert(Math.abs(returned.y-position.y)<3);assert(Math.abs(returned.colsX-position.colsX)<3);
  await evaluate('renderBoard();true');const refreshed=await geom();assert(Math.abs(refreshed.y-returned.y)<3);assert(Math.abs(refreshed.colsX-returned.colsX)<3);
  return {start,horizontal,vertical,card,position,returned,refreshed};
 });
 await record('quick filter Review displays only matching tasks',async()=>{
  await close();await tap('.board-quick-filters button:nth-child(5)');await until('boardSearchQuery',v=>v==='status:review');assert(await evaluate('_boardLastVisible.every(i=>i.status==="review")'));await shot('board-review-filter');await tap('.board-quick-filters button:first-child');return {cleared:await evaluate('boardSearchQuery')};
 });
 const after=JSON.parse((await request(new URL('/health',base))).bytes);assert.equal(before.build,after.build);
 await fs.writeFile(path.join(output,'mobile-board-results.json'),JSON.stringify({measured:true,n_considered:results.length,build:before.build,appVersion:await evaluate('APP_VER'),staticSource:staticRoot.pathname,beacons,simulator:target,userAgent:await evaluate('navigator.userAgent'),results},null,2));
 if(results.some(r=>!r.passed))process.exitCode=1;
 console.log('RESULT: '+results.filter(r=>r.passed).length+' passed, '+results.filter(r=>!r.passed).length+' failed');
}finally{if(started)await api('stop',{});proxy.closeAllConnections();await new Promise(r=>proxy.close(r));}
