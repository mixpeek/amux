// A message amux withheld (delivery=board) must never render as sent.
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';

const src = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const slice = (name) => {
  const i = src.indexOf('function ' + name + '(');
  assert.ok(i >= 0, name + ' not found');
  let d = 0;
  for (let k = src.indexOf('{', i); k < src.length; k++) {
    if (src[k] === '{') d++; else if (src[k] === '}' && --d === 0) return src.slice(i, k + 1);
  }
};
const chip = new Function('_fmtDur', '_msgQueued', slice('_msgDeliveryChip') + '\nreturn _msgDeliveryChip;')(
  (ms) => ms + 'ms', () => false);
const label = (e) => chip(e).replace(/<[^>]+>/g, '');

test('withheld row says not sent and names the card', () => {
  assert.equal(label({ delivery: 'board', card_id: 'MG-1918' }), 'not sent → MG-1918');
});
test('board row later handed over reads as delivered', () => {
  assert.equal(label({ delivery: 'board', delivered_at: 1790248576000 }), 'delivered via board');
});
test('direct row is unchanged', () => {
  assert.equal(label({ delivery: 'direct' }), 'direct');
});
