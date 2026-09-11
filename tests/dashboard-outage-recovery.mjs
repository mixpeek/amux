// Execute shipped browser functions, with deterministic transport/storage seams.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';
const require = createRequire(import.meta.url);
const { parse } = require('espree');
const source = readFileSync(new URL('../crates/amux-dashboard/static/app.js', import.meta.url), 'utf8');
const ast = parse(source, {ecmaVersion: 'latest', range: true});
function code(name) {
  const node = ast.body.find(n => n.type === 'FunctionDeclaration' && n.id.name === name);
  assert.ok(node, 'shipped function exists: ' + name);
  return source.slice(...node.range);
}
function fixture(names = [], shared = {}) {
  const stored = shared.stored || new Map();
  const timers = new Map(); let tid = 0;
  const elements = new Map();
  const element = id => {
    if (!elements.has(id)) elements.set(id, {value: '', textContent: '', innerHTML: '', style: {}, scrollHeight: 0, classList: {add() {}, remove() {}, contains() {return false;}}});
    return elements.get(id);
  };
  const sandbox = { Response, AbortController, AbortSignal, DOMException, crypto: globalThis.crypto,
    console, Date, Promise, Set, Map, JSON, Math,
    location: {origin: 'https://amux.test'}, navigator: {locks: shared.locks || sharedStorage().locks},
    document: {getElementById: element},
    localStorage: {setItem(k,v) { stored.set(k,v); }, getItem(k) { return stored.get(k) ?? null; }},
    setTimeout(fn) { timers.set(++tid, fn); return tid; }, clearTimeout(id) { timers.delete(id); },
    API: '', offlineQueue: [], drafts: [], online: true, _syncFlight: null, _syncRetryTimer: null, _syncBackoffMs: 0, _SYNC_MIN_MS: 2000, _SYNC_MAX_MS: 60000,
    _writeError: '', _outboxActive: new Set(), _bdSaveRequests: new Set(), consecutiveFailures: 0,
    _OUTBOX_SKIP: /\/api\/client-debug/, _OUTBOX_METHODS: {POST:1,PATCH:1,PUT:1,DELETE:1},
    _authHeaders: h => h, esc: s => s, describeOp: q => q.url,
    showToast() {}, amuxTrack() {}, updateConnectionStatus() {}, fetchSessions() {}, fetchBoard() {},
    _loadCmdHistoryFromServer: () => Promise.resolve(), _peekMessagesBadge() {}, _outboxBoardAcknowledged() {},
    _origFetch: async () => new Response('{"id":"TASK-1"}', {status:200}),
    _apiErrText: async r => `${r.status}: ${await r.text()}`,
  };
  const ctx = vm.createContext(sandbox);
  for (const name of ['_validateMessageAcknowledgement', '_validateBoardAcknowledgement', '_readQueue', '_outboxLock', '_mutateQueue', '_outboxQueueable', '_queueOp', '_boundedMutationFetch', '_syncOneDraft', '_syncBackoffReset', '_scheduleSyncRetry', '_runSyncBanner', 'runSyncBanner', ...names]) vm.runInContext(code(name), ctx);
  return {ctx, stored, timers, element};
}
const patch = {method:'PATCH', body:'{"title":"saved","expect_rev":1}'};
async function enqueue(ctx) { assert.equal(await ctx._queueOp('/api/board/TASK-1', patch), true); }

test('queue is durable throughout replay, and simultaneous flushes share one delivery', async () => {
  const {ctx, stored} = fixture(); await enqueue(ctx);
  let finish; let calls = 0;
  ctx._origFetch = () => { calls++; return new Promise(resolve => { finish = resolve; }); };
  const first = ctx.runSyncBanner(); const second = ctx.runSyncBanner();
  assert.equal(first, second);
  await new Promise(setImmediate);
  assert.equal(JSON.parse(stored.get('amux_offline_queue')).length, 1);
  assert.equal(calls, 1);
  finish(new Response('{"id":"TASK-1"}', {status:200}));
  await first;
  assert.equal(JSON.parse(stored.get('amux_offline_queue')).length, 0);
});

