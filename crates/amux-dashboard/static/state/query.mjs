import { QueryClient } from '@tanstack/query-core';

export function createQueries() {
  const client = new QueryClient({defaultOptions:{queries:{retry:false, staleTime:0, gcTime:300000}}});
  return {
    client,
    get:key => client.getQueryData(key),
    set:(key, value) => client.setQueryData(key, value),
    state:key => client.getQueryState(key),
    // Response bodies are single-use: each concurrent reader gets its own clone.
    async response(key, fetcher) {
      const response = await client.fetchQuery({queryKey:key, queryFn:fetcher});
      return response.clone();
    },
    invalidate: keys => Promise.all(keys.map(key => client.invalidateQueries({queryKey:Array.isArray(key) ? key : [key]}))),
  };
}
