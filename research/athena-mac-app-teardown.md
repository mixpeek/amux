# Athena Mac app teardown, and what amux should take from it

Date: 2026-11-22. Static analysis of Athena 0.1.749 (the build on disk at the start), E2E walkthrough on 0.1.836 (Squirrel installed a pending update when I restarted the app for CDP access; see "Side effects" at the end).

Extracted bundle: `a local extract of `app.asar`` (main process is one unminified 153,543-line file, `main/index.js`, so most claims below cite a line number in it).
Screenshots were taken of Athena's own windows during the walkthrough. They show the owner's account and chats, so they are kept on the owner's Mac and not committed.

Claims marked **(inferred)** are my reading of the design, not something I read in code or saw on screen.

---

## 0. The short version

Athena's computer use is impressive for one reason: **it works on your real Mac, in the background, per window, without taking your mouse or keyboard.** It drives apps through Accessibility element tokens plus per-process event posting (private SkyLight APIs), screenshots one window at a time with ScreenCaptureKit, draws its own cursor, and yields when you start typing in the same app. Everything around it (per-app consent, blocked app categories, a live preview window with Stop, a window outline, a menu bar dot, a global ⌘⇧Esc kill switch, prompt-injection gating when outside content is in the chat) is careful product work.

The engine is open source: it is **trycua's `cua-driver` 0.44.0**, a macOS binary embedded in a helper app. amux already uses trycua for its Linux Docker desktop, so the most valuable single move is adopting the same `cua-driver` on the Mac, inside amux's Swift app, with Athena's consent and visibility model on top.

---

## 1. How Athena is built

