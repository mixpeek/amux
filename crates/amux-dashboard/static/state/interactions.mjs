import { effectsDue, withDeadline } from './effects.mjs';

export const phases = Object.freeze(['accepted', 'queued', 'sending', 'running', 'waiting', 'blocked', 'applied', 'noop', 'refused', 'failed', 'reconciled', 'unknown']);
export const settled = new Set(['applied', 'noop', 'refused', 'failed', 'reconciled']);
export const phaseLabels = Object.freeze({accepted:'Accepted', queued:'Queued', sending:'Sending', running:'Running', waiting:'Waiting', blocked:'Needs attention', applied:'Completed', noop:'No change', refused:'Not applied', failed:'Failed', reconciled:'Confirmed', unknown:'Outcome unknown'});
const copy = value => JSON.parse(JSON.stringify(value));
const id = () => 'int_' + globalThis.crypto.randomUUID();

// Transport declarations are independent of outbox policy: non-queueable commands
// (uploads, live controls, auth) still owe feedback and a receipt.
export function commandFor(method, pathname) {
  const segments = pathname.split('/').filter(Boolean);
  const domain = segments[1] || 'unknown';
  const primitive = ({sessions:'worker', workers:'worker', board:'board', schedules:'scheduler',
    fs:'filesystem', file:'filesystem', files:'filesystem', upload:'filesystem', uploads:'filesystem',
    groups:'group', tags:'group', memories:'memory', memory:'memory', messages:'message'})[domain] || 'environment';
  const verb = segments.at(-1);
  const specific = domain === 'sessions' && ['send', 'steer', 'start', 'stop'].includes(verb);
  return {id:id(), kind:specific ? (['send','steer'].includes(verb) ? 'message.' : 'worker.') + verb : primitive + '.' + method.toLowerCase(),
    target:{primitive:specific && ['send','steer'].includes(verb) ? 'message' : primitive, id:segments[2] ? decodeURIComponent(segments[2]) : domain}};
}

export function classify(status, body = {}, locallyQueued = false) {
  if (locallyQueued) return {phase:'queued', message:'Queued on this device'};
  if (status >= 400) return {phase:status < 500 ? 'refused' : 'failed', message:body.error || body.message || `Request failed (${status})`};
  if (body.ok === false || body.error || body.ignored_fields?.length) return {phase:'refused', message:body.error || (body.ignored_fields?.length ? 'Not applied: ignored ' + body.ignored_fields.join(', ') : body.message || 'Not applied')};
  if (body.phase && phases.includes(body.phase)) return {phase:body.phase, message:body.message};
  if (body.queued || body.submission === 'deferred') return {phase:'queued', message:'Queued by server'};
  if (status === 202) return {phase:'running', message:'Accepted; awaiting completion'};
  if (body.applied === false || body.deduped === true) return {phase:'noop', message:body.deduped ? 'Already accepted' : 'No change'};
  if (body.applied === true || body.ok === true || body.submitted === true) return {phase:'applied', message:'Completed'};
  return {phase:'unknown', message:'Response received; outcome unconfirmed'};
}

