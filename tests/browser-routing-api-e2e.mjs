// Acceptance through the shipped routing endpoint, including real contention
// and a missing selected Chrome source. Requires an isolated server/profile root.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {mkdirSync,writeFileSync,readFileSync,copyFileSync,renameSync,existsSync} from 'node:fs';
import {join} from 'node:path';
import {chromium} from 'playwright';
import {createHash} from 'node:crypto';
import {api} from '../scripts/browser-route-driver.mjs';
const base=process.env.AMUX_ROUTING_E2E_BASE,home=process.env.AMUX_ROUTING_E2E_HOME;
const root=process.env.AMUX_ROUTING_CHROME_ROOT,out=process.env.AMUX_ROUTING_EVIDENCE;
assert(base&&home&&root&&out,'use a candidate launched with AMUX_BROWSER_CHROME_USER_DATA_DIR matching AMUX_ROUTING_CHROME_ROOT');
assert(root.startsWith(home+'/'),'Chrome fixture must belong to the isolated AMUX_HOME');
mkdirSync(root,{recursive:true});mkdirSync(out,{recursive:true});
const profile='routing-api-proof',chromeProfile='Profile API '+Date.now(),token='api-fixture-'+Date.now(),submissions=[],checks=[];
const prove=(label,c)=>{assert(c,label);checks.push(label);console.log('PASS '+label);};
const site=createServer((q,s)=>{
  if(q.url==='/prove'){let text='';q.on('data',c=>text+=c);q.on('end',()=>{submissions.push(text);s.end('accepted');});return;}
  const logged=(q.headers.cookie||'').includes('api_session='+token);
  if(q.url==='/seed')s.setHeader('Set-Cookie',`api_session=${token}; Path=/; HttpOnly; Max-Age=86400; SameSite=Lax`);
  s.setHeader('Content-Type','text/html');s.end(`<h1>${logged||q.url==='/seed'?'Authenticated API proof':'Logged out'}</h1><input id="proof"><button id="submit" onclick="fetch('/prove',{method:'POST',body:document.querySelector('#proof').value}).then(()=>document.querySelector('h1').textContent='Submission confirmed')">Submit</button>`);
});await new Promise(r=>site.listen(0,'0.0.0.0',r));
const url=`http://${process.env.AMUX_ROUTING_FIXTURE_HOST||'127.0.0.1'}:${site.address().port}`;
const source=await chromium.launchPersistentContext(root,{executablePath:'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true,ignoreDefaultArgs:['--use-mock-keychain']});
await source.pages()[0].goto(url+'/seed');const cookies=await source.cookies();await source.close();
const stateFile=join(root,'Local State');const local=JSON.parse(readFileSync(stateFile));
renameSync(join(root,'Default'),join(root,chromeProfile));
local.profile.info_cache[chromeProfile]={...local.profile.info_cache.Default,name:'Isolated API acceptance',user_name:'api-proof@example.test'};delete local.profile.info_cache.Default;
writeFileSync(stateFile,JSON.stringify(local));
const dir=join(home,'playwright-auth','profiles',profile);mkdirSync(dir,{recursive:true});
writeFileSync(join(dir,'cookies.json'),JSON.stringify(cookies),{mode:0o600});
await api(base,'/api/browser/profile/meta','POST',{name:profile,identity:'api-proof@example.test',role:'test',label:'Isolated API acceptance'});
const discovered=await api(base,'/api/browser/routing/config');prove('candidate discovers only the configured Chrome fixture',discovered.chrome_profiles.some(p=>p.name===chromeProfile&&p.identity==='api-proof@example.test')&&!discovered.chrome_profiles.some(p=>p.name==='Profile 14'));
await api(base,'/api/browser/routing/config','POST',{native_profile:profile,chrome_profile:chromeProfile,cua_profile:profile,allow_cua:true});
const sessions=['routing-api-native','routing-api-cdp','routing-api-cua','routing-api-goal'];
mkdirSync(join(home,'sessions'),{recursive:true});
writeFileSync(join(home,'sessions',sessions[0]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW='+profile+'\n');
writeFileSync(join(home,'sessions',sessions[1]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW='+profile+',Profile*\n');
writeFileSync(join(home,'sessions',sessions[3]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW='+profile+',Profile*\n');
writeFileSync(join(home,'sessions','routing-api-refusal.env'),'AMUX_BROWSER_PROFILES_ALLOW='+profile+'\n');
const route=(session,verb,body={})=>api(base,'/api/browser/routing/request','POST',{session,verb,body},session);
const wait=async(fn)=>{const end=Date.now()+15000;do{if(await fn())return;await new Promise(r=>setTimeout(r,100));}while(Date.now()<end);throw Error('acceptance observation timed out');};
async function perform(session,name){
  await wait(async()=>JSON.stringify(await route(session,'state')).includes('Authenticated API proof'));
  await route(session,'action',{action:'click',selector:'#proof'});await route(session,'action',{action:'type',text:name});await route(session,'action',{action:'click',selector:'#submit'});
  await wait(async()=>submissions.includes(name));prove(name+' reached the HTTP server through routing API',submissions.filter(s=>s===name).length===1);
  const shot=await route(session,'screenshot');prove(name+' produced a real PNG',readFileSync(shot.path).subarray(0,8).equals(Buffer.from([137,80,78,71,13,10,26,10])));copyFileSync(shot.path,join(out,name+'.png'));
}
let moved=false,dashboard,ui;
try{
  const native=await route(sessions[0],'start',{url:url+'/protected',profile});prove('routing API opens allowed Amux even when unused Chrome fallback is outside scope',native.route.backend==='amux');await perform(sessions[0],'api-native');
  let refused;try{await route('routing-api-refusal','start',{url:url+'/protected',profile});}catch(e){refused=e;}
  prove('routing API refuses out-of-scope CDP without bypassing into CUA',refused?.status===403&&refused.payload.attempts.some(a=>a.backend==='cdp'&&a.status===403)&&!refused.payload.attempts.some(a=>a.backend==='cua'));
  const cdp=await route(sessions[1],'start',{url:url+'/protected',profile});prove('routing API reaches CDP after actual native contention',cdp.route.backend==='cdp'&&cdp.route.attempts[0].status===409);await perform(sessions[1],'api-cdp');
  writeFileSync(join(home,'sessions',sessions[1]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW=Profile*\n');
  let revoked;try{await route(sessions[1],'state');}catch(e){revoked=e;}
  prove('receipt retains selected profile for authorization after handoff',revoked?.status===403);
  writeFileSync(join(home,'sessions',sessions[1]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW='+profile+',Profile*\n');
  const slowAction=route(sessions[1],'action',{action:'eval',script:'new Promise(r=>setTimeout(()=>r("pending-probe"),2200))'});
  await new Promise(r=>setTimeout(r,350));
  const begun=Date.now(),pending=await route(sessions[1],'status');
  prove('status remains observable while a route request owns the lane',pending.pending===true&&pending.running===null&&pending.route.profile===chromeProfile&&Date.now()-begun<1000);
  writeFileSync(join(home,'sessions',sessions[1]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW='+profile+'\n');
  let pendingRevoked;try{await route(sessions[1],'status');}catch(e){pendingRevoked=e;}
  prove('pending status still enforces active Chrome scope',pendingRevoked?.status===403);
  let spoofed;try{await api(base,'/api/browser/routing/request','POST',{session:sessions[1],verb:'status',body:{}},'another-worker');}catch(e){spoofed=e;}
  prove('pending status refuses caller identity spoofing',spoofed?.status===403);
  await slowAction;
  let chromeRevoked;try{await route(sessions[1],'state');}catch(e){chromeRevoked=e;}
  prove('active Chrome scope revocation refuses observation while Amux remains allowed',chromeRevoked?.status===403);
  let revokedAdvance;try{await route(sessions[1],'advance',{reason:'Try to bypass revoked active Chrome'});}catch(e){revokedAdvance=e;}
  prove('active Chrome refusal cannot advance around its scope into CUA',revokedAdvance?.status===403&&(await api(base,'/api/computer/status')).running===0);

  writeFileSync(join(home,'sessions',sessions[1]+'.env'),'AMUX_BROWSER_PROFILES_ALLOW='+profile+',Profile*\n');
  const observation=await route(sessions[1],'state');
  prove('CDP state shares the native shape and provides fresh element references',observation.elements.some(e=>e.tag==='INPUT'&&Number.isInteger(e.index))&&observation.observation_id&&observation.viewport.w>0);
  let stale;try{await route(sessions[1],'action',{action:'click',index:0,observation_id:'stale-observation'});}catch(e){stale=e;}
  prove('stale CDP observation refuses an index action',stale?.status===409);
  dashboard=await chromium.launch({headless:true});ui=await dashboard.newPage({ignoreHTTPSErrors:true,viewport:{width:1280,height:900}});
  await ui.goto(base+'/?view=browser');await ui.waitForFunction(()=>typeof window.switchView==='function');await ui.evaluate(()=>switchView('browser'));
  await ui.waitForFunction(value=>Array.from(document.querySelector('#bw-profile').options).some(o=>o.value===value),profile);await ui.selectOption('#bw-profile',profile);
  await ui.evaluate(session=>{_bwSession=session;},sessions[1]);await ui.locator('#bw-el-btn').click();await ui.waitForSelector('#bw-elements-list .bw-el');
  prove('dashboard Elements panel renders real CDP controls',await ui.locator('#bw-elements-list .bw-el').count()>=2);
  await ui.locator('#bw-elements-list .bw-el').filter({hasText:'INPUT'}).first().click();
  await wait(async()=> (await route(sessions[1],'action',{action:'eval',script:'document.activeElement.id'})).data?.result==='proof');
  prove('dashboard CDP element click focuses the actual input',true);
  await ui.evaluate(()=>_bwScreenshot(0,true));await ui.waitForFunction(()=>document.querySelector('#bw-img').naturalWidth>0);await ui.screenshot({path:join(out,'dashboard-cdp.png'),fullPage:true});
  renameSync(join(root,chromeProfile),join(root,chromeProfile+'-disabled'));moved=true;
  const cua=await route(sessions[2],'start',{url:url+'/protected',profile});prove('routing API reaches CUA after native contention and missing Chrome source',cua.route.backend==='cua'&&cua.route.attempts[0].status===409&&cua.route.attempts[1].status===503);
  const status=await route(sessions[2],'status');prove('CUA route reports an actually running desktop',status.running);
  const fleet=await api(base,'/api/computer/status');const box=fleet.sandboxes.find(b=>b.lane===sessions[2]);
  const browser=await chromium.connectOverCDP(`http://127.0.0.1:${box.chromium_devtools_port}`);
  try{
    const p=browser.contexts()[0].pages().find(p=>p.url().startsWith(url));await p.waitForSelector('#proof');prove('routing API CUA saved login accepted by the fixture',(await p.locator('h1').innerText())==='Authenticated API proof');
    const offset=await p.evaluate(()=>({x:screenX+(outerWidth-innerWidth)/2,y:screenY+outerHeight-innerHeight}));
    const click=async selector=>{const r=await p.locator(selector).boundingBox();await route(sessions[2],'action',{action:'click',x:r.x+r.width/2+offset.x,y:r.y+r.height/2+offset.y});};
    await click('#proof');await route(sessions[2],'action',{action:'type',text:'api-cua'});await click('#submit');await wait(async()=>submissions.includes('api-cua'));
    prove('routing API CUA real OS actions reached HTTP exactly once',submissions.filter(s=>s==='api-cua').length===1);
    const shot=await route(sessions[2],'screenshot');copyFileSync(shot.path,join(out,'api-cua.png'));
    await ui.evaluate(session=>{_bwSession=session;_bwViewport={w:23,h:23};},sessions[2]);await ui.evaluate(()=>_bwScreenshot(0,true));
    await ui.waitForFunction(()=>document.querySelector('#bw-img').naturalWidth===_bwViewport?.w&&document.querySelector('#bw-img').naturalHeight===_bwViewport?.h);
    prove('CUA dashboard replaces the old viewport with desktop pixel dimensions',true);
    const r=await p.locator('#proof').boundingBox();const image=await ui.locator('#bw-img').boundingBox();const viewport=await ui.evaluate(()=>_bwViewport);
    await ui.locator('#bw-img').click({position:{x:(r.x+r.width/2+offset.x)/viewport.w*image.width,y:(r.y+r.height/2+offset.y)/viewport.h*image.height}});
    await wait(async()=>await p.evaluate(()=>document.activeElement.id)==='proof');prove('dashboard CUA image click reaches the actual desktop input',true);
    await ui.screenshot({path:join(out,'dashboard-cua.png'),fullPage:true});
  }finally{await browser.close();}
  prove('all three endpoint paths submitted exactly once',submissions.length===3);
  renameSync(join(root,chromeProfile+'-disabled'),join(root,chromeProfile));moved=false;
  const goal=await route(sessions[3],'start',{url:url+'/protected',profile});prove('goal recovery fixture begins on real CDP after native contention',goal.route.backend==='cdp');
  let missingReason;try{await route(sessions[3],'advance');}catch(e){missingReason=e;}
  prove('advance requires an explicit unmet-goal reason',missingReason?.status===400);
  const goalCua=await route(sessions[3],'advance',{reason:'Synthetic functioning browser cannot complete this goal'});
  prove('advance retains the prior CDP target for owned cleanup',goalCua.route.backend==='cua'&&goalCua.route.previous_routes.some(r=>r.backend==='cdp'&&r.target===goal.route.target));
  let exhausted;try{await route(sessions[3],'advance',{reason:'Still cannot complete goal'});}catch(e){exhausted=e;}
  prove('exhausted CUA refuses another route without changing accounts',exhausted?.status===409&&submissions.length===3);
  await route(sessions[3],'stop');
  const tabs=await api('http://127.0.0.1:'+goal.route.cdp_port,'/json/list');
  prove('stop after CDP to CUA closes exactly its previous CDP tab',!tabs.some(t=>t.id===goal.route.target)&&tabs.some(t=>t.id===cdp.route.target));
  const afterStop=await api(base,'/api/computer/status');prove('stop after handoff removes only its own CUA desktop',!afterStop.sandboxes.some(b=>b.lane===sessions[3])&&afterStop.sandboxes.some(b=>b.lane===sessions[2]));

  const nativeStatus=await api(base,'/api/browser/status');prove('fallback preserved the native owner process',nativeStatus.browsers.some(b=>b.pid===native.pid&&b.profile===profile));
  let conditionalStop;try{await api(base,'/api/browser/stop','POST',{profile,expected_started_by:'another-route'},sessions[0]);}catch(e){conditionalStop=e;}
  prove('conditional route cleanup preserves a browser owned by another lane',conditionalStop?.status===409&&conditionalStop.payload.code==='browser_stop_ownership_changed'&&(await route(sessions[0],'status')).running);
  const driverBytes=readFileSync(new URL('../scripts/browser-route-driver.mjs',import.meta.url));
  const driverPath=join(home,'browser-routing','driver-'+createHash('sha256').update(driverBytes).digest('hex').slice(0,24)+'.mjs');
  assert(existsSync(driverPath),'server must materialize the exact embedded driver content');
  writeFileSync(driverPath,'process.stdout.write(JSON.stringify({error:"stale cached driver"}));');
  const repaired=await route(sessions[0],'state');prove('server repairs a stale cached driver before execution',repaired.ok&&readFileSync(driverPath).equals(driverBytes));
  await api(base,'/api/browser/stop','POST',{profile,expected_started_by:sessions[0]},sessions[0]);
  const replacement='routing-api-replacement',replacementOwner='routing-api-replacement-owner';
  const replacementDir=join(home,'playwright-auth','profiles',replacement);mkdirSync(replacementDir,{recursive:true});writeFileSync(join(replacementDir,'cookies.json'),JSON.stringify(cookies),{mode:0o600});
  await api(base,'/api/browser/profile/meta','POST',{name:replacement,identity:'replacement@example.test',role:'test',label:'Owned replacement fixture'});
  const replacementBrowser=await api(base,'/api/browser/start','POST',{profile:replacement,url:url+'/protected',session:replacementOwner},replacementOwner);
  await route(sessions[0],'stop');
  prove('stopping an expired native route preserves the sole unrelated native replacement',(await api(base,'/api/browser/status')).browsers.some(b=>b.pid===replacementBrowser.pid&&b.started_by===replacementOwner));
  await api(base,'/api/browser/stop','POST',{profile:replacement,expected_started_by:replacementOwner},replacementOwner);
  writeFileSync(join(out,'api-result.json'),JSON.stringify({verdict:'PASS',measured:true,n_considered:checks.length,checks,submissions,chrome_profile:chromeProfile},null,2));console.log(`VERDICT: PASS (${checks.length} endpoint checks)`);
}catch(e){writeFileSync(join(out,'api-result.json'),JSON.stringify({verdict:'FAIL',measured:true,n_considered:checks.length,checks,error:e.stack},null,2));throw e;}
finally{await dashboard?.close();if(moved&&existsSync(join(root,chromeProfile+'-disabled')))renameSync(join(root,chromeProfile+'-disabled'),join(root,chromeProfile));for(const session of sessions.reverse())await route(session,'stop').catch(()=>{});site.close();}