| Piece | What it is | Evidence |
|---|---|---|
| Shell | Electron app, bundle id `com.athenaintel.desktop`, team `55KP7UFLHA`. Package description: "Midas (ships as Athena): Athena's Electron desktop app for chat, FlexLayout pop-outs and the rnc spreadsheet engine." | `Contents/Info.plist`; `asar/package.json` |
| Main process | `out/main/index.js`, unminified ESM bundle; `out/preload/index.cjs` (2,312 lines) exposes `window.midas` and `window.__ATHENA_SDK__` | file listing; CDP `Object.keys(window)` |
| Renderer pages | `index.html` (main), `quick-ask.html`, `preview.html` (computer-use live preview), `window-badge.html` (outline on the window being driven), `follow-banner.html`, `meeting-pill.html`, `meeting-prompt.html`, `recorder.html`, `dictation-bar.html`, `permission-guide.html`, `browser.html`, `workbook.html`, `slideshow.html`, `popout.html` | `out/renderer/*.html` |
| Cloud | "agora" is the backend API (`api.athenaintel.com`), "keryx" is a websocket relay (`wss://keryx.prd.athenaintel.com`), "iris" serves threads (`iris.prd.athenaintel.com/v2/threads`). The model runs server side; the Mac is a **device** that executes tool calls. | URL grep of `main/index.js`; `createDeviceTunnel` at line 25262 |
| `Helpers/Athena Access.app` | LSUIElement helper (`com.athenaintel.desktop.access`) holding `athena-dictation` (Swift: AppKit, CoreAudio, Carbon, ApplicationServices) and `cua-driver` (82 MB, links ScreenCaptureKit, AVFoundation, ApplicationServices). Runs as `athena-dictation mic-watch <meeting bundle ids>` and `athena-dictation listen fn` at all times. | `ls`, `otool -L`, `pgrep -fl` |
| `cua-driver` | trycua's macOS driver, version string `cua-driver 0.44.0`. Started as `cua-driver serve --embedded --socket <tmp>/athena-cua-<pid>.sock --host-bundle-id ... --permission-mode standard` plus an MCP client process `cua-driver mcp --embedded ...`. Telemetry, update checks, run-script and keep-display-awake are switched off by env vars. | `strings`; `cuaPaths`/`cuaCommands` at lines 77638-77740 |
| `Helpers/microsandbox/msb` + `libkrunfw.5.dylib` | microsandbox (libkrun microVM on Hypervisor.framework, entitlement `com.apple.security.hypervisor`). Boots OCI image `docker.io/library/node:22-bookworm@sha256:e273...` and injects `Resources/vm-guest/bun-linux-aarch64` (Bun 1.4.4) at `/usr/local/bin/bun`. Apple silicon only, idle stop after 15 minutes. This is the **code workspace VM**. | `VM_IMAGE`, `VM_IDLE_STOP_MS` at lines 97165-97178; `codesign --entitlements` |
| `Helpers/Athena Browser.app` | `athena-browser-launcher`, registers `http`/`https` URL schemes so Athena's own browser can be the default browser. | its `Info.plist` |
| `athena-chrome-bridge` | Swift Chrome **native messaging host**. Allows origins `chrome-extension://lfkjfdfecgcbbakflpnnnifhldaogbld/` (Athena in Chrome) and `kombkoghiainfagcopkgehmcdkelnnid`, and checks its parent with the code requirement `anchor apple generic and identifier "com.google.Chrome" and certificate leaf[subject.OU] = "EQHXZ8M8AV"` ("not started by Google Chrome"). Installed into `~/Library/Application Support/Google/Chrome/NativeMessagingHosts`. | `strings`; line 71922 |
| `athena-pty` | Swift PTY helper for the chat's Terminal tab (Ctrl+`). | line 119223, menu at line 7536 |
| `athena-panel.node` | N-API module linking AppKit only; exports just `napi_register_module_v1` (inferred: converts Electron windows into non-activating NSPanels for quick ask, preview, badges). | `nm -gU`, `otool -L` |
| `athena-keychain.node` | Security.framework wrapper, keeps the API key out of plain files. | line 108290 |
| `athena-print.node` | AppKit + PDFKit printing for the Office documents. | `otool -L` |
| Updates | Squirrel.Mac + electron-updater, `generic` provider at `https://d32z4164ji6o76.cloudfront.net/mac/employees` (channel "employees"). Plus `out/desktop-update/*.json` shell hashes (inferred: content-addressed renderer updates). | `app-update.yml`, `desktop-update/build.json` |
| Entitlements | main app: `allow-jit`, `device.audio-input` only (not sandboxed, hardened runtime). | `codesign -d --entitlements` |
| Electron fuses | `RunAsNode=1`, `EnableNodeOptionsEnvironmentVariable=1`, `EnableNodeCliInspectArguments=1`, `EnableEmbeddedAsarIntegrityValidation=0`, `OnlyLoadAppFromAsar=0`. A local process can therefore run code inside an app that holds Accessibility and Screen Recording. A hardening gap amux's Swift app does not have. | fuse bytes read from `Electron Framework` |
| Office suite | Native-feeling Excel (rnc engine), Word and PowerPoint editors in-app, PDFium wasm, bundled fonts; registers as an Alternate editor for xlsx/xls/xlsb/ods/csv/tsv/docx/dotx and more. | `Info.plist` `CFBundleDocumentTypes`; `renderer/assets/workbook-view-*.js`, `presentation-studio-*.js` |

---

## 2. Computer use in depth

### 2.1 How it sees

- **One window at a time.** `desktop_app_state` returns a screenshot of a single window (ScreenCaptureKit `SCScreenshotManager`/`SCStream`, plus `SLSHWCaptureWindowList` for off-screen capture) together with up to 150 (max 500) **Accessibility elements**, each with an `element_token` like `s1a2b3c4d:17`, role, label, value, frame in screenshot pixels, enabled and focused. Optional `query` filters elements. Lines 64140-64230 (`StateInputSchema`, `CuaElementSchema`).
- Screenshots are capped at 1568 px on the long edge and 1.15 MP, and re-encoded as JPEG 80 when over 1.5 MB of base64 (`SCREENSHOT_MAX_EDGE`, `JPEG_ABOVE_BASE64_CHARS`, line 63735). These match Anthropic's recommended computer-use image size **(inferred)**.
- A window on another Space or unreachable gets a "look-only" screenshot and the model is told so (`UNREACHABLE_READ_NOTE`).
- Snapshots are remembered per conversation (`SNAPSHOTS_KEPT = 32`), so x/y coordinates are always relative to a specific read; an x/y click with no matching read is refused with `screenshot_context_missing`.

### 2.2 How it acts

