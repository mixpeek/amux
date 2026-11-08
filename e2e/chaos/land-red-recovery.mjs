#!/usr/bin/env node
// Z5 through a real server: a red main after a queue land gets one revert, and a
// controller SIGKILLed the moment the remote accepts that revert recovers without
// a second one. Local bare origin and a stub gh; no GitHub, no models.
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {startAmux,waitFor,git} from './harness.mjs';

const amux=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{
  PATH:[path.join(path.dirname(fileURLToPath(import.meta.url)),'bin'),process.env.PATH].join(path.delimiter),
  RUST_LOG:'info',AMUX_ISOLATED:'0',AMUX_BOARD_DRIVE_SECS:'0',
  AMUX_AUTOFIX_SECS:'0',AMUX_GHOST_RESCUE_SECS:'0',AMUX_MODEL_CATALOG_REFRESH_SECS:'0',
}});
const checks=[];
const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw Error(name+': '+JSON.stringify(detail));};
const lane=path.join(amux.root,'lane'),origin=path.join(amux.root,'origin.git'),runs=path.join(amux.root,'runs');
const report=async()=> (await amux.req('GET','/api/land')).body;
const log=()=>fs.readFileSync(amux.serverLog,'utf8');
const mainSha=()=>git(origin,'rev-parse','refs/heads/main');
const revertsOnMain=()=>git(origin,'log','--format=%s','refs/heads/main').split('\n').filter(s=>s.startsWith('Revert "')).length;
const ci=(sha,conclusion,attempt)=>fs.writeFileSync(path.join(runs,sha+'.json'),JSON.stringify({workflow_runs:[
  {id:7,name:'rust',status:'completed',conclusion,run_attempt:attempt,event:'push',html_url:'http://run/7'}]}));
try {
  fs.mkdirSync(runs);fs.mkdirSync(origin);git(origin,'init','--bare','-q','-b','main');
  fs.mkdirSync(lane);git(lane,'init','-q','-b','main');
  git(lane,'config','user.name','fixture');git(lane,'config','user.email','fixture@example.invalid');
  fs.mkdirSync(path.join(lane,'.amux'));
  fs.writeFileSync(path.join(lane,'.amux/land.conf'),'[amux]\n\tqueueGate = true\n\tqueueChecks = rust\n\tqueueOnRed = revert\n');
  git(lane,'add','.');git(lane,'commit','-qm','base');
  git(lane,'remote','add','origin',origin);git(lane,'push','-q','origin','HEAD:main');git(lane,'fetch','-q','origin');
  const stub=path.join(amux.root,'gh-stub');
  fs.writeFileSync(stub,`#!/bin/sh\ncase "$*" in\n*rerun-failed-jobs*) ;;\n*head_sha=*) q="$*"; s="\${q#*head_sha=}"; s="\${s%%&*}"; cat ${runs}/$s.json 2>/dev/null || echo '{"workflow_runs":[]}' ;;\nesac\n`,{mode:0o755});
  fs.writeFileSync(path.join(amux.home,'server.env'),`AMUX_LAND_RED_GH=${stub}\nAMUX_LAND_RED_SLUG=o/r\nAMUX_LAND_RED_TICK_S=1\n`);
  const base=mainSha();
  fs.writeFileSync(path.join(lane,'feature.txt'),'goes red\n');git(lane,'add','.');git(lane,'commit','-qm','feature');
  const sha=git(lane,'rev-parse','HEAD');
  const queued=await amux.req('POST','/api/land',{sha,tree:lane},10000,{'X-Amux-Session':'land-worker'});
  check('a declared repository queues with no contract rule',queued.status===202&&queued.body.declared===true,queued.body);
  await waitFor('the land merged',async()=> (await report()).items.some(x=>x.id===queued.body.id&&x.state==='merged'),60000);
  const tip=mainSha();
  check('the gate verdict is on the row',(await report()).items[0].gate_verdict==='green',(await report()).items);
  // The remote accepts the revert, then kills the controller before its receipt.
  const hook=path.join(origin,'hooks/post-receive');
  fs.writeFileSync(hook,'#!/bin/sh\nrm -f "$0"\n[ -n "$AMUX_LAND_HOLDER_PID" ] || exit 41\nkill -KILL "$AMUX_LAND_HOLDER_PID"\nsleep 1\n',{mode:0o755});
  ci(base,'success',1);ci(tip,'failure',2);
  await waitFor('the remote accepted the revert',()=>mainSha()!==tip&&!fs.existsSync(hook),90000);
  await amux.down();
  check('the revert reached main exactly once',revertsOnMain()===1,git(origin,'log','--format=%s','refs/heads/main'));
  check('the change left main',!git(origin,'ls-tree','--name-only','refs/heads/main').includes('feature.txt'));
  const accepted=mainSha();
  ci(accepted,'success',1);
  await amux.up();
  await waitFor('the episode finished after the restart',async()=>(await report()).red?.[0]?.state==='reverted',90000);
  const r=await report();
  check('one episode, one revert row',r.red.length===1&&r.items.filter(x=>x.state==='merged').length===2,r);
  check('the accepted push was adopted, nothing replayed',mainSha()===accepted&&revertsOnMain()===1);
  await amux.down();await amux.up();
  await new Promise(res=>setTimeout(res,25000));
  const again=await report();
  check('a second restart changes nothing',mainSha()===accepted&&again.red.length===1&&again.red[0].state==='reverted',again.red);
  check('no revert failed to build',!log().includes('land_red_revert_unbuildable'));
  for (const v of ['land_gate_green','land_red_revert_queued','land_push_receipt_adopted','land_red_reverted'])
    check('verdict in the log: '+v,log().includes(v));
}catch(e){
  // A timed-out wait says only that it timed out: carry the queue, the episodes
  // and the land log lines, so a red CI run shows where it stopped (AF-1162).
  let land=null;try{land=await report();}catch(err){land=String(err);}
  let lines=[];try{lines=log().split('\n').filter(l=>/verdict="land_|gh timed out/.test(l)).slice(-40);}catch{}
  checks.push({name:'scenario completed',ok:false,detail:{error:String(e.stack||e),land,log:lines}});
}
finally{await amux.stop();}
const receipt={measured:true,n_considered:checks.length,failed:checks.filter(x=>!x.ok).length,artifacts:amux.root,
  fixture_boundary:'real HTTPS land queue and land-red watch, local git/bare origin, stub gh, one controller SIGKILL at remote acceptance of the revert',checks};
fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
