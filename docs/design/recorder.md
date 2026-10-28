# The recorder: durable, passive, native where it can be

Owner directive MSG-98487 (2026-10-27): make recording reliable, unobtrusive and
low-overhead enough to forget it is running, benchmarked against Granola. Priority:
reliability, durability, low overhead, background operation, UX, then advanced
features. This file records what existed, how it failed, and what replaces it.

## What existed (audit, 2026-10-27)

Every capture path was the web page's `MediaRecorder` on the microphone. No native
capture, no system audio.

| Path | Capture | Where the bytes went |
|---|---|---|
| Record tab | 1 s chunks, AAC or Opus at 32 kbps | IndexedDB per chunk as it arrives; after stop, ONE `POST /api/recordings/upload` of the whole file |
| Dictation | one blob at stop (no timeslice) | RAM until stop, then an IndexedDB outbox, then `POST /api/dictate` |
| Converse | 1 s chunks in a JS array | RAM; uploaded only after the transcript saved |
| Teleprompter | 1 s video chunks in a JS array | RAM |

Server: the recordings folder is the store (`api/recordings.rs`): `<base>.<ext>`,
`<base>.json` sidecar, transcript files. Transcription is a background job
(whisper.cpp locally, or Gemini), decoupled from capture.

### Failure modes

1. A long recording could never finish uploading: one POST, a 300 s client abort
   that did not count as a failed try, so it retried forever. The server buffered up
   to 1 GB in RAM.
2. No gap detection: a failed chunk write set a flag; upload concatenated whatever
   chunks existed.
3. Two windows interfered: a second window's recovery finalized the first window's
   live recording as a partial, and the real upload then hit a 409.
4. Closing a Mac app window tore the page down mid-recording; quitting ignored a live
   recording. A recording orphaned that way stayed `recording` until a reload.
5. Dictation and Converse audio lived only in RAM until stop.
6. The dictation outbox kept the newest 25 entries whatever their state, and
   swallowed storage errors.
7. Gemini transcription failed every recording over about an hour (18 MB inline cap)
   and a crash mid-transcription used up one of three attempts.
8. iOS had no background audio mode, so hiding the app ended the recording.
9. No sleep/wake or audio device change handling, no menu bar indicator, no system
   audio, no meeting detection.

## The design

One rule carries it: **audio is on disk, in finished pieces, before anything else
happens to it.** Upload, assembly and transcription are later stages that read those
pieces and can fail, retry or wait for a network without touching capture.

### Pieces and the session manifest

A recording is a SESSION: an id, a start time, one or more TRACKS (`mic`, `system`),
and per track a sequence of numbered CHUNKS. Two chunk kinds:

- `segment`: an independently decodable file (native capture: 10 s AAC `.m4a`).
  Assembly concatenates them with ffmpeg.
