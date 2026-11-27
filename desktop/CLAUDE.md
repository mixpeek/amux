<!-- rules: parent=../CLAUDE.md owner=amux-vision enforces=A7,A11 -->
# desktop (the Mac app)

A Swift app (`amux-*.swift`) that shows the dashboard in WebKit windows, tabs and panes, with
native panes, hotkeys, dictation, the recorder and workspaces. It talks to the page through
script message handlers (`amuxPane`, `amuxWindowLabel`, `amuxNotify`, `amuxZoom`,
`amuxRecorder`, `amuxDictation`).

- Build: `AMUX_DESKTOP_NO_INSTALL=1 bash desktop/build-desktop.sh` builds `Amux.app` in the
  checkout without touching the installed app.
- Tests: `desktop/test/*-e2e.py [path/to/Amux.app]` start a second copy of the app with its own
  config (`AMUX_DESKTOP_CONFIG`) and drive it through `amux://test/...` hooks in test mode. A
  test that needs this checkout's dashboard starts a private server with
  `AMUX_DASHBOARD_DIR` (`palette-focus-e2e.py` shows how); otherwise the app talks to this Mac's
  running amux. `amux://test/key?k=f&mods=cmd` presses a key as AppKit routes it, and
  `amux://test/menu?title=Copy` chooses a menu item (`find-copy-e2e.py` uses both).
- Dictation runs on device (`amux-dictation-core.swift`, macOS 26 SpeechAnalyzer) and talks
  to the page through `amuxDictation`. `amux://test/globe?down=1|0` presses and lets go of 🌐
  (`?tap=N` taps it), and `AMUX_DESKTOP_DICTATION_FILE=<audio>` makes a file the microphone in a
  test copy, so `dictation-e2e.py` hears a known take without recording the room.
  A microphone plugged in or unplugged shows a toast saying whether dictation uses it
  (`MicWatch`, `window._dictMicsChanged`); `amux://test/mics?add=|remove=|default=` fakes one
  (`mic-change-e2e.py`).
- An agent's build is **Amux Dev** (`com.amux.desktop.dev`, no `amux://` handler, never a
  login item), a separate app from the owner's Amux. A test copy (`AMUX_DESKTOP_TEST=1`) is
  quiet: no Dock icon or menu bar, it never comes to the front or posts a notification, and
  its windows are invisible and let clicks through (owner, 2026-11-09: test copies "disrupt my
  experience"). `AMUX_DESKTOP_TEST_VISIBLE=1` shows them, only for a screenshot.
  `test-copy-quiet-e2e.py` proves it. Tests that add a virtual display (`workspace-e2e`,
  `grid-workspace-e2e`, `perf-bench`, `tab-growth`, `memory-guard-e2e`) or record audio still
  reach the owner's screen or devices: ask first.
- An agent's build is **Amux Dev** (`com.amux.desktop.dev`, no `amux://` handler, never a
  login item), a separate app from the owner's Amux. A test copy (`AMUX_DESKTOP_TEST=1`) is
  quiet: no Dock icon or menu bar, it never comes to the front or posts a notification, and
  its windows are invisible and let clicks through (owner, 2026-11-09: test copies "disrupt my
  experience"). `AMUX_DESKTOP_TEST_VISIBLE=1` shows them, only for a screenshot.
  `test-copy-quiet-e2e.py` proves it. Tests that add a virtual display (`workspace-e2e`,
  `grid-workspace-e2e`, `perf-bench`, `tab-growth`, `memory-guard-e2e`) or record audio still
  reach the owner's screen or devices: ask first.
- The menu bar item is the app icon with the needs-you count (`amux-statusmenu.swift`): who
  needs you, Open amux, New Window/Tab, recording, Computer Use (watch a running sandbox),
  Pins, Workspaces, the server, CPU and memory, Settings, Updates, Quit. A quiet test copy
  keeps it off the menu bar; `amux://test/status-menu?needs=N|title=` drives it and
  `status-menu-e2e.py` proves it.
- Computer use on this Mac (`amux-computer.swift`): the app bundles trycua's cua-driver
  (pinned and checksummed in `build-desktop.sh`) and runs it embedded, with Amux's own
  permissions, when Settings > Computer use is on; it writes `<AMUX_HOME>/cua/endpoint.json`
  for the server (`integrations/computer_mac.rs` holds the per-app gate). A copy removes that
  file only when it names its own driver (a test copy shares `~/.amux`; `computer-endpoint-e2e.py`),
  and a running driver writes it again if it goes missing. ⌘⇧Esc or the menu
  bar's Stop Computer Use ends it. `computer-mac-e2e.py` proves it with reads only.
- While a worker uses this Mac the app shows a live preview and an outline over the driven
  window, and a dot on the menu bar icon (`amux-agentview.swift`, from the server's activity
  feed); both overlays use `sharingType = .none`. ⌥Space opens Quick Ask (Settings can turn it
  off; a quiet test copy never registers it). Settings > Permissions shows what macOS granted.
  `agent-view-e2e.py` proves them; `amux://test/quickask` drives Quick Ask.
- Settings is one place, in its own window: amux > Settings… (⌘,), the Dock menu and the menu
  bar item open the Settings window (`SettingsWindowController`, apart from every amux window),
  which loads the server's dashboard as Settings alone (`/?settings_window=1`, app.js
  `_settingsWindowMode`) on its This Mac tab, with the server's tabs beside it (app.js
  `_macSettings*`, bridge `amuxSettings`, `settingsAction` and `macSettingsState` in
  `amux-settings.swift`). A link out of it opens in an amux window. A new app setting goes on
  This Mac, never as a second switch in a menu. With no server, or one too old for the mode, the
  window shows the app's own page (`verdict=settings_window_shown source=app reason=`).
  `mac-settings-e2e.py` proves it; `amux://test/settings-action` sends one action and
  `amux://test/js-pane?in=settings` runs script in the window.
- A test screenshot captures its own window or rectangle, never the whole screen: the owner's
  screen can hold a live call.
- The page side of a feature lives in `crates/amux-dashboard`; the app side is opt-in behind
  the bridge, so an older app keeps the old behaviour.