test('500 and restart retain exact intent; a later acknowledged retry drains it', async () => {
  const first = fixture(); await enqueue(first.ctx);
  first.ctx._origFetch = async () => new Response('timed out waiting for connection', {status:500});
  await first.ctx.runSyncBanner();
  const bytes = first.stored.get('amux_offline_queue');
  assert.equal(JSON.parse(bytes)[0].options.body, patch.body);
  assert.match(first.ctx._writeError, /timed out/);
  const restarted = fixture(); restarted.ctx.offlineQueue = JSON.parse(bytes); restarted.stored.set('amux_offline_queue', bytes);
  await restarted.ctx.runSyncBanner();
  assert.equal(restarted.ctx.offlineQueue.length, 0);
});

test('a timeout ends replay without deleting intent and permits another attempt', async () => {
  const {ctx, timers} = fixture(); await enqueue(ctx);
  ctx._origFetch = (_url, options) => new Promise((_resolve, reject) => {
    options.signal.addEventListener('abort', () => reject(new DOMException('Timed out', 'AbortError')));
  });
  const first = ctx.runSyncBanner();
  await new Promise(setImmediate);
  // _queueOp scheduled the background retry first; fire only request timeout.
  [...timers.values()].at(-1)();
  await first;
  assert.equal(ctx.offlineQueue.length, 1);
  assert.equal(ctx._syncFlight, null);
  ctx._origFetch = async () => new Response('{"id":"TASK-1"}', {status:200});
  await ctx.runSyncBanner();
  assert.equal(ctx.offlineQueue.length, 0);
});

test('409 retains a visible blocked intent and automatic retries do not overwrite peer work', async () => {
  const {ctx} = fixture(); await enqueue(ctx); let calls = 0;
  ctx._origFetch = async () => { calls++; return new Response('rev conflict', {status:409}); };
  await ctx.runSyncBanner(); await ctx.runSyncBanner();
  assert.equal(calls, 1);
  assert.equal(ctx.offlineQueue[0].state, 'blocked');
  assert.match(ctx.offlineQueue[0].error, /rev conflict/);
});

test('full device storage refuses a new queue entry without false acceptance', async () => {
  const {ctx} = fixture();
  ctx.localStorage.setItem = () => { throw new DOMException('ENOSPC', 'QuotaExceededError'); };
  assert.equal(await ctx._queueOp('/api/board/TASK-1', patch), false);
  assert.equal(ctx.offlineQueue.length, 0);
  assert.match(ctx._writeError, /not safely queued/);
});

test('full queue preserves all older writes and refuses the new one', async () => {
  const {ctx, stored} = fixture();
  ctx.offlineQueue = Array.from({length:200}, (_,id) => ({id}));
  stored.set('amux_offline_queue', JSON.stringify(ctx.offlineQueue));
  assert.equal(await ctx._queueOp('/api/board/TASK-1', patch), false);
  assert.equal(ctx.offlineQueue[0].id, 0);
  assert.equal(ctx.offlineQueue.length, 200);
});

test('wrong-card acknowledgement retains the queued mutation', async () => {
  const {ctx} = fixture(); await enqueue(ctx);
  ctx._origFetch = async () => new Response('{"id":"OTHER-2"}', {status:200});
  await ctx.runSyncBanner();
  assert.equal(ctx.offlineQueue.length, 1);
  assert.match(ctx._writeError, /exact card/);
});

