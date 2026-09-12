export function invalidationKeys(event) {
  const keys = new Set(event.keys || []);
  if (keys.has('messages')) keys.add('history');
  return [...keys];
}

export function createSync({query, fetchSync, refresh}) {
  let revision = 0;
  let flight;
  return {
    revision: () => revision,
    async invalidate(event) {
      const keys = invalidationKeys(event);
      await query.invalidate(keys);
      return keys;
    },
    catchUp() {
      if (flight) return flight;
      flight = (async () => {
        const data = await fetchSync(revision);
        if (data.full_sync_required || data.full_required) {
          await query.invalidate(['board','sessions','messages','history']);
          await refresh();
        } else {
          const keys = new Set();
          for (const event of data.events || []) {
            const type = event.entity_type?.kind || event.entity_type;
            keys.add(({task:'board',worker:'sessions',message:'messages'})[type] || type);
          }
          await query.invalidate([...keys].filter(Boolean));
          if (keys.size) await refresh();
        }
        // The response rev is the journal head, not the end of a capped page.
        // Refreshing authority covers a truncated window; advancing blindly
        // would silently skip events after the page limit.
        if (data.more) {
          await query.invalidate(['board','sessions','messages','history']);
          await refresh();
        }
        revision = Math.max(revision, Number(data.rev || data.current_rev || 0));
        return data;
      })().finally(() => { flight = null; });
      return flight;
    },
  };
}
