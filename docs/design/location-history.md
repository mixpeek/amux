# Location history

Ethan, 2026-10-01: "make it so that the amux app tracks my location in the most
accurate best-practice way, using the iOS native capability ... my goal is to
store my full location history (walking, driving, biking, train riding, etc.)
so I can track where I am at any given point and associate it with different
kinds of things. It should be part of the map functionality: on the map
details page there should be a Location history tab in maps."

## What it is

- **The iPhone app records where you are.** It uses Core Location and Core
  Motion, buffers the points on the phone, and uploads them to your own amux
  server in batches.
- **The server keeps every point forever** and works out stops and trips, each
  labelled with how you moved: walking, running, cycling, driving or train.
- **The Map gets a Location history tab.** It shows a day (or a date range) as a
  line coloured by mode, plus a timeline of stops and trips. Tapping a row
  focuses it on the map.
- **Stops and trips have stable ids.** Other things (a board card, a calendar
  event, a note) can point at "where I was then".

The data is Ethan's and stays on his amux server. It is never sent to a third
party, never fed to analytics, and nothing outside the server and the phone
reads it.

## Why it is shaped this way (primitives)

amux has eight primitives: board, workers, schedulers, filesystem, groups,
memories, environment, messages. Location history is not a ninth. It is data
the **Map feature** owns, alongside the pins that already live in
`~/.amux/map.json`, and it is served under the Map's API (`/api/map/location/*`).

It is not stored in `map.json`, because the volume is different: a day of
driving is a few thousand points, and the map document is rewritten whole on
every save. So points get append-only SQLite tables, the same store every
other high-volume amux record uses, with an index on time.

Segmenting (stops, trips, modes) is computed on read, not stored. A better
classifier then improves every past day for free, and nothing has to be
migrated when the rules change (ethos rule: the feature gets better as the
inputs do, instead of freezing an old guess into rows).

## Raw capture first (Ethan, 2026-10-01 22:35)

"The point of location history is to be able to plot out everywhere I've been
and routes and do fun analytics/statistics, so it needs to have raw granular
data capture." So the phone keeps **every fix Core Location delivers**, the
server stores every one of them unchanged, and all cleaning (accuracy limits,
stale fixes, thinning for the map) happens at query time. Analytics can always
go back to the raw rows and be recomputed with better rules.

## iPhone: how it records

- **Every fix is stored.** No thinning on the phone and no dropping of
  low-accuracy or stale fixes. Each point keeps all `CLLocation` fields:
  latitude, longitude, altitude, ellipsoidal altitude, horizontal and vertical
  accuracy, speed and speed accuracy, course and course accuracy, floor,
  timestamp, and `sourceInformation` (simulated by software, produced by an
  accessory). It also records `age_s` (how old the fix was when it arrived)
  and `source` (live updates, manager, significant change).
- **Two modes, a switch in Settings** (default **Full detail**):
  - **Full detail.** iOS 17+: `CLLocationUpdate.liveUpdates(.otherNavigation)`
    held by a `CLBackgroundActivitySession` (and a `CLServiceSession` on iOS
    18). That configuration is navigation-grade accuracy with no distance
    filter, so a moving phone delivers about one fix a second. When you are
    still, iOS 18 marks updates `stationary` and stops sending new fixes, which
    costs no data because nothing moved. iOS 16: `CLLocationManager` with
    `kCLLocationAccuracyBestForNavigation`, `distanceFilter =
    kCLDistanceFilterNone`, background updates on.
  - **Battery saver.** No continuous updates. Only significant-location
    changes (roughly every 500 m or 5 minutes) and visits. Good enough for
    "where was I", not for routes.
  - Both modes keep visits and significant-change monitoring, the two services
    that relaunch an app iOS has terminated.
- **Motion, as its own raw stream.** Every `CMMotionActivity` transition is
  stored separately (time, stationary, walking, running, cycling, automotive,
  unknown, confidence), and each point also carries the activity current when
  it arrived.
- **Delivered equals stored.** The app counts fixes delivered by Core Location
  and fixes written to the buffer; the two must be equal, and both are shown in
  Settings and sent with the status so a gap is visible.
