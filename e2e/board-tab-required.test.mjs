import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
const app=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const html=fs.readFileSync('crates/amux-dashboard/static/index.html','utf8');
test('the global Board tab exists and cannot be hidden by a saved tab layout',()=>{
 assert.match(html,/id="tab-board"/);
 assert.match(app,/const REQUIRED = new Set\(\['sessions', 'board'\]\);/);
 assert.match(app,/hiddenTabs\.has\(t\.id\) && !t\.required \? 'none' : ''/);
});
test('the worker-details Board tab shows for every worker, isolated included',()=>{
 assert.match(html,/id="peek-tab-issues"/);
 assert.match(app,/const PEEK_REQUIRED_TABS = new Set\(\['issues'\]\);/);
 assert.doesNotMatch(app,/raw && \['issues', 'schedules'\]\.includes/);
 assert.match(app,/if \(PEEK_REQUIRED_TABS\.has\(id\)\) show = true;/);
});
