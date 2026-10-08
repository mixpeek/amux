#!/usr/bin/env node
// Kill between a resume decision and durable handoff by holding only this
// fixture's SQLite writer. The stopped turn's retry clock is explicitly seeded.
import fs from 'node:fs';
import path from 'node:path';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {startAmux,waitFor} from './harness.mjs';
const amux=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{RUST_LOG:'info',AMUX_ISOLATED:'0',AMUX_BOARD_DRIVE_SECS:'0',AMUX_AUTOFIX_SECS:'0',AMUX_GHOST_RESCUE_SECS:'0',AMUX_MODEL_CATALOG_REFRESH_SECS:'0',AMUX_RATE_LIMIT_SWEEP_S:'2',AMUX_AUTO_RESUME:'',ANTHROPIC_API_KEY:'',OPENAI_API_KEY:'',FAKE_CLAUDE_SPAWN_BACKGROUND:'1',FAKE_CLAUDE_EXTRA_FRAME:'API Error: Connection lost mid-response. The response above may be incomplete.\nChurned for 1m 30s · done · 1 shell still running',FAKE_CLAUDE_BACKGROUND_FOOTER:' · 1 shell · ← 5 agents · ↓ to manage'}});
const checks=[];const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw Error(name+': '+JSON.stringify(detail));};
let lock;let lockExit;
const received=()=>amux.fakeLog().filter(x=>x.text==='continue');
try{
 const name='handoff-parent';const dir=path.join(amux.root,name);fs.mkdirSync(dir);
 const created=await amux.req('POST','/api/sessions',{name,dir,start:false});check('private worker created',created.status===201,created.body);
 const ep=path.join(amux.home,'sessions',name+'.env');fs.appendFileSync(ep,'\nAMUX_AUTO_RESUME=0\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\n');
 const started=await amux.req('POST',`/api/sessions/${name}/start`);check('real provider starts',started.status<300,started.body);
 await waitFor('provider launch',()=>amux.fakeLog().some(x=>x.event==='launch'),30000);
 const cid='11111111-1111-4111-8111-000000000001';const folder=path.join(amux.userHome,'.claude','projects',fs.realpathSync(dir).replace(/[^a-zA-Z0-9]/g,'-'));fs.mkdirSync(folder,{recursive:true});
 fs.writeFileSync(path.join(folder,cid+'.jsonl'),JSON.stringify({type:'assistant',error:'server_error',isApiErrorMessage:true,timestamp:new Date().toISOString(),message:{role:'assistant',content:[{type:'text',text:'API Error: Connection lost mid-response. The response above may be incomplete.'}]}})+'\n');
 const mp=path.join(amux.home,'sessions',name+'.meta.json');
 await waitFor('observer records retryable stop',()=>fs.existsSync(mp)&&JSON.parse(fs.readFileSync(mp)).api_error_since>0,30000);
 await amux.down();const meta=JSON.parse(fs.readFileSync(mp));const errorSince=Math.floor(Date.now()/1000)-180;Object.assign(meta,{api_error_since:errorSince,api_error_code:'server_error',cc_conversation_id:cid,cc_cwd:fs.realpathSync(dir)});delete meta.auto_resume_for;fs.writeFileSync(mp,JSON.stringify(meta));await amux.up();
 lock=spawn('python3',['-u','-c','import sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=10); c.execute("BEGIN IMMEDIATE"); print("locked",flush=True); sys.stdin.read(); c.rollback(); c.close()',path.join(amux.home,'amux.db')],{env:amux.env,stdio:['pipe','pipe','pipe']});lockExit=once(lock,'exit');
 let locked=false;lock.stdout.on('data',d=>{if(String(d).includes('locked'))locked=true;});
 await waitFor('fixture owns writer lock',()=>locked,12000);check('writer is genuinely held',locked);
 fs.writeFileSync(ep+'.new',fs.readFileSync(ep,'utf8').replace('AMUX_AUTO_RESUME=0','AMUX_AUTO_RESUME=1'));fs.renameSync(ep+'.new',ep);
 const key=`api:${errorSince}`;
 await waitFor('resume reaches its handoff boundary',()=>JSON.parse(fs.readFileSync(mp)).auto_resume_for===key||fs.readFileSync(amux.serverLog,'utf8').includes('auto_resume_staging'),20000);
 check('nothing reached provider before crash',received().length===0);
 await amux.down();check('no terminal delivery during the interrupted handoff',received().length===0);
 lock.stdin.end();await lockExit;lock=null;
 await amux.up();
 await waitFor('uncommitted resume handoff recovers after SIGKILL',()=>received().length===1,30000);
 check('one continuation reaches actual provider',received().length===1,received());
 const child=amux.fakeLog().find(x=>x.event==='background_child');check('background child survives the handoff crash',!!child&&(()=>{try{process.kill(child.pid,0);return true;}catch{return false;}})(),child);
 await amux.down();
 // Explicitly model loss of the producer's acknowledgement after the durable
 // consumer accepted it. The queue/history identity must adopt, not repeat it.
 const lostAck=JSON.parse(fs.readFileSync(mp));delete lostAck.auto_resume_for;delete lostAck.auto_resume_api_count;delete lostAck.auto_resume_api_window_start;fs.writeFileSync(mp,JSON.stringify(lostAck));
 await amux.up();await new Promise(r=>setTimeout(r,6500));
 check('committed continuation is not repeated after another crash and lost producer acknowledgement',received().length===1,received());
 check('accepted continuation has a named durable handoff signal',fs.readFileSync(amux.serverLog,'utf8').includes('auto_resume_queued'));
}catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});console.error(fs.readFileSync(amux.serverLog,'utf8').slice(-6000));}
finally{if(lock){lock.stdin.end();await lockExit;}for(const x of amux.fakeLog().filter(x=>x.event==='background_child')){try{process.kill(x.pid,'SIGKILL');}catch{}}await amux.stop();}
const receipt={measured:true,n_considered:checks.length,failed:checks.filter(x=>!x.ok).length,artifacts:amux.root,fixture_boundary:'private writer held at decision/handoff; retry clock and loss of producer acknowledgement seeded with controller down; actual provider child and two real SIGKILL/restarts',checks};fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