Tools announced to the cloud agent (`DesktopAppTool`, line 63716): `desktop_list_apps`, `desktop_app_state`, `desktop_app_click`, `desktop_app_type`, `desktop_app_key`, `desktop_app_scroll`, `desktop_app_drag`, `desktop_app_set_value`, `desktop_app_launch`, plus `desktop_app_consent` (agora's prompt request, never called by the model).

- **Background by default.** `cua-driver` posts events to the target process with private SkyLight calls (`SLEventPostToPid`, `SLPSPostEventRecordTo`, `SLEventSetAuthenticationMessage`, `SLPSStealKeyFocusReturningID`/`SLPSReleaseKeyFocusWithID`) and AX actions (`AXUIElementPerformAction`). The person's pointer and keyboard focus are untouched. Prompt text shown to the user: "Athena will see Calculator's windows and act in them in the background, with its own cursor.".
- **Its own cursor.** `cua-driver` draws a separate cursor (flags `--cursor-theme`, `--glide-ms`, `--dwell-ms`, `--idle-hide-ms`, `--cursor-reduced-motion`). Visible as a pink cursor in the live preview. The `Athena Access` full-screen transparent window (2560x1440, seen in the window list during runs) is that cursor overlay **(inferred)**.
- **Element tokens first, x/y second.** `desktop_app_click` takes `element_token` or x/y; `desktop_app_set_value` sets a field's AX value directly; `desktop_app_type` with an element token focuses the field first.
- **Foreground fallback, made explicit.** Where background delivery cannot work (drag-and-drop sessions, Electron pages reading the real mouse, toolkits that read the real pointer, Screen Sharing), the driver refuses and Athena turns each refusal code into a model-facing instruction (`DRIVER_REFUSAL_WORDS`, lines 63800-63960): `element_disabled`, `minimized_or_hidden_window`, `off_space_or_ax_unresolved`, `background_unavailable`, `no_focused_text_input`, `browser_field_needs_keystrokes`, `select_menu_would_raise_window`, `SCREEN_SHARING_REQUIRES_FOREGROUND_HID`. Every message says what happened, that nothing was sent, and the next tool to try.
- **Drags:** first attempt is background; if the model repeats the same drag within 60 s (±4 px), Athena sends it in front with the real pointer and a banner "Athena is using your pointer in {App}" (`DragDelivery`, lines 64010-64080).
- **Front given back.** If an action pulled an app forward (Chrome takes ⌘T as a request for focus), Athena gives the front back to the app the person was in (`giveActionFrontBack`, line 141960).
- **Yields to the person.** Before acting in the app that is in front, Athena reads hardware key and click clocks from the helper; if the person typed or clicked in the last 1.5 s it waits up to 5 s, then returns `person_active` ("The person is typing or clicking in Spotify right now, so Athena did not act there...") so the model can do something else (`waitForPerson`, lines 63649-63715).
- **Timeouts:** 45 s per driver call; a hung driver is killed and restarted (`CUA_CALL_TIMEOUT_MS`).
- Per-conversation queues serialise actions so two chats never interleave inside one app.

### 2.3 The VM

The microsandbox VM is for code and files (`workspace_exec`, `workspace_exec_start/poll/stop`, `workspace_read/write/list`, `workspace_checkpoint`, `workspace_restore` (person only), `workspace_terminal_read`, `workspace_bind`, a preview relay for dev servers), at `/workspace` in a node:22 Linux guest. Computer use always targets the real Mac; the VM has no GUI. Separately, "Work in Folders on This Mac" runs commands on the host under `/usr/bin/sandbox-exec` with a generated Seatbelt profile (writes only inside the folder, no network unless allowed), or with Full access per chat (lines 93242-93400).

### 2.4 The agent loop

- The chat model runs in Athena's cloud (picker showed "Opus 5.5" with effort Low/Med/High/XHigh/Max). The Mac app opens a websocket **device tunnel** (`createDeviceTunnel`, line 25262) and announces `device_info`: kind `desktop`, platform, a device id `desktop-<32 hex>`, the tool list, and a `consent_protocol` block (`mac_apps`, `outside_content`, `remote`, `any_app`). The cloud sends `tool_call` frames and the Mac answers `tool_result` frames with `success` and `data` or a coded error string (`toolResultFrame`, line 25248).
- Because the Mac is a device, **other Athena surfaces (phone, web, Office, Chrome, another Mac) can use this Mac** when "Let Your Other Athena Apps Use This Mac" is on; the menu bar then says "Athena is using Spotify, from Athena on iPhone" **(label from `remoteUseLabel`, line 55457; the surface name is inferred)**.
- The chat UI groups tool calls under a narrated sentence ("The text didn't land in the search box, so I'll find the search field directly.") with collapsible "Ran desktop app state" rows and "Show tool details". The run header says "Worked for 48s · Used 13 tools" or "Completed with issues".

### 2.5 Safety and permission UX

- **Per-app consent**, native dialog: "Allow Athena to use {App}?" with Allow (this conversation), Always Allow (every conversation), Cancel, and "Press ⌘⇧Esc to stop computer use at any time." Recorded by agora per Mac app and revocable in Settings ("Always-Allowed Apps: Spotify  [Forget]"). 100 s deadline, after which the tool fails cleanly with `timed_out`.
- **Blocked categories** unless "Let Athena Use Any App" is on: terminals (14 bundle ids), automation apps (Script Editor, Automator, Shortcuts, Keyboard Maestro, Hammerspoon), password managers (1Password, Bitwarden, Keychain Access, Passwords...), System Settings, system security prompts (SecurityAgent, loginwindow, passkey sheet), and Athena itself. With the switch on, the prompt carries a caution line per category ("{App} is a terminal: Athena can run any command in it, as you.") and defaults to Cancel. Lines 62950-63100.
- **Prompt-injection gate:** when outside content (an email from someone else, a web page) is in the conversation, every marked action asks once: "Allow Athena to do this in {App} once?" (`OUTSIDE_CONTENT_NOTICE`, line 63205; `PromptKind.AppOnce`).
- **Passkey guard:** while a passkey "decision check" is open in a browser, Mac app tools stay out of every browser (`decisionCheckOpenMessage`).
- **Site consent** for the in-app browser, per site, same Allow/Always shape (`PromptKind.Site` shows the run waiting on it).
- **Visibility:** live preview panel ("Athena is reading Spotify", live thumbnail with the agent cursor, buttons "Stop Using Spotify" and "Stop"); an outline window over the exact window being driven ("Athena is working in this window"); a menu bar icon with a dot (never red, so it cannot read as recording) and a "Stop Computer Use ⌘⇧Esc" item; headline states Reading / Working in / Using your pointer in / Waiting for you in (line 11819).
- **Overlays exclude themselves from capture** (`setContentProtection(true)` on preview, badge and banner windows; lines 100122, 137304, 143194). I confirmed it: `screencapture -l` on those windows returned "could not create image from window". This keeps Athena's own UI out of the model's screenshots.
- **Follow mode** (off by default): the app Athena works in comes to the front so you can watch; one conversation follows at a time; the app you were in is given back when follow ends.
- macOS permissions: Accessibility via the helper's prompt, Screen Recording via `desktopCapturer.getSources` to trigger the system prompt or deep-link to Settings if denied (`requestComputerUsePermissions`, line 143109). A dedicated Permissions page shows each permission with status (On / Not set up / Can't tell) and a Turn On button, and a "Finish setting up Athena" checklist sits behind the header warning badge.