test('editor refuses loading/stale identity and pins its target across asynchronous gate confirmation', async () => {
  const {ctx, element} = fixture(['boardDetailSave']);
  Object.assign(ctx, {boardDetailId:'TASK-1', _boardDetailOpenGeneration:2, _bdHydrated:true,
    _bdLoadedIdentity:{id:'TASK-1',generation:1,rev:1}, _tagState:{bd:[]}, _boardDrafts:{},
    boardItems:[{id:'TASK-1',status:'todo'}], boardDetailStatus:'doing', _boardDraftsPersist() {},
    _bdAudit(kind, detail) {ctx.audit = {kind, detail};},
    _boardDetailIdentityDiscard() { ctx.discarded = true; }, updateBoardItem() { throw new Error('must not save'); }});
  element('bd-title').value = 'Old card content';
  assert.equal(await ctx.boardDetailSave(), false);
  assert.equal(ctx.audit.kind, 'card-save-refused');
  assert.equal(ctx.audit.detail.verdict, 'card_identity_unloaded');
  assert.equal(ctx.audit.detail.measured, true);
  ctx._bdLoadedIdentity.generation = 2;
  ctx._gateConfirm = async () => {ctx.boardDetailId='TASK-2'; ctx._boardDetailOpenGeneration++; return true;};
  assert.equal(await ctx.boardDetailSave(), false);
  assert.equal(ctx.discarded, true);
  assert.equal(ctx._boardDrafts['TASK-1'].title, 'Old card content');
  assert.equal(ctx._boardDrafts['TASK-2'], undefined);
});


test('message retry carries the same server deduplication ID as the first attempt', async () => {
  const {ctx} = fixture(['_outboxRequestOptions']);
  const first = ctx._outboxRequestOptions('/api/sessions/lane/send', {method:'POST', body:'{"text":"continue"}'});
  const id = JSON.parse(first.body).msg_id;
  assert.ok(id);
  assert.equal(await ctx._queueOp('/api/sessions/lane/send', first), true);
  assert.equal(JSON.parse(ctx.offlineQueue[0].options.body).msg_id, id);
  assert.equal(ctx._outboxRequestOptions('/api/sessions/lane/send', first), first);
});

test('editor keeps its draft on failed save and reports Saved only after acknowledgement', async () => {
  const {ctx, element} = fixture(['boardDetailSave']);
  Object.assign(ctx, {boardDetailId:'TASK-1', _boardDetailOpenGeneration:2, _bdHydrated:true,
    _bdLoadedIdentity:{id:'TASK-1',generation:2,rev:1}, _tagState:{bd:[]}, _boardDrafts:{},
    boardItems:[{id:'TASK-1',status:'todo'}], boardDetailStatus:'todo', _boardDraftsPersist() {},
    updateBoardItem: async () => false});
  element('bd-title').value = 'Retained draft';
  assert.equal(await ctx.boardDetailSave(), false);
  assert.match(element('bd-save-status').textContent, /Not saved/);
  assert.equal(ctx._boardDrafts['TASK-1'].title, 'Retained draft');
  ctx.updateBoardItem = async (id, body) => ({id, ...body, rev:2});
  await ctx.boardDetailSave();
  assert.equal(element('bd-save-status').textContent, 'Saved');
  assert.equal(ctx._boardDrafts['TASK-1'], undefined);
  assert.equal(ctx._bdLoadedIdentity.rev, 2);
});

test('connection status follows reads while pending write errors remain visible on their operations', () => {
  const {ctx, element} = fixture(['updateConnectionStatus']);
  const connection = element('connection');
  ctx.document.querySelectorAll = () => [connection];
  Object.assign(ctx, {_sessionLoadError:null, _boardReadError:'', _syncReadError:'',
    _liveSSE:true, _recordConnState() {}, _sessionReadNotice:() => ''});
  ctx.updateConnectionStatus(); assert.equal(connection.textContent, 'Live');
  ctx._writeError = '500: pool timeout';
  ctx.updateConnectionStatus(); assert.equal(connection.textContent, 'Live');
  assert.equal(ctx._writeError, '500: pool timeout', 'a live connection does not erase the failed write');
  ctx.offlineQueue = [{url:'/api/board/TASK-1',timestamp:Date.now(),error:ctx._writeError}];
  ctx.updateConnectionStatus(); assert.equal(connection.textContent, '1 pending');
  assert.match(element('offline-ops').innerHTML, /500: pool timeout/, 'the failed operation retains its actionable error');
  ctx._writeError = ''; ctx.offlineQueue = []; ctx._boardReadError = '500';
  ctx.updateConnectionStatus(); assert.equal(connection.textContent, 'Sync error');
  ctx._boardReadError = ''; ctx._syncReadError = 'network_error';
  ctx.updateConnectionStatus(); assert.equal(connection.textContent, 'Sync error');
  ctx._sessionLoadError = {status:401};
  ctx.updateConnectionStatus(); assert.equal(connection.textContent, 'Access required');
});

