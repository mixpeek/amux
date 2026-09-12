export async function withDeadline(operation, timeoutMs = 5000) {
  const controller = new AbortController();
  let timer;
  try {
    return await Promise.race([
      Promise.resolve().then(() => operation(controller.signal)),
      new Promise((_, reject) => {
        timer = setTimeout(() => {
          const error = new Error('Interaction read timed out');
          controller.abort(error);
          reject(error);
        }, timeoutMs);
      }),
    ]);
  } finally { clearTimeout(timer); }
}

export function effectsDue(receipt, now = Date.now()) {
  return (receipt.effect_sync?.next_attempt_at || 0) <= now;
}

export function createEffectReconciler({interactions, read, diagnostic, now = Date.now,
  timeoutMs = 5000, refreshMs = 60000, retryMs = 5000, maxRetryMs = 300000}) {
  const flights = new Map();
  return function reconcile(id) {
    if (flights.has(id)) return flights.get(id);
    const receipt = interactions.get(id);
    if (!receipt || !effectsDue(receipt, now())) return Promise.resolve();
    const previous = receipt.effect_sync || {};
    interactions.effectStatus(id, {phase:'syncing', measured:false, n_considered:0,
      next_attempt_at:now() + timeoutMs, error:undefined});
    const flight = (async () => {
      try {
        // The deadline includes fetching headers AND consuming the response body.
        const data = await withDeadline(signal => read(id, signal), timeoutMs);
        if (data?.measured !== true || !Array.isArray(data.effects)
          || !Number.isInteger(data.n_considered) || data.n_considered < 0) {
          throw new Error(data?.why_unmeasured || 'Invalid effects measurement');
        }
        for (const effect of data.effects) interactions.effect(id, effect);
        interactions.effectStatus(id, {phase:'synced', measured:true, n_considered:data.n_considered,
          more:!!data.more, checked_at:now(), next_attempt_at:now() + refreshMs, failures:0, error:undefined});
        if (previous.failures) diagnostic({verdict:'interaction_effects_recovered', interaction_id:id,
          measured:true, n_considered:data.n_considered, failed_attempts:previous.failures});
      } catch (error) {
        const failures = (previous.failures || 0) + 1;
        const retryIn = Math.min(maxRetryMs, retryMs * 2 ** Math.min(failures - 1, 16));
        interactions.effectStatus(id, {phase:'failed', measured:false, n_considered:0,
          failures, error:String(error), next_attempt_at:now() + retryIn});
        diagnostic({verdict:'interaction_reconcile_failed', interaction_id:id,
          error:String(error), measured:false, n_considered:0, attempts:failures, retry_in_ms:retryIn});
        throw error;
      } finally { flights.delete(id); }
    })();
    flights.set(id, flight);
    return flight;
  };
}
