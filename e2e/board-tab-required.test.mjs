import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
const app=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const html=fs.readFileSync('crates/amux-dashboard/static/index.html','utf8');
// 3f2aef8b (amux-helper, 2026-09-26, "the Board tab can be hidden again")
// made Board hideable on purpose; only the Workers tab stays required.
test('the global Board tab exists and the Workers tab cannot be hidden by a saved tab layout',()=>{
 assert.match(html,/id="tab-board"/);
 assert.match(app,/const REQUIRED = new Set\(\['sessions'\]\);/);
 assert.match(app,/hiddenTabs\.has\(t\.id\) && !t\.required \? 'none' : ''/);
});
// 2026-11-26 (owner): "The only tab that should not be able to be hidden is
// terminal", so the worker-details Board tab is hideable by an explicit uncheck.
test('the worker-details Board tab shows for isolated workers and only an explicit uncheck hides it',()=>{
 assert.match(html,/id="peek-tab-issues"/);
 assert.match(app,/const PEEK_REQUIRED_TABS = new Set\(\);/);
 assert.doesNotMatch(app,/raw && \['issues', 'schedules'\]\.includes/);
 assert.match(app,/if \(id === 'issues'\) peekHiddenTabs\.add\('issues!'\);/);
 const fn=app.match(/function _honourPeekBoardHide\(set\) \{[\s\S]*?\n\}/)[0];
 const honour=(0,eval)('('+fn+')');
 assert.deepEqual([...honour(new Set(['issues','git']))],['git']);
 assert.deepEqual([...honour(new Set(['issues','issues!']))].sort(),['issues','issues!']);
});