- **Buffer and upload:** points go to a JSON-lines file in Application Support
  before any upload, leave it only when the server confirmed them, and upload
  1000 at a time. Ingest is idempotent by point id.
- **Energy (2026-10-31).** Uploads go every 60 s while amux is on screen, every
  5 minutes while it records in the background, at once at 1000 waiting, on
  backgrounding, on coming to the foreground and on visit wake-ups. A failed
  upload backs off (1, 2, 4 ... minutes, capped at 30) instead of retrying on
  every fix. iOS 16 standard updates and the iOS 17+ stall backstop let iOS
  pause them while the phone is still; the next significant-change or visit
  wake-up restarts them. Battery saver runs no live motion stream: each
  wake-up asks the motion coprocessor for its history since the last stored
  transition. The 60 s health timer has a 15 s tolerance. Turning recording
  off stops every location and motion service, the background session and the
  timer. Each of these logs under `io.amux.app` / `Location`, and `status`
  carries `upload_failures`, `next_upload_s`, `paused_when_still` and
  `motion_source`.

### Battery, honestly

Not measured on a device yet; these are estimates from how iOS behaves.

- **Full detail:** while moving, roughly what a navigation app costs: about
  5 to 10% of battery per hour of continuous movement. While still, close to
  nothing, because iOS stops delivering fixes. A typical day with 1 to 2 hours
  of travel: about 10 to 20% extra.
- **Battery saver:** about 1 to 2% a day.

The Settings screen states this next to the switch.

### Permissions and controls

- Usage strings say exactly what happens:
  - `NSLocationWhenInUseUsageDescription`,
  - `NSLocationAlwaysAndWhenInUseUsageDescription`,
  - `NSMotionUsageDescription`,
  - `NSLocationTemporaryUsageDescriptionDictionary` (key `history`).
- `UIBackgroundModes` gains `location`.
- Flow: turning recording on asks When In Use, then Always, then Motion.
- Controls: Settings > Location history (on/off, Full detail or Battery saver,
  permission state, fixes delivered and stored, waiting to upload, last
  upload, Upload now). The dashboard tab shows the same status through the
  `amuxLocation` web view bridge on the iPhone.

## Server

Raw tables are **append-only and never thinned**. Migrations 0099 and 0100:

```
location_points(id PK, device, ts, lat, lon, alt, ell_alt, h_acc, v_acc,
                speed, speed_acc, course, course_acc, floor, simulated,
                accessory, age_s, activity, activity_conf, source, received)
  indexes: (ts), (device, ts)
location_motion(id PK, device, ts, stationary, walking, running, cycling,
                automotive, unknown, confidence, received)
  indexes: (ts), (device, ts)
location_visits(id PK, device, arrival, departure, lat, lon, h_acc, received)
  index: (arrival)
```

All timestamps are epoch seconds (declared in `TIMESTAMP_COLUMNS`).

### Size

A raw row is about 250 bytes including both indexes. At one fix a second:

| Activity | Fixes per hour | Storage per hour |
|---|---|---|
| Driving | 3,600 | about 0.9 MB |
| Walking | 3,600 | about 0.9 MB |
| Still | near 0 | near 0 |

A day with 3 hours of movement is about 11,000 fixes, roughly 2.7 MB; a year
of that is about 1 GB. A one-day query reads about 11,000 rows through the
`(ts)` index, which SQLite does in tens of milliseconds.

### Cleaned view (query time only)

The map, timeline and stats read a cleaned view of the raw rows: fixes with
invalid or worse-than-100 m accuracy, fixes older than 30 s on arrival, and
simulated fixes are left out (the raw rows keep them). Raw export
ignores the cleaning.

### Routes