function sharedStorage() {
  const flights = new Map();
  return {stored: new Map(), locks: {request(name, work) {
    const flight = (flights.get(name) || Promise.resolve()).then(work);
    flights.set(name, flight.catch(() => {}));
    return flight;
  }}};
}

test('concurrent tabs append without overwriting another tab and replay each intent once', async () => {
  const shared = sharedStorage();
  const a = fixture([], shared); const b = fixture([], shared);
  await Promise.all([a.ctx._queueOp('/api/board/TASK-1', patch), b.ctx._queueOp('/api/board/TASK-2', patch)]);
  assert.equal(JSON.parse(shared.stored.get('amux_offline_queue')).length, 2);
  const delivered = [];
  for (const {ctx} of [a,b]) ctx._origFetch = async url => {
    delivered.push(url);
    return new Response(JSON.stringify({id:url.split('/').pop()}), {status:200});
  };
  await Promise.all([a.ctx.runSyncBanner(), b.ctx.runSyncBanner()]);
  assert.deepEqual(delivered.sort(), ['/api/board/TASK-1','/api/board/TASK-2']);
  assert.equal(JSON.parse(shared.stored.get('amux_offline_queue')).length, 0);
});

test("acknowledging an in-flight write does not erase a different tab's new intent", async () => {
  const shared = sharedStorage(); const a = fixture([], shared); const b = fixture([], shared);
  await enqueue(a.ctx);
  let finish;
  a.ctx._origFetch = () => new Promise(resolve => { finish = resolve; });
  const flight = a.ctx.runSyncBanner(); await new Promise(setImmediate);
  await b.ctx._queueOp('/api/board/TASK-2', patch);
  finish(new Response('{"id":"TASK-1"}', {status:200}));
  await flight;
  const pending = JSON.parse(shared.stored.get('amux_offline_queue'));
  assert.equal(pending.length, 1); assert.equal(pending[0].url, '/api/board/TASK-2');
});

test('an acknowledgement preserves newer editor keystrokes and advances their expected revision', async () => {
  const {ctx, element} = fixture(['boardDetailSave']);
  let finish;
  Object.assign(ctx, {boardDetailId:'TASK-1', _boardDetailOpenGeneration:2, _bdHydrated:true,
    _bdLoadedIdentity:{id:'TASK-1',generation:2,rev:1}, _tagState:{bd:[]}, _boardDrafts:{},
    boardItems:[{id:'TASK-1',status:'todo'}], boardDetailStatus:'todo', _boardDraftsPersist() {},
    updateBoardItem: () => new Promise(resolve => { finish = resolve; })});
  element('bd-title').value = 'Submitted edit';
  const save = ctx.boardDetailSave();
  assert.equal(await ctx.boardDetailSave(), false, 'a second click cannot submit the same revision twice');
  element('bd-title').value = 'Later unsaved edit';
  finish({id:'TASK-1',rev:2}); await save;
  assert.equal(ctx._boardDrafts['TASK-1'].title, 'Later unsaved edit');
  assert.equal(ctx._boardDrafts['TASK-1'].expect_rev, 2);
  assert.match(element('bd-save-status').textContent, /newer changes not saved/);
});


