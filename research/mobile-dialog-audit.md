# Mobile dialog audit — AF-749

The owner's limited-workers screenshot reproduced in local iOS 26.5 Safari: a 1017px dialog extended from -131.5 to 885.5 in a 754px viewport. Close was above the screen and the dialog itself did not scroll. Shared dialogs now bound the content, preserve the action row and follow the visible viewport when the software keyboard opens.

The audit also corrected missing explicit dismissal in orchestration/connection history/command selection, a proxy form missing its interactive active class, a PWA-incompatible native discard confirmation, an unbounded calendar status read, unreadable theme surfaces, and a video Close button inside auto-hiding playback controls. Video Close is now persistent. Shared modal buttons and corrected dismiss controls are at least 44px. Sticky modal action rows meet the bottom of their panel without content showing through underneath.

## Measurement

`e2e/mobile-modal-layout.spec.ts` exercises long content at 320px and 375px phone widths, keyboard height and landscape; reachable actions and upward scrolling; proxy hit testing; discard cancellation and confirmation; bounded calendar failures; light/dark dialog surfaces; persistent video dismissal. Positive controls deliberately clip a dialog and make a surface transparent, requiring real client diagnostic beacons.

Final candidate regression run: 21 passed across desktop Chromium, mobile Chromium and Safari. Final deployed proof is recorded on AF-749. `npm run test:state`: 27 passed. SPA lint: 0 errors, 48 existing warnings. Client JS and state assets carry version 0.9.929.

Native audit uses the installed iPhone 17 simulator, local iOS 26.5 and the amux Browser iOS driver. It opens the shipped UI handlers, inspects screenshots, performs native scroll/tap/input actions and cancels drafts. Production reads provide the loaded fleet. A local candidate proxy rejects command writes; this audit does not submit worker prompts, edit real board items, invite people or start billing. Existing functional suites and isolated fixtures cover submitted command behavior separately.

The 54-entry inventory includes shared confirm/alert/prompt, both vocabulary forms, limited workers, worker info/create/edit/connect/orchestration/queue, About, message history/filters/saved messages, scope memory, skills, file preview, schedules, board creation/detail/help/focus/saved view, map pin/group, proxy, calendar event/subscription, connection history/scope, lookup, steer and gate dialogs, team/member/invite, journal configuration, mdai connection, command picker/custom command, speech, worker task queue, authentication, browser profile, worker channel, video, teleprompter, settings and the cloud upgrade gate. Variants sharing a form use that form's audit. The obsolete file-explorer overlay is no longer opened by the UI; current Files is a view. Journal photo selection opens an entry rather than another modal. The upgrade gate intentionally has no dismissal; only its presentation/navigation exit is audited, never checkout.

Local evidence (kept outside public source because screenshots may include workspace information):

- `scratch/ios-simulator-review/modal-native-before/limited-top.png`
- `scratch/ios-simulator-review/modal-native-final/results.json` and screenshots, including failed specimens
- `scratch/ios-simulator-review/modal-native-corrections/results.json`
- `scratch/ios-simulator-review/modal-native-dismissal/results.json`
- `scratch/ios-simulator-review/modal-regression-final-source.log`
- `scratch/ios-simulator-review/modal-regression-desktop.log`

Do not summarize those files as an unconditional all-pass: retained early runs contain fixture failures and native interaction failures. AF-749's final outcome identifies corrected reruns and remaining limitations. Geometry is also not a substitute for viewing screenshots: transparent panels and unreadable text passed the first geometry checks.

## Logs

`modal-layout-clipped` reports `measured`, `n_considered`, affected dialog identifiers and viewport height. Reasons include clipping, inactive proxy form, missing dismiss action, footer gap, transparent surface and inadequate foreground/background contrast. It excludes dialog text. `tunnel-status-unavailable` reports bounded status failures; `scope-discard-choice` reports the discard decision without configuration contents.
