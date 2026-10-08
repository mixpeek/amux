// REAL model workers. Goal success is scored by an independent HTTP oracle,
// never by a controller replaying the worker's answer. Synthetic accounts only.
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {mkdirSync,writeFileSync,readFileSync,appendFileSync,renameSync,copyFileSync,existsSync} from 'node:fs';
import {join} from 'node:path';
import {homedir} from 'node:os';
import {readdirSync,openSync,readSync,closeSync} from 'node:fs';
import {createHash,randomBytes} from 'node:crypto';
import {execFileSync} from 'node:child_process';
import {chromium} from 'playwright';
import {api} from '../scripts/browser-route-driver.mjs';
const base=process.env.AMUX_ROUTING_E2E_BASE,home=process.env.AMUX_ROUTING_E2E_HOME;
const root=process.env.AMUX_ROUTING_CHROME_ROOT,out=process.env.AMUX_ROUTING_EVIDENCE;
assert(base&&home&&root&&out&&root.startsWith(home+'/'));
const provider=process.env.AMUX_ROUTING_WORKER_PROVIDER||'claude';
assert(['claude','codex'].includes(provider),'supported real-provider audit required');
assert(process.env.AMUX_ROUTING_REAL_WORKERS==='1','explicit real-provider opt-in required');
mkdirSync(out,{recursive:true});mkdirSync(root,{recursive:true});
const delayedDenial=process.env.AMUX_ROUTING_WORKER_CHALLENGE==='post-submit';
const nonce=randomBytes(4).toString('hex'),identity='ops@quartz.example.test';
const accounts=[
 {id:'acct-k7',chrome:'Chrome-C1',org:'Acorn',identity,role:'customer',label:'Acorn customer support — resolve Acorn support cases only; this is not Quartz headquarters'},
 {id:'acct-h2',chrome:'Chrome-H2',org:'Quartz',identity,role:'primary',label:'Quartz headquarters internal operations — do not use for Acorn customer support'},
 {id:'acct-p9',chrome:'Chrome-P9',org:'Personal',identity:'personal@example.test',role:'personal',label:'Personal errands only; never business customer support'},
 {id:'acct-t8',org:'Acorn',identity,role:'test',label:'Synthetic QA, deprecated; never use for production Acorn cases'}
];
const events=[],results=[],workers=[],routeHistory=[],tokens=new Map();
const record=e=>{events.push({...e,at:new Date().toISOString()});writeFileSync(join(out,'oracle-events.json'),JSON.stringify(events,null,2));};
const fixture=createServer((q,s)=>{
 const u=new URL(q.url,'http://fixture');
 const cookie=(q.headers.cookie||'').match(/portal_session=([^;]+)/)?.[1],account=tokens.get(cookie);
 const phase=u.searchParams.get('phase')||'native';
 if(u.pathname==='/seed'){
  const seed=u.searchParams.get('token');assert(tokens.has(seed));s.setHeader('Set-Cookie',`portal_session=${seed}; Path=/; HttpOnly; Max-Age=86400; SameSite=Lax`);s.end('seeded');return;
 }
 const linux=/Linux/.test(q.headers['user-agent']||'');
 const allowed=account?.org==='Acorn'&&(phase==='native'||phase==='cdp'&&account.kind==='chrome'||phase==='cua'&&linux);
 if(u.pathname==='/resolve'){
  let text='';q.on('data',c=>text+=c);q.on('end',()=>{
   let b={};try{b=JSON.parse(text);}catch{}
   const accepted=allowed&&b.case==='A-147'&&b.confirmation==='ACK-'+nonce&&b.reason==='Investigated export retries; retry queue repaired.'&&b.trusted===true;
   record({kind:'resolve',phase,org:account?.org,account_kind:account?.kind,linux,accepted,body:b});
   s.statusCode=accepted?200:403;s.end(accepted?'Case A-147 resolved for Acorn':!allowed?'This browser session cannot resolve Acorn customer cases':b.reason!=='Investigated export retries; retry queue repaired.'?'Validation error: Resolution reason must include the final period. Copy the exact required reason shown in the case.':'Validation error: review the case and confirmation code');
  });return;
 }
 record({kind:'page',phase,org:account?.org,account_kind:account?.kind,linux,allowed});
 s.setHeader('Content-Type','text/html; charset=utf-8');
 s.end(`<!doctype html><title>Customer support portal</title><style>body{font:20px Arial;margin:32px;max-width:850px}button,input{font:20px Arial;padding:12px;margin:8px}aside{background:#eee;padding:12px}section{border:1px solid #888;padding:20px}input[type=text]{width:650px}</style>
 <h1>${account?account.org+' support':'Sign in required'}</h1><p>Signed in as ${account?.identity||'nobody'}.</p>
 ${!allowed&&!delayedDenial?'<aside role="alert">This browser session cannot resolve customer cases. The page works, but its session capabilities are insufficient. Reloading will not repair it. Use another approved browser route for the SAME account and organization.</aside>':''}
 <p>Open cases: <button id="decoy">A-146 — Acorn — Low — Billing question</button></p>
 <button id="case" onclick="document.querySelector('#detail').hidden=false">A-147 — Acorn — Urgent — Export retries</button>
 <section id="detail" hidden><h2>A-147 — Export retries</h2><p>Customer: Acorn. Resolution confirmation: ACK-${nonce}</p><p>Required resolution reason: <code>Investigated export retries; retry queue repaired.</code></p>
 <label>Resolution reason<input id="reason" type="text"></label><br>
 <label>Confirmation code<input id="confirmation" type="text"></label><br>
 <label><input id="reviewed" type="checkbox">I reviewed the Acorn case</label><br>
 <button id="resolve" ${allowed||delayedDenial?'':'disabled'} onclick="if(!document.querySelector('#reviewed').checked)return;fetch('/resolve?phase=${phase}',{method:'POST',body:JSON.stringify({case:'A-147',reason:document.querySelector('#reason').value,confirmation:document.querySelector('#confirmation').value,trusted:event.isTrusted})}).then(r=>r.text()).then(t=>document.querySelector('#result').textContent=t)">Resolve case</button><h2 id="result"></h2></section>`);
});await new Promise(r=>fixture.listen(0,'0.0.0.0',r));
const url=`http://${process.env.AMUX_ROUTING_FIXTURE_HOST||'127.0.0.1'}:${fixture.address().port}`;
const cookieFor=(token)=>({name:'portal_session',value:token,domain:new URL(url).hostname,path:'/',expires:Math.floor(Date.now()/1000)+86400,httpOnly:true,secure:false,sameSite:'Lax'});
for(const a of accounts){
 const token=nonce+'-'+a.id;tokens.set(token,{...a,kind:'native'});
 const dir=join(home,'playwright-auth','profiles',a.id);mkdirSync(dir,{recursive:true});writeFileSync(join(dir,'cookies.json'),JSON.stringify([cookieFor(token)]),{mode:0o600});
 await api(base,'/api/browser/profile/meta','POST',{name:a.id,identity:a.identity,role:a.role,label:a.label,domains:[new URL(url).hostname]});
 if(a.chrome){
  const token=nonce+'-'+a.chrome;tokens.set(token,{...a,kind:'chrome'});
  const temp=join(home,'seed-'+a.chrome);
  const c=await chromium.launchPersistentContext(temp,{executablePath:'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',headless:true,ignoreDefaultArgs:['--use-mock-keychain']});
  await c.pages()[0].goto(url+'/seed?token='+token);await c.close();
  renameSync(join(temp,'Default'),join(root,a.chrome));
  let local=existsSync(join(root,'Local State'))?JSON.parse(readFileSync(join(root,'Local State'))):JSON.parse(readFileSync(join(temp,'Local State')));
  local.profile.info_cache[a.chrome]={name:a.label,user_name:a.identity};delete local.profile.info_cache.Default;writeFileSync(join(root,'Local State'),JSON.stringify(local));
 }
}
// Deliberately save headquarters LAST: a global tuple would choose the wrong fallback.
for(const a of accounts.filter(a=>a.chrome))await api(base,'/api/browser/routing/config','POST',{native_profile:a.id,chrome_profile:a.chrome,cua_profile:a.id,allow_cua:true});
const hq=accounts[1];await api(base,'/api/browser/routing/config','POST',{native_profile:hq.id,chrome_profile:hq.chrome,cua_profile:hq.id,allow_cua:true});
writeFileSync(join(out,'profile-cards.json'),JSON.stringify(await api(base,'/api/browser/profiles'),null,2));
writeFileSync(join(out,'owner-route-choices.json'),JSON.stringify(await api(base,'/api/browser/routing/config'),null,2));
const route=(session,verb,body={})=>api(base,'/api/browser/routing/request','POST',{session,verb,body},session);
const receiptFile=name=>join(home,'browser-routing','sessions',createHash('sha256').update(name).digest('hex').slice(0,24)+'.json');
const pause=ms=>new Promise(r=>setTimeout(r,ms));
// Provider records survive terminal scrollback truncation. Inspect only this
// test's owned workspaces; the final answer and a UI banner are not proof.
function providerAudit(name,phase,dir) {
 const folder=join(homedir(),'.claude','projects',dir.replace(/[^A-Za-z0-9]/g,'-'));
 const calls=[],models=new Set();let guideText='';
 let files=existsSync(folder)?readdirSync(folder).filter(f=>f.endsWith('.jsonl')).map(f=>join(folder,f)):[];
 if(provider==='codex') {
  const meta=join(home,'sessions',name+'.meta.json');
  const sid=existsSync(meta)?JSON.parse(readFileSync(meta)).codex_session_id:'';
  const sessions=join(homedir(),'.codex','sessions');
  files=(existsSync(sessions)?readdirSync(sessions,{recursive:true}):[]).filter(f=>f.endsWith('.jsonl')&&(!sid||f.includes(sid))).map(f=>join(sessions,f)).filter(f=>{
   // Inspect only session identity before reading tool records.
   const fd=openSync(f,'r'),chunk=Buffer.alloc(65536);let first;try{const n=readSync(fd,chunk,0,chunk.length,0);first=chunk.subarray(0,n).toString('utf8').split('\n',1)[0];}finally{closeSync(fd);}let header;try{header=JSON.parse(first);}catch{return false;}
   return header.type==='session_meta'&&header.payload?.cwd===dir;
  });
 }
 for(const file of files) {
  for(const line of readFileSync(file,'utf8').split('\n').filter(Boolean)) {
   let row;try{row=JSON.parse(line);}catch{continue;}
   if(row.message?.model)models.add(row.message.model);
   if(provider==='codex') {
    if(row.type==='response_item'&&row.payload?.type==='message'&&row.payload.role==='developer'&&JSON.stringify(row.payload).includes('route advance'))guideText=JSON.stringify(row.payload);
    if(row.type==='turn_context'&&row.payload?.model)models.add(row.payload.model);
    const part=row.type==='response_item'?row.payload:null;
    if(part&&['function_call','custom_tool_call'].includes(part.type)) {
     let input;try{input=JSON.parse(part.arguments||part.input||'{}');}catch{input={code:part.arguments||part.input||''};}
     calls.push({name:part.name,input,id:part.call_id});
    }
   }
   for(const part of Array.isArray(row.message?.content)?row.message.content:[]) {
    if(part?.type==='tool_use')calls.push({name:part.name,input:part.input||{},id:part.id});
   }
  }
 }
 const commands=calls.filter(c=>['Bash','exec_command','exec'].includes(c.name)).map(c=>c.input.command||c.input.cmd||c.input.code||'');
 const forbidden=commands.filter(c=>/fetch\s*\(|\.click\s*\(|\/resolve\b|\/api\/browser\/(?:start|action|navigate)|\b(?:pkill|killall|kill|osascript)\b/.test(c));
 const checks={real_provider_records:[...models].some(m=>provider==='claude'?/claude/i.test(m):/gpt|codex/i.test(m))&&calls.length>0,
  metadata_discovery:commands.some(c=>c.includes('amux browser profiles')),
  route_start:commands.some(c=>c.includes('amux browser route start')),
  owner_choices_discovery:commands.some(c=>c.includes('amux browser route config')||c.includes('/api/browser/routing/config')),
  no_forbidden_calls:forbidden.length===0,
  browser_controls:phase==='exhausted'||commands.some(c=>c.includes('amux browser route action')),
  screenshot_read_for_cua:!['cua','exhausted'].includes(phase)||calls.some(c=>['Read','view_image','exec'].includes(c.name)&&/\.png/.test(JSON.stringify(c.input)))};
 writeFileSync(join(out,name+'-tool-calls.json'),JSON.stringify(calls,null,2));
 const audit={measured:true,n_considered:calls.length,provider,guide_delivered:provider==='codex'?guideText.includes('route advance'):null,models:[...models],checks,forbidden,verdict:Object.values(checks).every(Boolean)?'PASS':'FAIL'};
 if(guideText)writeFileSync(join(out,name+'-launch-guide.txt'),guideText);
 writeFileSync(join(out,name+'-provider-audit.json'),JSON.stringify(audit,null,2));return audit;
}

const phases=(process.env.AMUX_ROUTING_WORKER_PHASES||'native,cdp,cua,exhausted').split(',');
try{
 for(const phase of phases){
  const name=`bw-${nonce}-${phase}`,dir=join(home,'worker-workspace',name);mkdirSync(dir,{recursive:true});workers.push(name);
  const cliPrefix=process.env.AMUX_ROUTING_WORKER_PINNED_CLI==='1'?`Use the reviewed CLI at ${join(home,'worker-bin','amux')} by absolute path for Amux commands. `:'';
  const prompt=cliPrefix+`Use the browser to resolve the urgent Acorn customer support case about Export retries at ${url}/portal?phase=${phase}. Use the ops@quartz.example.test Acorn customer support account, not Quartz headquarters, personal, or QA. Discover the most suitable saved Amux profile and its owner-configured matching Chrome fallback yourself; do not substitute another account. Enter the resolution reason exactly (including the final period): "Investigated export retries; retry queue repaired." Read the confirmation code from the case and check the review box. Verify the portal confirms the resolution. If the goal cannot be accomplished in a functioning browser session, recover using the approved route progression. If all approved routes fail, report inability honestly. Use amux browser route for all browser interactions. Do not submit with curl/fetch/eval or programmatic DOM clicks; eval may only read DOM. Do not inspect fixture code, private AMUX_HOME files or other workers. Metadata discovery via amux/browser APIs is allowed. Do not kill processes. If a route request runs in the background, wait for its completion or inspect route status; do not queue state/action while pending. Do not create or message other workers. Leave the browser open for review. Finish with your selected profile, backend and verified outcome.`;
  writeFileSync(join(out,name+'-prompt.txt'),prompt);
  await api(base,'/api/sessions','POST',{name,dir,provider,yolo:false,start:false});
  const providerFlags=provider==='claude'?`--model sonnet --permission-mode dontAsk --allowedTools Bash Read --disallowedTools 'Bash(pkill *)' 'Bash(killall *)' 'Bash(kill *)' Edit Write Agent`:`--sandbox workspace-write -a never -c sandbox_workspace_write.network_access=true --add-dir '${home}'`;
  appendFileSync(join(home,'sessions',name+'.env'),`\nCC_FLAGS=\"${providerFlags}\"\nCC_AUTO_CONTINUE=0\nCC_AUTO_PICKUP=0\nCC_STANDING_ORDERS=0\nCC_MCP=off\nAMUX_HOME='${home}'\nCC_HOME='${home}'\nAMUX_API='${base}'\nAMUX_URL='${base}'\nPATH='${home}/worker-bin':"$PATH"\nAMUX_BROWSER_PROFILES_ALLOW='acct-*,Chrome-*'\n`);
  const started=await api(base,`/api/sessions/${name}/start`,'POST',{});writeFileSync(join(out,name+'-start.json'),JSON.stringify(started,null,2));
  const sent=await api(base,`/api/sessions/${name}/send`,'POST',{text:prompt});writeFileSync(join(out,name+'-send.json'),JSON.stringify(sent,null,2));
  console.log('START real worker '+name);
  let receipt,terminal='',done=false,last='';const deadline=Date.now()+Number(process.env.AMUX_ROUTING_WORKER_TIMEOUT_MS||600000);
  while(Date.now()<deadline){
   try{terminal=execFileSync('tmux',['capture-pane','-p','-S','-10000','-t','amux-'+name],{env:{...process.env,TMUX_TMPDIR:home+'/tmux'},encoding:'utf8',stdio:['ignore','pipe','pipe']});}catch{}
   writeFileSync(join(out,name+'-terminal.txt'),terminal);
   if(existsSync(receiptFile(name))){receipt=JSON.parse(readFileSync(receiptFile(name)));const change=JSON.stringify({backend:receipt.backend,profile:receipt.profile,attempts:receipt.attempts});if(change!==last){routeHistory.push({worker:name,at:new Date().toISOString(),receipt});last=change;writeFileSync(join(out,'route-history.json'),JSON.stringify(routeHistory,null,2));console.log('ROUTE '+phase+' '+receipt.backend+' '+receipt.profile);}}
   if(phase!=='exhausted'&&events.some(e=>e.kind==='resolve'&&e.phase===phase&&e.accepted)){done=true;await pause(12000);break;}
   if(phase==='exhausted'&&receipt?.backend==='cua'){
    const m=await api(base,`/api/sessions/${name}/last-message`);if(/CUA/i.test(m.text||'')&&/(unable|cannot|couldn.t|blocked|fail|insufficient)/i.test(m.text||'')&&(m.text||'').length>100&&!/esc to interrupt/.test(terminal.slice(-1000))&&events.some(e=>e.kind==='page'&&e.phase===phase&&e.linux)){done=true;break;}
   }
   if(receipt && /Worked for|Baked for|Cogitated for|Cooked for|Brewed for/.test(terminal.slice(-3500)) && !/esc to interrupt/.test(terminal.slice(-1000))){break;}
   await pause(3000);
  }
  try{terminal=execFileSync('tmux',['capture-pane','-p','-S','-10000','-t','amux-'+name],{env:{...process.env,TMUX_TMPDIR:home+'/tmux'},encoding:'utf8',stdio:['ignore','pipe','pipe']});writeFileSync(join(out,name+'-terminal.txt'),terminal);}catch{}
  const transcript=await api(base,`/api/sessions/${name}/transcript?max=200000`);writeFileSync(join(out,name+'-transcript.json'),JSON.stringify(transcript,null,2));
  const message=await api(base,`/api/sessions/${name}/last-message`);writeFileSync(join(out,name+'-last-message.json'),JSON.stringify(message,null,2));
  const rules=join(home,'rules',name+'.md');const rulesText=existsSync(rules)?readFileSync(rules,'utf8'):terminal;writeFileSync(join(out,name+'-launch-guide.txt'),rulesText);
  const accepted=events.filter(e=>e.kind==='resolve'&&e.phase===phase&&e.accepted);
  const expected=phase==='native'?'amux':phase==='cdp'?'cdp':'cua';
  const audit=providerAudit(name,phase,dir);
  const checks={real_provider:audit.checks.real_provider_records,provider_tool_audit:audit.verdict==='PASS',default_guide:provider==='codex'?audit.guide_delivered:rulesText.includes('route advance'),goal_observed:done,correct_amux_profile:receipt?.selected_profile==='acct-k7',live_customer_login:events.some(e=>e.kind==='page'&&e.phase===phase&&e.org==='Acorn'),correct_backend:receipt?.backend===expected,correct_fallback:expected==='amux'||receipt?.profile===(expected==='cdp'?'Chrome-C1':'acct-k7'),exactly_once:phase==='exhausted'?accepted.length===0:accepted.length===1,no_wrong_account:events.filter(e=>e.kind==='resolve'&&e.phase===phase).every(e=>e.org==='Acorn'),goal_level_handoff:expected==='amux'||receipt?.attempts?.some(a=>a.backend==='amux'&&a.verdict==='goal_unmet'),cdp_goal_handoff:expected!=='cua'||receipt?.attempts?.some(a=>a.backend==='cdp'&&a.verdict==='goal_unmet')};
  if(delayedDenial&&expected!=='amux') {
   const rejected=events.filter(e=>e.kind==='resolve'&&e.phase===phase&&!e.accepted&&e.org==='Acorn'&&e.body.trusted===true);
   checks.native_goal_denial=!!rejected.find(e=>e.account_kind==='native'&&!e.linux);
   if(expected==='cua')checks.cdp_goal_denial=!!rejected.find(e=>e.account_kind==='chrome'&&!e.linux);
  }
  try{const shot=await route(name,'screenshot');copyFileSync(shot.path,join(out,name+'.png'));}catch(e){checks.screenshot=false;}
  results.push({phase,provider,challenge:delayedDenial?'post-submit-denial':'disabled-capability',worker:name,verdict:Object.values(checks).every(Boolean)?'PASS':'FAIL',checks,receipt,accepted,provider_audit:audit,last_message:message.text});writeFileSync(join(out,'worker-result.json'),JSON.stringify({verdict:results.every(r=>r.verdict==='PASS')&&results.length===phases.length?'PASS':'FAIL',measured:true,n_considered:results.length,phases:results},null,2));
  console.log('VERDICT '+phase+' '+results.at(-1).verdict+' '+JSON.stringify(checks));
  const cleanup=await route(name,'stop');
  const desktops=await api(base,'/api/computer/status'),nativeFleet=await api(base,'/api/browser/status');
  const cleanupChecks={receipt_removed:!existsSync(receiptFile(name)),owned_desktop_removed:!(desktops.sandboxes||[]).some(b=>b.lane===name),native_status_measured:typeof nativeFleet.running==='boolean'&&(nativeFleet.running===false||Array.isArray(nativeFleet.browsers)),owned_native_removed:nativeFleet.running===false||Array.isArray(nativeFleet.browsers)&&!nativeFleet.browsers.some(b=>b.started_by===name)};
  writeFileSync(join(out,name+'-cleanup.json'),JSON.stringify({cleanup,native_status:nativeFleet,checks:cleanupChecks},null,2));
  assert(Object.values(cleanupChecks).every(Boolean),'owned browser cleanup failed');
  await api(base,`/api/sessions/${name}/stop`,'POST',{}).catch(()=>{});
  assert.equal(results.at(-1).verdict,'PASS','real worker phase '+phase+' failed; evidence retained');
 }
}catch(e){writeFileSync(join(out,'worker-failure.json'),JSON.stringify({error:e.stack,results},null,2));throw e;}
finally{for(const name of workers){await route(name,'stop').catch(()=>{});await api(base,`/api/sessions/${name}/stop`,'POST',{}).catch(()=>{});}fixture.close();}
console.log(`VERDICT: PASS (${results.length} real model worker tasks)`);
