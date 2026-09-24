// Group pills cycle neutral -> only -> hidden, and the list view survives a
// reload for that client (Ethan, 2026-09-24).
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

test('a pill cycles neutral -> only -> hidden -> neutral', () => {
  const next = new Function(slice('_nextTagState') + '\nreturn _nextTagState;')();
  let st = { active: '', hidden: new Set() };
  st = next('ops', st.active, st.hidden); assert.equal(st.active, 'ops'); assert.equal(st.hidden.size, 0);
  st = next('ops', st.active, st.hidden); assert.equal(st.active, ''); assert.ok(st.hidden.has('ops'));
  st = next('ops', st.active, st.hidden); assert.equal(st.active, ''); assert.equal(st.hidden.size, 0);
});

test('hidden groups drop their workers unless narrowed to another of their groups', () => {
  const vis = new Function(slice('_workerVisibleWithHiddenTags') + '\nreturn _workerVisibleWithHiddenTags;')();
  const hidden = new Set(['gtm']);
  assert.equal(vis(['gtm'], '', hidden), false);
  assert.equal(vis(['ops'], '', hidden), true);
  assert.equal(vis(['gtm', 'ops'], 'ops', hidden), true);
});

test('the list view round-trips through localStorage', () => {
  const store = {};
  const localStorage = { getItem: k => (k in store ? store[k] : null), setItem: (k, v) => { store[k] = String(v); } };
  const i = src.indexOf("const _LIST_VIEW_KEY");
  const body = src.slice(i, src.indexOf('_restoreListView();', i));
  const make = () => new Function('localStorage', `
    let activeTag = '', hiddenTags = new Set(), filterProviders = new Set(), filterModels = new Set(),
        filterStatuses = new Set(), searchQuery = '', logSearchMode = false;
    ${body}
    return { _restoreListView, _saveListView,
      set(v) { activeTag = v.tag; hiddenTags = new Set(v.hidden); filterStatuses = new Set(v.statuses); searchQuery = v.q; },
      get() { return { activeTag, hidden: [...hiddenTags], statuses: [...filterStatuses], searchQuery }; } };`)(localStorage);
  const a = make();
  a.set({ tag: 'ops', hidden: ['gtm'], statuses: ['working'], q: 'mvs' });
  a._saveListView();
  const b = make();
  b._restoreListView();
  assert.deepEqual(b.get(), { activeTag: 'ops', hidden: ['gtm'], statuses: ['working'], searchQuery: 'mvs' });
});

test('render() saves the view on every repaint', () => {
  const r = slice('render');
  assert.ok(r.indexOf('_saveListView();') > 0 && r.indexOf('_saveListView();') < r.indexOf('if (openMenu'),
    'the save must run before the menu guard can return early');
});
