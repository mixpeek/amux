#!/usr/bin/env node
// Real worker credentials and providers; global group confinement survives old
// open settings, queued reports, reply history, grants and controller crashes.
import fs from 'node:fs';
import {DatabaseSync} from 'node:sqlite';
import path from 'node:path';
import {startAmux, waitFor} from './harness.mjs';
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG:'info', AMUX_ISOLATED:'0', AMUX_GROUP_SEND_ENFORCE:'0',
  AMUX_BOARD_DRIVE_SECS:'0', AMUX_AUTOFIX_SECS:'0', AMUX_GHOST_RESCUE_SECS:'0',
  AMUX_MODEL_CATALOG_REFRESH_SECS:'0', FAKE_CLAUDE_ENV_KEYS:'AMUX_WORKER_TOKEN',
  AMUX_NEEDS_INPUT_AUTO:'0',
}});
const checks=[];
const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw Error(name+': '+JSON.stringify(detail));};
// node:sqlite, not the container's python3: Debian bookworm's SQLite 3.40
// evaluates board_change_log.changed_at's DEFAULT unixepoch('subsec') to NULL
// (the modifier needs 3.42), so any raw issues write failed NOT NULL there.
const db=(sql,args=[])=>{
  const c=new DatabaseSync(path.join(amux.home,'amux.db'));
  try{
    c.exec('PRAGMA busy_timeout=10000');
    const st=c.prepare(sql);
    return /^\s*(select|with|pragma)\b/i.test(sql)||/\breturning\b/i.test(sql)?st.all(...args).map(r=>({...r})):(st.run(...args),[]);
  }finally{c.close();}
};
const peer='gs12-peer', hub='gs12-hub', harness='amux-harness', outside='unrelated-peer';
const received=text=>amux.fakeLog().filter(x=>x.text?.includes(text)).length;
let headers={};
try {
  for(const [name,group] of [[peer,'gs12-platform'],[hub,'ops,gs12-platform'],[harness,'amux'],[outside,'unrelated']]) {
    const dir=path.join(amux.root,name);fs.mkdirSync(dir);
    const created=await amux.req('POST','/api/sessions',{name,dir,start:false});
    check('private worker created '+name,created.status===201,created.status);
    fs.appendFileSync(path.join(amux.home,'sessions',name+'.env'),`\nCC_TAGS=${group}\nAMUX_CONTRACT_DONE=1\nAMUX_BOARD_DELEGATION=1\nAMUX_CONTRACT_HUB=${peer}\nAMUX_CONTRACT_RULES_OFF=1,2,3,4,5,6,7,8,9\nCC_AUTO_PICKUP=0\nCC_AUTO_CONTINUE=0\nCC_SEND_ALLOW=*\nCC_RECEIVE_ANY=1\n`);
    const started=await amux.req('POST',`/api/sessions/${name}/start`);
    check('provider start accepted '+name,started.status<300,started.status);
    await waitFor('actual credentialed provider launch '+name,()=>amux.fakeLog().some(x=>x.event==='launch'&&x.argv.includes(name)&&x.env_sha256?.AMUX_WORKER_TOKEN),30000);
  }
  const token=amux.tmux('show-environment','-t','amux-'+peer,'AMUX_WORKER_TOKEN').trim().split('=')[1];
  headers={'X-Amux-Session':peer,'X-Amux-Worker-Token':token};
  fs.writeFileSync(path.join(amux.home,'amux.env'),'CC_SEND_ALLOW=*\nCC_RECEIVE_ANY=1\n');
  for(const target of [harness,outside]) {
    const blocked=await amux.req('POST',`/api/workers/${target}/send`,{text:'blocked-to-'+target,source_session:target},10000,headers);
    check('worker outside shared groups refused '+target,blocked.status===403&&blocked.body.code==='worker_group_boundary'&&blocked.body.submitted===false&&!blocked.body.grant_id&&received('blocked-to-'+target)===0,blocked.body);
  }
  const policy=await amux.req('GET','/api/config/cross-group');
  check('effective group gate stays closed despite legacy off toggle',policy.body.policy==='shared-group-only'&&policy.body.gate_enforcing===true&&policy.body.enabled===false&&policy.body.editable===false,policy.body);
  for(const endpoint of [`/api/sessions/${peer}/config`,`/api/workers/${peer}/config`]) {
    const membership=await amux.req('PATCH',endpoint,{tags:['gs12-platform','amux']},10000,headers);
    check('worker cannot self-join outside groups '+endpoint,membership.status===403&&membership.body.code==='worker_group_membership_refused',membership.body);
  }
  const steer=await amux.req('POST',`/api/workers/${outside}/steer`,{text:'blocked-queue-api',sender:''},10000,headers);
  check('queue API cannot turn worker input into owner input',steer.status===403&&received('blocked-queue-api')===0,steer.body);
  const before=db('select count(*) n from issues')[0].n;
  const request=await amux.req('POST','/api/board',{title:'out-of-group automatic request',type:'chore',status:'backlog',request_to:outside},10000,headers);
  check('routed board request cannot cross the same boundary',request.status===403&&db('select count(*) n from issues')[0].n===before,request.body);
  const ownerCard=await amux.req('POST','/api/board',{title:'owner-created target card',session:outside,type:'chore',status:'backlog'});
  check('owner can create outside-group card',ownerCard.status===201,ownerCard.body);
  const id=ownerCard.body.id||ownerCard.body.item?.id;
  const progress=await amux.req('PATCH',`/api/board/${id}`,{desc_append:'peer progress cannot become outside-group input'},10000,headers);
  check('saved board progress cannot inject a peer notification',progress.status===200&&progress.body.owner_notified===false&&received('peer progress cannot become outside-group input')===0,progress.body);
  const owner=await amux.req('POST',`/api/workers/${outside}/send`,{text:'explicit-owner-outside-input'});
  check('direct owner outside-group input accepted',owner.status<300,owner.status);
  await waitFor('owner actual provider receipt',()=>received('explicit-owner-outside-input')===1,30000);
  const own=await amux.req('POST',`/api/workers/${hub}/send`,{text:'actual-own-group-input'},30000,headers);
  check('overlapping group membership allows actual hub input',own.status<300,own.body);
  await waitFor('hub actual provider receipt',()=>received('actual-own-group-input')===1,30000);
  const reviewCard=await amux.req('POST','/api/board',{title:'Exhausted independent proof review',session:peer,type:'ops',status:'backlog'});
  check('private review proof card created',reviewCard.status===201,reviewCard.body);
  const reviewId=reviewCard.body.id;
  // Persist unfinished work only with this private controller down. These are
  // historical queued rows, not artificial live production inputs.
  await amux.down();
  db("update issues set status='needsyou',ask_actor='ethan',ask_type='decision',ask_question='Your call: reopen with direction?' where id=?",[reviewId]);
  db("insert into card_contracts(card,acceptance,command,hash,frozen_at,state,review_state,review_rounds,review_log) values(?, 'unchanged criteria', '(none: reviewed from evidence)', '', 1, 'frozen', 'escalated', 3, 'retained three failed reviews')",[reviewId]);
  for(const target of [harness,outside])db('insert into steering_queue(id,session,text,queued_at,guard,sender) values(?,?,?,?,?,?)',
    ['old-cross-'+target,target,'old-cross-input-'+target,Date.now()/1000,'message',peer]);
  db('insert into steering_queue(id,session,text,queued_at,guard,sender) values(?,?,?,?,?,?)',
    ['old-cross-retired',harness,'old-cross-input-retired',Date.now()/1000,'message','retired-gs12-peer']);
  db('insert into cmd_history(session,ts,text,origin) values(?,?,?,?)',[peer,Date.now(),'historic inbound reply window',outside]);
  const grants=path.join(amux.home,'grants');fs.mkdirSync(grants,{recursive:true});
  fs.writeFileSync(path.join(grants,`allow_${peer}__${outside}.json`),JSON.stringify({origin:peer,target:outside,grant:'grn_aabb',created:Date.now()/1000}));
  amux.env.AMUX_GROUP_SEND_ENFORCE='1';
  await amux.up();
  const exhausted=await amux.req('PATCH',`/api/board/${reviewId}`,{status:'done',evidence:'worker claims a fourth review will run'},10000,headers);
  check('a worker done move cannot answer the exhausted-review owner question',exhausted.status===409&&exhausted.body.code==='contract_review_owner_direction_required',exhausted.body);
  // The code verification shortcut must reach the same owner-direction guard.
  db("update issues set status='doing', type='code' where id=?",[reviewId]);
  db("update card_contracts set command='true',hash='real-frozen-contract' where card=?",[reviewId]);
  const contracted=await amux.req('PATCH',`/api/board/${reviewId}`,{status:'done'},10000,headers);
  check('contracted code cannot enqueue verification past exhausted review',contracted.status===409&&contracted.body.code==='contract_review_owner_direction_required'&&db("select state,review_rounds from card_contracts where card=?",[reviewId]).some(x=>x.state==='frozen'&&x.review_rounds===3),contracted.body);
  db("update issues set status='needsyou',type='ops' where id=?",[reviewId]);
  await waitFor('older cross-group rows receive durable refusals',()=>db("select id from steering_history where id like 'old-cross-%' and outcome='refused:worker_group_boundary'").length===3,15000);
  check('older peer input is fenced and retained across crash',db("select count(*) n from steering_queue where id like 'old-cross-%'")[0].n===0&&received('old-cross-input-')===0);
  const retry=await amux.req('POST',`/api/workers/${outside}/send`,{text:'legacy-reply-grant-bypass'},10000,headers);
  check('old reply window and single-use grant cannot widen membership',retry.status===403&&retry.body.code==='worker_group_boundary'&&received('legacy-reply-grant-bypass')===0,retry.body);
  const wide=await amux.req('PUT','/api/config/cross-group',{allow:'*'});
  check('global UI configuration refuses obsolete widening',wide.status===403&&wide.body.code==='worker_group_boundary',wide.body);
  const perWorker=await amux.req('PATCH',`/api/sessions/${peer}/config`,{send_allow:'*'});
  check('worker UI configuration refuses obsolete widening',perWorker.status===403&&perWorker.body.code==='worker_group_boundary',perWorker.body);
  await amux.down();await amux.up();
  check('owner review decision and quality budget survive another crash',db("select i.status,c.review_state,c.review_rounds,c.review_log from issues i join card_contracts c on c.card=i.id where i.id=?",[reviewId]).some(x=>x.status==='needsyou'&&x.review_state==='escalated'&&x.review_rounds===3&&x.review_log==='retained three failed reviews'));
  const after=await amux.req('POST',`/api/workers/${harness}/send`,{text:'post-second-crash-cross-input'},10000,headers);
  check('group confinement survives another controller crash',after.status===403&&after.body.code==='worker_group_boundary',after.body);
  check('accepted owner and hub inputs are not replayed',received('explicit-owner-outside-input')===1&&received('actual-own-group-input')===1&&received('post-second-crash-cross-input')===0);
  check('refused queued reports have exactly one durable audit record',db("select count(*) n from steering_history where id like 'old-cross-%'")[0].n===3);
  check('group refusal has a named measured server signal',fs.readFileSync(amux.serverLog,'utf8').includes('worker_group_boundary'));
} catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});}
finally{await amux.stop();}
const receipt={measured:true,n_considered:checks.length,failed:checks.filter(x=>!x.ok).length,artifacts:amux.root,
  fixture_boundary:'real HTTPS/authenticated worker identity, actual private tmux providers, shared-group hub and direct owner controls, legacy off/open/wildcard/reply/grant negatives, routed board and progress notifications, durable queued reports and actual SIGKILL/restarts',checks};
fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
