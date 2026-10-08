#!/usr/bin/env node
// Actual git checkout failure, retained queue intent, automatic retry and
// controller crashes. Local bare origin only; no production pushes or models.
import fs from 'node:fs';
import path from 'node:path';
import {startAmux,waitFor,git} from './harness.mjs';
const amux=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{
  RUST_LOG:'info',AMUX_ISOLATED:'0',AMUX_BOARD_DRIVE_SECS:'0',
  AMUX_AUTOFIX_SECS:'0',AMUX_GHOST_RESCUE_SECS:'0',AMUX_MODEL_CATALOG_REFRESH_SECS:'0',
}});
const checks=[];
const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw Error(name+': '+JSON.stringify(detail));};
const lane=path.join(amux.root,'lane'),origin=path.join(amux.root,'origin.git');
const items=async()=> (await amux.req('GET','/api/land')).body.items;
const candidates=()=>fs.existsSync(path.join(amux.home,'tmp/land'))?fs.readdirSync(path.join(amux.home,'tmp/land')):[];
try {
  fs.mkdirSync(origin);git(origin,'init','--bare','-q');
  fs.mkdirSync(lane);git(lane,'init','-q');git(lane,'checkout','-qb','main');
  git(lane,'config','user.name','fixture');git(lane,'config','user.email','fixture@example.invalid');
  fs.writeFileSync(path.join(lane,'base.txt'),'base\n');git(lane,'add','.');git(lane,'commit','-qm','base');
  git(lane,'remote','add','origin',origin);git(lane,'push','-q','origin','HEAD:main');
  const before=git(origin,'rev-parse','refs/heads/main');
  fs.writeFileSync(path.join(lane,'feature.txt'),'only once\n');git(lane,'add','.');git(lane,'commit','-qm','feature');
  const sha=git(lane,'rev-parse','HEAD');
  const created=await amux.req('POST','/api/sessions',{name:'land-worker',dir:lane,start:false});
  check('private lane registered',created.status===201,created.body);
  fs.appendFileSync(path.join(amux.home,'sessions/land-worker.env'),'\nAMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_RULES_OFF=1,2,3,4,6,7,8,9,10\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\n');
  const hook=path.join(lane,'.git/hooks/post-checkout');
  fs.writeFileSync(hook,'#!/bin/sh\ngit worktree lock --reason initializing "$PWD"\necho actual-checkout-start-failed >&2\nexit 37\n',{mode:0o755});
  const queued=await amux.req('POST','/api/land',{sha},10000,{'X-Amux-Session':'land-worker'});
  check('actual land intent queued',queued.status===202,queued.body);
  const id=queued.body.id;
  await waitFor('failed checkout is requeued after cleanup',async()=> (await items()).some(x=>x.id===id&&x.state==='queued'&&x.output?.includes('actual-checkout-start-failed')),45000);
  check('failed startup cannot land or pretend success',git(origin,'rev-parse','refs/heads/main')===before);
  check('failed candidate leaves no locked registry or checkout',candidates().length===0&&!git(lane,'worktree','list','--porcelain').includes('locked initializing'));
  check('repository land lock released before retry',!fs.readdirSync(path.join(amux.home,'locks')).some(x=>x.startsWith('land-')));
  check('failure has a named retained signal',fs.readFileSync(amux.serverLog,'utf8').includes('land_candidate_start_failed'));
  await amux.down();
  fs.unlinkSync(hook);
  await amux.up();
  await waitFor('retained intent automatically lands after restart',async()=> (await items()).some(x=>x.id===id&&x.state==='merged'),45000);
  check('committed feature reached local main',git(origin,'show','refs/heads/main:feature.txt')==='only once');
  check('successful candidate is also cleaned',candidates().length===0&&!git(lane,'worktree','list','--porcelain').includes('/tmp/land/'));
  const merged=git(origin,'rev-parse','refs/heads/main');
  await amux.down();await amux.up();
  const final=await items();
  check('second crash retains one terminal landing',final.length===1&&final[0].id===id&&final[0].state==='merged',final);
  check('already completed landing is never pushed twice',git(origin,'rev-parse','refs/heads/main')===merged&&git(origin,'rev-list','--count','refs/heads/main')==='2');
  check('cleanup has an actual completion signal',fs.readFileSync(amux.serverLog,'utf8').includes('land_candidate_removed'));
}catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});}
finally{await amux.stop();}
const receipt={measured:true,n_considered:checks.length,failed:checks.filter(x=>!x.ok).length,artifacts:amux.root,
  fixture_boundary:'real HTTPS land queue, local git/bare origin, actual failed locked checkout, durable requeue and released lock, automatic retry, two controller SIGKILL restarts, no duplicate push',checks};
fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
