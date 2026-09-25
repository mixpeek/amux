// A message's root epic chip shows its children's progress, not its own
// "backlog" (LV-102 read backlog beside a done and a doing child).
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const i = app.indexOf('function _msgCardChip('); let d = 0, end = 0;
for (let k = app.indexOf('{', i); k < app.length; k++) { if (app[k] === '{') d++; else if (app[k] === '}' && --d === 0) { end = k + 1; break; } }
const make = (items) => new Function('boardItems', 'esc', 'escJs', app.slice(i, end) + '\nreturn _msgCardChip;')(items, s => String(s), s => String(s));
const text = h => h.replace(/<[^>]+>/g, '');
test('an epic root shows child progress', () => {
  const items = [
    { id: 'LV-102', type: 'epic', status: 'backlog', title: 'Build a video' },
    { id: 'LV-103', epic: 'LV-102', status: 'done' },
    { id: 'LV-104', epic: 'LV-102', status: 'doing' },
    { id: 'LV-105', epic: 'LV-102', status: 'backlog' },
  ];
  const chip = make(items)('LV-102', {}, items[0]);
  assert.match(text(chip), /LV-102 · 1 of 3 done/);
  assert.match(chip, /#d29922/, 'coloured as in progress');
});
test('a plain card still shows its own status', () => {
  const items = [{ id: 'LV-104', type: 'code', status: 'doing' }];
  assert.match(text(make(items)('LV-104', {}, items[0])), /LV-104 · doing/);
});
