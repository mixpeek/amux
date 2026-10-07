#!/usr/bin/env node
// Real idle-pool claim and terminal dispatch, with SIGKILL between them.
// The 13-card component exceeds the normal cap (8). Only the ready root moves;
// the dependent proof ownership and ordering edges must survive two crashes.
// The fake CLI records pasted lines separately: count the pickup marker, not
// every occurrence of the card ID in its criteria and artifact instructions.
import fs from 'node:fs';
import path from 'node:path';
import { startAmux, waitFor } from './harness.mjs';
const checks=[];
const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw new Error(name+': '+JSON.stringify(detail));};
const amux=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{AMUX_ISOLATED:'0',AMUX_BOARD_DRIVE_SECS:'3600',AMUX_AUTOFIX_SECS:'0',AMUX_GHOST_RESCUE_SECS:'0',AMUX_MODEL_CATALOG_REFRESH_SECS:'0',ANTHROPIC_API_KEY:'',OPENAI_API_KEY:'',GEMINI_API_KEY:'',GOOGLE_API_KEY:''}});
try{
 for(const name of ['pool-hub','pool-worker']){
  const r=await amux.req('POST','/api/sessions',{name,dir:amux.root,start:false});check('worker created '+name,r.status===201,r.body);
  fs.appendFileSync(path.join(amux.home,'sessions',name+'.env'),`\nCC_ISOLATED=0\nAMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_HUB=pool-hub\nAMUX_CONTRACT_RULES_OFF=1,2,4,5,6,7,10,A1\n${name==='pool-worker'?'CC_STANDING_ORDERS=1\nCC_AUTO_PICKUP=1\nCC_AUTO_CONTINUE=0\nAMUX_BOARD_FORCE_ADHERENCE=1\nAMUX_A2_POOL_TITLE_REGEX=^GS12 proof\n':''}`);
 }
 const root=await amux.req('POST','/api/board',{title:'Measure the root readiness',desc:'Run echo readiness-proved and record its output. This concrete prerequisite unblocks the proof population.',next_action:'Run echo readiness-proved',type:'chore',status:'backlog',session:'pool-hub'});check('root created',root.status<300,root.body);
 const parents=[];
 for(let i=0;i<12;i++){const r=await amux.req('POST','/api/board',{title:'GS12 proof real pool '+i,type:'ops',status:'backlog',session:'pool-hub',depends_on:[root.body.id]});check('dependent created '+i,r.status<300,r.body);parents.push(r.body.id);}
 const started=await amux.req('POST','/api/sessions/pool-worker/start');check('real terminal starts',started.status<300,started.body);
 await waitFor('fake agent launch',()=>amux.fakeLog().find(r=>r.event==='launch'),30000);
 await waitFor('worker ready',async()=>{const s=(await amux.req('GET','/api/sessions')).body;const w=s.find(w=>w.name==='pool-worker');return w?.running&&w.status==='idle';},60000);
 const tick=await amux.req('POST','/api/system-jobs/board-drive/run');check('real board clock triggered',tick.status===200,tick.body);
 const assigned=await waitFor('autonomous prerequisite claim',async()=>{const r=(await amux.req('GET','/api/board/'+root.body.id)).body;return r.session==='pool-worker'?r:null;},30000);
 check('one root claimed autonomously from over-cap graph',assigned.status==='todo',assigned);
 check('assignment precedes delivery',amux.fakeLog().filter(r=>r.text?.startsWith(`[amux auto-pickup] Claimed ${root.body.id} `)).length===0,amux.fakeLog());
 await amux.down();await amux.up();
 await waitFor('automatic delivery after restart',()=>amux.fakeLog().find(r=>r.text?.startsWith(`[amux auto-pickup] Claimed ${root.body.id} `)),60000);
 check('one pickup command reaches the actual terminal after restart',amux.fakeLog().filter(r=>r.text?.startsWith(`[amux auto-pickup] Claimed ${root.body.id} `)).length===1,amux.fakeLog());
 check('recovered task is doing',(await amux.req('GET','/api/board/'+root.body.id)).body.status==='doing');
 for(const id of parents){const r=(await amux.req('GET','/api/board/'+id)).body;check('rollup edge and owner retained '+id,r.session==='pool-hub'&&r.depends_on.includes(root.body.id),r);}
 await amux.down();await amux.up();
 await new Promise(r=>setTimeout(r,2000));
 check('another restart does not repeat the pickup command',amux.fakeLog().filter(r=>r.text?.startsWith(`[amux auto-pickup] Claimed ${root.body.id} `)).length===1,amux.fakeLog());
}catch(e){console.error(JSON.stringify((await amux.req('GET','/api/debug/board-drive')).body,null,2));checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});console.error(fs.readFileSync(amux.serverLog,'utf8').slice(-15000));}
finally{await amux.stop();}
const receipt={measured:checks.length>0,n_considered:checks.length,failed:checks.filter(c=>!c.ok).length,artifacts:amux.root,checks};fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
