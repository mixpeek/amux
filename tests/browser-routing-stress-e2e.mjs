// Adversarial acceptance against an isolated instance of the deployed binary.
// No human account jars, live worker tabs, or public-site writes are involved.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {mkdirSync, writeFileSync, readFileSync, cpSync, existsSync} from 'node:fs';
import {join} from 'node:path';
import {execFileSync} from 'node:child_process';
import {chromium} from 'playwright';
import {api, run} from '../scripts/browser-route-driver.mjs';

const base=process.env.AMUX_ROUTING_E2E_BASE, home=process.env.AMUX_ROUTING_E2E_HOME;
const root=process.env.AMUX_ROUTING_CHROME_ROOT, out=process.env.AMUX_ROUTING_EVIDENCE;
assert(base&&home&&root&&out&&root.startsWith(home+'/'), 'requires isolated server/home/Chrome root/evidence');
mkdirSync(out,{recursive:true});
const checks=[], capabilities=[], submissions=[], timings={};
const prove=(label,condition)=>{assert(condition,label);checks.push(label);console.log('PASS '+label);};
const poll=async fn=>{const until=Date.now()+20000;do{const result=await fn();if(result)return result;await new Promise(r=>setTimeout(r,100));}while(Date.now()<until);throw Error('stress observation timed out');};
// Use an exact scope token: the current allowlist parser also splits on spaces.
const token='stress-fixture-'+Date.now(), nativeProfile='routing-stress-proof', chromeProfile='Profile-Stress-'+Date.now();
let revoked=false;
const site=createServer((q,s)=>{
  if(q.url==='/commit'){let body='';q.on('data',c=>body+=c);q.on('end',()=>{submissions.push(body);s.end('accepted');});return;}
  s.setHeader('Content-Type','text/html; charset=utf-8');
  if(q.url==='/frame'){s.end('<button aria-label="Frame-only control">Frame action</button>');return;}
  if(q.url==='/seed')s.setHeader('Set-Cookie',`stress_session=${token}; Path=/; HttpOnly; Max-Age=86400; SameSite=Lax`);
  const authenticated=!revoked&&((q.headers.cookie||'').includes('stress_session='+token)||q.url==='/seed');
  s.end(`<title>Adversarial worker fixture</title><h1>${authenticated?'Authenticated stress account':'Session revoked'}</h1>
    <input id="proof" aria-label="Worker proof"><button id="commit" onclick="fetch('/commit',{method:'POST',body:document.querySelector('#proof').value}).then(()=>document.querySelector('#effect').textContent='Submission accepted')">Commit proof</button>
    <p id="effect">No submission</p><button id="popup" onclick="window.open('/popup','_blank')">Open popup</button>
    <iframe title="Account details" src="/frame"></iframe><div id="shadow"></div>
    <script>document.querySelector('#shadow').attachShadow({mode:'open'}).innerHTML='<button aria-label="Shadow-only control">Shadow action</button>';</script>`);
});
await new Promise(r=>site.listen(0,'127.0.0.1',r));
const url='http://127.0.0.1:'+site.address().port;
const workers=Array.from({length:8},(_,i)=>'routing-stress-worker-'+i), owner='routing-stress-owner';
const route=(session,verb,body={})=>api(base,'/api/browser/routing/request','POST',{session,verb,body},session);
const rejection=async(session,verb,body)=>{try{await route(session,verb,body);return null;}catch(e){return e;}};
const seedRoot=join(home,'stress-source');mkdirSync(seedRoot,{recursive:true});
const seed=await chromium.launchPersistentContext(seedRoot,{executablePath:'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true,ignoreDefaultArgs:['--use-mock-keychain']});
await seed.pages()[0].goto(url+'/seed');await seed.pages()[0].evaluate(()=>localStorage.setItem('stress-storage','persistent-marker'));
const cookies=await seed.cookies();await seed.close();
mkdirSync(root,{recursive:true});cpSync(join(seedRoot,'Default'),join(root,chromeProfile),{recursive:true});
const localState=join(root,'Local State');const local=existsSync(localState)?JSON.parse(readFileSync(localState)):JSON.parse(readFileSync(join(seedRoot,'Local State')));
local.profile.info_cache[chromeProfile]={name:'Adversarial acceptance',user_name:'stress@example.test'};writeFileSync(localState,JSON.stringify(local));
const profileDir=join(home,'playwright-auth','profiles',nativeProfile);mkdirSync(profileDir,{recursive:true});
writeFileSync(join(profileDir,'cookies.json'),JSON.stringify(cookies),{mode:0o600});
await api(base,'/api/browser/profile/meta','POST',{name:nativeProfile,identity:'stress@example.test',role:'test',label:'Adversarial acceptance only'});
await api(base,'/api/browser/routing/config','POST',{native_profile:nativeProfile,chrome_profile:chromeProfile,cua_profile:nativeProfile,allow_cua:false});
mkdirSync(join(home,'sessions'),{recursive:true});
for(const worker of [owner,...workers])writeFileSync(join(home,'sessions',worker+'.env'),'AMUX_BROWSER_PROFILES_ALLOW='+nativeProfile+','+chromeProfile+'\n');
let observer, proxy;
try{
  const native=await route(owner,'start',{profile:nativeProfile,url:url+'/protected'});prove('native owner is established before contention',native.route.backend==='amux');
  const begun=Date.now();const started=await Promise.all(workers.map(session=>route(session,'start',{profile:nativeProfile,url:url+'/protected'})));
  prove('eight simultaneous workers fall back to real CDP',started.every(r=>r.route.backend==='cdp'&&r.route.attempts[0].status===409));
  prove('eight workers share one isolated Chrome process',new Set(started.map(r=>r.route.cdp_port)).size===1);
  prove('eight worker tabs are distinct',new Set(started.map(r=>r.route.target)).size===8);
  timings.concurrent_start_ms=Date.now()-begun;
  console.log('Concurrent start wall time: '+timings.concurrent_start_ms+'ms');
  await Promise.all(workers.map(session=>poll(async()=>JSON.stringify(await route(session,'state')).includes('Authenticated stress account'))));
  prove('all eight worker tabs have accepted authentication',true);
  const states=await Promise.all(workers.map(session=>route(session,'state')));
  prove('observations are unique across workers',new Set(states.map(s=>s.observation_id)).size===8);
  const inputIndex=states[0].elements.find(e=>e.label==='Worker proof').index;
  const commitIndex=states[0].elements.find(e=>e.label==='Commit proof').index;
  const foreign=await rejection(workers[1],'action',{action:'click',index:inputIndex,observation_id:states[0].observation_id});
  prove('another worker observation cannot address this tab',foreign?.status===409);
  await route(workers[0],'action',{action:'eval',script:'document.querySelector("#commit").outerHTML=document.querySelector("#commit").outerHTML'});
  const detached=await rejection(workers[0],'action',{action:'click',index:commitIndex,observation_id:states[0].observation_id});
  prove('same-label replacement rejects the detached observed node',detached?.status===409&&submissions.length===0);
  const state=await route(workers[0],'state'), index=state.elements.find(e=>e.label==='Commit proof').index;
  await route(workers[0],'action',{action:'eval',script:'document.querySelector("#commit").disabled=true'});
  const disabled=await rejection(workers[0],'action',{action:'click',index,observation_id:state.observation_id});
  prove('disabled observed control is refused without submission',disabled?.status===409&&submissions.length===0);
  await route(workers[0],'action',{action:'eval',script:'document.querySelector("#commit").disabled=false;const o=document.createElement("div");o.id="overlay";o.style="position:fixed;inset:0;z-index:999;background:white";document.body.appendChild(o)'});
  const occluded=await rejection(workers[0],'action',{action:'click',index,observation_id:state.observation_id});
  prove('occluded observed control is refused without submission',occluded?.status===409&&submissions.length===0);
  const dispatch=await route(workers[0],'action',{action:'click',selector:'#commit'});
  prove('selector dispatch can be distinguished from a missing page effect',dispatch.ok&&submissions.length===0);
  capabilities.push({capability:'selector actionability and automatic effect verification',verdict:'UNSUPPORTED',dispatched_returned_ok:dispatch.ok,observed_http_effect:false});
  await route(workers[0],'action',{action:'eval',script:'document.querySelector("#overlay").remove();history.pushState({},"","/spa-transition")'});
  const navigated=await rejection(workers[0],'action',{action:'click',index,observation_id:state.observation_id});
  prove('SPA URL transition invalidates the old observation',navigated?.status===409&&submissions.length===0);
  await route(workers[0],'action',{action:'click',selector:'#proof'});
  const missing=await rejection(workers[0],'action',{action:'type',selector:'#does-not-exist',text:'wrong-field-write'});
  const unchanged=await route(workers[0],'action',{action:'eval',script:'document.querySelector("#proof").value'});
  prove('missing typing target cannot alter the previously focused field',missing?.status===422&&unchanged.data.result==='');
  const texts=workers.map((_,i)=>`worker ${i}: café Ω 東京 🧪 ' " \\`);
  const inputBegun=Date.now();
  await Promise.all(workers.map((session,i)=>route(session,'action',{action:'type',selector:'#proof',text:texts[i]})));
  timings.concurrent_input_ms=Date.now()-inputBegun;
  const typed=await Promise.all(workers.map(session=>route(session,'action',{action:'eval',script:'document.querySelector("#proof").value'})));
  prove('parallel Unicode input remains in the correct worker tabs',typed.every((r,i)=>r.data.result===texts[i]));
  const submitBegun=Date.now();
  await Promise.all(workers.map(session=>route(session,'action',{action:'click',selector:'#commit'})));
  await poll(()=>submissions.length===8);
  timings.concurrent_submit_ms=Date.now()-submitBegun;
  prove('eight concurrent submissions reach HTTP exactly once each',texts.every(t=>submissions.filter(s=>s===t).length===1)&&submissions.length===8);
  await Promise.all(workers.map(session=>poll(async()=>JSON.stringify(await route(session,'state')).includes('Submission accepted'))));
  prove('all eight real page effects are verified after dispatch',true);
  const ownerField=await route(owner,'action',{action:'eval',script:'document.querySelector("#proof").value'});
  prove('concurrent fallback leaves the native owner field untouched',ownerField.data.result==='');
  observer=await chromium.connectOverCDP('http://127.0.0.1:'+started[0].route.cdp_port);
  const workerPage=observer.contexts()[0].pages().find(p=>p.url().endsWith('/spa-transition'));
  await route(workers[0],'action',{action:'click',selector:'#popup'});
  await poll(()=>observer.contexts()[0].pages().some(p=>p.url()===url+'/popup'));
  prove('popup does not silently replace the worker route target',(await route(workers[0],'state')).url===url+'/spa-transition');
  const observed=await route(workers[0],'state');
  const nativeObserved=await route(owner,'state');
  const framePresent=await workerPage.frameLocator('iframe').getByRole('button',{name:'Frame-only control'}).count();
  const shadowPresent=await workerPage.getByRole('button',{name:'Shadow-only control'}).count();
  capabilities.push({capability:'CDP iframe grounded observation',verdict:observed.elements.some(e=>e.label==='Frame-only control')?'SUPPORTED':'UNSUPPORTED',native_observation_has_control:nativeObserved.elements.some(e=>e.label==='Frame-only control'||e.name==='Frame-only control'),playwright_locator_matches:framePresent});
  capabilities.push({capability:'CDP open shadow DOM grounded observation',verdict:observed.elements.some(e=>e.label==='Shadow-only control')?'SUPPORTED':'UNSUPPORTED',native_observation_has_control:nativeObserved.elements.some(e=>e.label==='Shadow-only control'||e.name==='Shadow-only control'),playwright_locator_matches:shadowPresent});
  prove('frame and shadow comparison uses actual existing controls',framePresent===1&&shadowPresent===1);
  for(const page of observer.contexts()[0].pages().filter(p=>p.url()===url+'/popup'))await page.close();
  const spoof=await api(base,'/api/browser/routing/request','POST',{session:workers[1],verb:'state',body:{}},workers[0]).then(()=>null,e=>e);
  prove('worker cannot impersonate another session',spoof?.status===403);
  writeFileSync(join(home,'sessions',workers[1]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW=unrelated-profile\n');
  const scope=await rejection(workers[1],'action',{action:'click',selector:'#commit'});
  prove('revoked profile scope blocks a mutation before any submission',scope?.status===403&&submissions.length===8);
  writeFileSync(join(home,'sessions',workers[1]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW='+nativeProfile+','+chromeProfile+'\n');
  // Kill only the Node child materialized in this isolated AMUX_HOME.
  const pending=route(workers[0],'action',{action:'eval',script:'(window.__stressDriverStarted=true,new Promise(r=>setTimeout(()=>r("delayed"),15000)))'}).then(()=>null,e=>e);
  await poll(()=>workerPage.evaluate(()=>window.__stressDriverStarted===true));
  const driverPid=await poll(()=>{
    const rows=execFileSync('ps',['-axo','pid=,command='],{encoding:'utf8'}).split('\n');
    const row=rows.find(r=>r.includes('node '+join(home,'browser-routing','driver-'))&&!r.includes('ps -axo'));
    return row?Number(row.trim().split(/\s+/)[0]):null;
  });
  process.kill(driverPid,'SIGKILL');const killed=await pending;
  prove('actual driver SIGKILL is reported as an error',killed?.status===502);
  const recovered=await route(workers[2],'state');
  prove('driver SIGKILL releases the shared profile request lock',recovered.url===url+'/protected');
  prove('driver failure did not replay a mutation',submissions.length===8);
  await route(workers[7],'stop');
  prove('stopping one worker leaves seven other CDP tabs usable',(await route(workers[3],'state')).url===url+'/protected');
  await observer.close();observer=undefined;
  for(const session of workers.slice(0,7))await route(session,'stop');
  const reopened=await route(workers[0],'start',{profile:nativeProfile,url:url+'/protected'});
  await poll(async()=>JSON.stringify(await route(workers[0],'state')).includes('Authenticated stress account'));
  const storage=await route(workers[0],'action',{action:'eval',script:'localStorage.getItem("stress-storage")'});
  prove('last-tab shutdown and reopen preserves CDP authentication and storage',reopened.route.backend==='cdp'&&storage.data.result==='persistent-marker');
  revoked=true;await route(workers[0],'navigate',{url:url+'/protected'});
  await poll(async()=>JSON.stringify(await route(workers[0],'state')).includes('Session revoked'));
  prove('server-side session revocation is visible despite saved cookies',true);
  // Lose the reply AFTER Chrome's real click has committed at the HTTP server.
  // This is an uncertain outcome, not a fault injected before dispatch.
  revoked=false;
  await route(owner,'action',{action:'click',selector:'#proof'});
  await route(owner,'action',{action:'type',selector:'#proof',text:'native-ack-lost'});
  const commitField=await route(owner,'action',{action:'eval',script:'document.querySelector("#proof").value'});
  prove('lost-reply fixture has the intended native input value',commitField.data.result==='native-ack-lost');
  proxy=createServer(async(q,s)=>{
    let text='';for await(const chunk of q)text+=chunk;
    const body=text?JSON.parse(text):undefined;
    try{
      const result=await api(base,q.url,q.method,body,q.headers['x-amux-session']);
      if(q.url.startsWith('/api/browser/action')&&body?.action==='click'){
        await poll(()=>submissions.includes('native-ack-lost'));s.destroy();return;
      }
      s.setHeader('Content-Type','application/json');s.end(JSON.stringify(result));
    }catch(e){s.statusCode=e.status||502;s.end(JSON.stringify({error:e.message}));}
  });await new Promise(r=>proxy.listen(0,'127.0.0.1',r));
  const lost=await run({context:{base:'http://127.0.0.1:'+proxy.address().port,home,session:owner,config:{native_profile:nativeProfile,chrome_profile:chromeProfile,allow_cua:false},chrome_root:root,chrome_binary:'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',identity:'stress@example.test',chrome_identity:'stress@example.test'},verb:'action',body:{action:'click',selector:'#commit'}});
  prove('lost reply after a real committed click forces explicit handoff',lost.status===409&&lost.route?.backend==='cdp');
  prove('committed action is not duplicated when its reply is lost',submissions.length===9&&submissions.filter(s=>s==='native-ack-lost').length===1);
  const nativeStatus=await api(base,'/api/browser/status');
  prove('all stress cases preserved the original native owner process',nativeStatus.browsers.some(b=>b.pid===native.pid&&b.profile===nativeProfile));
  const shot=await route(workers[0],'screenshot');cpSync(shot.path,join(out,'stress-revoked.png'));
  writeFileSync(join(out,'stress-result.json'),JSON.stringify({verdict:capabilities.some(c=>c.verdict==='UNSUPPORTED')?'PASS_WITH_LIMITATIONS':'PASS',measured:true,n_considered:checks.length,checks,capabilities,timings,workers:8,submissions,chrome_profile:chromeProfile},null,2));
  console.log(`VERDICT: ${checks.length} stress checks passed; capability comparison: `+JSON.stringify(capabilities));
}catch(e){writeFileSync(join(out,'stress-result.json'),JSON.stringify({verdict:'FAIL',measured:true,n_considered:checks.length,checks,capabilities,error:e.stack},null,2));throw e;}
finally{await observer?.close();for(const session of [...workers,owner])await route(session,'stop').catch(()=>{});await api(base,'/api/browser/stop','POST',{profile:nativeProfile,session:owner},owner).catch(()=>{});proxy?.close();site.close();}
