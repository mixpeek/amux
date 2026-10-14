// T-IOS-5: the iOS app is a free companion to a paid web service, so it may not
// show prices or link to buying (App Store guideline 3.1.3(f)). Scans every string
// literal in the app's Swift sources; comments are ignored.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';

const dir = new URL('../ios/AmuxApp/Sources/', import.meta.url);
const BANNED = /(\$\s?\d|\bprice|\bpricing\b|\bbuy\b|\bpurchase|\bsubscribe|\bcheckout\b|\bupgrade\b|amux\.io\/(cloud|pricing)|cloud\.amux\.io\/start)/i;

function literals(src) {
  const noComments = src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '').replace(/([^:"])\/\/.*$/gm, '$1');
  return [...noComments.matchAll(/"((?:[^"\\]|\\.)*)"/g)].map(m => m[1]);
}

test('no Swift string literal offers a price or a way to buy', () => {
  const hits = [];
  for (const f of readdirSync(dir).filter(n => n.endsWith('.swift'))) {
    for (const s of literals(readFileSync(new URL(f, dir), 'utf8'))) {
      if (BANNED.test(s)) hits.push(`${f}: "${s}"`);
    }
  }
  assert.deepEqual(hits, []);
});

test('the scanner itself catches an offer to buy', () => {
  assert.ok(literals('let a = "Get amux Cloud for $39/month" // fine').some(s => BANNED.test(s)));
  assert.ok(!literals('// Buy now at $39\nlet b = "Sign in"').some(s => BANNED.test(s)));
});
