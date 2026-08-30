# Mobile bandwidth assessment

Date: 2026-08-22. All numbers measured against the live server (build 399c80b2,
118 session rows, 1,660-card board). Card: AMUX-3502.

## What one mobile client costs today

| Path | Raw | Gzipped | Notes |
|---|---|---|---|
| SSE connect snapshot | 1,060 KB | not compressed | full board (883 KB) + full sessions (177 KB) pushed on every connect |
| Board poll `?archived=0&slim=1` | 718 KB | 176 KB | ETag'd: unchanged board answers 304 with no body |
| `/api/sessions` | 177 KB | 19 KB | no ETag; 118 rows including stopped lanes; preview text is most of each row |
| `index.html` | 234 KB | 51 KB | |
| `app.js` | 1,710 KB | 498 KB | service worker caches it until the APP_VER bump |
| `/api/workers` | 1 KB | 0.6 KB | |

Steady-state SSE when the fleet is quiet is pings (28 B each, every 10 s) plus
sub-KB revisioned `state` events. The cost center is not the steady state. It
is the reconnect: the client declares the stream dead after 18 s of silence,
iOS drops connections on every backgrounding, and each reconnect re-ships the
full 1,060 KB snapshot, uncompressed, because `tower-http`'s CompressionLayer
exempts `text/event-stream`. On an intermittent connection the snapshot is the
bill you pay per flap. Ten flaps in a subway ride is 10 MB.

A second structural fact: the server already emits a modern delta vocabulary
(`{"type":"state"}` with revisions and gap detection), but `connectSSE` in
app.js only handles the legacy vocabulary (`board`, `sessions`, `workers`,
`invalidate`, `ping`). The deltas arrive and are ignored, so the client depends
on full-payload pushes it could do without.

## What is already good

- gzip on every HTTP response (4x to 9x on JSON), negotiated HTTP/2.
- ETag + If-None-Match on the board poll: unchanged polls cost a 304.
- Service worker cache-first for statics; repeat loads skip the 498 KB bundle.
- Offline write queue for board ops and dictation; `_onClientResume` refetches
  only when data is older than 4 s.
- The board default list is slim since AMUX-3496 (7.1 MB down to 986 KB raw).

## Recommendations, ordered by measured impact

1. **Stop shipping snapshots over SSE.** On connect send `hello` with the
   current rev and nothing else; let the client run its existing ETag'd,
   gzipped board fetch. Reconnect cost falls from 1,060 KB raw to about 1 KB
   plus a conditional fetch that is a 304 when nothing changed and 176 KB
   gzipped when something did. This one change is most of the intermittent-
   mobile story. (Filed: AMUX-3503.)
2. **ETag + slim rows on `/api/sessions`.** Same rev-based ETag mechanism the
   board uses; most polls become 304s instead of 19 KB. Gate `preview_lines`
   and `preview` behind a param or drop them for stopped lanes; they are the
   bulk of every row. (Filed: AMUX-3504.)
3. **Move the SPA to the delta vocabulary.** The server already serves
   revisioned `state` events with gap detection; the client should apply
   deltas and use `lagged` to trigger one full refetch. This removes the full
   board re-push on every change for connected clients. Larger change,
   biggest steady-state win on an active fleet.
4. **Brotli precompression + immutable caching for statics.** app.js 498 KB
   gzip would be roughly 380 KB brotli; with content-hashed names (APP_VER
   already exists) it can ship `Cache-Control: immutable`. Per-version cost
   only, so this is polish.
5. **Virtualize or paginate the board list.** 1,660 cards ship on every
   changed poll; a phone renders a fraction of them. The slim diet made each
   card small; the count is the next axis. Defer until 1 through 3 land.

## What intermittent connectivity already survives

Board writes queue offline and replay; dictation flushes on reconnect; the SW
serves the shell offline. The gap is read-side: a flapping connection re-pays
the SSE snapshot every time, which items 1 and 3 remove.
