import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const app = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const start = app.indexOf('function _focusKey(e) {');
const end = app.indexOf('async function _focusPatch(', start);
assert.ok(start >= 0 && end > start, 'focus key handler exists');
const handler = app.slice(start, end);

function focusKeys() {
  const actions = [];
  const answer = {
    value: '', isContentEditable: false,
    closest: () => ({}),
    blur: () => actions.push('blur'),
  };
  const outside = { isContentEditable: false, closest: () => null };
  const modal = { open: true, classList: { contains: name => name === 'open' && modal.open } };
  const context = {
    document: { getElementById: id => id === 'focus-ans' ? answer : id === 'modal-backdrop' ? modal : null },
    _modalClose: value => { actions.push(['modal-close', value]); modal.open = false; },
    _focusClose: () => actions.push('close'),
    _focusNext: () => actions.push('next'),
    _focusPrev: () => actions.push('prev'),
    _focusAnswer: () => actions.push('answer'),
    _focusDecide: verdict => actions.push(verdict),
    _focusUnblock: () => actions.push('unblock'),
    _focusNudge: () => actions.push('nudge'),
  };
  runInNewContext(handler + '\nthis.key = _focusKey;', context);
  const press = (key, target, extras = {}) => {
    const event = {
      key, target, defaultPrevented: false, propagationStopped: false,
      preventDefault() { this.defaultPrevented = true; },
      stopImmediatePropagation() { this.propagationStopped = true; },
      ...extras,
    };
    context.key(event);
    return event;
  };
  return { actions, answer, modal, outside, press };
}

test('focus shortcuts ignore Answer typing and preserve outside approve and nudge', () => {
  const { actions, answer, outside, press } = focusKeys();
  for (const key of 'approve now') {
    const event = press(key, answer);
    assert.equal(event.defaultPrevented, false, `${key} must remain typeable`);
  }
  assert.deepEqual(actions, [], 'typing must not approve, reject, nudge, or navigate');
  press('a', outside);
  press('n', outside);
  assert.deepEqual(actions, ['approved', 'nudge']);
});

test('focus Escape blurs nonempty Answer first, then closes its modal and overlay', () => {
  const { actions, answer, modal, outside, press } = focusKeys();
  answer.value = 'approve now';
  const first = press('Escape', answer);
  assert.equal(first.defaultPrevented, true);
  assert.equal(first.propagationStopped, true);
  assert.deepEqual(actions, ['blur']);
  press('a', outside, { ctrlKey: true });
  press('n', outside, { isComposing: true });
  press('a', outside, { keyCode: 229 });
  assert.deepEqual(actions, ['blur']);
  press('Escape', outside);
  assert.deepEqual(actions, ['blur', ['modal-close', false], 'close']);
  assert.equal(modal.open, false);
});

test('Escape on empty Answer closes its modal before the focus overlay', () => {
  const { actions, answer, modal, press } = focusKeys();
  press('Escape', answer);
  assert.deepEqual(actions, [['modal-close', false], 'close']);
  assert.equal(modal.open, false);
});
