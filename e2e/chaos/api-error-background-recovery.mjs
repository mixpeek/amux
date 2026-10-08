#!/usr/bin/env node
// Actual sweep + private terminal + current provider transcript + real surviving
// background PID. The current failed turn is seeded; the real retry clock must elapse.
import fs from 'node:fs';
import path from 'node:path';
import { startAmux, waitFor } from './harness.mjs';
const checks=[];
const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw new Error(name+': '+JSON.stringify(detail));};
const amux=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{RUST_LOG:'info',AMUX_ISOLATED:'0',AMUX_BOARD_DRIVE_SECS:'0',AMUX_AUTOFIX_SECS:'0',AMUX_GHOST_RESCUE_SECS:'0',AMUX_MODEL_CATALOG_REFRESH_SECS:'0',AMUX_RATE_LIMIT_SWEEP_S:'10',AMUX_AUTO_RESUME:'1',ANTHROPIC_API_KEY:'',OPENAI_API_KEY:'',GEMINI_API_KEY:'',GOOGLE_API_KEY:'',FAKE_CLAUDE_SPAWN_BACKGROUND:'1',FAKE_CLAUDE_COMPOSER_FOOTER_FILE:'.native-quota-footer',FAKE_CLAUDE_EXTRA_FRAME:'API Error: Connection lost mid-response. The response above may be incomplete.\nChurned for 1m 30s · done · 1 shell still running',FAKE_CLAUDE_BACKGROUND_FOOTER:' · 1 shell · ← 5 agents · ↓ to manage'}});
let backgroundPid;
const ownedChildren=()=>amux.fakeLog().filter(r=>r.event==='background_child').map(r=>r.pid);
const alive=()=>{try{process.kill(backgroundPid,0);return true;}catch{return false;}};
const lanes=[];
const received=name=>{const pid=amux.fakeLog().find(r=>r.event==='launch'&&r.cwd===lanes.find(l=>l.name===name)?.realDir)?.pid;return amux.fakeLog().filter(r=>r.pid===pid&&r.text==='continue');};
try{
 const before=(await amux.req('GET','/health')).body;
 const futureReset=Math.floor(Date.now()/1000)+600;
 for(const [index,name,kind,isolated] of [[1,'retry-parent','server_error',false],[2,'auth-parent','authentication_failed',false],[3,'isolated-parent','server_error',true],[4,'quota-parent','rate_limit',false],[5,'unclocked-parent','rate_limit',false]]){
  const dir=path.join(amux.root,name);fs.mkdirSync(dir);
  if(name==='quota-parent'){
   const date=new Date(futureReset*1000);const h=date.getHours();const clock=`${h%12||12}:${String(date.getMinutes()).padStart(2,'0')}${h>=12?'pm':'am'}`;
   fs.writeFileSync(path.join(dir,'.native-quota-footer'),`⚠ Usage limit reached · limit resets ${clock} · clau.de/wrap-up\nContinuing automatically at ${clock} · esc to cancel · /usage-credits to continue now`);
  }
  const r=await amux.req('POST','/api/sessions',{name,dir,start:false});check('private worker created '+name,r.status===201,r.body);
  fs.appendFileSync(path.join(amux.home,'sessions',name+'.env'),`\nCC_ISOLATED=${isolated?1:0}\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\n`);
  const started=await amux.req('POST','/api/sessions/'+name+'/start');check('real terminal starts '+name,started.status<300,started.body);
  await waitFor('fake launch '+name,()=>amux.fakeLog().find(r=>r.event==='launch'&&r.cwd===fs.realpathSync(dir)),30000);
  lanes.push({name,dir,realDir:fs.realpathSync(dir)});
  const cid=`11111111-1111-4111-8111-${String(index).padStart(12,'0')}`;
  const folder=path.join(amux.userHome,'.claude','projects',fs.realpathSync(dir).replace(/[^a-zA-Z0-9]/g,'-'));fs.mkdirSync(folder,{recursive:true});
  fs.writeFileSync(path.join(folder,cid+'.jsonl'),JSON.stringify({type:'assistant',error:kind,isApiErrorMessage:true,...(kind==='rate_limit'?{quotaLimits:{status:'rejected',resetsAt:name==='quota-parent'?futureReset:0}}:{}),timestamp:new Date().toISOString(),message:{role:'assistant',content:[{type:'text',text:'API Error: Connection lost mid-response. The response above may be incomplete.'}]}})+'\n');
  const mp=path.join(amux.home,'sessions',name+'.meta.json');const meta=fs.existsSync(mp)?JSON.parse(fs.readFileSync(mp,'utf8')):{};
  Object.assign(meta,{cc_conversation_id:cid,cc_cwd:dir});fs.writeFileSync(mp,JSON.stringify(meta));
 }
 const launch=amux.fakeLog().find(r=>r.event==='launch'&&r.cwd===lanes[0].realDir);
 const child=amux.fakeLog().find(r=>r.event==='background_child'&&r.parent_pid===launch.pid);
 check('background is an actual provider CLI child',!!child,child);
 backgroundPid=child.pid;
 check('background process is genuinely alive before retry',alive(),backgroundPid);
 await waitFor('normal sweep resumes failed foreground with live background',()=>received('retry-parent').length===1,155000);
 check('one continue reaches actual parent terminal',received('retry-parent').length===1,received('retry-parent'));
 check('foreground retry preserves background PID',alive(),backgroundPid);
 check('authentication failure is never retried',received('auth-parent').length===0);
 check('isolation is never overridden',received('isolated-parent').length===0);
 check('future quota reset remains parked behind live child',received('quota-parent').length===0);
 const quotaMetaPath=path.join(amux.home,'sessions','quota-parent.meta.json');
 const quotaMeta=JSON.parse(fs.readFileSync(quotaMetaPath,'utf8'));
 check('normal sweep recognizes actual split native footer',quotaMeta.rate_limited_by==='auto-resume'&&quotaMeta.rate_limited_until>Math.floor(Date.now()/1000),quotaMeta);

 check('unclocked limit is never retried',received('unclocked-parent').length===0);
 // Explicit private fixture clock transition while the controller is killed:
 // metadata cannot race a consumer write, and recovery must rediscover the hold.
 await amux.down();
 const quota=lanes.find(l=>l.name==='quota-parent');
 const qp=path.join(amux.userHome,'.claude','projects',quota.realDir.replace(/[^a-zA-Z0-9]/g,'-'),'11111111-1111-4111-8111-000000000004.jsonl');
 const record=JSON.parse(fs.readFileSync(qp,'utf8'));record.quotaLimits.resetsAt=Math.floor(Date.now()/1000)-120;fs.writeFileSync(qp,JSON.stringify(record)+'\n');
 const advancedMeta=JSON.parse(fs.readFileSync(quotaMetaPath,'utf8'));advancedMeta.rate_limited_until=record.quotaLimits.resetsAt;fs.writeFileSync(quotaMetaPath,JSON.stringify(advancedMeta));
 await amux.up();
 check('quota clock advancement is rediscovered after SIGKILL',(await amux.req('GET','/health')).body.pid!==before.pid);
 await waitFor('passed quota reset resumes stopped parent with surviving child',()=>received('quota-parent').length===1,30000);
 const qlaunch=amux.fakeLog().find(r=>r.event==='launch'&&r.cwd===quota.realDir);
 const qchild=amux.fakeLog().find(r=>r.event==='background_child'&&r.parent_pid===qlaunch.pid);
 check('quota recovery preserves actual provider child',!!qchild&&(()=>{try{process.kill(qchild.pid,0);return true;}catch{return false;}})(),qchild);
 check('quota boundary emits its distinct signal',fs.readFileSync(amux.serverLog,'utf8').includes('steer_usage_reset_boundary'));
 check('recovery emits its named boundary signal',fs.readFileSync(amux.serverLog,'utf8').includes('steer_api_error_boundary'));
 await amux.down();await amux.up();
 const after=(await amux.req('GET','/health')).body;check('SIGKILL restarts same binary',after.pid!==before.pid&&after.build===before.build,{before,after});
 await new Promise(r=>setTimeout(r,12500));
 check('durable retry key prevents a second continue after crash',received('retry-parent').length===1,received('retry-parent'));
 check('background PID survives server SIGKILL and resumed foreground',alive(),backgroundPid);
 check('quota reset is deduplicated across server crash',received('quota-parent').length===1);
 check('unclocked limit stays parked across crash',received('unclocked-parent').length===0);
 check('controls remain untouched after crash',received('auth-parent').length===0&&received('isolated-parent').length===0);
}catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});console.error(fs.readFileSync(amux.serverLog,'utf8').slice(-8000));}
finally{for(const pid of ownedChildren()){try{process.kill(pid,'SIGKILL');}catch{}}await amux.stop();}
const receipt={measured:checks.length>0,n_considered:checks.length,failed:checks.filter(c=>!c.ok).length,artifacts:amux.root,fixture_boundary:'seeded current failed transcript; API two-minute bound uses real periodic clock; quota future-to-passed transcript/metadata clock is explicitly seeded; actual split native footer rendered in private terminal; actual CLI children survive',checks};fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
