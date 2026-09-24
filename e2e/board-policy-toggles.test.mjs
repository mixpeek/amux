import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
const app=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const html=fs.readFileSync('crates/amux-dashboard/static/index.html','utf8');
const sv=fs.readFileSync('crates/amux-server/src/api/session_verbs.rs','utf8');
const bl=fs.readFileSync('crates/amux-server/src/api/board_lifecycle.rs','utf8');
test('the worker Board tab carries both policy toggles, wired to worker config',()=>{
 assert.match(html,/id="peek-board-decompose"[^>]*togglePeekBoardPolicy\('board_decompose'/);
 assert.match(html,/id="peek-board-force"[^>]*togglePeekBoardPolicy\('board_force_adherence'/);
 assert.match(app,/function renderPeekIssues\(\) \{\n  _renderPeekIssuesBody\(\);\n  _peekBoardPolicySync\(\);/);
 assert.match(sv,/"board_decompose",\s*super::board_lifecycle::DECOMPOSE_KEY/);
 assert.match(sv,/"board_force_adherence",\s*super::board_lifecycle::FORCE_ADHERENCE_KEY/);
});
test('withholding an owner message requires force adherence, which defaults off',()=>{
 assert.match(bl,/pub\(crate\) fn stage_owner_command\(session: &str, text: &str\) -> bool \{\n    enabled\(session\)\n        && force_adherence\(session\)/);
});
test('an isolated worker shows the toggles off and disabled, with the reason',()=>{
 const i=app.indexOf('function _peekBoardPolicySync(');
 let d=0,end=0;for(let k=app.indexOf('{',i);k<app.length;k++){if(app[k]==='{')d++;else if(app[k]==='}'&&--d===0){end=k+1;break;}}
 const els={'peek-board-policy':{style:{}},'peek-board-decompose':{},'peek-board-force':{},'peek-board-policy-note':{style:{}}};
 const run=(s)=>new Function('document','sessions','peekSession',app.slice(i,end)+'\n_peekBoardPolicySync();')(
   {getElementById:id=>els[id]},[s],s.name);
 run({name:'iso',isolated:true,board_decompose:false,board_force_adherence:false});
 assert.equal(els['peek-board-policy'].style.display,'flex');
 assert.equal(els['peek-board-decompose'].disabled,true);
 assert.match(els['peek-board-policy-note'].textContent,/Isolated worker/);
 run({name:'w',isolated:false,board_decompose:true,board_force_adherence:false});
 assert.equal(els['peek-board-decompose'].disabled,false);
 assert.equal(els['peek-board-decompose'].checked,true);
 assert.equal(els['peek-board-policy-note'].style.display,'none');
});