test('ignored board fields cannot acknowledge or silently drain an edit', async () => {
  const {ctx} = fixture(); await enqueue(ctx);
  ctx._origFetch = async () => new Response('{"id":"TASK-1","ignored_fields":["gate"]}');
  await ctx.runSyncBanner();
  assert.equal(ctx.offlineQueue.length, 1);
  assert.equal(ctx.offlineQueue[0].state, 'blocked');
  assert.match(ctx._writeError, /ignored gate/);
});

test('a failed worker start retains the draft and a restart retries only unfinished steps', async () => {
  const {ctx, stored} = fixture();
  Object.assign(ctx, {render() {}, _applyYoloDefault:async () => {},
    saveDrafts() {stored.set('drafts', JSON.stringify(ctx.drafts));},
    removeDraft(name) {ctx.drafts = ctx.drafts.filter(d => d.name !== name);}});
  ctx.drafts = [{name:'fixture',dir:'/tmp/fixture',prompt:'one prompt'}];
  ctx._origFetch = async url => new Response(url.endsWith('/start') ? 'pool timeout' : '{}', {status:url.endsWith('/start') ? 500 : 200});
  await ctx.runSyncBanner();
  assert.equal(ctx.drafts.length, 1);
  assert.equal(ctx.drafts[0].synced_create, true);
  assert.match(ctx.drafts[0].error, /Start worker/);
  // Reconstruct exactly the persisted draft, as a page restart would.
  ctx.drafts = JSON.parse(stored.get('drafts'));
  const calls = []; let firstId;
  ctx._origFetch = async (url, options) => {
    calls.push(url);
    if (url.endsWith('/send')) { firstId = JSON.parse(options.body).msg_id; return new Response('lost response', {status:500}); }
    return new Response('{}');
  };
  await ctx.runSyncBanner();
  assert.equal(calls.some(url => url === '/api/sessions'), false);
  ctx.drafts = JSON.parse(stored.get('drafts'));
  ctx._origFetch = async (url, options) => {
    assert.ok(url.endsWith('/send'));
    assert.equal(JSON.parse(options.body).msg_id, firstId);
    return new Response('{}');
  };
  await ctx.runSyncBanner();
  assert.equal(ctx.drafts.length, 0);
  assert.equal(ctx._writeError, '');
});


test('a board poll during hydration cannot make stale controls look like user edits', async () => {
  const {ctx, element} = fixture(['_bdHydrate']);
  let finish;
  Object.assign(ctx, {boardDetailId:'TASK-1', _boardDetailOpenGeneration:2, _bdHydrated:false,
    _bdActiveDirty:false, _boardDrafts:{}, _tagState:{bd:[]},
    boardItems:[{id:'TASK-1', title:'Old snapshot', status:'todo'}],
    apiCall:() => new Promise(resolve => {finish = resolve;}),
    _bdDraftHasActiveEdits:() => false, _renderDetailStatusBtns() {}, _bdRenderHistory() {},
    _bdRenderStatusBanner() {}, _bdRenderMeta() {}, _populateSessionSelect() {}, _bdConfigureGo() {},
    _beTagRenderChips() {}, _beTagInputUpdate() {}});
  element('bd-title').value = 'Old snapshot';
  const hydration = ctx._bdHydrate('TASK-1');
  const fresh = {id:'TASK-1', title:'Committed update', status:'todo', rev:2};
  ctx.boardItems = [fresh];
  finish(new Response(JSON.stringify(fresh)));
  assert.equal(await hydration, true);
  assert.equal(element('bd-title').value, 'Committed update');
  assert.equal(ctx._bdLoadedIdentity.rev, 2);
});


test("unavailable storage coordination refuses a write instead of risking another tab's pending data", async () => {
  const {ctx, stored} = fixture();
  ctx.navigator.locks = undefined;
  assert.equal(await ctx._queueOp('/api/board/TASK-1', patch), false);
  assert.equal(stored.has('amux_offline_queue'), false);
  assert.match(ctx._writeError, /storage is unavailable/);
});