- `bytes`: a fragment of one continuous stream (the browser's `MediaRecorder`).
  Assembly joins the bytes in order.

The manifest lists every chunk with its sequence number, start offset, duration,
size and sha256, plus every GAP the recorder detected (sleep, device change, a
dropped buffer, a write failure) with its start and length. A missing sequence number
is a gap by construction, so nothing can be silently skipped.

### Server: resumable sessions (`api/recordings.rs`)

- `POST /api/recordings/sessions` opens or resumes a session.
- `PUT /api/recordings/sessions/{id}/chunks/{track}/{seq}` writes one chunk to
  `<folder>/.sessions/<id>/` atomically (temp file, then rename). Idempotent by sha;
  different bytes under the same seq are refused.
- `GET /api/recordings/sessions/{id}` returns what the server holds, so a client
  resumes by sending only what is missing.
- `POST /api/recordings/sessions/{id}/finish` checks the sequence is contiguous
  (answering 409 with the missing list otherwise, unless the client declares them
  lost), assembles the tracks, writes `<base>.<ext>` and the sidecar through the same
  code the one-shot upload uses, records gaps in the sidecar, and queues
  transcription.
- A session idle past `AMUX_RECORDING_ABANDON_H` (default 6 h) with no finish is
  assembled as `partial` with what arrived. Never discarded.

The old `POST /api/recordings/upload` stays for older clients.

### Browser recorder (Record tab)

Keeps IndexedDB as the device copy, and now streams each chunk to the server as it
is written. A chunk leaves IndexedDB only after the server acknowledged its sha.
Each window holds a lease (a heartbeat in IndexedDB); recovery adopts only a
recording whose lease went stale.

### Native Mac recorder (`desktop/amux-recorder.swift`)

Owned by the app delegate, not a window, so closing, hiding or reloading windows
does not touch it.

- Mic: `AVAudioEngine` input tap. System audio: ScreenCaptureKit audio-only stream
  (macOS 13+, weak-linked; the app still runs on 12 with mic only).
- Writer: one serial queue per track, 10 s AAC segments written to
  `~/Library/Application Support/Amux/Recordings/<id>/`, `.part` renamed on close,
  manifest rewritten atomically after each segment. A crash loses at most the open
  segment.
- Bounded queue: if the writer falls more than 5 s behind, buffers are dropped AND
  recorded as a gap. Never silent.
- Continuity: each buffer's host time is checked against the frames written; a jump
  over 250 ms is a gap.
- Sleep/wake and device changes close the segment, record the gap, and restart the
  engine.
- Shipper: uploads finished segments to the session API with backoff, offline-safe;
  deletes the local spool only after `finish` confirms every sha.
- Recovery: on launch, any spool without a finish is shipped and finished as
  `recovered`.
- Menu bar: a red dot with elapsed time while recording; Pause, Resume, Stop, Open.
- Quit while recording asks first.
- The Record tab uses it through the `amuxRecorder` script message handler when the
  page runs in the app, and its own `MediaRecorder` everywhere else.

### Transcription

Long audio is split into 10-minute pieces for Gemini, with timestamps offset back.
A crash during transcription no longer spends an attempt.

### Live transcription (2026-10-28, `api/recording_live.rs`)

On by default (Settings in the Record tab). While a session is open, the
`recordings-live` job (every 5 s) takes each track's newly arrived audio in
windows of about 15 s and transcribes it with the same engine as the finished
transcript (local whisper.cpp here, Gemini where that is the engine). Each
window lands in `<session>/live.json` and the readable `live.md` before the next
one starts, so a crash of the app, the device or the server keeps every word up
to that point. The Record tab shows the words as they arrive. When the session
is filed, the live pieces travel in its sidecar (`live`) and the finished
transcript is those pieces plus only the untranscribed tail: progressive, never
all at the end. Two tracks are labelled You (microphone) and Them (the
computer's sound). Verdicts: `recording_live_transcribed`, `recording_live_failed`,
`recording_live_unavailable`, `recording_transcript_from_live`.

### Notes, filing, sending to workers, the calendar

- **Your notes.** A notes box in the Record tab while recording (also kept in
  the browser, so a crash cannot take it), and "Add a note" stamps a line with
  the time in the recording. They ride on the recording as `user_notes`.
- **Meeting notes** (opt-in). After the transcript, the helper model
  (`AMUX_HELPER_MODEL`) writes `<base>.notes.md`: title, summary, key points,
  decisions, action items with owners (ticked in the Record tab), then your
  notes verbatim. It also suggests the workers the meeting concerns, choosing
  only from the fleet's names (a made-up name is dropped).
- **Filing** (opt-in, off by default). The meeting is filed with the workers
  and groups it concerns and a summary is posted in each group's channel as the
  Recorder, with no @mention, so it wakes nobody.
- **Sending to a worker.** Tag a worker (the Workers panel, or `@worker` in your
  notes) and once the transcript (and notes, if on) is ready the worker gets a
  message from the owner in its Messages thread (an isolated worker, a direct
  send): date, length, participants, the calendar event, absolute paths to the
  audio, transcript and notes, your instructions for it, and "handle it as fits
  your role" (a presales worker catalogues the meeting, for example). It
  answers in the thread. `POST /api/recordings/<id>/send`.
- **The calendar.** A recording is matched to the event it overlaps most (at
  least half the recording or ten minutes) in the amux calendar and every
  connected Google account; the event names the meeting and lists its people.
- **Hotkey.** "Start or stop recording" in the Hotkeys tab, Alt+Shift+R by
  default; the Mac app's Control-Option-R works from any app.
- **Microphone choice** (Mac). The Record tab lists the inputs; the recorder
  checks microphone permission before starting and says "the microphone gives
  no audio" within 4 s instead of recording nothing.
- **Signing.** The Mac app is signed ad hoc with its bundle id as the
  designated requirement. With the default (the binary's hash) every rebuild
  silently lost the microphone grant.

### Dictation

Records in 1 s chunks persisted as they arrive, so a reload keeps the take, and the
outbox never drops a clip that has not been sent.

### iOS

Background audio mode and a native recorder through the same session API.

## Evidence (2026-10-27)

`desktop/test/recorder-e2e.py` on the built app and a private server, 21 checks
pass: two tracks recording, segments on disk and on the server while recording,
a sleep recorded as a gap on both tracks, the window closed with recording
going on, the filed file's length within 0.7 s of wall time with gaps filled,
nothing lost, the spool cleared; a SIGKILL mid-segment with the open segment
rescued and the recording filed as recovered; offline recording filed once the
server returned; SIGTERM stopping and saving first.

Measured on that run (M-series Mac under heavy fleet load):

| | CPU | Memory |
|---|---|---|
| App idle | 0.5% | 111 MB |
| Recording two tracks | 2.8% | 115 MB |
| Start (test hook round trip included) | 376 ms | |

The audio in these tests comes from synthetic tone sources that use the same
writers, segments, manifest, gaps and upload as the microphone, because macOS
asks a person to grant microphone and Screen Recording access and an agent
cannot click that prompt. The real microphone and system-audio paths compile
and are wired; their first run needs the owner to grant the two permissions.

The long-recording run (`desktop/test/recorder-long.py`, 2 h 5 min, real
time, two tracks), three times on 2026-10-27:

| Run | Build | Gaps | Lost | File for recording | CPU mean / max | RSS max |
|---|---|---|---|---|---|---|
| 1 | first cut | 8 (false: delivery jitter read as gaps) | 0 | 11133 s for 7514 s | 0.79% / 4.8% | 110 MB |
| 2 | source-clock gaps | 0 | 0 | 13903 s for 7506 s (old assembly) | 1.12% / 3.2% | 113 MB |
| 3 | both fixes | 0 | 0 | 7503.9 s for 7504.0 s | 1.46% / 6.2% | 113 MB |

Run 1 found both bugs. Gap detection read delivery time, so a late buffer
looked like lost audio; it reads the source clock now (sample time, sample
buffer PTS). Assembly let ffmpeg's concat demuxer time each raw ADTS piece,
and with no duration in the stream it guessed one from the bitrate (18.5 s
for a 10 s piece, 140 s for one that opens quietly); every piece now carries
the manifest's duration. The Mac writer also summed whole-ms buffer
durations, running start_ms 50 ms a segment behind the audio; it keeps the
timeline in exact milliseconds. At most two segments ever waited for upload.

### Evidence, 2026-10-28

- `desktop/test/recorder-live-e2e.py`, real speech (macOS `say`, 53 s): the
  browser leg (Chromium's microphone is the speech) passed every check: the
  hotkey started and stopped it, live words 28 s in, the finished transcript
  built from the live pieces, typed notes and an `@worker` in them delivered to
  the worker's thread with the file paths, notes and filing by a stand-in helper
  (said in the script). The Mac leg records the "Background Music" loopback
  through the real microphone path; it needs the one microphone grant for the
  newly signed app.
- Live windows on a private server: three 5 s pieces of real speech were in
  the live transcript 18.7 s after upload, word for word.
- `ios/AmuxAppTests/RecorderCoreTests`: the recorder core on the iOS runtime
  (Simulator): segments and manifest, a call interruption filed as a gap with
  its reason, a killed recording recovered on relaunch. It found that an
  expected gap's reason was wiped by audio still in flight.
- `e2e/recorder-panes.spec.ts`: the three panes, desktop and phone.

## Limits

- System audio needs macOS 13 and the Screen Recording permission; on macOS 12
  the app records the microphone only and says so.
- The uploader borrows the owner token from an open dashboard window, like the
  rest of the app's native calls. A recording made with no window open since
  launch waits on disk until a window opens.
- iOS: the app compiles with the shared core and the `audio` background mode;
  a run on a device is still owed.
- Meeting detection (offer to record when a call starts) is not built yet.