### 2.6 What makes it impressive

1. The person keeps working while the agent works, on the same Mac, because nothing goes through the global pointer or focus.
2. AX element tokens plus a window screenshot give the model both a reliable click target and a visual check.
3. Every refusal is a coded, actionable message. The model recovers instead of looping (the Spotify run below recovered from "text didn't land" and "stale screenshot" on its own).
4. Consent and visibility are first-class: you always know which app, which window, and how to stop.
5. The Mac is reachable from any other surface as a device, so a phone chat can act on the desktop.

### 2.7 Where it was weak in my runs

- Spotify (a CEF app) returned **stale screenshots** in the background for the whole run; the model fell back to the AX element list to get the answer. Background capture of Chromium-based apps is a known weak spot.
- Every first use of an app or site needs a human click on a native modal; unattended runs just time out after 100 s. Fine for a personal assistant, wrong for amux's unattended workers unless consent can be granted ahead of time.

---

## 3. Other notable features

| Feature | What Athena does | Evidence |
|---|---|---|
| Built-in browser agent | Electron tabs as "Athena's browser", separate profile. Tools mirror Anthropic's Claude in Chrome MCP names exactly: `tabs_context_mcp`, `tabs_create_mcp`, `computer`, `navigate`, `read_page`, `find`, `form_input`, `get_page_text`, `javascript_tool`, `read_console_messages`, `read_network_requests`, `wait_for`, `handle_dialog` | `BROWSER_TAB_VERBS`, line 62184 |
| Page subscriptions | `page_subscription` lets the agent watch the person's clicks, inputs and page changes on a tab (max 5 per session), with secret-looking fields redacted by a regex over names like password/otp/cvv/ssn | lines 11330-11345, `SECRET_FIELD_NAME` line 11384 |
| Sign-in import | Import cookies from Chrome, Edge, Brave, Arc, Vivaldi, Comet, Chromium, Firefox into Athena's browser, per chosen site, "nothing leaves this Mac"; "Keep Sign-ins Up to Date" mirrors one Chrome profile one-way through the Chrome extension; "browse profiles" upload a cookie archive so cloud browsers have your logins | `ImportBrowser` line 18472; `[browser] saved Athena's browser to a browse profile` line 61870 |
| Athena in Chrome | Chrome extension + native messaging host with parent code-signature check, so chats can drive the person's real signed-in Chrome | `athena-chrome-bridge` strings; Settings "Use Your Own Chrome" |
| Preferred browser | Athena's Browser or Chrome, passed to the run config | `PreferredBrowser` line 18136 |
| Quick Ask | ⌥Space opens a chat over any app (56-900 px tall panel) | `QUICK_ASK_ACCELERATOR` line 7077 |
| Attach from screen | "+" menu: The Window You Were In, A Screenshot…, The Text You Selected (via Accessibility) | seen in the walkthrough |
| Dictation | Hold Fn (Globe), Right ⌥ or Right ⌘; Fn+Space hands-free; ⌃⌥Space toggle; ⌘⌃V pastes last; mutes other Mac audio while listening; learned-words page | lines 6980-7000; `dictation-learned.html` |
| Meetings | `mic-watch` on Zoom, Teams, Webex, Spark etc.; "Meeting detected" prompt offers Start Notetaker; records system audio plus optionally the meeting window's video; red menu bar icon while recording; transcripts in cloud; LiveKit for live view | process args; lines 12149-12300; `livekit-client` asset |
| Folders + checkpoints | Chat bound to a folder runs commands under Seatbelt; checkpoints per turn; "Show What Changed", "Review This Turn's Changes First", "Work on a Copy" (worktree) | Settings page; `WorkspaceVerb` line 11545 |
| Terminal tab | Per-chat PTY (Ctrl+`), readable by the agent via `workspace_terminal_read`, never typed into | line 11563 |
| Handoff | "Continue on another device: Phone, Chrome, web or Office"; chat sources filter lists Web, Desktop, Chrome, Mobile, Office, Terminal, Computer app, Slack, Teams, Email, Text, Phone call, Meeting, Voice memo | screenshots 26, 25-Source |
| Office editors | Full spreadsheet, Word and PowerPoint editing inside the app, with Excel/Word keymaps, version history, comments, print via PDFKit | renderer assets; `athena-print.node` |
| Notifications | "Notify Me When a Chat Needs Me": Dock badge counts chats waiting on approval or a question | Settings General |
| Window-level overlays | Separate small always-on-top windows for preview, badge, banners, pill, dictation bar | renderer pages; window list during the run |
| Settings polish | One page per area with search, each toggle explains its effect in one line, shortcut editor with conflict rules (no bare ⌘ shortcuts, reserved system keys refused) | screenshots 03-08; `checkDictationShortcut` line 7040 |