| Route | What it does |
|---|---|
| `POST /api/map/location/points` | Raw batch ingest, idempotent by id. Rejects only what cannot be a fix at all (missing id, impossible coordinates or time). |
| `POST /api/map/location/motion` | Raw Core Motion transitions, idempotent by id. |
| `POST /api/map/location/visits` | iOS visits. |
| `GET /api/map/location/timeline?from=&to=` | Stops and trips over the cleaned view, with `measured`, `n_considered` and `n_raw`. |
| `GET /api/map/location/segments/{id}` | One stop or trip by stable id. |
| `GET /api/map/location/stats?from=&to=&bucket=day\|week\|month` | Analytics (below). |
| `GET /api/map/location/heatmap?from=&to=` | Everywhere ever been, as counted grid cells. |
| `GET /api/map/location/export?from=&to=&format=geojson\|gpx\|csv` | The raw points, every field. |
| `GET /api/map/location/summary` | Totals, first and last point, devices. |
| `GET /api/map/location/points?from=&to=` | Raw points as JSON. |
| `DELETE /api/map/location/points/range?from=&to=&confirm=delete` | Delete a range. |

Authentication: writes need the owner bearer (the loopback shortcut does not
count); reads follow dashboard auth.

### Segmenting

- **Stops:** fixes within 100 m of an anchor for 5 minutes or more, trimmed of
  moving fixes at either end, merged with iOS visits.
- **Trips:** runs of fixes between stops.
- **Mode:** time-weighted motion activity, with speed as fallback. **Train is
  inferred and says so** (straight, 8 to 45 m/s, station-like dwells).
- **Stable ids:** `stop_<first point id>`, `trip_<first point id>`.

### Analytics (first set)

All over the cleaned view, every answer with `measured` and `n_considered`:

- distance and moving time per mode per day, week or month;
- top places by time spent (stops grouped into about 150 m places);
- new places: places first visited inside the range;
- longest trip;
- average speed per mode;
- heatmap of everywhere ever been (grid cells about 50 m wide, with counts).

The place grouping is deliberately simple (a fixed grid) and lives in one
function, so a better clustering can replace it without touching storage.

## Dashboard: Map > Location history

- **Day:** day picker, a line per trip coloured by mode, stops, start and end
  markers, a timeline of stops and trips; tapping a row zooms to it.
- **Stats:** period (week, month, year, all time), distance and time per mode,
  top places, new places, longest trip, average speeds.
- **Heatmap:** a layer of everywhere ever been.
- **Export:** raw GeoJSON, GPX or CSV for the day or range.
- On the iPhone app: recording switch, Full detail or Battery saver, and the
  delivered/stored counts.
- Mobile first: 44 px targets, works at 375 px, both themes.
- **One leak to know about:** the Map's base tiles come from OpenStreetMap, as
  they already do for pins. Viewing a day tells the tile server which map
  squares you looked at, not your points or times.

## Ethan: App Store

TestFlight internal testing needs no App Review, so the build that ships with
this goes straight to TestFlight. If the app is later submitted to the App
Store, these are the answers.

**App Review notes (paste as-is):**

> amux is a personal dashboard that connects to a server the user runs
> themselves. With the user's permission, the app records the user's location
> history in the background (Settings > Location history, off by default) and
> uploads it only to that user's own server, where it is shown on a map
> timeline of the user's own trips and stops. Location is never shared with
> third parties or used for advertising or tracking. Background location is
> required because the feature records the full history of where the user goes,
> including while the app is closed. To review: turn on Settings > Location
> history in the app, grant Always, and walk or drive; the Map > Location
> history tab shows the recorded day.

**App privacy label answers:**
- Data type: Precise Location. Collected: yes.
- Linked to the user's identity: yes (it is their own account on their own
  server).
- Used for tracking (Apple's definition): no.
- Purpose: App Functionality.
- Shared with third parties: no.
- Also add: Other usage data is not collected. Motion activity is used on
  device to label points and is uploaded with them, so declare it under
  "Fitness" only if Apple asks; it is part of the location record.

**Developer portal:** nothing to change. Background location and Core Motion
need Info.plist keys, not an App ID capability or entitlement.

**On the phone, after installing the TestFlight build:**
1. Open amux, open Settings (long-press the screen or "Switch Server"), turn on
   **Location history**.
2. Allow location **While Using**, then **Change to Always Allow** when iOS
   asks.
3. Allow **Motion & Fitness**.
4. iOS Settings > amux > Location: make sure **Precise Location** is on.
5. Do not swipe amux away in the app switcher: a force-quit stops background
   tracking until the app is opened again (an iOS rule).
