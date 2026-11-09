<!-- rules: parent=../CLAUDE.md owner=amux-vision enforces=A7,A11 -->
# desktop (the Mac app)

A Swift app (`amux-*.swift`) that shows the dashboard in WebKit windows, tabs and panes, with
native panes, hotkeys, dictation, the recorder and workspaces. It talks to the page through
script message handlers (`amuxPane`, `amuxWindowLabel`, `amuxNotify`, `amuxZoom`,
`amuxRecorder`).

- Build: `AMUX_DESKTOP_NO_INSTALL=1 bash desktop/build-desktop.sh` builds `Amux.app` in the
  checkout without touching the installed app.
- Tests: `desktop/test/*-e2e.py [path/to/Amux.app]` start a second copy of the app with its own
  config (`AMUX_DESKTOP_CONFIG`) and drive it through `amux://test/...` hooks in test mode. A
  test that needs this checkout's dashboard starts a private server with
  `AMUX_DASHBOARD_DIR` (`palette-focus-e2e.py` shows how); otherwise the app talks to this Mac's
  running amux.
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
- A test screenshot captures its own window or rectangle, never the whole screen: the owner's
  screen can hold a live call.
- The page side of a feature lives in `crates/amux-dashboard`; the app side is opt-in behind
  the bridge, so an older app keeps the old behaviour.
