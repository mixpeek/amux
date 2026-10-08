#!/usr/bin/env node
// Real storage consumer and persistent SQLite, interrupted by SIGKILL.
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { startAmux, waitFor } from './harness.mjs';
const checks=[];
const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw new Error(name);};
const a=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{RUST_LOG:'info',AMUX_STORAGE_SWEEP_SECS:'3600',AMUX_BOARD_DRIVE_SECS:'0',AMUX_AUTOFIX_SECS:'0'}});
const db=path.join(a.home,'amux.db');
const sql=(q)=>JSON.parse(execFileSync('python3',['-c','import sqlite3,json,sys;c=sqlite3.connect(sys.argv[1]);r=c.execute(sys.argv[2]);o=r.fetchall() if r.description else [];c.commit();print(json.dumps(o))',db,q],{encoding:'utf8'}));
const counts=()=>sql("SELECT COUNT(*),SUM(capture_pending=1),SUM(ts>1000) FROM cmd_history WHERE session='retention-fixture'")[0];
try {
 const before=(await a.req('GET','/health')).body;
 // Explicit fixture rows: genuine pending capture and a fresh survivor.
 sql("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<600) INSERT INTO cmd_history(id,text,type,session,ts,capture_pending) SELECT 10000+x,'aged','user','retention-fixture',1,0 FROM n");
 sql(`INSERT INTO cmd_history(id,text,type,session,ts,capture_pending) VALUES(10601,'pending','user','retention-fixture',1,1),(10602,'recent','user','retention-fixture',${Date.now()},0)`);
 const run=async()=>{const r=await a.req('POST','/api/system-jobs/storage/run',{});check('normal storage consumer accepted',r.status===200,r.body);};
 await run(); await waitFor('first bounded batch',()=>counts()[0]===346,15000);
 check('first pass preserves unfinished and recent history',counts()[1]===1&&counts()[2]===1,counts());
 await a.down(); await a.up();
 const after=(await a.req('GET','/health')).body;
 check('SIGKILL restart uses identical binary',before.build===after.build&&before.pid!==after.pid,{before,after});
 // Startup may itself run storage; discover remaining rows rather than depend
 // on a volatile batch cursor or assume a particular periodic startup order.
 for(let i=0;i<3&&counts()[0]>2;i++){await run();await new Promise(r=>setTimeout(r,1200));}
 await waitFor('retention finishes remaining aged rows',()=>counts()[0]===2,15000);
 check('protected pending input survives recovery',counts()[1]===1&&counts()[2]===1,counts());
 const plans=sql("EXPLAIN QUERY PLAN SELECT COUNT(*) FROM steering_history WHERE queued_at>=0");
 check('retention uses covering timestamp index',plans.some(r=>String(r[3]).includes('COVERING INDEX idx_steering_history_queued_at')),plans);
 await a.down();await a.up();await run();await new Promise(r=>setTimeout(r,1200));
 check('second crash does not delete protected survivors',counts()[0]===2,counts());
} catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});console.error(fs.readFileSync(a.serverLog,'utf8').slice(-6000));}
finally{await a.stop();}
const receipt={measured:checks.length>0,n_considered:checks.length,failed:checks.filter(c=>!c.ok).length,artifacts:a.root,fixture_boundary:'seeded retention history; real storage consumer and SIGKILL',checks};
fs.writeFileSync(path.join(a.root,'recovery-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
