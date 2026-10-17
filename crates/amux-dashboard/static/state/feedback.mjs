import { createElement, Activity, ChevronDown } from 'lucide';
import { settled, phaseLabels } from './interactions.mjs';
import { actionFromHandler } from './controls.mjs';

// The command registry is generated from app.js (scripts/build-state.mjs) and
// fetched at runtime instead of bundled, so kernel.js only changes when the
// state/ sources do. Bundling it made kernel.js, a minified one-line file,
// change on almost every app.js edit, and every pair of dashboard PRs then
// conflicted on it. The service worker precaches this URL for offline use.
export const CONTROL_REGISTRY_URL = '/state/control-registry.json';

export function installFeedback(interactions, ui, diagnostic = () => {}, registryUrl = CONTROL_REGISTRY_URL) {
  // Empty until the fetch below lands. A control declared before then gets its
  // registry-derived action and kind when the registry arrives (see refresh).
  let commandHandlers = {};
  const derivedAction = new WeakSet();
  const controls = new Map();
  // The control whose click/change/submit is being dispatched, and that event.
  // A request is the press's only while that event is still the CURRENT event
  // (window.event), i.e. it was issued from the press's own listeners or the
  // microtasks they ran. The old rule, "until a setTimeout(0) clears it", let
  // every task already queued ahead of that timer through: a background
  // fetchBoard continuation whose response landed in that gap issued
  // GET /api/board/changes and became the press's receipt, relabelling a Save
  // as board.get (interaction-receipts e2e, runs 38944452996, 38947919221).
  let source = null, sourceEvent = null;
  const dispatchVisible = 'event' in globalThis;
  const foreignReported = new WeakSet();
  function pressSource() {
    if (!source) return null;
    if (!dispatchVisible || globalThis.event === sourceEvent) return source;
    if (sourceEvent && !foreignReported.has(sourceEvent)) {
      foreignReported.add(sourceEvent);
      diagnostic({verdict:'press_bind_skipped_foreign_task', measured:true, n_considered:1,
        action:source.dataset.action, current_event:globalThis.event?.type || null});
    }
    return null;
  }
  const hub = document.createElement('details');
  hub.id = 'interaction-feedback';
  const summary = document.createElement('summary');
  summary.title = 'Action status';
  summary.append(createElement(Activity, {width:16, height:16}));
  const heading = document.createElement('span');
  heading.textContent = 'Recent actions';
  summary.append(heading);
  const status = document.createElement('span');
  status.className = 'interaction-summary-status';
  status.setAttribute('role', 'status');
  status.setAttribute('aria-live', 'polite');
  summary.append(status, createElement(ChevronDown, {width:14, height:14}));
  const list = document.createElement('div');
  list.className = 'interaction-list';
  list.setAttribute('aria-label', 'Recent actions');
  hub.append(summary, list);
  const panel = document.getElementById('notif-panel');
  if (panel) panel.insertBefore(hub, document.getElementById('notif-panel-list'));
  else diagnostic({verdict:'interaction_inspector_host_missing', measured:false, n_considered:0,
    why_unmeasured:'Notifications panel is missing'});
  function reportVisibility() {
    if (!panel?.classList.contains('active')) return;
    requestAnimationFrame(() => {
      // Opening and dismissing can both occur before this frame executes.
      if (!panel.classList.contains('active')) return;
      const rect = summary.getBoundingClientRect();
      const visible = rect.width > 0 && rect.height > 0 && rect.left >= 0 && rect.right <= innerWidth
        && rect.top >= 0 && rect.bottom <= innerHeight;
      diagnostic({verdict:visible ? 'interaction_inspector_visible' : 'interaction_inspector_clipped',
        measured:true, n_considered:1, open:hub.open, visible,
        rect:{left:rect.left, top:rect.top, width:rect.width, height:rect.height},
        viewport:{width:innerWidth, height:innerHeight}});
    });
  }
  if (panel) new MutationObserver(reportVisibility).observe(panel, {attributes:true, attributeFilter:['class']});
  hub.addEventListener('toggle', () => { ui.setState({activityOpen:hub.open}); reportVisibility(); });
  document.addEventListener('keydown', event => { if (event.key === 'Escape') hub.open = false; });
  const selectors = 'button, [role="button"], input, select, textarea, a[href], [onclick], [onchange]';
  function declare(element) {
    if (element.closest('#interaction-feedback')) return;
    const handler = actionFromHandler(element.getAttribute('onclick') || element.getAttribute('onchange'), commandHandlers);
    if (!element.dataset.action) {
      element.dataset.action = handler || element.id || element.getAttribute('name') || element.tagName.toLowerCase();
      derivedAction.add(element);
    }
    const declaration = commandHandlers[element.dataset.action];
    if (declaration) element.dataset.interactionKind ||= declaration.kind;
    element.dataset.feedbackRequired ||= 'true';
    element.dataset.targetId ||= element.closest('[data-session], [data-id]')?.dataset.session || element.closest('[data-id]')?.dataset.id || element.id || element.dataset.action;
  }
  document.querySelectorAll(selectors).forEach(declare);
  // Re-derive the actions THIS layer chose before the registry was known.
  // An action written in markup is the author's and is left alone.
  function refresh(element) {
    if (derivedAction.has(element)) {
      if (element.dataset.targetId === element.dataset.action) delete element.dataset.targetId;
      delete element.dataset.action;
      derivedAction.delete(element);
    }
    declare(element);
  }
  fetch(registryUrl, {credentials:'same-origin'})
    .then(response => response.ok ? response.json() : Promise.reject(new Error('HTTP ' + response.status)))
    .then(registry => {
      commandHandlers = registry?.command_handlers || {};
      document.querySelectorAll(selectors).forEach(refresh);
    })
    .catch(error => diagnostic({verdict:'control_registry_unavailable', measured:false, n_considered:0,
      why_unmeasured:String(error?.message || error)}));
  new MutationObserver(records => {
    for (const record of records) for (const node of record.addedNodes) {
      if (node.nodeType !== 1 || node.closest('#interaction-feedback')) continue;
      if (node.matches(selectors)) declare(node);
      node.querySelectorAll(selectors).forEach(declare);
    }
  }).observe(document.body, {childList:true, subtree:true});
  // PRESS CONTRACT (AMUX-5417, Ethan 2026-10-01: "audit every button press and
  // make sure theres progress indicators/feedback"). Every press on a control
  // whose handler reaches a request gets, from this one place:
  //   1. busy: aria-busy + a bar on its bottom edge while running, and
  //      disabled while a WRITE it started is running;
  //   2. no double-fire: a press on a control with a write in flight is swallowed;
  //   3. an outcome: a green outline, or a red one plus a toast on failure.
  // Handlers stay plain fetch code; docs/ux/button-feedback-audit.md lists them.
  //
  // ARMED, not just synchronous. The synchronous `source` below only catches a
  // request issued in the same tick as the click. A third of the command
  // handlers await something first (a draft save, a lookup, a confirmation),
  // and those presses got no busy state at all. A press on a command control
  // stays armed for ARM_MS, and the first request accepted in that window binds
  // to it. Reads never create receipts, so a background poll cannot borrow it.
  //
  // A READ a press starts shows progress and never locks the control. Opening
  // Settings fires its loads as reads, and locking on them left the Settings
  // button disabled until they returned (11.6s under load), so the menu could
  // not be closed and a quick second tap was swallowed (AMUX-5373, iOS e2e).
  // A repeated read is harmless; a repeated write is what the lock is for, so
  // the lock lifts when the press's WRITES settle (Settings also saves its tab,
  // and that write must not be held open by the slowest of a dozen loads).
  const ARM_MS = 5000, DOUBLE_MS = 700, STUCK_MS = 30000, SPEAK_MS = 600;
  let armed = null;
  const writes = new Set();   // receipt ids of in-flight writes bound to a press
  const outcome = new WeakMap();   // element -> {worst phase, toastAt baseline, label}
  let lastToastAt = 0;
  const toastEl = document.getElementById('toast');
  if (toastEl) new MutationObserver(() => { if (toastEl.classList.contains('visible')) lastToastAt = Date.now(); })
    .observe(toastEl, {attributes:true, attributeFilter:['class'], childList:true, characterData:true, subtree:true});
  const isCommand = element => !!element.dataset.interactionKind && !('repeatable' in element.dataset);
  // The pressed CONTROL's own name, or nothing. A worker card is a div with an
  // onclick, so its textContent is the whole card ("amux ⬇ ⋯ ✏Task label (none)
  // ☷Task queue 💻…") and that became the toast (2026-10-01). Text is used only
  // when the element holds no other control and is short enough to be a name.
  const labelOf = element => {
    const named = (element.getAttribute('aria-label') || element.title || '').replace(/\s+/g, ' ').trim();
    if (named) return named.slice(0, 40);
    const text = (element.textContent || '').replace(/\s+/g, ' ').trim();
    const container = element.querySelector(selectors) !== null;
    return !container && text.length <= 40 ? text : '';
  };
  document.addEventListener('click', event => {
    const element = event.target.closest(selectors);
    if (!element || element.closest('#interaction-feedback') || !element.isConnected) return;
    const busy = element.getAttribute('aria-busy') === 'true' && element.dataset.pressLocked === 'true';
    // The quick-repeat rule covers only controls this page has SEEN make a
    // request. The static registry over-approximates (navigation that renders
    // can reach a writer), so applying it to every declared control would eat
    // the second tap of Next/Previous message.
    const doubled = armed?.element === element && !armed.bound && element.dataset.commandObserved === 'true'
      && Date.now() - armed.at < DOUBLE_MS;
    if ((busy || doubled) && isCommand(element)) {
      event.preventDefault();
      event.stopImmediatePropagation();
      diagnostic({verdict:'press_double_suppressed', measured:true, n_considered:1,
        action:element.dataset.action, reason:busy ? 'busy' : 'within_' + DOUBLE_MS + 'ms'});
    }
  }, true);
  for (const eventName of ['click','change','submit']) document.addEventListener(eventName, event => {
    const element = event.target.closest(selectors);
    if (!element || element.closest('#interaction-feedback')) return;
    declare(element);
    if (eventName === 'click' && isCommand(element)) armed = {element, at:Date.now(), bound:false};
    source = element;
    sourceEvent = event;
    element.classList.add('action-responded');
    setTimeout(() => element.classList.remove('action-responded'), 450);
    // Only synchronous dispatch is causal. A later unrelated poll must never
    // borrow the most recently clicked button as its alleged origin.
    // pressSource() is the causal gate; this only drops the reference.
    setTimeout(() => { if (source === element) { source = null; sourceEvent = null; } }, 0);
  }, true);
  function render(receipt) {
    if (receipt && controls.has(receipt.id)) {
      const element = controls.get(receipt.id);
      const active = ['accepted','sending','running'].includes(receipt.phase);
      if (!active) controls.delete(receipt.id);
      const busy = active || [...controls.values()].includes(element);
      element.dataset.interactionPhase = receipt.phase;
      element.setAttribute('aria-busy', String(busy));
    }
    const receipts = interactions.recent(200);
    const pending = receipts.filter(r => !settled.has(r.phase));
    const last = receipts.at(-1);
    status.textContent = pending.length ? pending.length + ' active' : last ? phaseLabels[last.phase] : 'Actions';
    hub.dataset.phase = pending.some(r => ['unknown','blocked'].includes(r.phase)) ? 'blocked' : pending.length ? 'running' : last?.phase || 'idle';
    const visible = [...pending, ...receipts.filter(r => settled.has(r.phase)).slice(-20)].reverse();
    const focused = document.activeElement;
    const focusedReceipt = panel?.classList.contains('active') && list.contains(focused)
      && focused.matches('article > details > summary') ? focused.closest('article').dataset.interactionId : null;
    const expanded = new Set([...list.querySelectorAll('article > details[open]')]
      .map(details => details.parentElement.dataset.interactionId));
    list.replaceChildren();
    if (!visible.length) { const empty = document.createElement('p'); empty.textContent = 'No recent actions'; list.append(empty); }
    for (const item of visible) {
      const row = document.createElement('article');
      row.dataset.interactionId = item.id;
      row.dataset.phase = item.phase;
      const title = document.createElement('strong');
      title.textContent = item.command.label || item.command.kind.replaceAll('.', ' ');
      const target = document.createElement('span');
      target.className = 'interaction-target';
      target.textContent = item.command.target.label || item.command.target.id || '';
      const label = document.createElement('div');
      label.className = 'interaction-status';
      label.textContent = item.feedback.message || phaseLabels[item.phase];
      row.append(title, target, label);
      if (!['GET','HEAD'].includes(item.request?.method) && item.command.kind !== 'filesystem.upload') {
        const effects = document.createElement('div');
        effects.className = 'interaction-effects-status';
        const sync = item.effect_sync;
        effects.textContent = sync?.phase === 'failed' ? 'Changes unavailable; retrying'
          : sync?.phase === 'syncing' ? 'Checking changes'
          : sync?.measured ? item.effects.length + ' recorded changes' + (sync.more ? ' (partial)' : ' at last check')
          : 'Changes not yet checked';
        row.append(effects);
      }
      if (['accepted','sending','running'].includes(item.phase)) {
        const progress = document.createElement('progress');
        progress.setAttribute('aria-label', title.textContent + ' progress');
        if (item.progress?.total > 0) {
          progress.max = item.progress.total;
          progress.value = item.progress.completed;
          const fraction = document.createElement('span');
          fraction.textContent = Math.floor(item.progress.completed / item.progress.total * 100) + '%';
          row.append(fraction);
        }
        row.append(progress);
      }
      if (item.why_unmeasured) { const why = document.createElement('p'); why.textContent = item.why_unmeasured; row.append(why); }
      const explanation = document.createElement('details');
      explanation.open = expanded.has(item.id);
      const explainLabel = document.createElement('summary');
      explainLabel.append(createElement(ChevronDown, {width:14, height:14}), document.createTextNode('Details'));
      const metadata = document.createElement('p');
      metadata.textContent = item.id + (item.acknowledgement?.status ? ' | HTTP ' + item.acknowledgement.status : '') + ' | ' + item.effects.length + ' recorded changes';
      explanation.append(explainLabel, metadata);
      if (item.effect_sync?.error) {
        const error = document.createElement('p'); error.textContent = item.effect_sync.error; explanation.append(error);
      }
      const remedy = item.acknowledgement?.remedy;
      if (remedy) { const text = document.createElement('p'); text.textContent = typeof remedy === 'string' ? remedy : JSON.stringify(remedy); explanation.append(text); }
      row.append(explanation);
      list.append(row);
    }
    if (focusedReceipt && visible.some(item => item.id === focusedReceipt)) {
      const replacement = [...list.querySelectorAll('article')]
        .find(row => row.dataset.interactionId === focusedReceipt)?.querySelector('details > summary');
      if (panel.classList.contains('active') && document.activeElement === document.body)
        replacement?.focus({preventScroll:true});
      const restored = document.activeElement === replacement;
      diagnostic({verdict:restored ? 'interaction_focus_restored' : 'interaction_focus_lost',
        measured:true, n_considered:1, interaction_id:focusedReceipt, restored});
    }
    const retainedExpanded = visible.filter(item => expanded.has(item.id));
    if (retainedExpanded.length) {
      const restored = list.querySelectorAll('article > details[open]').length;
      diagnostic({verdict:restored === retainedExpanded.length ? 'interaction_disclosures_preserved' : 'interaction_disclosures_lost',
        measured:true, n_considered:retainedExpanded.length, restored});
    }
  }
  function bind(element, receipt) {
    const write = !['GET','HEAD'].includes(receipt.request?.method);
    if (write) { element.dataset.commandObserved = 'true'; writes.add(receipt.id); }
    element.dataset.interactionKind = receipt.command.kind;
    element.dataset.targetId = receipt.command.target.id || receipt.request?.path || element.dataset.targetId;
    element.dataset.interactionId = receipt.id;
    controls.set(receipt.id, element);
    if (!outcome.has(element)) outcome.set(element, {worst:null, since:Date.now(), label:labelOf(element)});
    pressBusy(element, true, write);
    // A request that never settles must not leave its button disabled forever.
    setTimeout(() => {
      if (controls.get(receipt.id) !== element) return;
      pressBusy(element, false);
      element.setAttribute('aria-busy', 'false');
      const label = outcome.get(element)?.label;
      globalThis.showToast?.((label ? label + ': ' : '') + 'still waiting on the server; you can try again');
      diagnostic({verdict:'press_busy_released_unsettled', measured:true, n_considered:1,
        action:element.dataset.action, after_ms:STUCK_MS, phase:receipt.phase});
    }, STUCK_MS);
  }
  // Busy: disabled where the element can be (a native disabled control cannot
  // be pressed twice at all), aria-busy everywhere, and the bar from CSS.
  // `data-press-disabled` remembers that WE disabled it, so a handler that
  // disables its own button keeps control of that state.
  // Only press-type controls are disabled: a text field that saves on `change`
  // (Enter) would lose focus and the phone keyboard if it were disabled.
  const pressable = element => element.tagName === 'BUTTON'
    || (element.tagName === 'INPUT' && /^(button|submit|reset|checkbox|radio)$/i.test(element.type));
  function unlock(element) {
    delete element.dataset.pressLocked;
    if (element.dataset.pressDisabled) { element.disabled = false; delete element.dataset.pressDisabled; }
  }
  function pressBusy(element, on, write = true) {
    if (on) {
      element.classList.add('press-busy');
      if (!write) return;
      element.dataset.pressLocked = 'true';
      if (pressable(element) && !element.disabled) { element.disabled = true; element.dataset.pressDisabled = 'true'; }
    } else {
      element.classList.remove('press-busy');
      unlock(element);
    }
  }
  // Can the user still see this control? A closed dialog is often hidden by
  // opacity or visibility, not display (the board editor fades out), and then
  // it keeps a layout box, so getClientRects alone reads a closed dialog's
  // button as visible and the outcome was never shown (AMUX-5417).
  const seen = element => element.isConnected && element.getClientRects().length > 0
    && (typeof element.checkVisibility !== 'function'
      || element.checkVisibility({checkOpacity:true, checkVisibilityCSS:true}))
    && !element.closest('[inert], [aria-hidden="true"], .modal-overlay:not(.active), .board-edit-overlay:not(.active)');
  const RANK = {applied:0, reconciled:0, noop:0, queued:1, unknown:2, refused:3, failed:3};
  // Outcome, once every request the press started has settled.
  function settle(element, receipt) {
    if (writes.delete(receipt.id)
        && ![...controls].some(([id, el]) => el === element && writes.has(id))) unlock(element);
    const state = outcome.get(element) || {worst:null, since:Date.now(), label:labelOf(element), wrote:false};
    // Did this press CHANGE anything? A press whose every request was a read
    // only opened a view, and that view is its outcome (2026-10-01: the peek
    // needs-input chip opened the card and then toasted "<chip>: done" over the
    // answer box it had just opened).
    if (!['GET','HEAD'].includes(receipt.request?.method)) state.wrote = true;
    // A 2xx whose body does not use the ok/applied vocabulary (the board's
    // create answers with the card itself) settles the RECEIPT as 'unknown'.
    // For the PRESS it is accepted: the server took the request. The receipt
    // keeps its honest phase; only the button's outcome reads it as done.
    const status = receipt.acknowledgement?.status;
    const phase = receipt.phase === 'unknown' && status >= 200 && status < 300 ? 'applied' : receipt.phase;
    if (state.worst === null || (RANK[phase] ?? 2) > (RANK[state.worst] ?? 2)) state.worst = phase;
    state.message = ['refused','failed'].includes(receipt.phase) ? receipt.feedback?.message : state.message;
    outcome.set(element, state);
    if ([...controls.values()].includes(element)) return;   // another request from this press is still running
    outcome.delete(element);
    pressBusy(element, false);
    const failed = ['refused','failed'].includes(state.worst);
    const cls = failed ? 'press-failed' : state.worst === 'queued' ? 'press-queued'
      : ['applied','reconciled','noop'].includes(state.worst) ? 'press-done' : null;
    if (cls && element.isConnected) {
      element.classList.remove('press-done','press-failed','press-queued');
      element.classList.add(cls);
      setTimeout(() => element.classList.remove(cls), failed ? 2400 : 1300);
    }
    // A failure is never silent: if the handler has not said anything by
    // SPEAK_MS after the press settled, say what the server said. The wait
    // matters: apiCall toasts the server's sentence only after the response
    // reaches it, which is after this receipt settles. Success gets a toast
    // only when the control is gone or hidden by then (a dialog that closes
    // on Save), because then the green outline was never seen.
    setTimeout(() => {
      const handlerSpoke = lastToastAt >= state.since;
      const toast = globalThis.showToast;
      if (handlerSpoke || typeof toast !== 'function') return;
      if (failed) {
        toast((state.label ? state.label + ': ' : '') + (state.message || 'Not done'));
        diagnostic({verdict:'press_failure_toast', measured:true, n_considered:1, action:element.dataset.action, phase:state.worst});
      } else if (cls === 'press-done' && !seen(element) && !state.wrote) {
        diagnostic({verdict:'press_success_toast_skipped', measured:true, n_considered:1, action:element.dataset.action, reason:'read_only_press'});
      } else if (cls === 'press-done' && !seen(element)) {
        // A success toast is a notification: it needs a name for what was done
        // (a container press has none, and whatever it opened is the outcome)
        // and it obeys the in-app pop-up switch through the shared gate.
        if (!state.label) {
          diagnostic({verdict:'press_success_toast_skipped', measured:true, n_considered:1, action:element.dataset.action, reason:'no_control_label'});
        } else if (globalThis.amuxNotifyAllowed?.('toast') === false) {
          diagnostic({verdict:'press_success_toast_skipped', measured:true, n_considered:1, action:element.dataset.action, reason:'notifications_off'});
        } else {
          toast(state.label + ': done');
        }
      }
    }, SPEAK_MS);
  }
  interactions.subscribe(receipt => {
    // A write the page made on its own is nobody's press. Bound to the armed
    // button, the startup peek-tab seed locked a button that made no request
    // and swallowed half of four quick presses (press-feedback "navigation ...
    // quick repeat presses all run", iOS, run 36978904635).
    if (receipt.phase === 'accepted' && receipt.origin?.actor === 'system') {
      if (armed && !armed.bound) diagnostic({verdict:'press_bind_skipped_background', measured:true,
        n_considered:1, action:armed.element.dataset.action, path:receipt.request?.path});
    } else if (receipt.phase === 'accepted') {
      const pressed = pressSource();
      if (pressed) {
        bind(pressed, receipt);
        if (armed?.element === pressed) armed.bound = true;
      } else if (armed && !armed.bound && armed.element.isConnected && Date.now() - armed.at < ARM_MS) {
        armed.bound = true;
        bind(armed.element, receipt);
        diagnostic({verdict:'press_bound_async', measured:true, n_considered:1,
          action:armed.element.dataset.action, delay_ms:Date.now() - armed.at});
      }
    }
    const element = controls.get(receipt.id);
    render(receipt);
    if (element && !['accepted','sending','running'].includes(receipt.phase)) settle(element, receipt);
  });
  render();
  return {render, source:pressSource, coverage:() => {
    const all = [...document.querySelectorAll(selectors)].filter(e => !e.closest('#interaction-feedback'));
    const commands = all.filter(e => e.dataset.interactionKind || e.dataset.commandObserved || commandHandlers[e.dataset.action]);
    return {measured:true, n_considered:all.length, declared:all.filter(e => e.dataset.action && e.dataset.feedbackRequired && e.dataset.targetId).length,
      declared_command_controls:commands.filter(e => e.dataset.interactionKind).length,
      observed_command_controls:commands.filter(e => e.dataset.commandObserved).length,
      receipts:interactions.recent(200).length,
      missing:commands.filter(e => !e.dataset.action || !e.dataset.interactionKind || !e.dataset.targetId || !e.dataset.feedbackRequired).map(e => e.outerHTML.slice(0,200))};
  }};
}