export function createInteractions({storage, now = Date.now, diagnostic = () => {}, max = 200} = {}) {
  const key = 'amux_interactions_v2';
  let receipts = [];
  const listeners = new Set();
  try {
    const saved = JSON.parse(storage?.getItem(key) || '[]');
    if (Array.isArray(saved)) receipts = saved.filter(r => r?.id && phases.includes(r.phase) && r.command?.kind && Array.isArray(r.effects));
    // A reload is not proof that an in-flight command finished.
    receipts = receipts.map(r => ['sending','accepted','running'].includes(r.phase)
      ? {...r, phase:'unknown', measured:false, why_unmeasured:'Page reloaded before completion was observed'} : r);
    receipts = receipts.map(r => r.effect_sync?.phase === 'syncing'
      ? {...r, effect_sync:{...r.effect_sync, phase:'pending', measured:false, next_attempt_at:0}} : r);
  } catch (error) { diagnostic({verdict:'receipt_restore_failed', error:String(error), measured:true, n_considered:1}); }
  function changed(receipt) {
    const unresolved = receipts.filter(r => !settled.has(r.phase));
    const slots = Math.max(0, max - unresolved.length);
    const complete = slots ? receipts.filter(r => settled.has(r.phase)).slice(-slots) : [];
    receipts = [...unresolved, ...complete].sort((a,b) => a.created_at - b.created_at);
    try { storage?.setItem(key, JSON.stringify(receipts)); }
    catch (error) { diagnostic({verdict:'receipt_persistence_failed', error:String(error), measured:true, n_considered:receipts.length}); }
    for (const listener of listeners) listener(copy(receipt));
    return copy(receipt);
  }
  function update(receiptId, patch) {
    const receipt = receipts.find(r => r.id === receiptId);
    if (!receipt) return null;
    if (patch.phase && !phases.includes(patch.phase)) throw new Error('Unknown interaction phase: ' + patch.phase);
    Object.assign(receipt, patch, {updated_at:now()});
    const severity = ['failed','refused'].includes(receipt.phase) ? 'error' : ['unknown','blocked'].includes(receipt.phase) ? 'warning' : settled.has(receipt.phase) ? 'success' : 'info';
    receipt.feedback = {required:true, persistence:['error','warning'].includes(severity) ? 'durable' : 'until-settled', severity, message:phaseLabels[receipt.phase], ...patch.feedback};
    if (['unknown','failed','refused','blocked'].includes(receipt.phase)) diagnostic({kind:'interaction-receipt', verdict:receipt.phase, interaction_id:receipt.id, command_kind:receipt.command.kind, status:receipt.acknowledgement?.status, measured:true, n_considered:1});
    return changed(receipt);
  }
  const api = {
    accept({command, request, origin = {actor:'human', surface:'dashboard'}, feedback, interactionId}) {
      if (interactionId && receipts.some(r => r.id === interactionId)) return api.get(interactionId);
      const receipt = {id:interactionId || id(), command, origin, request, phase:'accepted',
        feedback:{required:true, persistence:'until-settled', severity:'info', message:'Accepted', ...feedback},
        effects:[], acknowledgement:{}, measured:true, n_considered:1, created_at:now(), updated_at:now()};
      receipts.push(receipt);
      return changed(receipt);
    },
    update,
    get: receiptId => { const receipt = receipts.find(r => r.id === receiptId); return receipt ? copy(receipt) : null; },
    recent: (count = 50) => copy(receipts.slice(-Math.max(1, count))),
    subscribe(listener) { listeners.add(listener); return () => listeners.delete(listener); },
    effectStatus(receiptId, patch) {
      const receipt = receipts.find(r => r.id === receiptId);
      if (!receipt) return;
      receipt.effect_sync = {...receipt.effect_sync, ...patch};
      return changed(receipt);
    },
    effect(receiptId, effect) {
      const receipt = receipts.find(r => r.id === receiptId);
      if (!receipt) return;
      if (effect.id && receipt.effects.some(e => e.id === effect.id)) return;
      receipt.effects.push({...effect, id:effect.id || id(), interaction_id:receiptId});
      changed(receipt);
    },
    async acknowledge(receiptId, response) {
      const status = response.status;
      let body = {};
      if (response.headers.get('content-type')?.includes('json')) {
        try { body = await response.clone().json(); }
        catch (_) { return update(receiptId, {phase:'unknown', measured:false, why_unmeasured:'Invalid acknowledgement JSON', acknowledgement:{status}}); }
      }
      if (!body || typeof body !== 'object') body = {};
      const read = ['GET','HEAD'].includes(api.get(receiptId)?.request?.method);
      const result = read && response.ok && !body.error
        ? {phase:'reconciled', message:'Loaded'}
        : classify(status, body, response.headers.get('X-Amux-Outbox') === 'queued');
      const acknowledgement = {status, locally_queued:response.headers.get('X-Amux-Outbox') === 'queued', applied:body.applied, rev:body.rev || body.global_rev, version:body.version, ignored_fields:body.ignored_fields || [], error:body.error, remedy:body.fix || body.remedy || body.how_to_ack};
      // Effects come from authority, never invented from HTTP success or a rev.
      for (const effect of body.effects || []) api.effect(receiptId, effect);
      const measured = body.measured !== false && result.phase !== 'unknown';
      if (measured && api.get(receiptId)?.measured === false) diagnostic({
        verdict:'interaction_measurement_recovered', interaction_id:receiptId, measured:true, n_considered:1});
      return update(receiptId, {phase:result.phase, acknowledgement, measured,
        why_unmeasured:measured ? undefined : body.why_unmeasured || 'Acknowledgement did not confirm the outcome',
        feedback:{message:String(result.message || phaseLabels[result.phase])}});
    },
    expire(age = 120000) {
      for (const receipt of [...receipts]) {
        if (['accepted','sending','running'].includes(receipt.phase) && now() - receipt.updated_at > age) {
          update(receipt.id, {phase:'unknown', measured:false, why_unmeasured:'Completion has not been observed; inspect before retrying'});
        }
      }
    },
  };
  return api;
}

