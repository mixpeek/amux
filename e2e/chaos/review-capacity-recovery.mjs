#!/usr/bin/env node
// Real HTTPS board/clock/reviewer CLI, then SIGKILL/restart. No live fleet or
// provider spend. Only the retry clock is advanced directly in the fixture DB;
// positive fresh-headroom release is separately covered in Rust integration tests.
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { startAmux, waitFor, git } from './harness.mjs';
const checks = [];
const check = (name, ok, detail) => { checks.push({ name, ok: !!ok, detail }); if (!ok) throw new Error(name + ': ' + JSON.stringify(detail)); };
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY, env: { AMUX_ISOLATED:'0', AMUX_BOARD_DRIVE_SECS:'0', AMUX_AUTOFIX_SECS:'0', AMUX_GHOST_RESCUE_SECS:'0', AMUX_MODEL_CATALOG_REFRESH_SECS:'0', AMUX_CONTRACT_REVIEW_CONCURRENCY:'1', ANTHROPIC_API_KEY:'', OPENAI_API_KEY:'', GEMINI_API_KEY:'', GOOGLE_API_KEY:'' } });
const db = path.join(amux.home, 'amux.db');
const sql = (statement, args = []) => JSON.parse(execFileSync('python3', ['-c', 'import sqlite3,json,sys;c=sqlite3.connect(sys.argv[1]);c.row_factory=sqlite3.Row;r=c.execute(sys.argv[2],json.loads(sys.argv[3]));o=[dict(x) for x in r] if r.description else [];c.commit();print(json.dumps(o))', db, statement, JSON.stringify(args)], { encoding:'utf8' }));
const clock = async () => { const r = await amux.req('POST','/api/system-jobs/contract-watch/run',{}); check('normal contract clock accepted',r.status===200,r.body); };
const attempts = () => fs.existsSync(path.join(amux.root,'attempts')) ? fs.readFileSync(path.join(amux.root,'attempts'),'utf8').trim().split('\n') : [];
try {
  const before = (await amux.req('GET','/health')).body;
  const repo = path.join(amux.root,'repo'); fs.mkdirSync(repo); git(repo,'init','-q'); git(repo,'commit','--allow-empty','-qm','fixture'); git(repo,'update-ref','refs/remotes/origin/main','HEAD');
  const cli = path.join(amux.root,'reviewer.sh');
  fs.writeFileSync(cli, `#!/bin/sh\ncat > prompt.txt\nif grep -q 'BEFORE its run' prompt.txt; then echo advisory >> '${amux.root}/attempts'; else echo completion >> '${amux.root}/attempts'; fi\nsleep 1\nif test "$(cat '${amux.root}/mode')" = quota; then echo "You've hit your weekly limit · resets Oct 11 at 10pm (America/New_York)"; exit 1; fi\necho '{"verdict":"pass","findings":[]}'\n`, {mode:0o755});
  fs.writeFileSync(path.join(amux.root,'mode'),'quota');
  const worker = await amux.req('POST','/api/sessions',{name:'capacity-lane',dir:repo,start:false}); check('private stopped lane created',worker.status===201,worker.body);
  fs.appendFileSync(path.join(amux.home,'sessions','capacity-lane.env'), `\nAMUX_CONTRACT_DONE=1\nAMUX_REVIEW_UNCONTRACTED=1\nAMUX_CONTRACT_REVIEW_CLI="${cli}"\nAMUX_CONTRACT_REVIEW_MODEL=claude-opus-5-5\n`);
  for(let i=0;i<3;i++) { const r=await amux.req('POST','/api/board',{title:`GS12 proof advisory fixture ${i}`,type:'ops',session:'capacity-lane',status:'doing',acceptance_criteria:['measure the workload']});check('advisory plan fixture created '+i,r.status<300,r.body); }
  const card = await amux.req('POST','/api/board',{title:'GS12 proof completed workload',type:'ops',session:'capacity-lane',status:'done',acceptance_criteria:['record measured workload'],evidence:`fixture workload command -> 100 measured requests at ${git(repo,'rev-parse','HEAD')}`}); check('done fixture created through API',card.status<300,card.body);
  const id = card.body.id;
  await clock();
  const held = await waitFor('capacity hold from real reviewer exit',()=>sql('SELECT * FROM card_contracts WHERE card=?',[id]).find(r=>r.review_state==='capacity_wait'),30000);
  check('completion receives first shared slot',attempts()[0]==='completion',attempts());
  check('quota failure spends zero quality rounds',held.review_rounds===0,held);
  check('capacity hold has a durable retry deadline',held.review_retry_at>Date.now()/1000,held.review_retry_at);
  check('passed check and pinned commit preserved',held.state==='passed'&&held.sha===git(repo,'rev-parse','HEAD'),held);
  check('quota failure does not reopen completed work',(await amux.req('GET','/api/board/'+id)).body.status==='done');
  check('failed provider output retained outside temporary checkout',fs.readdirSync(path.join(amux.home,'review-evidence',id)).some(n=>fs.readFileSync(path.join(amux.home,'review-evidence',id,n,'.amux-review.out'),'utf8').includes("You've hit your weekly limit")));
  await amux.down(); await amux.up();
  const after=(await amux.req('GET','/health')).body; check('abrupt restart replaces process with identical binary',after.pid!==before.pid&&after.build===before.build,{before,after});
  check('capacity wait survives SIGKILL',sql('SELECT review_state,review_rounds,review_retry_at FROM card_contracts WHERE card=?',[id])[0].review_retry_at===held.review_retry_at);
  await clock();
  await new Promise(r=>setTimeout(r,1800));
  check('restart and unknown quota do not bypass backoff',attempts().filter(x=>x==='completion').length===1,attempts());
  check('task remains done while capacity is unmeasured',(await amux.req('GET','/api/board/'+id)).body.status==='done');
  fs.writeFileSync(path.join(amux.root,'mode'),'pass');
  // Explicitly seeded clock boundary, not a production status edit or a claimed
  // fresh provider measurement. The shipped consumer performs the actual retry.
  sql('UPDATE card_contracts SET review_retry_at=0 WHERE card=? AND review_state=?',[id,'capacity_wait']);
  await waitFor('advisory attempt releases its slot',()=>!sql("SELECT card FROM card_prereviews WHERE state='running'").length,15000);
  await clock();
  const verified = await waitFor('successful bounded retry verifies the card',async()=>{const r=(await amux.req('GET','/api/board/'+id)).body;return r.status==='verified'?r:null;},30000);
  check('verified comes from fresh successful reviewer',verified.reviewer?.startsWith('harness:reviewer:'),verified.reviewer);
  check('only successful review counts as one quality round',sql('SELECT review_rounds FROM card_contracts WHERE card=?',[id])[0].review_rounds===1);
  check('exactly two completion CLI invocations',attempts().filter(x=>x==='completion').length===2,attempts());
  await amux.down(); await amux.up(); await clock(); await new Promise(r=>setTimeout(r,1000));
  check('verified result is not reviewed again after another crash',attempts().filter(x=>x==='completion').length===2,attempts());
} catch(e) { checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)}); console.error(fs.readFileSync(amux.serverLog,'utf8').slice(-6000)); }
finally { await amux.stop(); }
const receipt={measured:checks.length>0,n_considered:checks.length,failed:checks.filter(c=>!c.ok).length,artifacts:amux.root,clock_boundary:'seeded retry deadline; real recovery consumer',checks};
fs.writeFileSync(path.join(amux.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2)); console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
