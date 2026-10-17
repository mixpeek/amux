---
description: Use when a task needs the owner's real iPhone (TestFlight installs, Sign in with Apple, checking an iOS app on a device) and the Mac has iPhone Mirroring. Screenshots, taps, long presses, scrolling, Home and opening apps.
allowed-tools: Bash, Read
argument-hint: <shot|tap|press|scroll|home|switcher|open|type|frame> [args...]
---

# /iphone-mirroring: drive the owner's iPhone from the Mac

macOS iPhone Mirroring shows the phone in a window on the Mac. This skill turns that window into
something a worker can see and operate: `skills/iphone-mirroring/scripts/phone.sh`.

Learned 2026-10-17 while installing a TestFlight build and reaching Sign in with Apple on the
owner's phone. Every rule below is something that failed first.

## Commands

```bash
S=skills/iphone-mirroring/scripts/phone.sh   # from the amux repo root
$S shot                 # prints a PNG path; Read it. Image pixels are the coordinates below.
$S tap X Y              # tap at image coordinates
$S press X Y [SECS]     # long press (default 1 s)
$S scroll [DY]          # DY < 0 scrolls down (default -40); repeat for long pages
$S home                 # Home screen (Cmd-1)
$S switcher             # app switcher (Cmd-2)
$S open TestFlight      # Home, Spotlight (Cmd-3), type the name, Return
$S type "text"          # into the focused field
$S frame                # the window's x,y,w,h on the Mac screen
```

The first run compiles `scripts/mx.swift` into `~/.amux/bin/iphone-mx` (needs `swiftc`, which
ships with the Xcode command line tools).

Loop: `shot`, read coordinates off the image, act, `shot` again to confirm. Never chain several
taps without looking: the phone animates, and a tap during a transition lands on the next screen.

## What works and what does not

| Need | Works | Does not work |
|---|---|---|
| Screenshot | `/usr/sbin/screencapture -x -R<frame>` (full path: launchd shells lack /usr/sbin on PATH) | plain `screencapture` |
| Tap | real CGEvent mouse down/up (`iphone-mx click`) | System Events `click at {x,y}`: it hits the window's accessibility group and never reaches the phone |
| Scroll | continuous scroll-wheel events with phases began/changed/ended (`iphone-mx scroll`) | plain wheel events, mouse drags, Page Down, arrow keys |
| Home, switcher, Spotlight | System Events keystrokes Cmd-1, Cmd-2, Cmd-3 | |
| Typing | System Events `keystroke` | |

## Rules

- **The phone must be locked and not in the owner's hand.** Mirroring pauses while the phone is
  in use; the window then disappears from the screen and `shot` captures whatever is behind it
  (a mail list, a chat). If a shot shows no phone, stop and say so. Do not keep tapping: the
  clicks land on the owner's other windows.
- **It moves the owner's real pointer and brings the mirroring window to the front.** Keep each
  session short, and do not drive it while the owner is visibly working on the Mac unless they
  asked for it.
- **The window's title bar is the top ~30 image pixels.** The phone's own status bar starts below it.
- **Things only the owner can do stay the owner's:** a passcode, Face ID, an Apple ID password,
  a purchase confirmation. Stop and ask at those.
- **Verify by what changed.** After an install or sign-in, confirm on the other side too
  (App Store Connect, the server's logs), not only on the phone's screen.

## Related

- TestFlight: a build appears on the phone only after it is added to the tester's group in App
  Store Connect (TestFlight → the group → Builds → Add Build to Group). Internal groups need no
  review. App Store Connect is reachable through `amux cdp` in the owner's Chrome.
- iOS Simulator (no TestFlight there): `docs/` and the `ios-simulator-rig` memory.