export function createInteractionPoller({interactions, read, reconcile, diagnostic, limit = 8, timeoutMs = 6000, now = Date.now}) {
  let queue = [];
  let flight = false;
  return async function poll() {
    if (flight) return;
    const pending = interactions.recent(Infinity).filter(r => (!settled.has(r.phase) || effectsDue(r, now()))
      && r.command.kind !== 'filesystem.upload' && !['GET','HEAD'].includes(r.request?.method)
      && !(r.phase === 'queued' && r.acknowledgement?.locally_queued));
    const eligible = new Set(pending.map(r => r.id));
    queue = queue.filter(id => eligible.has(id));
    const scheduled = new Set(queue);
    for (const receipt of pending) if (!scheduled.has(receipt.id)) queue.push(receipt.id);
    // Rotate the whole backlog, not just the most recent history page. New
    // arrivals join behind existing work, and a failed read cannot block peers.
    const batch = queue.splice(0, limit);
    queue.push(...batch);
    flight = true;
    try {
      for (const id of batch) {
        try {
          const before = interactions.get(id);
          if (!before) continue;
          if (settled.has(before.phase)) {
            await withDeadline(() => reconcile(id), timeoutMs);
            continue;
          }
          const server = await withDeadline(signal => read(id, signal), timeoutMs);
          const current = interactions.get(id);
          if (!phases.includes(server?.phase)) throw new Error('Invalid interaction status response');
          if (!current || current.phase !== before.phase || current.updated_at !== before.updated_at
            || JSON.stringify(current.acknowledgement) !== JSON.stringify(before.acknowledgement) || settled.has(current.phase)) continue;
          const acknowledgement = server.acknowledgement || current.acknowledgement;
          const changed = server.phase !== current.phase || server.measured !== current.measured
            || server.why_unmeasured !== current.why_unmeasured
            || JSON.stringify(acknowledgement) !== JSON.stringify(current.acknowledgement)
            || Object.entries(server.feedback || {}).some(([key,value]) => current.feedback[key] !== value);
          if (changed) interactions.update(id, {phase:server.phase, measured:server.measured,
            why_unmeasured:server.why_unmeasured, feedback:server.feedback, acknowledgement});
          await withDeadline(() => reconcile(id), timeoutMs);
        } catch (error) {
          diagnostic({verdict:'interaction_status_poll_failed', interaction_id:id,
            error:String(error), measured:true, n_considered:1, pending_count:pending.length});
        }
      }
    } finally { flight = false; }
  };
}
