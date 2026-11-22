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

// A Web Storage stand-in with the parts the list view uses (key/length for the
// open-page scan), and a page harness: each page has its own sessionStorage
// and window listeners, and every page shares one localStorage.
const mem = () => {
  const s = {};
  return { getItem: k => (k in s ? s[k] : null), setItem: (k, v) => { s[k] = String(v); },
    removeItem: k => { delete s[k]; }, key: i => Object.keys(s)[i] ?? null, get length() { return Object.keys(s).length; } };
};
const listViewBody = () => {
  const i = src.indexOf("const _LIST_VIEW_KEY");
  return src.slice(i, src.indexOf('_restoreListView();', i));
};
const makePage = (localStorage, search = '', sessionStorage = mem()) => {
  const on = {};
  const window = { addEventListener: (ev, fn) => { (on[ev] ||= []).push(fn); } };
  const p = new Function('localStorage', 'sessionStorage', 'location', 'window', 'setInterval', 'amuxTrack', `
    let activeTag = '', hiddenTags = new Set(), filterProviders = new Set(), filterModels = new Set(),
        filterStatuses = new Set(), searchQuery = '', logSearchMode = false;
    ${listViewBody()}
    return { _restoreListView, _saveListView,
      set(v) { activeTag = v.tag || ''; hiddenTags = new Set(v.hidden || []); filterStatuses = new Set(v.statuses); searchQuery = v.q; },
      get() { return { activeTag, hidden: [...hiddenTags], statuses: [...filterStatuses], searchQuery }; } };`)(
    localStorage, sessionStorage, { search }, window, () => 0, () => {});
  p.close = () => (on.pagehide || []).forEach(fn => fn());
  p.sessionStorage = sessionStorage;
  return p;
};

test('the app reopened with no other page open gets its last list view back', () => {
  const local = mem();
  const a = makePage(local);
  a._restoreListView();
  a.set({ tag: 'ops', hidden: ['gtm'], statuses: ['working'], q: 'mvs' });
  a._saveListView();
  a.close();
  const b = makePage(local);
  b._restoreListView();
  assert.deepEqual(b.get(), { activeTag: 'ops', hidden: ['gtm'], statuses: ['working'], searchQuery: 'mvs' });
});

// Owner, 2026-11-21: a new tab (a Mac app tab, a browser tab) opened on the
// last tab's pills, and a tab reloaded behind a worker came back on another
// tab's filter. Each page keeps its own; a new one beside an open one is clean.
test('a new page beside an open one starts clean, and each page keeps its own across a reload', () => {
  const local = mem();
  const a = makePage(local);
  a._restoreListView();
  a.set({ statuses: ['working'], q: 'alpha' });
  a._saveListView();
  const b = makePage(local);
  b._restoreListView();
  assert.deepEqual(b.get(), { activeTag: '', hidden: [], statuses: [], searchQuery: '' }, 'the new tab is free and open');
  b.set({ statuses: ['stopped'], q: 'beta' });
  b._saveListView();
  // A reloads (same sessionStorage): its own view, not B's later one.
  a.close();
  const aAgain = makePage(local, '', a.sessionStorage);
  aAgain._restoreListView();
  assert.deepEqual(aAgain.get(), { activeTag: '', hidden: [], statuses: ['working'], searchQuery: 'alpha' });
});

test('render() saves the view on every repaint', () => {
  const r = slice('render');
  assert.ok(r.indexOf('_saveListView();') > 0 && r.indexOf('_saveListView();') < r.indexOf('if (openMenu'),
    'the save must run before the menu guard can return early');
});

// Owner, 2026-11-16: a new tab of the in-page tab bar opened with the first
// tab's search and filter, and a tab's filter came back in the first tab after
// a reload. A tab (?_tab=<id>) starts clean and keeps its own copy.
test('a tab of the in-page tab bar keeps its own list view and leaves the page\'s alone', () => {
  // Every in-page tab is an iframe of one browser tab: one sessionStorage.
  const local = mem(), session = mem();
  const page = makePage(local, '', session);
  page._restoreListView();
  page.set({ statuses: ['working'], q: 'alpha' });
  page._saveListView();

  const tab = makePage(local, '?_tab=2', session);
  tab._restoreListView();
  assert.deepEqual(tab.get().statuses, [], 'a new tab starts clean');
  assert.equal(tab.get().searchQuery, '');
  tab.set({ statuses: ['stopped'], q: 'beta' });
  tab._saveListView();

  const pageAgain = makePage(local, '', session);
  pageAgain._restoreListView();
  assert.deepEqual([pageAgain.get().statuses, pageAgain.get().searchQuery], [['working'], 'alpha'], 'the page keeps its own view');
  const tabAgain = makePage(local, '?_tab=2', session);
  tabAgain._restoreListView();
  assert.deepEqual([tabAgain.get().statuses, tabAgain.get().searchQuery], [['stopped'], 'beta'], 'the tab is rebuilt on its own view');
  const otherTab = makePage(local, '?_tab=3', session);
  otherTab._restoreListView();
  assert.deepEqual([otherTab.get().statuses, otherTab.get().searchQuery], [[], ''], 'another tab does not see it');
});
