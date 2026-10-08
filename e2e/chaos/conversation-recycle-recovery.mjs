import fs from 'node:fs';
import path from 'node:path';
import {startAmux,waitFor} from './harness.mjs';
const checks=[];const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw new Error(name+': '+JSON.stringify(detail));};
const amux=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{RUST_LOG:'info',AMUX_ISOLATED:'0',AMUX_BOARD_DRIVE_SECS:'0',AMUX_AUTOFIX_SECS:'0',AMUX_GHOST_RESCUE_SECS:'0',AMUX_MODEL_CATALOG_REFRESH_SECS:'0',AMUX_AUTO_RESUME:'0',ANTHROPIC_API_KEY:'',OPENAI_API_KEY:'',GEMINI_API_KEY:'',GOOGLE_API_KEY:''}});
const lanes=[];const metaPath=n=>path.join(amux.home,'sessions',n+'.meta.json');const meta=n=>JSON.parse(fs.readFileSync(metaPath(n),'utf8'));const launches=n=>amux.fakeLog().filter(x=>x.event==='launch'&&x.cwd===lanes.find(x=>x.name===n).realDir);const delivered=text=>amux.fakeLog().filter(x=>x.text===text);const alive=pid=>{try{process.kill(pid,0);return true;}catch{return false;}};
try{
 for(const name of ['recycle-live','recycle-dual','recycle-capped','recycle-expired']){
  const dir=path.join(amux.root,name);fs.mkdirSync(dir);fs.writeFileSync(path.join(dir,'work-preserved.txt'),'unfinished work\n');
  const created=await amux.req('POST','/api/sessions',{name,dir,start:false});check('private lane created '+name,created.status===201,created.body);
  fs.appendFileSync(path.join(amux.home,'sessions',name+'.env'),'\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\nCC_ISOLATED=0\n');
  const started=await amux.req('POST','/api/sessions/'+name+'/start');check('real provider starts '+name,started.status<300,started.body);lanes.push({name,dir,realDir:fs.realpathSync(dir)});await waitFor('actual launch '+name,()=>launches(name).length===1);
 }
 const originals=Object.fromEntries(lanes.map(x=>[x.name,launches(x.name)[0].pid]));
 for(const name of ['recycle-live','recycle-dual']){
  const r=await amux.req('PATCH','/api/sessions/'+name+'/config',{new_conversation:true,restart:true});check('owner recycle accepted '+name,r.status===202,r.body);
  check('accepted recycle has durable intent '+name,meta(name).recycle_in_progress_since>0,meta(name));
  await waitFor('actual retiring provider receives exit '+name,()=>amux.fakeLog().some(x=>x.pid===originals[name]&&x.text==='/exit'));
 }
 await amux.down();check('controller SIGKILL leaves retiring provider alive',alive(originals['recycle-live']),originals);
 // Explicit private phase controls: a crash can leave both lifecycle markers,
 // and capped/expired intentions must not destructively replay a live worker.
 process.kill(originals['recycle-dual'],'SIGKILL');
 const now=Math.floor(Date.now()/1000);
 for(const [name,fields] of [['recycle-dual',{start_in_progress_since:now}],['recycle-capped',{recycle_in_progress_since:now,recycle_resume_attempts:3}],['recycle-expired',{recycle_in_progress_since:now-7200,recycle_resume_attempts:0}]])fs.writeFileSync(metaPath(name),JSON.stringify({...meta(name),...fields}));
 await amux.up();
 const response=await amux.req('POST','/api/sessions/recycle-live/send',{text:'intent-for-replacement',msg_id:'recycle-replacement-proof',no_board:true,record_history:true});
 check('post-crash owner input is accepted',response.status<300,response.body);
 check('post-crash input waits durably for replacement',response.body.submitted!==true&&(response.body.submission==='deferred'||!!response.body.queue_id||String(response.body.message).includes('queued')),response.body);
 await new Promise(r=>setTimeout(r,4000));check('old provider never receives replacement input',!delivered('intent-for-replacement').some(x=>x.pid===originals['recycle-live']),delivered('intent-for-replacement'));
 await waitFor('interrupted recycles finish on actual boot clock',()=>launches('recycle-live').length===2&&launches('recycle-dual').length===2,65000);
 await waitFor('queued input reaches replacement exactly once',()=>delivered('intent-for-replacement').length===1,30000);
 check('input reaches actual replacement PID',delivered('intent-for-replacement')[0].pid===launches('recycle-live')[1].pid,delivered('intent-for-replacement'));
 check('both markers produce one replacement, not two',launches('recycle-dual').length===2,launches('recycle-dual'));
 check('recovered launches do not resume old conversations',['recycle-live','recycle-dual'].every(n=>!launches(n)[1].argv.some(x=>x==='--resume'||x==='--continue')),lanes.slice(0,2).map(x=>launches(x.name)[1].argv));
 check('completed recycle clears its durable marker',['recycle-live','recycle-dual'].every(n=>!meta(n).recycle_in_progress_since));
 check('capped and expired controls never restart',['recycle-capped','recycle-expired'].every(n=>launches(n).length===1));
 check('workspace survives interrupted recycle',lanes.every(x=>fs.readFileSync(path.join(x.dir,'work-preserved.txt'),'utf8')==='unfinished work\n'));
 check('boot recovery emits its durable boundary',fs.readFileSync(amux.serverLog,'utf8').includes('interrupted_recycle_resumed'));
 await amux.down();await amux.up();await new Promise(r=>setTimeout(r,20000));
 check('second controller crash never repeats completed recycles',['recycle-live','recycle-dual'].every(n=>launches(n).length===2));
 check('accepted input is not duplicated across another crash',delivered('intent-for-replacement').length===1);
 const renewed=await amux.req('PATCH','/api/sessions/recycle-capped/config',{new_conversation:true,restart:true});
 check('new explicit owner recycle is accepted after earlier cap',renewed.status===202,renewed.body);
 check('a new owner operation resets only its own retry counter',meta('recycle-capped').recycle_in_progress_since>0&&meta('recycle-capped').recycle_resume_attempts===0,meta('recycle-capped'));
}catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});console.error(fs.readFileSync(amux.serverLog,'utf8').slice(-6000));}
finally{await amux.stop();}
const receipt={measured:true,n_considered:checks.length,failed:checks.filter(x=>!x.ok).length,artifacts:amux.root,fixture_boundary:'actual owner recycle API and exit bytes, controller SIGKILL, real fifteen-second boot pass, actual provider consumer; explicit private dual-start/capped/expired metadata controls',checks};fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
