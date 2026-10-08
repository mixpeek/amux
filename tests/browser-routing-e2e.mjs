// Real-browser acceptance: HTTP assertions, screenshots, persistent storage,
// isolated profiles, deliberate transport failures, and no replay of mutations.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,existsSync,copyFileSync} from 'node:fs';
import {join,resolve} from 'node:path';
import {tmpdir} from 'node:os';
import {createHash} from 'node:crypto';
import {chromium} from 'playwright';
import {run,api} from '../scripts/browser-route-driver.mjs';
const base=process.env.AMUX_ROUTING_E2E_BASE;
const home=process.env.AMUX_ROUTING_E2E_HOME;
assert(base&&home,'point at an isolated candidate server and its AMUX_HOME');
const evidence=resolve(process.env.AMUX_ROUTING_EVIDENCE||'work/browser-routing-evidence');mkdirSync(evidence,{recursive:true});
const session='routing-e2e';const profile='routing-proof';const chrome=process.env.AMUX_ROUTING_CHROME||'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const checks=[];const prove=(label,condition)=>{assert(condition,label);checks.push(label);console.log('PASS '+label);};
let counter=0;const submissions=[];const token='route-test-session-'+Date.now();
const site=createServer((req,res)=>{
  if(req.url==='/prove'){let body='';req.on('data',c=>body+=c);req.on('end',()=>{submissions.push(body);counter++;res.end('confirmed');});return;}
  res.setHeader('Content-Type','text/html');
  const logged=(req.headers.cookie||'').includes('route_session='+token);
  if(req.url==='/seed')res.setHeader('Set-Cookie',`route_session=${token}; Path=/; HttpOnly; Max-Age=86400; SameSite=Lax`);
  res.end(`<title>Browser routing proof</title><h1>${req.url==='/seed'||logged?'Authenticated route proof':'Logged out'}</h1><p id="count">${counter}</p><input id="proof" aria-label="Proof" value=""><button id="submit" onclick="fetch('/prove',{method:'POST',body:document.querySelector('#proof').value}).then(()=>document.querySelector('#count').textContent='confirmed')">Submit proof</button>`);
});await new Promise(r=>site.listen(0,'0.0.0.0',r));
const host=process.env.AMUX_ROUTING_FIXTURE_HOST||'127.0.0.1';const siteUrl=`http://${host}:${site.address().port}`;
const source=mkdtempSync(join(tmpdir(),'amux-route-source-'));
const profileDir=join(home,'playwright-auth','profiles',profile);mkdirSync(profileDir,{recursive:true});
const fixture=await chromium.launchPersistentContext(source,{executablePath:chrome,headless:true,ignoreDefaultArgs:['--use-mock-keychain']});
let page=fixture.pages()[0];await page.goto(siteUrl+'/seed');await page.evaluate(async()=>{localStorage.setItem('route-local','persisted');await new Promise((r,j)=>{const q=indexedDB.open('route-auth',1);q.onupgradeneeded=()=>q.result.createObjectStore('tokens');q.onsuccess=()=>{const tx=q.result.transaction('tokens','readwrite');tx.objectStore('tokens').put('persisted','marker');tx.oncomplete=r;tx.onerror=j;};});});
const cookies=await fixture.cookies();await fixture.close();
writeFileSync(join(profileDir,'cookies.json'),JSON.stringify(cookies),{mode:0o600});writeFileSync(join(profileDir,'state.json'),JSON.stringify({cookies,origins:[]}),{mode:0o600});
const primary={native_profile:profile,chrome_profile:'Default',cua_profile:profile,allow_cua:false};
const ctx={base,home,session,config:primary,chrome_root:source,chrome_binary:chrome};
const request=(verb,body={},context=ctx)=>run({context,verb,body});
const poll=async(fn)=>{const end=Date.now()+10000;let v;do{v=await fn();if(v)return v;await new Promise(r=>setTimeout(r,100));}while(Date.now()<end);throw Error('condition did not become true');};
const getText=async context=>{const d=await request('state',{},context);return JSON.stringify(d.data||d);};
async function interact(context,label){
  await poll(async()=> (await getText(context)).includes('Authenticated route proof'));
  await request('action',{action:'click',selector:'#proof'},context);
  const t=await request('action',{action:'type',text:label},context);prove(label+' type sent',!t.error);
  const clicked=await request('action',{action:'click',selector:'#submit'},context);prove(label+' click sent',!clicked.error);
  await poll(async()=>submissions.includes(label));prove(label+' server received exact proof',submissions.includes(label));
  const shot=await request('screenshot',{},context);prove(label+' real PNG',existsSync(shot.path)&&readFileSync(shot.path).subarray(0,8).equals(Buffer.from([137,80,78,71,13,10,26,10])));copyFileSync(shot.path,join(evidence,label+'.png'));
}
let proxy;
try {
  await poll(async()=>{try {return (await api(base,'/health')).status==='ok';}catch{return false;}});
  const native=await request('start',{url:siteUrl+'/protected',profile});writeFileSync(join(evidence,'native-start.json'),JSON.stringify(native,null,2));prove('native route launched',native.route?.backend==='amux');
  await interact(ctx,'native');prove('cookie import consumed once',existsSync(join(profileDir,'import-receipt.json'))&&!existsSync(join(profileDir,'cookies.json'))&&!existsSync(join(profileDir,'state.json')));
  await request('stop');const restarted=await request('start',{url:siteUrl+'/protected',profile});prove('native route restarted',restarted.route?.backend==='amux');writeFileSync(join(evidence,'restart-state.json'),JSON.stringify(await request('state'),null,2));const nb=await chromium.connectOverCDP(`http://127.0.0.1:${restarted.cdp_port}`);const jar=await nb.contexts()[0].cookies();console.log('Restart cookie:',jar.map(c=>({name:c.name,domain:c.domain,matches:c.value===token,length:c.value.length,fixtureSuffix:c.value.startsWith("route-test-session-")?c.value.slice(-13):"not-fixture"})));await nb.close();await poll(async()=> (await getText(ctx)).includes('Authenticated route proof'));prove('native login survives process restart',true);await request('stop');
  let failActions=false;
  proxy=createServer(async(req,res)=>{
    if(req.url.startsWith('/api/browser/start')||(failActions&&req.url.startsWith('/api/browser/action'))){res.statusCode=503;res.setHeader('Content-Type','application/json');res.end(JSON.stringify({error:'deliberate native transport fault'}));return;}
    let body='';for await(const c of req)body+=c;
    try {const d=await api(base,req.url,req.method,body?JSON.parse(body):undefined,req.headers['x-amux-session']);res.setHeader('Content-Type','application/json');res.end(JSON.stringify(d));}catch(e){res.statusCode=e.status||502;res.end(JSON.stringify({error:e.message}));}
  });await new Promise(r=>proxy.listen(0,'127.0.0.1',r));
  const cdCtx={...ctx,session:'routing-e2e-cdp',base:`http://127.0.0.1:${proxy.address().port}`};
  const cd=await request('start',{url:siteUrl+'/protected',profile},cdCtx);prove('forced native failure reaches real CDP',cd.route?.backend==='cdp'&&cd.route.attempts[0].verdict==='failed');
  await interact(cdCtx,'cdp');
  const storage=await request('action',{action:'eval',script:`(async()=>({ls:localStorage.getItem('route-local'),idb:await new Promise(r=>{const q=indexedDB.open('route-auth');q.onsuccess=()=>{const x=q.result.transaction('tokens').objectStore('tokens').get('marker');x.onsuccess=()=>r(x.result)}})}))()`},cdCtx);prove('full Chrome snapshot preserves localStorage and IndexedDB',storage.data?.result?.ls==='persisted'&&storage.data?.result?.idb==='persisted');
  const identityRefused=await request('start',{url:siteUrl+'/protected',profile},{...cdCtx,session:'routing-e2e-mismatch',identity:'personal@example.test',chrome_identity:'work@example.test',config:{...primary,allow_cua:true}});prove('fallback refuses a different account identity',identityRefused.status===403&&!identityRefused.attempts.some(a=>a.backend==='cua'));
  const second={...cdCtx,session:'routing-e2e-cdp-two'};const tab2=await request('start',{url:siteUrl+'/protected',profile},second);prove('two workers have distinct CDP tabs',tab2.route?.target!==cd.route?.target);await request('stop',{},second);
  // Kill only the fixture's isolated Chrome, then reopen from durable storage.
  const crashBrowser=await chromium.connectOverCDP(`http://127.0.0.1:${cd.route.cdp_port}`);
  const crashSession=await crashBrowser.newBrowserCDPSession();
  const command=await crashSession.send('Browser.getBrowserCommandLine');
  assert(command.arguments.includes('--user-data-dir='+cd.route.user_data_dir));
  const processes=await crashSession.send('SystemInfo.getProcessInfo');
  const browserPid=processes.processInfo.find(p=>p.type==='browser').id;
  await crashBrowser.close();process.kill(browserPid,'SIGKILL');
  await poll(async()=>{try{process.kill(browserPid,0);return false;}catch{return true;}});
  const recovered=await request('start',{url:siteUrl+'/protected',profile},cdCtx);
  prove('CDP reopens after a real SIGKILL',recovered.route?.backend==='cdp'&&!recovered.error);
  await poll(async()=> (await getText(cdCtx)).includes('Authenticated route proof'));
  const persistent=await request('action',{action:'eval',script:'localStorage.getItem("route-local")'},cdCtx);
  prove('CDP authentication and storage survive SIGKILL',persistent.data?.result==='persisted');
  const native2=await request('start',{url:siteUrl+'/protected',profile},ctx);prove('native opened for no-replay test',native2.route?.backend==='amux');
  const busyCtx={...ctx,session:'routing-e2e-busy'};const busy=await request('start',{url:siteUrl+'/protected',profile},busyCtx);prove('occupied native profile uses an independent CDP copy',busy.route?.backend==='cdp'&&busy.route.attempts[0].status===409);await poll(async()=> (await getText(busyCtx)).includes('Authenticated route proof'));const current=await api(base,'/api/browser/status');prove('busy fallback preserves the original native process',current.browsers.some(b=>b.profile===profile&&b.pid===native2.pid));await request('stop',{},busyCtx);
  // Route receipt is native, but the proxy now faults action after submission.
  failActions=true;const handoffCtx={...ctx,base:cdCtx.base};const before=counter;
  const failed=await request('action',{action:'click',selector:'#submit'},handoffCtx);prove('uncertain action changes route without replay',failed.status===409&&failed.route?.backend==='cdp'&&counter===before);
  await api(base,'/api/browser/stop','POST',{profile,session},session);
  if(process.env.AMUX_ROUTING_CUA==='1') {
    const cuaCtx={...ctx,session:'routing-e2e-cua',base:cdCtx.base,config:{...primary,chrome_profile:'',allow_cua:true}};
    const cu=await request('start',{url:siteUrl+'/protected',profile},cuaCtx);writeFileSync(join(evidence,'cua-start.json'),JSON.stringify(cu,null,2));prove('two forced failures reach real CUA',cu.route?.backend==='cua'&&cu.route.attempts.filter(a=>a.verdict==='failed').length===2);
    const shot=await request('screenshot',{},cuaCtx);prove('CUA actual desktop screenshot',existsSync(shot.path));copyFileSync(shot.path,join(evidence,'cua-before.png'));
    // Observe Chromium through its relay only to identify pixel geometry. All
    // interactions below are the computer-server's native desktop input APIs.
    await poll(async()=>{const b=await request('screenshot',{},cuaCtx);return b.path;});
    const status=await api(base,'/api/computer/status');const sandbox=status.sandboxes.find(b=>b.lane===cuaCtx.session);const port=sandbox.chromium_devtools_port;
    const browser=await chromium.connectOverCDP(`http://127.0.0.1:${port}`);const p=browser.contexts()[0].pages().find(p=>p.url().startsWith(siteUrl));await p.waitForSelector('h1');prove('CUA transferred login accepted by real site',(await p.locator('h1').innerText())==='Authenticated route proof');
    const input=await p.locator('#proof').boundingBox();const button=await p.locator('#submit').boundingBox();const offset=await p.evaluate(()=>({x:screenX+(outerWidth-innerWidth)/2,y:screenY+outerHeight-innerHeight}));
    writeFileSync(join(evidence,'cua-geometry.json'),JSON.stringify({input,button,offset},null,2));
    const click=r=>request('action',{action:'click',x:r.x+r.width/2+offset.x,y:r.y+r.height/2+offset.y},cuaCtx);
    await click(input);await request('action',{action:'type',text:'cua'},cuaCtx);await click(button);await poll(async()=>submissions.includes('cua'));prove('CUA OS click/type reached HTTP server',submissions.includes('cua'));
    const after=await request('screenshot',{},cuaCtx);copyFileSync(after.path,join(evidence,'cua.png'));await browser.close();await request('stop',{},cuaCtx);
  }
  writeFileSync(join(evidence,'result.json'),JSON.stringify({verdict:'PASS',measured:true,n_considered:checks.length,checks,submissions,cua_tested:process.env.AMUX_ROUTING_CUA==='1'},null,2));console.log(`VERDICT: PASS (${checks.length} checks)`);
} catch(e) {writeFileSync(join(evidence,'result.json'),JSON.stringify({verdict:'FAIL',measured:true,n_considered:checks.length,checks,error:e.stack},null,2));throw e;}
finally {await request('stop',{}, {...ctx,session:'routing-e2e-cua'}).catch(()=>{});await request('stop',{}, {...ctx,session:'routing-e2e-cdp'}).catch(()=>{});await request('stop',{},ctx).catch(()=>{});site.close();proxy?.close();await api(base,'/api/browser/stop','POST',{profile,session},session).catch(()=>{});}
