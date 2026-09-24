import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
const app=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const html=fs.readFileSync('crates/amux-dashboard/static/index.html','utf8');
test('Needs You lives inside the alert panel, not as its own header button',()=>{
 assert.doesNotMatch(html,/id="needsyou-btn"/);
 const panel=html.slice(html.indexOf('<div id="notif-panel"'),html.indexOf('<div id="notif-panel-list"></div>'));
 assert.match(panel,/id="needsyou-panel-list"/);
 assert.match(panel,/id="needsyou-badge"/);
});
test('the bell badge carries what is waiting on you, and opening the bell loads it',()=>{
 const badge=app.slice(app.indexOf('function _notifUpdateBadge()'),app.indexOf('function _notifShowBanner('));
 assert.match(badge,/_notifUnread \+ needsYou/);
 const toggle=app.slice(app.indexOf('function toggleNotifPanel()'),app.indexOf('function _positionNotifPanel()'));
 assert.match(toggle,/_needsYouFetch\(true\)/);
 assert.match(app,/function toggleNeedsYouPanel\(\) \{ toggleNotifPanel\(\); \}/);
});