test('message receipt loss replays the same msg_id after reload; dedup receipt drains it', async () => {
  const first = fixture();
  const body = JSON.stringify({text:'one logical message', msg_id:'stable-message-id'});
  await first.ctx._queueOp('/api/sessions/owned/send', {method:'POST', body});
  let delivered;
  first.ctx._origFetch = async (_, opts) => { delivered = opts.body; throw new Error('receipt lost after delivery'); };
  await first.ctx.runSyncBanner();
  assert.equal(delivered, body);
  const second = fixture([], {stored:first.stored});
  second.ctx._origFetch = async (_, opts) => {
    assert.equal(opts.body, body);
    return new Response(JSON.stringify({ok:true,deduped:true}));
  };
  await second.ctx.runSyncBanner();
  assert.equal(JSON.parse(first.stored.get('amux_offline_queue')).length, 0);
});

test('ambiguous 200 blocks a message and preserves ordering behind it across retries', async () => {
  const {ctx, stored} = fixture();
  for (const text of ['first','second']) await ctx._queueOp('/api/sessions/owned/send', {method:'POST',body:JSON.stringify({text,msg_id:text})});
  let calls = 0;
  ctx._origFetch = async () => { calls++; return new Response(JSON.stringify({ok:true,submitted:false})); };
  await ctx.runSyncBanner();
  const saved = JSON.parse(stored.get('amux_offline_queue'));
  assert.equal(calls, 1);
  assert.equal(saved.length, 2);
  assert.equal(saved[0].state, 'blocked');
  await ctx.runSyncBanner();
  assert.equal(calls, 1, 'later messages cannot overtake a blocked predecessor');
});

test('server deferred and steering receipts acknowledge storage without claiming terminal submission', async () => {
  for (const [endpoint, receipt] of [['send',{ok:true,submitted:null,submission:'deferred'}], ['steer',{ok:true,id:'steer-123',deliverable:false}]]) {
    const {ctx,stored} = fixture();
    await ctx._queueOp('/api/sessions/owned/'+endpoint, {method:'POST',body:JSON.stringify({text:'queued server-side',msg_id:endpoint})});
    ctx._origFetch = async () => new Response(JSON.stringify(receipt));
    await ctx.runSyncBanner();
    assert.equal(JSON.parse(stored.get('amux_offline_queue')).length, 0);
  }
});

test('replay retries a durable message even while connectivity is believed offline', async () => {
  const {ctx, stored, timers} = fixture();
  await enqueue(ctx);
  ctx.online = false;
  const retry = [...timers.values()].at(-1);
  assert.ok(retry);
  retry();
  await ctx._syncFlight;
  assert.equal(ctx.offlineQueue.length, 0);
  assert.deepEqual(JSON.parse(stored.get('amux_offline_queue')), []);
  assert.equal(ctx._syncBackoffMs, 0, 'successful drain resets outage backoff');
});

test('a newly queued send stays quiet while a stuck send shows its waiting state', () => {
  const {ctx, element} = fixture(['updateConnectionStatus']);
  ctx.document.querySelectorAll = () => [];
  Object.assign(ctx, {_sessionLoadError:null, _boardReadError:'', _syncReadError:'',
    _liveSSE:true, _recordConnState() {}, _sessionReadNotice:() => ''});
  const classes = new Set();
  element('offline-banner').classList = {add: x=>classes.add(x), remove:x=>classes.delete(x)};
  ctx.offlineQueue = [{url:'/api/sessions/worker/send',timestamp:Date.now()}];
  ctx.updateConnectionStatus();
  assert.equal(classes.has('active'), false);
  ctx.offlineQueue[0].timestamp -= 21000;
  ctx.updateConnectionStatus();
  assert.equal(classes.has('active'), true);
  assert.match(element('offline-banner-title').innerHTML, /Still sending/);
  ctx.online = false;
  ctx.updateConnectionStatus();
  assert.match(element('offline-banner-title').innerHTML, /will send on reconnect/);
});
