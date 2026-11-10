#!/usr/bin/env node
// A directive against real provider launches: only a sender the target's scope
// names gets through, it reaches the worker once with its origin, the opening does
// not widen an ordinary send, the worker has to answer it, and an open one survives
// SIGKILL: an undelivered one is delivered, an unanswered one is repeated, then told.
import fs from 'node:fs';
import {DatabaseSync} from 'node:sqlite';
import path from 'node:path';
import {startAmux, waitFor} from './harness.mjs';
const amux = await startAmux({binary: process.env.AMUX_CHAOS_BINARY, env: {
  RUST_LOG:'info', AMUX_ISOLATED:'0', AMUX_BOARD_DRIVE_SECS:'0', AMUX_AUTOFIX_SECS:'0',
  AMUX_GHOST_RESCUE_SECS:'0', AMUX_MODEL_CATALOG_REFRESH_SECS:'0', FAKE_CLAUDE_ENV_KEYS:'AMUX_WORKER_TOKEN',
  AMUX_NEEDS_INPUT_AUTO:'0', AMUX_DIRECTIVE_TICK_S:'1', AMUX_DIRECTIVE_ACK_S:'600', AMUX_DIRECTIVE_SETTLE_MS:'200',
}});
const checks=[];
const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw Error(name+': '+JSON.stringify(detail));};
const db=(sql,args=[])=>{
  const c=new DatabaseSync(path.join(amux.home,'amux.db'));
  try{
    c.exec('PRAGMA busy_timeout=10000');
    const st=c.prepare(sql);
    return /^\s*(select|with|pragma)\b/i.test(sql)?st.all(...args).map(r=>({...r})):(st.run(...args),[]);
  }finally{c.close();}
};
const lane='vm-lane', sender='remote-meta';
// The fake provider logs a pasted message line by line.
const seen=()=>amux.fakeLog().map(x=>x.text||'').join('\n');
const received=text=>amux.fakeLog().filter(x=>x.text?.includes(text)).length;
const from=name=>({'X-Amux-Session':name});
try {
  const dir=path.join(amux.root,lane);fs.mkdirSync(dir);
  const created=await amux.req('POST','/api/sessions',{name:lane,dir,start:false});
  check('worker created',created.status===201,created.status);
  fs.appendFileSync(path.join(amux.home,'sessions',lane+'.env'),
    '\nCC_TAGS=gs12-cloud\nAMUX_CONTRACT_DONE=1\nAMUX_CONTRACT_RULES_OFF=1,2,3,4,5,6,7,8,9\nCC_AUTO_PICKUP=0\nAMUX_BOARD_FORCE_ADHERENCE=0\nCC_AUTO_CONTINUE=0\n');
  const started=await amux.req('POST',`/api/sessions/${lane}/start`);
  check('provider start accepted',started.status<300,started.status);
  await waitFor('provider launch',()=>amux.fakeLog().some(x=>x.event==='launch'&&x.argv.includes(lane)),30000);
  const token=amux.tmux('show-environment','-t','amux-'+lane,'AMUX_WORKER_TOKEN').trim().split('=')[1];
  const asLane={...from(lane),'X-Amux-Worker-Token':token};

  const closed=await amux.req('POST',`/api/sessions/${lane}/directive`,{text:'closed-scope'},10000,from(sender));
  check('off by default',closed.status===403&&closed.body.code==='directive_sender_not_allowed',closed.body);
  fs.mkdirSync(path.join(amux.home,'env'),{recursive:true});
  fs.writeFileSync(path.join(amux.home,'env','gs12-cloud.env'),`AMUX_DIRECTIVE_SENDERS=${sender}\n`);
  const stranger=await amux.req('POST',`/api/sessions/${lane}/directive`,{text:'from-a-stranger'},10000,from('someone-else'));
  check('a sender the scope does not name is refused',stranger.status===403&&received('from-a-stranger')===0,stranger.body);
  const nobody=await amux.req('POST',`/api/sessions/${lane}/directive`,{text:'from-nobody'});
  check('no sender is refused',nobody.status===403&&received('from-nobody')===0,nobody.body);

  const issued=await amux.req('POST',`/api/sessions/${lane}/directive`,{text:'stop-and-read-this'},30000,from(sender));
  check('named sender delivered',issued.status===200&&issued.body.delivered===true,issued.body);
  const id=issued.body.id;
  // The ack line is the message's last line: it has no newline of its own, so it
  // is submitted by the Enter that follows the paste's settle, after the rest.
  await waitFor('provider receipt',()=>received('stop-and-read-this')===1&&seen().includes(`amux directive ack ${id}`),30000);
  check('origin stamped and answer named',seen().includes(`[amux-origin: ${sender}`)&&seen().includes(`amux directive ack ${id}`),seen().slice(-600));
  const row=db('select origin from cmd_history where session=? and text like ?',[lane,'%stop-and-read-this%']);
  check('visible in Messages under the sender',row.length===1&&row[0].origin===sender,row);

  const plain=await amux.req('POST',`/api/sessions/${lane}/send`,{text:'ordinary-send'},10000,from(sender));
  check('the opening does not widen an ordinary send',plain.status===403&&received('ordinary-send')===0,plain.body);

  const open=await amux.req('GET',`/api/directives?session=${lane}&open=1`);
  check('listed open with measured fields',open.body.measured===true&&open.body.n_considered===1&&open.body.directives[0].id===id,open.body);
  const selfAck=await amux.req('POST',`/api/directives/${id}/ack`,{note:'sender closing it'},10000,from(sender));
  check('the sender cannot answer it',selfAck.status===403&&selfAck.body.code==='directive_ack_refused',selfAck.body);
  const empty=await amux.req('POST',`/api/directives/${id}/ack`,{note:' '},10000,asLane);
  check('an empty answer is refused',empty.status===400,empty.body);
  const acked=await amux.req('POST',`/api/directives/${id}/ack`,{note:'stopped X, Y still runs, first Z'},10000,asLane);
  check('the worker answers it',acked.status===200,acked.body);
  check('answered row is closed',db('select ack_by from directives where id=?',[id])[0].ack_by===lane);

  // Recorded and never delivered, with the controller down: the watch delivers it after a restart.
  await amux.down();
  db("insert into directives(id,session,sender,text,issued_at) values('DIR-crash',?,?,?,?)",[lane,sender,'after-the-crash',Date.now()/1000]);
  await amux.up();
  await waitFor('undelivered directive delivered after restart',()=>db("select delivered_at from directives where id='DIR-crash'")[0].delivered_at!=null,60000);
  await waitFor('provider receipt after restart',()=>received('after-the-crash')===1,30000);
  await amux.down();await amux.up();
  await new Promise(r=>setTimeout(r,4000));
  check('delivered once across another crash',received('after-the-crash')===1,received('after-the-crash'));
  check('answered directive is not replayed',received('stop-and-read-this')===1,received('stop-and-read-this'));

  // Unanswered: repeated once, then the owner is told, then quiet.
  await amux.down();
  db("update directives set delivered_at=? where id='DIR-crash'",[Date.now()/1000-3600]);
  await amux.up();
  await waitFor('unanswered directive repeated',()=>db("select resent_at from directives where id='DIR-crash'")[0].resent_at!=null,60000);
  await waitFor('repeat received',()=>received('after-the-crash')===2,30000);
  await amux.down();
  db("update directives set resent_at=? where id='DIR-crash'",[Date.now()/1000-3600]);
  await amux.up();
  await waitFor('owner told',()=>db("select told_at from directives where id='DIR-crash'")[0].told_at!=null,60000);
  await new Promise(r=>setTimeout(r,4000));
  check('told once, then quiet',received('after-the-crash')===2,received('after-the-crash'));
  const log=fs.readFileSync(amux.serverLog,'utf8');
  check('the watch names the unanswered directive in the log',log.includes('directive_unanswered'));
  check('the refusal has a named signal',log.includes('directive_sender_not_allowed'));
} catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});}
finally{await amux.stop();}
const receipt={measured:true,n_considered:checks.length,failed:checks.filter(x=>!x.ok).length,artifacts:amux.root,checks};
fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
