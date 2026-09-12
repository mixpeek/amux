export function createStore(initial) {
  let state = initial;
  const listeners = new Set();
  return {
    getState: () => state,
    setState(patch) {
      const next = {...state, ...(typeof patch === 'function' ? patch(state) : patch)};
      if (Object.keys(next).every(key => Object.is(next[key], state[key]))) return;
      state = next;
      for (const listener of listeners) listener(state);
    },
    subscribe(listener) { listeners.add(listener); return () => listeners.delete(listener); },
    select(selector, listener) {
      let previous = selector(state);
      const subscriber = value => {
        const next = selector(value);
        if (!Object.is(previous, next)) { previous = next; listener(next); }
      };
      listeners.add(subscriber);
      return () => listeners.delete(subscriber);
    },
  };
}
