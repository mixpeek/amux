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
