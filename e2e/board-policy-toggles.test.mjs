import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
const app=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const html=fs.readFileSync('crates/amux-dashboard/static/index.html','utf8');
const sv=fs.readFileSync('crates/amux-server/src/api/session_verbs.rs','utf8');
const bl=fs.readFileSync('crates/amux-server/src/api/board_lifecycle.rs','utf8');
test('the worker Board tab has a config container and _peekBoardPolicySync renders all toggles',()=>{
 assert.match(html,/id="peek-board-config"/);
 assert.match(app,/function renderPeekIssues\(\) \{\n  _renderPeekIssuesBody\(\);\n  _peekBoardPolicySync\(\);/);
 assert.match(app,/_PEEK_BOARD_ALL_CONFIGS/);
 assert.match(app,/field: 'auto_drain_backlog'/);
 assert.match(app,/field: 'board_decompose'/);
 assert.match(app,/field: 'board_force_adherence'/);
 assert.match(app,/togglePeekBoardPolicy/);
 assert.match(sv,/"board_decompose",\s*super::board_lifecycle::DECOMPOSE_KEY/);
 assert.match(sv,/"board_force_adherence",\s*super::board_lifecycle::FORCE_ADHERENCE_KEY/);
});
test('withholding an owner message requires force adherence, which defaults off',()=>{
 assert.match(bl,/pub\(crate\) fn stage_owner_command\(session: &str, text: &str\) -> bool \{\n    enabled\(session\)\n        && force_adherence\(session\)/);
});
test('an isolated worker shows all toggles disabled with the reason',()=>{
 const fnEnd=(name)=>{const fi=app.indexOf(name);let d=0;for(let k=app.indexOf('{',fi);k<app.length;k++){if(app[k]==='{')d++;else if(app[k]==='}'&&--d===0)return k+1;}};
 const defs=app.slice(app.indexOf('const _BOARD_SWITCHES = ['),fnEnd('function _boardSwitchStates('));
 const ci=app.indexOf('const _PEEK_BOARD_ALL_CONFIGS');
 const snippet=defs+'\n'+app.slice(ci,fnEnd('function _peekBoardPolicySync('));
 const el={style:{},innerHTML:'',querySelector:()=>null};
 const doc={getElementById:()=>el};
 const run=(s)=>{
   el.innerHTML='';el.style.display='';
   new Function('document','sessions','peekSession','esc','escJs',
     snippet+'\n_peekBoardPolicySync();')(
     doc,[s],s.name,x=>String(x),x=>String(x).replace(/'/g,"\\'"));
 };
 run({name:'iso',isolated:true,board_decompose:false,board_force_adherence:false,
      auto_drain_backlog:false,auto_pickup:false,auto_continue:false,standing_orders:false});
 assert.match(el.innerHTML,/pbc-note/);
 assert.match(el.innerHTML,/Isolated worker/);
 assert.match(el.innerHTML,/pbc-disabled/);
 run({name:'w',isolated:false,board_decompose:true,board_force_adherence:false,
      auto_drain_backlog:true,auto_pickup:true,auto_continue:true,standing_orders:true});
 assert.doesNotMatch(el.innerHTML,/pbc-disabled/);
 assert.match(el.innerHTML,/Refill To Do from Backlog/);
 assert.match(el.innerHTML,/Plan my messages onto the board/);
 assert.match(el.innerHTML,/pbc-help/, 'every switch shows its helper text');
 assert.match(el.innerHTML,/togglePeekBoardPolicy/);
});

test('a switch held back by its parent says so instead of looking active',()=>{
 const fnEnd=(name)=>{const fi=app.indexOf(name);let d=0;for(let k=app.indexOf('{',fi);k<app.length;k++){if(app[k]==='{')d++;else if(app[k]==='}'&&--d===0)return k+1;}};
 const defs=app.slice(app.indexOf('const _BOARD_SWITCHES = ['),fnEnd('function _boardSwitchStates('));
 const states=new Function(defs+'\nreturn _boardSwitchStates;')();
 // The 2026-10-15 screenshot: steer on, pickup off, continue and drain on.
 const st=states({board_force_adherence:true,board_decompose:false,auto_pickup:false,auto_pickup_raw:false,
   auto_continue:false,auto_continue_raw:true,auto_drain_backlog:true});
 assert.equal(st.auto_drain_backlog.own,true);
 assert.equal(st.auto_drain_backlog.effective,false);
 assert.match(st.auto_drain_backlog.why,/Start To Do cards when idle/);
 assert.match(st.board_auto_continue.why,/Start To Do cards when idle/);
 const off=states({board_force_adherence:false,auto_pickup:true,auto_pickup_raw:true});
 assert.match(off.board_auto_pickup.why,/Board steers this worker/);
 const on=states({board_force_adherence:true,auto_pickup:true,auto_pickup_raw:true,auto_drain_backlog:true});
 assert.equal(on.auto_drain_backlog.effective,true);
 assert.equal(on.auto_drain_backlog.why,'');
});
test('the board Continue switch writes its own key, never the YOLO key',()=>{
 assert.match(sv,/"board_auto_continue",\s*BOARD_CONTINUE_KEY/);
 assert.match(sv,/pub const BOARD_CONTINUE_KEY: &str = "CC_BOARD_CONTINUE"/);
});