---

## 4. E2E walkthrough

Driven over CDP (`--remote-debugging-port`) after I restarted Athena, because this shell has no Accessibility permission (System Events returned "osascript is not allowed assistive access"). I captured screenshots of Athena's own renderer pages through CDP or of Athena's own windows with `screencapture -l`, never the full screen. I did not grant any macOS permission. Athena was already signed in (the owner's account, production) with Accessibility and Screen Recording On, Microphone not set up.

| # | Flow | What happened | Time / steps |
|---|---|---|---|
| 1 | Home and settings | Clean chat-first home: sidebar of chats with source filter, Files tab, ⌘K search, Record Meeting, inbox, warning badge for unfinished setup. Settings sections: General, Keyboard shortcuts, Dictation, Meetings, Permissions, Computer use, Browser, Folders, Archived chats, Editing, Word, PowerPoint, Account, Usage, AI usage. | n/a |
| 2 | Calculator: compute 12*34 | The chat named itself "Calculate 12 × 34 on Mac", narrated "I'll open Calculator and press the buttons", called `desktop_app_launch`, and a native modal appeared: "Allow Athena to use Calculator?" (Allow / Always Allow / Cancel, ⌘⇧Esc hint). I could not click a native dialog without Accessibility, so I pressed Stop in the chat. The run ended "Completed with issues · Conversation stopped". **Calculator never launched**: consent gates even the launch. | Prompt at ~6 s; stopped at 37 s; 1 tool call |
| 3 | Spotify (Always Allowed): search "Khruangbin", report top result, do not play | Ran without a prompt. A live preview panel appeared bottom-right ("Athena is reading Spotify", live thumbnail with the agent's pink cursor, "Stop Using Spotify" and "Stop"), and an outline window sat exactly over Spotify's window ("Athena is working in this window"). My pointer and focus were never taken. Steps: list apps, read, click, type (did not land), read, key Return, reads, click search icon, reads. The screenshots stayed stale (CEF app), and the model said so and answered from the AX element list: "The top result for "Khruangbin" in Spotify is Khruangbin, the artist." It also confirmed the current track was unchanged. Correct answer. | 48 s of work, 13 tool calls, ~79 s wall clock to final message |
| 4 | Browser: open https://example.com and read its title | "Opening a new tab · example.com", then a native site-consent dialog (could not be captured: content protected). After the 100 s deadline the run reported plainly: "I couldn't open the tab. Athena needs your permission to use example.com... nobody answered within 100 seconds, so nothing was opened", offered to fetch the text another way, and ended with a "Next:" line. | 1 m 42 s, 1 tool call |
| 5 | TextEdit typing, System Settings lookup | Not run. TextEdit would hit the same first-use consent modal as Calculator, which I cannot click. System Settings is a blocked category unless "Let Athena Use Any App" is on, and turning that on is the owner's call. | n/a |
| 6 | Composer menus | "+" menu: Files and folders, From your screen (window you were in, a screenshot, selected text), Mention, Work in (Chat only / This computer / Choose a folder), Project, Follow Athena, Continue on another device. Model menu: Opus 5.5 with Low, Med, High, XHigh, Max. Source filter lists 17 surfaces. | n/a |
| 7 | Quick Ask (⌥Space), dictation (Fn), menu bar menu, Stop shortcut (⌘⇧Esc) | Not exercised: each needs a real global key press or a menu bar click, which this shell cannot synthesise without Accessibility. Described from code in sections 2 and 3. | n/a |

### Feature checklist: Athena vs amux

| Capability | Athena | amux today |
|---|---|---|
| Computer use on the owner's real Mac apps | Yes (cua-driver, background, per window) | **Missing** (amux computer is a Linux Docker desktop) |
| Background input without taking the pointer | Yes | Missing |
| AX element tokens + window screenshot | Yes | Missing on Mac (Linux CUA is pixel only) |
| Disposable sandbox desktop | No GUI sandbox (code VM only) | **Have** (CUA Docker, caps and reaper) |
| Per-app consent with Allow / Always / revoke list | Yes | Missing (amux has `boundaries.toml` per worker, no per-app UI) |
| Blocked app categories with cautions | Yes | Missing |
| Prompt-injection "outside content" gate | Yes | Missing as such (amux gates outbound email and money) |
| Live preview + Stop per app | Yes | Partial (noVNC view of the Linux sandbox, browser screenshots) |
| Window outline, menu bar dot, global stop hotkey | Yes | Partial (menu bar readout and a global hotkey exist, no computer-use state or kill switch) |
| Yield when the person is typing | Yes | Missing |
| Overlays hidden from capture | Yes | Missing |
| Browser agent on own browser | Yes (Electron tabs, Claude-in-Chrome tool names) | **Have** (amux browser route, saved Playwright profiles) |
| Drive the owner's real Chrome | Yes (extension + native messaging host) | **Have** (CDP relay into the owner's Chrome, audited proxy) |
| Import sign-ins from local browsers | Yes, per site | Partial (profiles captured by logging in; `--profile` cookie copy into CUA) |
| Code VM | Yes (microsandbox, Bun) | Partial (workers run on host; CUA container) |
| Folder sandbox with Seatbelt | Yes | Missing (worktree isolation exists, no Seatbelt) |
| Per-turn checkpoints and change review | Yes | Partial (git commits, land gate) |
| Quick Ask over any app | Yes (⌥Space) | Partial (global hotkey brings the dashboard) |
| Attach window / screenshot / selected text | Yes | Missing in the Mac app |
| Dictation | Yes (Fn hold, hands-free, learned words) | **Have** (on-device SpeechAnalyzer, Globe key) |
| Meeting detection and recording | Yes (notetaker, video of meeting window) | **Have** (recorder with opt-in meeting detection) |
| Remote surfaces drive this Mac | Yes (device tunnel) | Partial (any amux client can reach the server; no Mac device tools) |
| Dock badge for "needs you" | Yes | **Have** |
| Auto-update | Yes (Squirrel, channels) | **Have** (desktop-latest release) |
| Permissions page with status and Turn On | Yes | Partial (inferred: no single permissions page in amux-settings.swift) |
| Office editors | Yes | Partial (file viewer renders docs and decks, no editors) |

---

## 5. Steal for amux, ranked

Effort: S (days), M (a week or two), L (more). "Have" marks what amux already does.

| Rank | Feature | What Athena does (evidence) | How amux would do it | Effort | Value |
|---|---|---|---|
| 1 | **Mac computer use through cua-driver** | Embeds trycua `cua-driver` 0.44.0 in a helper app, talks MCP over a Unix socket, tools `desktop_app_*` (section 2.2) | Ship `cua-driver` inside `Amux.app/Contents/Helpers/` (signed with amux's team so TCC attributes to Amux), start it from Swift like Athena does (`serve --embedded --socket`, telemetry and update check off). Expose `/api/computer/mac/*` on the server and `amux computer --mac state|click|type|key|scroll|drag|set-value|launch`, mirroring the existing Linux verbs. The Swift app relays calls over its existing bridge. Workers get a fourth rung on the access ladder: real Mac apps. | M |
| 2 | **Element tokens + per-window screenshots as the default loop** | `desktop_app_state` returns window screenshot plus AX elements with tokens; x/y only relative to a remembered read (lines 64140-64230) | Same contract in `amux computer`: `state` returns a PNG path plus a JSON element list; `click --token`. Keep snapshot ids so a stale x/y is refused | S once 1 lands |
| 3 | **Coded refusals that tell the model what to do next** | `DRIVER_REFUSAL_WORDS` maps each driver refusal to "nothing was sent; use X instead" (lines 63800-63960) | Port the table to Rust, return `{code, message, next}` from every computer verb; log the code as a verdict line so sweeps count them (fits amux's two-fix rule) | S |
| 4 | **Per-app consent with Always Allow and a revoke list, plus blocked categories** | Native Allow / Always / Cancel, agora-recorded consents, Forget button, categories for terminals, automation, password managers, System Settings, security prompts (lines 62950-63100) | Store consents in the amux DB per worker or group (Scope tab), pre-grantable so unattended workers do not stall on a modal; owner prompt as an amux notification card with Allow / Always / Deny that also works from the phone PWA; default-deny list identical to Athena's. Fits `boundaries.toml` | M |
| 5 | **Visibility: live preview, window outline, menu bar state, global Stop** | `preview.html`, `window-badge.html`, menu bar Active dot, "Stop Computer Use ⌘⇧Esc" (lines 55660-55700, 141900) | NSPanel preview in the Swift app (non-activating, `sharingType = .none`), a borderless outline window over the target window frame, menu bar item text "amux (worker) is working in Spotify", a `RegisterEventHotKey` kill switch that stops every computer-use session | M |
| 6 | **Yield to the person** | Reads key and click hardware clocks; waits up to 5 s; returns `person_active` (lines 63649-63715) | `CGEventSourceSecondsSinceLastEventType(.hidSystemState, .keyDown/.leftMouseDown)` in Swift, checked before any action in the frontmost app | S |
| 7 | **Overlays excluded from capture** | `setContentProtection(true)` on preview, badge, banners | `window.sharingType = .none` on every amux overlay (pinned notes, recorder pill, preview) | S |
| 8 | **Outside-content gate** | With an external email or page in context, each marked action asks once (line 63205) | Mark worker turns that ingested external content (email read, web fetch) and require an approval card before any Mac computer-use or outbound action in that turn | M |
| 9 | **Front given back, follow mode** | Restores the previously frontmost app after an action steals focus; optional follow mode (line 141960) | Record `NSWorkspace.frontmostApplication` before an action and reactivate it after; a per-worker "follow" toggle in the dashboard | S |
| 10 | **Mac as a device reachable from any surface** | Device tunnel announces tools and consent protocol; phone chats act on the Mac (line 25262) | amux already has one server; add the Mac tools to the worker tool list and let the phone PWA approve consents. Mostly wiring once 1 and 4 land | S-M | Medium-high |
| 11 | Sign-in import from local browsers, per site | Imports cookies from Chrome, Arc, Brave, Edge, Firefox, per chosen site; one-way Chrome sync (section 3) | `amux browser import --from chrome --site x.com --profile p` reading Chrome's cookie DB with Keychain decryption, writing into a Playwright profile | M | Medium (amux has profiles via manual login: **partial have**) |
| 12 | Folder work under Seatbelt | `sandbox-exec` profile: writes only in the folder, network off unless allowed (lines 93242-93400) | Optional `AMUX_SANDBOX=seatbelt` per worker: wrap the agent's shell in `sandbox-exec` with a generated profile scoped to its worktree | M | Medium |
| 13 | Quick Ask panel over any app | ⌥Space non-activating chat panel | NSPanel with the composer, sends to a chosen worker or Chat; amux has the hotkey and panel code from pinned notes | S | Medium |
| 14 | Attach the window you were in / selected text | "+" menu, From your screen | In the Mac app: capture the previous frontmost window with ScreenCaptureKit and selected text via AX, attach to the composer | S | Medium |
| 15 | Permissions page with live status | Each permission with On / Not set up / Can't tell and Turn On | Settings pane listing Accessibility, Screen Recording, Microphone, Notifications with `AXIsProcessTrusted`, `CGPreflightScreenCaptureAccess` and deep links | S | Medium (onboarding for #1) |
| 16 | Browser tool names that match Claude in Chrome | Same names as Anthropic's extension tools (line 62184) | Offer the same verb names on `amux browser route` so models trained on them work out of the box | S | Medium |
| 17 | Page subscriptions (watch the person's clicks with secret redaction) | `page_subscription` with secret-field regex (line 11384) | Teach-by-demonstration for amux browser profiles | M | Low-medium |
| 18 | Dictation, meetings, auto-update, Dock badge | Present | **Have** | n/a | n/a |
| 19 | Office editors | Full Excel/Word/PowerPoint editors | Out of scope for amux; the file viewer is enough | L | Low |

Things not to copy: Athena's Electron fuses leave `RunAsNode` and inspector flags on with no asar integrity check, so any local process can run code with Athena's Accessibility and Screen Recording rights. amux's Swift app avoids this by construction; keep it that way when adding a helper (hardened runtime, no `--inspect`-style escape, verify the helper's code signature before talking to it, as `athena-chrome-bridge` does for Chrome).

---

## Side effects of this investigation

- Restarting Athena for CDP access let Squirrel apply its pending update: Athena went from 0.1.749 to 0.1.836. Static analysis is of 0.1.749.
- Athena was relaunched a second time at the end **without** the debugging port; it is running normally (verified: port 9333 closed, plain command line).
- Three new chats now exist in the owner's Athena account ("Calculate 12 × 34 on Mac", "Khruangbin Spotify search", "Example.com page title and sentence").
- Spotify's search box now holds "Khruangbin"; nothing played and no setting changed.
- Calculator never launched; no other app was opened. No macOS permission was granted or changed.
- `~/Library/Application Support` was not read.
