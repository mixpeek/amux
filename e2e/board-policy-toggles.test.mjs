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
 const ci=app.indexOf('const _PEEK_BOARD_ALL_CONFIGS');
 const fi=app.indexOf('function _peekBoardPolicySync(');
 let d=0,end=0;for(let k=app.indexOf('{',fi);k<app.length;k++){if(app[k]==='{')d++;else if(app[k]==='}'&&--d===0){end=k+1;break;}}
 const snippet=app.slice(ci,end);
 const el={style:{},innerHTML:''};
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
 assert.match(el.innerHTML,/Auto-drain backlog/);
 assert.match(el.innerHTML,/Decompose onto board/);
 assert.match(el.innerHTML,/togglePeekBoardPolicy/);
});
