// Up-arrow recall hands the composer the message text, never the send stamp.
// Runs the shipped _cmdRecallBuild from app.js in a sandbox.
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';
const source = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
// Slice the top-level function out of the shipped source by matching braces.
// The body has no unbalanced braces in strings or regexes, so counting is exact.
function code(name) {
  const start = source.indexOf(`\nfunction ${name}(`);
  assert.ok(start !== -1, 'shipped function exists: ' + name);
  let depth = 0, i = source.indexOf('{', start);
  for (; i < source.length; i++) {
    if (source[i] === '{') depth++;
    else if (source[i] === '}' && --depth === 0) break;
  }
  return source.slice(start + 1, i + 1);
}

function recall(rows, session = 'w1') {
  const ctx = vm.createContext({ _cmdHistory: rows, _msgKind: () => 'human' });
  vm.runInContext(code('_cmdRecallBuild'), ctx);
  return vm.runInContext(`_cmdRecallBuild(${JSON.stringify(session)})`, ctx);
}

test('recall strips the 12-hour send stamp with an author', () => {
  const out = recall([{ session: 'w1', text: '[2:05 PM jav@example.com] run the migration' }]);
  assert.deepEqual([...out], ['run the migration']);
});

test('recall strips a 24-hour stamp with seconds', () => {
  const out = recall([{ session: 'w1', text: '[14:42:07 jav@example.com] ship it' }]);
  assert.deepEqual([...out], ['ship it']);
});

test('recall strips a time-only stamp', () => {
  const out = recall([{ session: 'w1', text: '[9:30 AM] status?' }]);
  assert.deepEqual([...out], ['status?']);
});

test('recall strips the stamp and the prompt glyph together, then collapses repeats', () => {
  const out = recall([
    { session: 'w1', text: '[2:05 PM jav@example.com] do it' },
    { session: 'w1', text: '❯ do it' },
  ]);
  assert.deepEqual([...out], ['do it']);
});

test('recall keeps a leading bracket that is not a time', () => {
  const out = recall([{ session: 'w1', text: '[todo] write the changelog' }]);
  assert.deepEqual([...out], ['[todo] write the changelog']);
});

test('recall stays scoped to the session', () => {
  const out = recall([{ session: 'w2', text: '[2:05 PM jav@example.com] other worker' }], 'w1');
  assert.deepEqual([...out], []);
});

// The edit path uses the same stamp shape (review F1 on the first gate run).
const stripPrefix = text => {
  const ctx = vm.createContext({});
  vm.runInContext(code('_msgStripPrefix'), ctx);
  return vm.runInContext(`_msgStripPrefix(${JSON.stringify(text)})`, ctx);
};

test('edit path strips 24-hour and seconds stamps as well as 12-hour', () => {
  assert.equal(stripPrefix('[14:42 jav@example.com] ship it'), 'ship it');
  assert.equal(stripPrefix('[14:42:07 jav@example.com] ship it'), 'ship it');
  assert.equal(stripPrefix('[2:05 PM jav@example.com] ship it'), 'ship it');
  assert.equal(stripPrefix('[todo] keep'), '[todo] keep');
});
