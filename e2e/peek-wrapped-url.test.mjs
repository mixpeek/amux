import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

// A worker's terminal hard-wraps a long link. The peek's link detector stopped
// at the newline, so an Apps link wrapped after 18 of its id's 32 characters
// opened nothing (2026-11-07). Runs the SHIPPED _detectTextLinks.

const source = fs.readFileSync('crates/amux-dashboard/static/app.js', 'utf8');
const start = source.indexOf('const _LINK_TLDS');
const end = source.indexOf('\n}\n', source.indexOf('function _detectTextLinks')) + 3;
assert.ok(start > 0 && end > start, '_detectTextLinks markers must exist in app.js');
const ctx = vm.createContext({ peekSessionDir: '' });
vm.runInContext(source.slice(start, end) + '\nglobalThis.detect = _detectTextLinks;', ctx);
const detect = plain => JSON.parse(JSON.stringify(ctx.detect(plain)));   // plain arrays, not the sandbox's
const urls = plain => detect(plain).filter(l => l.type === 'url').map(l => l.value);

const ID = 'd37f4cd27adc4a10ce990200680f2321';
const HOST = 'https://desktop.tail5ce8f5.ts.net:8824/#view=apps&open=';

test('a link the terminal wrapped mid-id is one link with the whole id', () => {
  const plain = 'Local server: a production build is running at\n' + HOST + ID.slice(0, 18) + '\n'
    + ID.slice(18) + ". It's waiting for your approval in the Apps tab.";
  const links = detect(plain).filter(l => l.type === 'url');
  assert.deepEqual(links.map(l => l.value), [HOST + ID]);
  // The anchor covers both visible pieces and stops before the full stop.
  assert.equal(plain.slice(links[0].start, links[0].end), HOST + ID.slice(0, 18) + '\n' + ID.slice(18));
});

test('a link wrapped twice is joined across both breaks', () => {
  const long = 'https://example.com/a/very/long/path/that/keeps/going/and/going';
  assert.deepEqual(urls(long + '\n/segment_two/more_text_here/and_more_text_after_that\n?q=1&r=2 trailing words'),
    [long + '/segment_two/more_text_here/and_more_text_after_that?q=1&r=2']);
});

test('prose after a link at a line end is not swallowed', () => {
  const long = 'see https://example.com/some/long/path/for/the/report/index.html';
  assert.deepEqual(urls(long + '\nand then the next sentence'), ['https://example.com/some/long/path/for/the/report/index.html']);
  assert.deepEqual(urls(long + '\n- a bullet 2026'), ['https://example.com/some/long/path/for/the/report/index.html']);
  // Trailing punctuation closes the sentence, so the next line stays its own.
  assert.deepEqual(urls(long + '.\n2026 was the year'), ['https://example.com/some/long/path/for/the/report/index.html']);
  // A short line is not a wrap.
  assert.deepEqual(urls('https://x.com/a\n123abc'), ['https://x.com/a']);
});
