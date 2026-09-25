// Held emails show full addresses and the subject, replies included.
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { test } from 'node:test';
const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const i = app.indexOf('function _apprHeaders('); let d = 0, end = 0;
for (let k = app.indexOf('{', i); k < app.length; k++) { if (app[k] === '{') d++; else if (app[k] === '}' && --d === 0) { end = k + 1; break; } }
const headers = new Function('esc', app.slice(i, end) + '\nreturn _apprHeaders;')(s => String(s));
const text = h => h.replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').trim();
test('a reply shows the full recipient, cc and the thread subject', () => {
  const t = text(headers({ endpoint: 'reply', reply_all: true, from: 'ethan@mixpeek.com',
    to: 'very.long.name@some-customer-domain.example', cc: 'a@x.example, b@y.example', subject: 'Re: Pricing for the pilot' }));
  assert.match(t, /From ethan@mixpeek\.com/);
  assert.match(t, /To very\.long\.name@some-customer-domain\.example/);
  assert.match(t, /Cc a@x\.example, b@y\.example/);
  assert.match(t, /Subject Re: Pricing for the pilot/);
  assert.match(t, /Replying to everyone on the thread/);
});
test('a new email without cc omits the row', () => {
  const t = text(headers({ endpoint: 'send', to: 'x@y.example', subject: 'Hello' }));
  assert.doesNotMatch(t, /Cc/);
  assert.match(t, /Subject Hello/);
});
