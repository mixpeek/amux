#!/usr/bin/env node
// Actual git checkout failure, retained queue intent, automatic retry and
// controller crashes. Local bare origin only; no production pushes or models.
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import os from 'node:os';
import {fileURLToPath} from 'node:url';
import {startAmux,waitFor,git} from './harness.mjs';
// Fail only this private process's first real candidate creation, after Git
// has registered a locked checkout; production disables checkout hooks.
const faultRoot=fs.mkdtempSync(path.join(os.tmpdir(),'amux-land-fault-'));
const faultFlag=path.join(faultRoot,'fail-once');
const realGit=execFileSync('which',['git'],{encoding:'utf8'}).trim();
fs.writeFileSync(path.join(faultRoot,'git'),`#!/usr/bin/env python3
import os,sys,subprocess
args=sys.argv[1:]
p=subprocess.run([${JSON.stringify(realGit)}]+args)
if p.returncode==0 and os.path.exists(${JSON.stringify(faultFlag)}) and 'worktree' in args and 'add' in args:
    subprocess.run([${JSON.stringify(realGit)},'-C',args[-2],'worktree','lock','--reason','initializing',args[-2]],check=True)
    mode=open(${JSON.stringify(faultFlag)}).read()
    os.unlink(${JSON.stringify(faultFlag)})
    if mode=='crash':
        holder=os.getppid()
        os.kill(holder,9)
        open(${JSON.stringify(faultFlag)}+'.killed','w').write(str(holder))
    print('actual-checkout-start-failed',file=sys.stderr)
    sys.exit(37)
sys.exit(p.returncode)
`,{mode:0o755});
const amux=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{
  PATH:[faultRoot,path.join(path.dirname(fileURLToPath(import.meta.url)),'bin'),process.env.PATH].join(path.delimiter),
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
  fs.writeFileSync(faultFlag,'fail this private checkout once');
  const queued=await amux.req('POST','/api/land',{sha},10000,{'X-Amux-Session':'land-worker'});
  check('actual land intent queued',queued.status===202,queued.body);
  const id=queued.body.id;
  await waitFor('failed checkout is requeued after cleanup',async()=> (await items()).some(x=>x.id===id&&x.state==='queued'&&x.output?.includes('actual-checkout-start-failed')),45000);
  check('failed startup cannot land or pretend success',git(origin,'rev-parse','refs/heads/main')===before);
  check('failed candidate leaves no locked registry or checkout',candidates().length===0&&!git(lane,'worktree','list','--porcelain').includes('locked initializing'));
  check('repository land lock released before retry',!fs.readdirSync(path.join(amux.home,'locks')).some(x=>x.startsWith('land-')));
  check('failure has a named retained signal',fs.readFileSync(amux.serverLog,'utf8').includes('land_candidate_start_failed'));
  await amux.down();
  // Crash after a real locked checkout is registered, before candidate() can
  // return or clean it. The next controller must reuse that same candidate.
  fs.writeFileSync(faultFlag,'crash');
  await amux.up();
  await waitFor('controller crash left one actual registered candidate',()=>fs.existsSync(faultFlag+'.killed')&&candidates().length===1&&git(lane,'worktree','list','--porcelain').includes('locked initializing'),45000);
  await amux.down();
  check('interrupted checkout retains bounded candidate state',candidates().length===1&&candidates()[0].startsWith('cand-'));
  check('interrupted checkout cannot advance main',git(origin,'rev-parse','refs/heads/main')===before);
  // Move main so the accepted composition differs from the originally queued
  // SHA. The bare remote kills the actual controller after accepting the push
  // and before its output/terminal receipt can return.
  const advance=path.join(amux.root,'advance');git(amux.root,'clone','-qb','main',origin,advance);
  fs.writeFileSync(path.join(advance,'main-moved.txt'),'main moved\n');git(advance,'add','.');git(advance,'commit','-qm','main moves');git(advance,'push','-q','origin','HEAD:main');
  const moved=git(origin,'rev-parse','refs/heads/main');
  const acceptedHook=path.join(origin,'hooks/post-receive');
  fs.writeFileSync(acceptedHook,'#!/bin/sh\nrm -f "$0"\n[ -n "$AMUX_LAND_HOLDER_PID" ] || exit 41\nkill -KILL "$AMUX_LAND_HOLDER_PID"\nsleep 1\n',{mode:0o755});
  await amux.up();
  await waitFor('remote accepted the rebased commit before controller receipt',()=>git(origin,'rev-parse','refs/heads/main')!==moved,45000);
  await amux.down();
  const accepted=git(origin,'rev-parse','refs/heads/main');
  const pending=JSON.parse(execFileSync('python3',['-c','import sqlite3,json,sys;c=sqlite3.connect("file:"+sys.argv[1]+"?mode=ro",uri=True);c.row_factory=sqlite3.Row;print(json.dumps(dict(c.execute("select state,merged_sha,done_at from land_queue where id=?",(sys.argv[2],)).fetchone())))',path.join(amux.home,'amux.db'),String(id)],{encoding:'utf8'}));
  check('exact push intent is durable without a false terminal receipt',pending.state==='running'&&pending.merged_sha===accepted&&pending.done_at===null,pending);
  check('accepted rebased commit differs from the worker original',accepted!==sha);
  check('remote acceptance occurs exactly once before recovery',git(origin,'rev-list','--count','refs/heads/main')==='3');
  await amux.up();
  await waitFor('retained intent automatically lands after restart',async()=> (await items()).some(x=>x.id===id&&x.state==='merged'),45000);
  check('lost acknowledgement adopts the accepted commit without another push',git(origin,'rev-parse','refs/heads/main')===accepted&&git(origin,'rev-list','--count','refs/heads/main')==='3');
  check('committed feature reached local main',git(origin,'show','refs/heads/main:feature.txt')==='only once');
  check('successful candidate stays bounded and reusable',candidates().length===1&&candidates()[0].startsWith('cand-'));
  check('interrupted candidate is reused after controller crash',fs.readFileSync(amux.serverLog,'utf8').includes('land_candidate_reused'));
  const merged=git(origin,'rev-parse','refs/heads/main');
  await amux.down();await amux.up();
  const final=await items();
  check('second crash retains one terminal landing',final.length===1&&final[0].id===id&&final[0].state==='merged',final);
  check('already completed landing is never pushed twice',git(origin,'rev-parse','refs/heads/main')===merged&&git(origin,'rev-list','--count','refs/heads/main')==='3');
  check('accepted-push adoption has an actual completion signal',fs.readFileSync(amux.serverLog,'utf8').includes('land_push_receipt_adopted'));
  check('cleanup has an actual completion signal',fs.readFileSync(amux.serverLog,'utf8').includes('land_candidate_removed'));
}catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});}
finally{await amux.stop();}
const receipt={measured:true,n_considered:checks.length,failed:checks.filter(x=>!x.ok).length,artifacts:amux.root,
  fixture_boundary:'real HTTPS land queue, local git/bare origin, actual failed locked checkout, durable requeue and released lock, automatic retry, four controller SIGKILL restarts including interrupted registered checkout and remote push acceptance before its receipt, no duplicate push',checks};
fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
