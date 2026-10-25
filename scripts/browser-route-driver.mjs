// One profile-aware browser ladder shared by the dashboard and workers.
// Observation and decisions remain with the worker, except for the `goal`
// verb, which hands a natural-language task to an agent method configured in
// browser-methods.toml and runs its loop over this route's own verbs.
//
// BACKENDS (where the browser runs) are a registry keyed by name, tried in the
// configured ladder order; AGENTS (who decides) are adapters loaded by
// protocol. Adding a method is a registry entry plus an adapter, not a new
// branch in this file.
import {request as httpRequest} from 'node:http';
import {request as httpsRequest} from 'node:https';
import {readFileSync, writeFileSync, mkdirSync, existsSync, cpSync, renameSync, rmSync, openSync, closeSync, lstatSync, realpathSync} from 'node:fs';
import {join, basename, resolve} from 'node:path';
import {createHash, randomUUID} from 'node:crypto';
import {spawn} from 'node:child_process';
import {pathToFileURL} from 'node:url';

const pause = ms => new Promise(r => setTimeout(r, ms));
const key = s => createHash('sha256').update(s).digest('hex').slice(0, 24);
const component = s => typeof s === 'string' && s.length > 0 && s !== '.' && s !== '..' && !/[\/\\\x00]/.test(s);
class RouteError extends Error { constructor(message, status = 502) { super(message); this.status = status; } }
const terminal = e => [400, 401, 403, 404, 409, 422].includes(e.status);
export function api(base, path, method = 'GET', body, session = '', token = '', timeoutMs = 180000) {
  const u = new URL(path, base);
  if (!['localhost', '127.0.0.1', '[::1]'].includes(u.hostname)) throw new RouteError('browser route requires a loopback Amux endpoint', 400);
  return new Promise((resolveResult, reject) => {
    const data = body === undefined ? undefined : JSON.stringify(body);
    const req = (u.protocol === 'https:' ? httpsRequest : httpRequest)(u, {
      method, rejectUnauthorized: false, headers: {'Content-Type':'application/json', 'X-Amux-Session':session,
        ...(token ? {Authorization:`Bearer ${token}`} : {})},
    }, res => {
      let text = ''; res.setEncoding('utf8');
      res.on('data', d => { text += d; if (text.length > 16 * 1024 * 1024) req.destroy(new Error('browser response exceeded 16 MiB')); });
      res.on('end', () => { try {
        const v = JSON.parse(text);
        if (res.statusCode >= 400 || v.error) reject(Object.assign(new RouteError(v.error || `HTTP ${res.statusCode}`, res.statusCode),{payload:v}));
        else resolveResult(v);
      } catch(e) { reject(e); } });
    });
    req.setTimeout(timeoutMs, () => req.destroy(new Error('browser request timed out')));
    req.on('error', reject); if (data) req.write(data); req.end();
  });
}
async function cdp(wsUrl, operation) {
  const url = new URL(wsUrl);
  if (!['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)) throw new RouteError('non-loopback CDP endpoint refused', 400);
  const ws = new WebSocket(wsUrl); let id = 0; const pending = new Map();
  await new Promise((r,j) => {const t = setTimeout(() => j(new Error('CDP connect timed out')), 10000); ws.onopen=()=>{clearTimeout(t);r();};ws.onerror=()=>{clearTimeout(t);j(new Error('CDP connect failed'));};});
  ws.onmessage = e => { const v = JSON.parse(e.data); const p = pending.get(v.id); if (!p) return; pending.delete(v.id); clearTimeout(p.t); v.error ? p.j(new Error(v.error.message)) : p.r(v.result); };
  ws.onclose = () => {for (const p of pending.values()) {clearTimeout(p.t);p.j(new Error('CDP closed before reply'));}pending.clear();};
  const send = (method, params = {}) => new Promise((r,j) => {const n=++id; const t=setTimeout(()=>{pending.delete(n);j(new Error(`${method} timed out`));},20000); pending.set(n,{r,j,t});ws.send(JSON.stringify({id:n,method,params}));});
  try { return await operation(send); } finally { ws.close(); }
}
const evaluate = async (send, expression) => {
  const v=await send('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});
  if(v.exceptionDetails) throw new RouteError(v.exceptionDetails.text || 'Page JavaScript exception',422);
  return v.result?.value;
};
function atomic(file, value) { mkdirSync(resolve(file, '..'), {recursive:true, mode:0o700}); const tmp=file+'.'+randomUUID();writeFileSync(tmp,JSON.stringify(value),{mode:0o600});renameSync(tmp,file); }
function read(file, fallback = {}) {try{return JSON.parse(readFileSync(file,'utf8'));}catch{return fallback;}}
function snapshot(root, profile, destination) {
  if (!component(profile)) throw new RouteError('select a Chrome profile directory',400);
  const source=join(root,profile);
  if (!existsSync(source)) throw new RouteError(`selected Chrome profile is missing: ${profile}`,503);
  if (existsSync(destination)) return;
  const stage=destination+'.'+randomUUID();
  mkdirSync(stage,{recursive:true,mode:0o700});
  const skip=new Set(['Cache','Code Cache','GPUCache','DawnCache','ShaderCache','GrShaderCache','Crashpad','BrowserMetrics']);
  try {
    cpSync(source,join(stage,'Default'),{recursive:true,dereference:false,filter:p=>!lstatSync(p).isSymbolicLink()&&!skip.has(basename(p))&&!basename(p).startsWith('Singleton')});
    if (existsSync(join(root,'Local State'))) cpSync(join(root,'Local State'),join(stage,'Local State'));
    atomic(join(stage,'amux-source.json'),{profile, imported_at:new Date().toISOString()});
    renameSync(stage,destination);
  } finally {if(existsSync(stage))rmSync(stage,{recursive:true,force:true});}
}
async function directStart(ctx, url, previous) {
  // The server holds the selected profile's launch mutex across this request.
  // A killed driver/server releases that mutex; no durable lease can strand it.
  return directStartLocked(ctx,url,previous);
}
async function directStartLocked(ctx, url, previous) {
  const source=ctx.config.chrome_profile;
  if(source&&ctx.chrome_access?.allowed===false)throw new RouteError(ctx.chrome_access.reason||'selected Chrome profile is outside this worker\'s scope',403);
  if(ctx.identity&&ctx.chrome_identity&&ctx.identity.toLowerCase()!==ctx.chrome_identity.toLowerCase())throw new RouteError('selected Chrome fallback identity differs from the chosen Amux profile; configure the matching Chrome profile',403);
  if(!source)throw new RouteError('CDP fallback has no preselected Chrome profile',503);
  const dir=join(ctx.home,'browser-routing','chrome',key(ctx.chrome_root+'\n'+source)); snapshot(ctx.chrome_root,source,dir);
  let port, launched=false;
  const portFile=join(dir,'DevToolsActivePort');
  if(existsSync(portFile)) {
    const candidate=Number(readFileSync(portFile,'utf8').split('\n')[0]);
    try {await api(`http://127.0.0.1:${candidate}`,'/json/version');port=candidate;} catch {}
  }
  if(!port) {
    if(!ctx.chrome_binary)throw new RouteError('Chrome binary unavailable for CDP fallback',503);
    // A live lock belongs to its owner. Never remove it to force a launch.
    const fd=openSync(join(dir,'route-chrome.log'),'a',0o600);
    const child=spawn(ctx.chrome_binary,['--user-data-dir='+dir,'--profile-directory=Default','--remote-debugging-port=0','--remote-debugging-address=127.0.0.1','--enable-automation','--no-first-run','--no-default-browser-check','--disable-dev-shm-usage',...(process.platform==='linux'?['--no-sandbox']:[]),'--headless=new','about:blank'],{detached:true,stdio:['ignore',fd,fd]});
    closeSync(fd);child.unref();launched=true;let spawnError;child.on('error',e=>{spawnError=e;});
    const deadline=Date.now()+30000;
    while(Date.now()<deadline) {
      if(spawnError)throw spawnError;
      if(existsSync(portFile)) { const candidate=Number(readFileSync(portFile,'utf8').split('\n')[0]);try {await api(`http://127.0.0.1:${candidate}`,'/json/version');port=candidate;break;}catch{} }
      await pause(200);
    }
    if(!port)throw new RouteError('selected Chrome profile did not expose CDP within 30 seconds',502);
  }
  const version=await api(`http://127.0.0.1:${port}`,'/json/version');
  const command=await cdp(version.webSocketDebuggerUrl,send=>send('Browser.getBrowserCommandLine'));
  if(!command.arguments.includes('--user-data-dir='+dir))throw new RouteError('CDP profile identity does not match the selected directory',403);
  const tab=await cdp(version.webSocketDebuggerUrl,async send=>{const t=await send('Target.createTarget',{url});if(launched){const all=await send('Target.getTargets');for(const p of all.targetInfos.filter(p=>p.type==='page'&&p.targetId!==t.targetId))await send('Target.closeTarget',{targetId:p.targetId});}return t;});
  return {backend:'cdp',profile:source,cdp_port:port,target:tab.targetId,user_data_dir:dir,url,attempts:previous};
}
async function target(state) {
  const tabs=await api(`http://127.0.0.1:${state.cdp_port}`,'/json/list');
  const page=tabs.find(t=>t.id===state.target&&t.type==='page');
  if(!page)throw Object.assign(new RouteError('this worker\'s CDP tab is gone; reopen the route',502),{code:'cdp_target_gone'});
  return page;
}
async function directVerb(ctx,state,verb,b) {
  const page=await target(state);
  return cdp(page.webSocketDebuggerUrl, async send=>{
    if(verb==='navigate') {await send('Page.navigate',{url:b.url});state.url=b.url;return {ok:true,url:b.url};}
    if(verb==='state') {
      const observation=randomUUID();
      const data=await evaluate(send,`(()=>{const elements=Array.from(document.querySelectorAll('a,button,input,select,textarea,[role=button]')).filter(e=>e.getClientRects().length);const name=e=>e.getAttribute('aria-label')||e.innerText||e.placeholder||e.id||e.tagName;window.__amuxRouteObservation={id:${JSON.stringify(observation)},url:location.href,elements:elements.map(e=>({node:e,label:name(e)}))};return {url:location.href,title:document.title,text:document.body?.innerText||'',viewport:{w:innerWidth,h:innerHeight},elements:elements.map((e,index)=>({index,tag:e.tagName,label:name(e),name:name(e),rect:e.getBoundingClientRect().toJSON()}))};})()`);
      state.observation_id=observation;
      return {ok:true,...data,observation_id:observation};
    }
    if(verb==='screenshot') {
      const r=await send('Page.captureScreenshot',{format:'png'});const path=join(ctx.home,'browser-screenshots','route-'+key(ctx.session)+'.png');mkdirSync(resolve(path,'..'),{recursive:true});writeFileSync(path,Buffer.from(r.data,'base64'),{mode:0o600});
      return {path,size:Buffer.from(r.data,'base64').length,viewport:await evaluate(send,'({w:innerWidth,h:innerHeight})')};
    }
    if(verb==='stop') {
      const version=await api(`http://127.0.0.1:${state.cdp_port}`,'/json/version');
      await cdp(version.webSocketDebuggerUrl,async browser=>{
        await browser('Target.closeTarget',{targetId:state.target});
        const deadline=Date.now()+3000;let tabs;
        do {tabs=await browser('Target.getTargets');if(!tabs.targetInfos.some(t=>t.targetId===state.target))break;await pause(50);}while(Date.now()<deadline);
        if(tabs.targetInfos.some(t=>t.targetId===state.target))throw new RouteError('owned CDP target closure was not confirmed; receipt retained',502);
        if(!tabs.targetInfos.some(t=>t.type==='page'))await browser('Browser.close').catch(()=>{});
      });return {ok:true,stopped:true};
    }
    if(verb==='keepalive')return {ok:true};
    if(verb==='status')return {running:true,profile:state.profile,backend:state.backend};
    if(verb==='inspect'||verb==='inspect/clear')throw new RouteError('Inspect is available on the amux route; use state/screenshot on CDP',400);
    if(verb!=='action')throw new RouteError('unsupported CDP verb',400);
    const a=b.action;
    if(a==='eval')return {ok:true,data:{result:await evaluate(send,b.script||b.js||'')}};
    if(a==='click') {
      let x=b.x,y=b.y;
      if(Number.isInteger(b.index)) {
        if(!state.observation_id)throw new RouteError('CDP index requires a fresh state after handoff',400);
        if(b.observation_id&&b.observation_id!==state.observation_id)throw new RouteError('CDP observation is stale; refresh state before clicking',409);
        const point=await evaluate(send,`(()=>{const o=window.__amuxRouteObservation;const ref=o?.elements[${JSON.stringify(b.index)}];const e=ref?.node;if(!o||o.id!==${JSON.stringify(state.observation_id)}||o.url!==location.href||!e?.isConnected)return {stale:true};const name=e.getAttribute('aria-label')||e.innerText||e.placeholder||e.id||e.tagName;if(name!==ref.label||e.disabled||!e.getClientRects().length)return {stale:true};e.scrollIntoView({block:'center'});const r=e.getBoundingClientRect(),x=r.x+r.width/2,y=r.y+r.height/2;const hit=document.elementFromPoint(x,y);return e===hit||e.contains(hit)?{x,y}:{stale:true};})()`);
        if(point.stale)throw new RouteError('CDP element changed; refresh state before clicking',409);
        ({x,y}=point);
      }
      if(b.selector) {const expr=`(()=>{const e=document.querySelector(${JSON.stringify(b.selector)});if(!e)throw Error('selector missing');e.scrollIntoView({block:'center'});const r=e.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2};})()`;({x,y}=await evaluate(send,expr));}
      if(!Number.isFinite(x)||!Number.isFinite(y))throw new RouteError('CDP click needs an observed selector or x,y; refresh state after handoff',400);
      await send('Input.dispatchMouseEvent',{type:'mousePressed',x,y,button:'left',clickCount:1});await send('Input.dispatchMouseEvent',{type:'mouseReleased',x,y,button:'left',clickCount:1});return {ok:true};
    }
    if(a==='type'||a==='input') {if(b.selector)await evaluate(send,`(()=>{const e=document.querySelector(${JSON.stringify(b.selector)});if(!e)throw Error('selector missing; refresh state before typing');e.focus();})()`);await send('Input.insertText',{text:b.text||''});return {ok:true};}
    if(a==='key') {const k=b.key||'Enter';const codes={Enter:13,Tab:9,Backspace:8,Escape:27,ArrowDown:40,ArrowUp:38};await send('Input.dispatchKeyEvent',{type:'keyDown',key:k,windowsVirtualKeyCode:codes[k]||0});await send('Input.dispatchKeyEvent',{type:'keyUp',key:k,windowsVirtualKeyCode:codes[k]||0});return {ok:true};}
    if(a==='scroll') {await send('Input.dispatchMouseEvent',{type:'mouseWheel',x:100,y:100,deltaX:b.dx||0,deltaY:b.dy||b.amount||300});return {ok:true};}
    if(a==='viewport') {const width=b.width||b.w,height=b.height||b.h;await send('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:width<=500});return {ok:true};}
    if(a==='back') {const h=await send('Page.getNavigationHistory');if(h.currentIndex>0)await send('Page.navigateToHistoryEntry',{entryId:h.entries[h.currentIndex-1].id});return {ok:true};}
    throw new RouteError(`CDP cannot execute ${a}; use an observed coordinate or selector`,400);
  });
}
async function native(ctx,verb,b) {
  if(verb==='action'&&b.action==='click'&&b.selector){
    const observed=await native(ctx,'action',{action:'eval',script:`(()=>{const e=document.querySelector(${JSON.stringify(b.selector)});if(!e)throw Error('selector missing');e.scrollIntoView({block:'center'});const r=e.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2};})()`});
    b={...b,...observed.data.result};delete b.selector;
  }
  return api(ctx.base,'/api/browser/'+verb+(verb==='state'||verb==='screenshot'?'?session='+encodeURIComponent(ctx.session):''),['state','screenshot','inspect','status'].includes(verb)?'GET':'POST',['state','screenshot','inspect','status'].includes(verb)?undefined:{...b,session:ctx.session},ctx.session,ctx.token);}
async function cua(ctx,state,verb,b) {
  if(verb==='status'){const s=await api(ctx.base,'/api/computer/status','GET',undefined,ctx.session,ctx.token);return {running:(s.sandboxes||[]).some(p=>p.lane===ctx.session&&p.state==='running'),profile:state.profile,backend:state.backend};}
  if(verb==='state')return {ok:true,url:state.url,text:'CUA route: inspect the screenshot and act with observed coordinates.',elements:[]};
  if(verb==='keepalive')return {ok:true};
  if(verb==='navigate') {const d=await api(ctx.base,'/api/computer/open','POST',{url:b.url,session:ctx.session},ctx.session,ctx.token);state.url=b.url;return d;}
  if(verb==='screenshot'||verb==='stop') {
    const result=await api(ctx.base,'/api/computer/'+verb,'POST',{session:ctx.session},ctx.session,ctx.token);
    if(verb==='screenshot'&&result.path){const png=readFileSync(result.path);if(png.length>=24&&png.subarray(0,8).equals(Buffer.from([137,80,78,71,13,10,26,10])))result.viewport={w:png.readUInt32BE(16),h:png.readUInt32BE(20)};}
    return result;
  }
  if(verb!=='action')throw new RouteError('CUA supports screenshot, coordinates, typing, keys and scrolling',400);
  if(!['click','type','key','scroll'].includes(b.action))throw new RouteError('CUA requires an observed visual action',400);
  const args=b.action==='scroll'?{dx:b.dx||0,dy:b.dy||b.amount||300}:b;
  return api(ctx.base,'/api/computer/'+b.action,'POST',{...args,session:ctx.session},ctx.session,ctx.token);
}
// The backends a route can run on. Each entry owns how it starts, serves a
// verb and stops; launch() and route() only walk the ladder.
const BACKENDS={
  amux:{
    // join:true: when another session already runs this profile, the server
    // opens a tab of our own in that browser (live sign-ins) instead of
    // refusing, so a busy profile no longer drops the route to the CDP copy.
    async start(ctx,b,profile){const {profile_chosen_by,...body}=b;const result=await native(ctx,'start',{...body,profile,join:b.join!==false});return {result,state:{backend:'amux',profile:result.profile,url:b.url,...(result.joined?{joined:true,owner:result.owner}:{})}};},
    verb:(ctx,state,verb,b)=>verb==='stop'?stopOwnedNative(ctx,state):native(ctx,verb,b),
  },
  cdp:{
    async start(ctx,b,profile,attempts){const state=await directStart(ctx,b.url,attempts);return {state,result:{ok:true,profile:state.profile,cdp_port:state.cdp_port,launch_url:b.url}};},
    verb:(ctx,state,verb,b)=>verb==='stop'?stopOwnedCdp(ctx,state):directVerb(ctx,state,verb,b),
  },
  cua:{
    async start(ctx,b,profile){if(ctx.cua_access?.allowed===false)throw new RouteError(ctx.cua_access.reason||'selected CUA profile is outside this worker\'s scope',403);if(ctx.identity&&ctx.cua_identity&&ctx.identity.toLowerCase()!==ctx.cua_identity.toLowerCase())throw new RouteError('CUA fallback identity differs from the selected profile; choose its matching saved profile',403);await api(ctx.base,'/api/computer/start','POST',{session:ctx.session},ctx.session,ctx.token,1980000);const result=await api(ctx.base,'/api/computer/open','POST',{url:b.url,profile:ctx.config.cua_profile||profile,session:ctx.session},ctx.session,ctx.token);return {result,state:{backend:'cua',profile:ctx.config.cua_profile||profile,url:b.url}};},
    verb:(ctx,state,verb,b)=>cua(ctx,state,verb,b),
  },
};
// The configured order (browser-methods.toml `ladder`), restricted to
// backends this driver implements; CUA only when enabled for the profile.
export function ladder(ctx) {
  const wanted=Array.isArray(ctx.methods?.ladder)&&ctx.methods.ladder.length?ctx.methods.ladder:['amux','cdp','cua'];
  return wanted.filter((n,i)=>BACKENDS[n]&&wanted.indexOf(n)===i&&(n!=='cua'||ctx.config.allow_cua));
}
const nextRung=(ctx,backend)=>{const l=ladder(ctx);const i=l.indexOf(backend);return i<0?l.length:i+1;};
async function launch(ctx,b,attempts=[],from=0) {
  if(!b.url)throw new RouteError('start requires a URL',400);
  if(!/^https?:|^about:/.test(b.url))throw new RouteError('route URL must be HTTP(S) or about:',400);
  const profile=b.profile||ctx.config.native_profile;
  const rungs=ladder(ctx);
  for(let i=from;i<rungs.length;i++) {
    const backend=rungs[i];const begun=Date.now();
    try {
      const {state,result}=await BACKENDS[backend].start(ctx,b,profile,attempts);
      attempts.push({backend,verdict:result.joined?'joined':'ready',elapsed_ms:Date.now()-begun,...(result.joined?{owner:result.owner}:{})});state.attempts=attempts;state.selected_profile=b.selected_profile||profile;if(b.profile_chosen_by)state.profile_chosen_by=b.profile_chosen_by;state.identity=ctx.identity||'';state.native_started=backend==='amux'||!!ctx.native_started;state.previous_routes=ctx.previous_routes||[];atomic(ctx.receipt,state);
      return {...result,ok:true,route:state,profile:state.profile};
    } catch(e) {
      attempts.push({backend,verdict:'failed',status:e.status||502,error:e.message,elapsed_ms:Date.now()-begun});
      const occupied=backend==='amux'&&e.status===409&&(e.payload?.running?.profile||e.payload?.error_code==='human_chrome_profile_in_use');
      if(terminal(e)&&!occupied)throw Object.assign(e,{attempts});
    }
  }
  throw Object.assign(new RouteError('all configured browser routes failed',503),{attempts});
}
async function stopOwnedNative(ctx,state) {
  try {return await native(ctx,'stop',{profile:state.selected_profile||state.profile,expected_started_by:ctx.session});}
  catch(e){
    if(!(e.status===409&&['browser_stop_target_unresolved','browser_stop_ownership_changed'].includes(e.payload?.code)))throw e;
    return {ok:true,stopped:false,note:'Original native browser was already released or its ownership changed; preserved the replacement.'};
  }
}
async function stopOwnedCdp(ctx,state) {
  try {return await directVerb(ctx,state,'stop',{});}
  catch(e) {
    // A reset pooled HTTP socket is inconclusive. Re-observe once without
    // replaying Target.closeTarget; only definite absence permits retirement.
    if(e.code==='ECONNRESET') {try {await target(state);}catch(observed){e=observed;}}
    // A refused local port or a confirmed absent target is already released.
    // Timeouts, protocol errors and authorization failures remain errors.
    if(!['ECONNREFUSED','cdp_target_gone'].includes(e.code))throw e;
    ctx.cleanup_events.push({backend:'cdp',profile:state.profile,target:state.target,verdict:'owned_cdp_already_closed'});
    return {ok:true,stopped:false,cleanup_verdict:'owned_cdp_already_closed',measured:true,n_considered:1};
  }
}
// ---- goal: an agent method drives this route --------------------------------
// The loop is shared; the agent's adapter only builds the model request and
// parses the reply (scripts/browser-agents/<protocol>.mjs). Every step goes
// through this route's own screenshot/action verbs, so the same rules apply
// as for a worker: one tab per session, no replay of an uncertain action, and
// the trajectory is kept for review.
function pngSize(buf){return buf.length>=24&&buf.subarray(0,8).equals(Buffer.from([137,80,78,71,13,10,26,10]))?{w:buf.readUInt32BE(16),h:buf.readUInt32BE(20)}:null;}
export function pickAgent(ctx,name){
  const agents=Array.isArray(ctx.methods?.agents)?ctx.methods.agents:[];
  if(name){const a=agents.find(x=>x.id===name);if(!a)throw new RouteError(`no browser agent ${JSON.stringify(name)}; configured: ${agents.map(x=>x.id).join(', ')||'none'}`,400);if(a.enabled===false)throw new RouteError(`browser agent ${JSON.stringify(name)} is disabled; enable it in ~/.amux/browser-methods.toml once its endpoint is serving`,409);return a;}
  const a=agents.find(x=>x.enabled!==false);
  if(!a)throw new RouteError('no browser agent is enabled; see browser-methods.toml',409);
  return a;
}
async function callAgent(agent,body){
  const ctl=new AbortController();const t=setTimeout(()=>ctl.abort(),Math.max(5,agent.timeout_s||180)*1000);
  try {
    const headers={'Content-Type':'application/json'};
    if(agent.api_key_env&&process.env[agent.api_key_env])headers.Authorization=`Bearer ${process.env[agent.api_key_env]}`;
    const r=await fetch(agent.endpoint,{method:'POST',headers,body:JSON.stringify(body),signal:ctl.signal});
    const text=await r.text();
    if(!r.ok)throw new RouteError(`agent endpoint answered HTTP ${r.status}: ${text.slice(0,300)}`,502);
    try{return JSON.parse(text);}catch{return text;}
  } catch(e){if(e instanceof RouteError)throw e;throw new RouteError(`agent endpoint ${agent.endpoint} unreachable: ${e.name==='AbortError'?'timed out':e.message}`,502);}
  finally{clearTimeout(t);}
}
async function viewportOf(ctx,state){
  if(state.backend==='cua')return null;
  try{const r=await route(ctx,'action',{action:'eval',script:'({w:innerWidth,h:innerHeight})'});return r?.data?.result||r?.result||null;}catch{return null;}
}
async function perform(ctx,a,scale){
  const px=v=>Math.round(v*scale);
  const click=async(x,y)=>route(ctx,'action',{action:'click',x:px(x),y:px(y)});
  switch(a.kind){
    case 'click': return click(a.x,a.y);
    case 'double_click': await click(a.x,a.y); return click(a.x,a.y);
    case 'hover': return route(ctx,'action',{action:'eval',script:`(()=>{const e=document.elementFromPoint(${px(a.x)},${px(a.y)});e&&e.dispatchEvent(new MouseEvent('mouseover',{bubbles:true}));return !!e;})()`});
    case 'type': if(Number.isFinite(a.x))await click(a.x,a.y); await route(ctx,'action',{action:'type',text:a.text||''}); return a.enter?route(ctx,'action',{action:'key',key:'Enter'}):{ok:true};
    case 'key': if(Number.isFinite(a.x))await click(a.x,a.y); return route(ctx,'action',{action:'key',key:a.key||'Enter'});
    case 'scroll': return route(ctx,'action',{action:'scroll',dx:a.dx||0,dy:a.dy||0});
    case 'goto': if(!/^https?:/.test(a.url||''))throw new RouteError('agent asked to open a non-HTTP(S) URL',422); return route(ctx,'navigate',{url:a.url});
    case 'back': return route(ctx,'action',{action:'back'});
    case 'forward': return route(ctx,'action',{action:'eval',script:'history.forward(),true'});
    case 'wait': await pause(Math.min(30,a.seconds||2)*1000); return {ok:true};
    default: throw new RouteError(`unsupported agent action ${JSON.stringify(a.kind)}`,422);
  }
}
async function goal(ctx,state,b){
  const task=typeof b.task==='string'?b.task.trim():'';
  if(!task)throw new RouteError('goal requires a task',400);
  const agent=pickAgent(ctx,b.agent);
  const modPath=agent.adapter_path||ctx.agent_modules?.[agent.protocol];
  if(!modPath)throw new RouteError(`no adapter for protocol ${JSON.stringify(agent.protocol)}`,500);
  const adapter=await import(pathToFileURL(modPath).href);
  const maxSteps=Math.max(1,Math.min(50,b.max_steps||agent.max_steps||15));
  const id=new Date().toISOString().replace(/[:.]/g,'-')+'-'+key(ctx.session).slice(0,8);
  const log=join(ctx.home,'browser-routing','goals',id+'.json');
  const record={id,session:ctx.session,task,agent:agent.id,protocol:agent.protocol,backend:state.backend,profile:state.profile,started_at:new Date().toISOString(),steps:[]};
  const history=[];let failures=0;
  const finish=(extra)=>{record.finished_at=new Date().toISOString();Object.assign(record,extra);atomic(log,record);return {...extra,steps:record.steps.length,agent:agent.id,backend:state.backend,trajectory:log,route:state};};
  for(let n=1;n<=maxSteps;n++){
    const shot=await route(ctx,'screenshot',{});
    const png=readFileSync(shot.path);const image=pngSize(png)||{w:1280,h:800};
    const vp=await viewportOf(ctx,state);
    const scale=vp&&vp.w&&image.w?vp.w/image.w:1;
    const step={task,screenshot_b64:png.toString('base64'),image,url:shot.url||state.url,history,agent};
    const begun=Date.now();
    const reply=await callAgent(agent,adapter.request(step));
    const out=adapter.parse(reply,step);
    const entry={n,think:out.think||null,tool:out.tool||null,action:out.action||null,done:out.done||null,error:out.error||null,model_ms:Date.now()-begun,url:step.url,screenshot:shot.path,image,scale};
    record.steps.push(entry);atomic(log,record);
    if(out.done)return finish({ok:true,done:true,answer:out.done.answer});
    if(!out.action){if(++failures>=2)return finish({ok:false,done:false,reason:'the agent gave no usable action twice in a row',last_error:out.error});continue;}
    failures=0;
    try{await perform(ctx,out.action,scale);}catch(e){entry.error=e.message;atomic(log,record);return finish({ok:false,done:false,reason:'action failed',last_error:e.message});}
    history.push({tool:out.tool,action:out.action});
    await pause(agent.settle_ms??800);
  }
  return finish({ok:false,done:false,reason:`no answer within ${maxSteps} steps`});
}
export async function route(ctx,verb,b={}) {
  if(!component(ctx.session))throw new RouteError('route requires an explicit session',400);
  ctx.cleanup_events=[];
  ctx.receipt=join(ctx.home,'browser-routing','sessions',key(ctx.session)+'.json');
  const state=read(ctx.receipt,null);
  if(verb==='start') {
    // Validate before closing a usable route. Replacement never abandons its
    // old receipt, tabs or desktop; an unknown cleanup failure retains them.
    if(!b.url||!/^https?:|^about:/.test(b.url))throw new RouteError('start requires an HTTP(S) or about: URL',400);
    const selectedIdentity=ctx.identity;
    if(state) {
      await route(ctx,'stop',{});
      ctx.cleanup_events.push({backend:state.backend,profile:state.profile,target:state.target,verdict:'owned_route_replaced'});
    }
    ctx.identity=selectedIdentity;ctx.native_started=false;ctx.previous_routes=[];
    const result=await launch(ctx,b);
    return {...result,...(ctx.cleanup_events.length?{cleanup_events:ctx.cleanup_events}:{})};
  }
  if(!state) {if(verb==='status')return {running:false};throw new RouteError('select a profile and start the browser route first',409);}
  ctx.identity=state.identity||ctx.identity;
  ctx.native_started=!!state.native_started;
  ctx.previous_routes=[...(state.previous_routes||[])];
  if(verb==='advance') {
    const reason=typeof b.reason==='string'?b.reason.trim():'';
    if(!reason)throw new RouteError('advance requires a reason describing the unmet goal',400);
    if(nextRung(ctx,state.backend)>=ladder(ctx).length)throw new RouteError(`${state.backend} is the final configured route; report the unmet goal instead of switching accounts`,409);
    ctx.previous_routes.push({...state,previous_routes:undefined});
    return launch(ctx,{url:state.url,profile:state.selected_profile||state.profile,selected_profile:state.selected_profile||state.profile},[...state.attempts,{backend:state.backend,verdict:'goal_unmet',reason:reason.slice(0,500)}],nextRung(ctx,state.backend));
  }
  if(verb==='stop'&&state.backend!=='amux'&&state.native_started) {
    // Only release the original browser when this worker started it. A busy
    // fallback owned by somebody else has native_started=false.
    await stopOwnedNative(ctx,state);
    state.native_started=false;atomic(ctx.receipt,state);
  }
  if(verb==='stop') {
    // Handoffs retain the worker's old tabs for inspection until explicit stop.
    // Keep each completed cleanup durable so a retry does not repeat it.
    for(const prior of [...(state.previous_routes||[])]) {
      if(prior.backend==='cdp') {
        await stopOwnedCdp(ctx,prior);
      }
      state.previous_routes=state.previous_routes.filter(p=>p!==prior);atomic(ctx.receipt,state);
    }
  }
  try {
    if(verb==='goal')return await goal(ctx,state,b);
    const backend=BACKENDS[state.backend];
    if(!backend)throw new RouteError(`this route's backend ${JSON.stringify(state.backend)} is not available in this driver`,409);
    const result=await backend.verb(ctx,state,verb,b);
    if(verb==='stop')rmSync(ctx.receipt,{force:true});else atomic(ctx.receipt,state);return {...result,route:state,...(ctx.cleanup_events.length?{cleanup_events:ctx.cleanup_events}:{})};
  } catch(e) {
    if(terminal(e)||verb==='stop'||verb==='goal'||nextRung(ctx,state.backend)>=ladder(ctx).length)throw e;
    ctx.previous_routes.push({...state,previous_routes:undefined});
    const next=await launch(ctx,{url:state.url,profile:state.selected_profile||state.profile,selected_profile:state.selected_profile||state.profile},[...state.attempts,{backend:state.backend,verdict:'failed',error:e.message}],nextRung(ctx,state.backend));
    // An uncertain mutation must never be repeated in a second identity/context.
    if(verb==='action')throw Object.assign(new RouteError('browser route changed; action was not replayed. Observe state/screenshot before retrying.',409),{route:next.route});
    return route(ctx,verb,b);
  }
}
export async function run(input) {
  try {return await route(input.context,input.verb,input.body||{});}catch(e){return {error:e.message,status:e.status||502,attempts:e.attempts,route:e.route,...(input.context.cleanup_events?.length?{cleanup_events:input.context.cleanup_events}:{})};}
}
// Node canonicalizes the module URL, but keeps the caller's symlink spelling
// in argv. Compare canonical paths so /tmp on macOS still executes the driver.
// An argv[1] that is not a file (a test runner's pattern, a deleted path) is
// not this module being executed; it must not throw on import.
const canonicalEntry=entry=>{try{return realpathSync(entry);}catch{return entry;}};
if(process.argv[1]&&import.meta.url===pathToFileURL(canonicalEntry(process.argv[1])).href) {
  let text='';for await(const chunk of process.stdin)text+=chunk;
  try {const result=await run(JSON.parse(text));process.stdout.write(JSON.stringify(result));}catch(e){process.stdout.write(JSON.stringify({error:e.message,status:400}));}
}
