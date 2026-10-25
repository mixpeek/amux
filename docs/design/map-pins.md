# Map pins: storage, status, visits, places, lists

2026-10-20. Ethan asked for eight improvements to managing pins and locations in
the Map tab ("do em all and verify e2e"). This is how they are built.

## Storage

Pins moved from `~/.amux/map.json` (one document rewritten on every save) into
SQLite (migration 0128, `api/map_store.rs`):

- `map_pins`: one row per pin. The pin stays a JSON document (`doc`) with
  `lat`, `lng` and a `rev` beside it.
- `map_pin_history`: every create, update, delete and merge, with who did it
  (`dashboard`, `worker:<name>`, `location-history`, `google-places`,
  `import:map.json`) and the before and after copies.
- `map_meta`: groups (`tags`), `settings`, `trials`, saved `lists`, and
  categories (`tagGroups`).
- `map_shares`: tokens for read-only links to a saved list.

On the first read after the upgrade, map.json is imported once (log verdict
`map_imported`) and the file is left where it is as the backup.

A save sends the pin's `rev`. If another device changed the pin since, the
server answers 409 with the current pin and the dashboard shows that copy
instead of overwriting it. The old whole-document `POST /api/map` still works
for older dashboards; it is applied pin by pin.

## Pin fields

| field | meaning |
|---|---|
| `status` | `want`, `been`, `favorite`, `closed`, or empty |
| `myRating` | the owner's own rating, 1 to 5 |
| `rating`, `ratingCount` | Google's rating, saved when a pin is added from search |
| `visits` | `{count, first, last}` matched from location history |
| `placeId`, `place` | the linked Google place: business status, rating, hours, last refresh |
| `eventAt`, `calEventId` | a dated pin and the calendar event it owns |

## What each feature does

1. **Status and rating**: set in the pin editor or in bulk; filter chips for
   each status; sort by rating or by last visit. Map markers: a closed place is
   grey, a favorite has a gold outline.
2. **"Been" from location history**: a phone-reported visit of at least 10
   minutes within 60 m of a pin counts. A pin with no status or `want` becomes
   `been` (`statusSource: visits`); a status set by hand is never changed. Runs
   after every visits upload and on `POST /api/map/visits/match` (log verdict
   `map_visits_matched`).
3. **Google places**: `POST /api/map/places/link` finds a pin's place with a
   text search near it, accepted only for a similar name within 150 m (or
   anything within 25 m). `POST /api/map/places/refresh` updates linked pins,
   oldest first; a permanently closed place turns the pin `closed`. Each pin is
   one billed Google request, and the response reports `google_calls`. Nothing
   runs on a schedule: a weekly refresh would spend money every week, so that
   is the owner's call.
4. **Categories**: a group can belong to a category (Cuisine, Vibe, Who). A
   filter then matches any selected group within a category and every category
   at once. Without categories the old AND/OR switch still applies.
5. **Table and bulk edits**: the table view selects pins and sets status or
   rating, adds or removes a group, merges two, or deletes
   (`POST /api/map/pins/bulk`, `POST /api/map/pins/merge`).
6. **Duplicates**: `GET /api/map/duplicates` lists pairs within 40 m with
   similar names, the same Google place, or within 10 m. Merging combines
   groups and notes and keeps the better status and rating. "Not a duplicate"
   hides a pair.
7. **Saved lists and sharing**: a list is a saved filter. Sharing creates
   `/s/map/<token>` (and `/api/share/map/<token>`), readable without signing
   in, showing name, description, groups, status, rating and hours. Notes,
   visits and history never leave through a share. Turning the link off makes
   it 404. On an amux Cloud server the door signs in every request, so shared
   links only work on a server reachable without the door.
8. **Agents and the calendar**: `amux map ls|add|set|visits|dups|merge` (see
   `amux help`). `amux map add "Name" place="search text" group=Coffee
   status=want` places a pin by search. A pin with `eventAt` (`at=` on the
   CLI, the Date field in the editor) owns one calendar event that moves,
   renames and disappears with it.

## Tests

- `map_store.rs` unit tests: import once, stale edit refused, history,
  whole-document guard, visits, duplicates and merge, place matching, closed
  places, categories, shares without notes, calendar events.
- `e2e/map-manage.spec.ts` (desktop, mobile, iOS Safari): editor fields reach
  the server and calendar, a stale editor does not overwrite, categories
  filter, table bulk edit and merge, duplicates merge, a shared list seen by a
  signed-out visitor and then turned off, and visits marking a pin "been".
