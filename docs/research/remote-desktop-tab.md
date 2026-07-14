# Research: Remote-desktop tab for amux — top 3 options + integration architectures

*Deep-research report, 2026-07-13. 5 search angles, 21 primary sources fetched, 103 claims
extracted, top 25 adversarially verified with 3-vote panels: 23 confirmed, 2 refuted.
**Research only — nothing was built or integrated.***

---

## The structural fact that frames everything

**On macOS, every browser-gateway option (Guacamole, noVNC, Kasm) speaks plain RFB/VNC to Apple's
built-in Screen Sharing on :5900.** None of them install their own capture agent, so **latency and
image quality on the Mac are capped by Apple's VNC server no matter which gateway you pick**
(verified 3-0 against Kasm, noVNC and RustDesk docs). The gateways therefore differentiate on
**client UX, auth embedding, and sidecar footprint — not on pixels.**

The one exception is **RustDesk**, which ships its own macOS capture agent with modern codecs — at
the cost of interactive TCC permission grants that *cannot* be automated (Apple's PPPC/MDM payloads
cannot pre-allow ScreenCapture; `tccutil` can only reset, not grant; macOS Sequoia adds recurring
screen-capture re-approval).

**Practical consequence:** don't pick based on hoped-for codec quality. Pick based on mobile UX and
how cleanly it hides behind amux's auth. If Apple's VNC pixels turn out to be unacceptable on the
iPhone, that's the *only* reason to escalate to RustDesk.

---

## #1 — Apache Guacamole *(best UX; heaviest sidecar)* — **recommended**

Clientless HTML5, explicitly embeddable via **`guacamole-common-js`** (Apache documents "writing
your own Guacamole application"). Apache **advises against iframe embedding** — keyboard-focus
problems — and recommends direct JS integration, which is exactly how amux's inline-JS dashboard is
built. Licence: **Apache-2.0** (safe inside MIT + Commons Clause).

It has the **best verified mobile story of any candidate** — the deciding factor for the iPhone PWA:
- **Touch mouse-emulation modes:** absolute (tap = left-click, press-and-hold = right-click) and
  relative touchpad mode (two-finger tap = right-click, three-finger = middle-click, two-finger
  drag = scroll).
- **Built-in on-screen keyboard** that sends keys to the remote host without affecting the local
  device — this is the thing noVNC structurally cannot do (see below).
- **Text-input mode with IME support** (CJK).

### Integration architecture
- **Sidecar:** `guacd` (C daemon) + the Guacamole webapp, as a Docker sidecar on the Mac or any
  tailnet node. Connection configured as **VNC → `localhost:5900`** (macOS Screen Sharing enabled).
- **Tab:** amux serves `guacamole-common-js` as a static asset; a `remote-desktop` view instantiates
  the JS client directly into the tab's DOM (**no iframe**). The WebSocket tunnel is proxied
  *through the amux Python server*, so it's same-origin over the existing Tailscale HTTPS listener —
  no second cert, no second port to trust on the phone.
- **Auth:** the **`json-auth` extension**. amux's Python server signs (HMAC-SHA-256) and encrypts
  (AES-128) a JSON blob asserting the user + the connection, POSTs it to Guacamole's `/api/tokens`,
  and hands the returned token to the embedded JS client. Guacamole's external-auth docs describe
  this exact scenario verbatim: *"for cases where Guacamole is embedded within an external
  application that performs its own authentication."* **Users never see a Guacamole login.**

### Risks (verified)
- **macOS ARD/VNC auth fragility is recurring, not theoretical.** GUACAMOLE-1133: libvncclient
  picked Apple's proprietary security type 30 (ARD) and failed auth with correct credentials —
  macOS VNC was broken out-of-the-box across 1.2.x–1.3.x (~18 months) until 1.4.0.
- **iOS touch regressed as recently as 1.6.0** (GUACAMOLE-2080), fixed in 1.6.1.
  → **Pin to ≥ 1.6.1** and treat the Apple-VNC auth path as a fragility to smoke-test on upgrade.
- **Heaviest footprint:** a C daemon + a Java/Tomcat webapp. Mitigation worth evaluating:
  **`guacamole-lite`** (Node) reimplements the WebSocket tunnel for apps that already have their own
  auth — it would drop the Java webapp while keeping `guacamole-common-js` and amux-issued auth.

---

## #2 — noVNC + websockify *(best architectural fit; weakest mobile keyboard)*

By far the **lightest** and most amux-shaped option: *"The most basic deployment consists of simply
serving these files from a web server and setting up a WebSocket proxy to the VNC server."*
noVNC is a **core JS library** — `new RFB(targetElement, wsUrl)` attaches a live canvas to any DOM
element, no iframe. It supports **Apple Diffie-Hellman auth**, so it can talk to macOS Screen
Sharing directly. Licence **MPL-2.0** (file-level copyleft — fine inside MIT/Commons-Clause).

### Integration architecture
- **Sidecar:** `websockify` (**Python** — runs as an amux child process, or the WS→TCP bridge gets
  reimplemented inside amux's existing raw HTTP server, killing the sidecar entirely).
  Bridges `wss://…` → `localhost:5900`.
- **Tab:** amux serves noVNC's `core/` modules as static assets and instantiates `RFB` in the tab.
- **Auth:** websockify ships **auth plugins** (`--auth-plugin CLASS --auth-source ARG`, with stock
  `BasicHTTPAuth` / `ExpectOrigin` / `ClientCertCN` and a documented custom-plugin extension point).
  A ~10-line custom plugin validates amux session tokens before the WebSocket handshake — so the
  proxy is never an open relay.

### The one disqualifying-ish weakness (verified live on GitHub, 2026-07-13)
**No gesture or button to summon the on-screen keyboard on touch devices** — issue #1501, open since
2020-12-16, labelled `patchwelcome`, milestone "Future Features". The maintainer confirms this is
**protocol-level**: RFB carries no widget/focus semantics, so a pure framebuffer renderer cannot know
a text field was focused (*"noVNC simply displays the image the VNC server sends us"*). Mobile users
fall back to a toolbar keyboard toggle. **Guacamole solves this by shipping its own OSK UI layer** —
which is precisely why it wins on the iPhone criterion.

---

## #3 — RustDesk self-hosted *(only real codec upgrade; most caveats)*

The **only** candidate that bypasses the Apple-VNC quality cap, because it installs its **own macOS
capture agent** with modern codecs (WebCodecs VP8/VP9/H.264/H.265 in the web client). Fully
self-hostable (`hbbs`/`hbbr` ID+relay servers on a tailnet node), satisfying the no-cloud rule.

### Verified caveats — all three matter
- **Web client V2 is "Preview"**, **relay-only from browsers** (no P2P), and **not fully
  open-sourced**; open issues question its reliability against a *non-Pro* OSS server
  (rustdesk-server #491, rustdesk #10773).
- **Server is AGPL-3.0** → must stay a **separate process, never vendored** into amux's codebase.
- **macOS host onboarding cannot be headless:** move to `/Applications`, clear Gatekeeper, grant
  Accessibility + Screen Recording (sometimes Input Monitoring) TCC permissions by hand.

**Architecture if chosen:** `hbbs`/`hbbr` on a tailnet node, native RustDesk agent on the Mac, web
client served/linked from an amux tab pointed at the self-hosted server.

---

## Ruled out (with reasons)

- **KasmVNC — disqualified.** (a) **No macOS server exists** — Kasm reaches Macs only via plain VNC
  to Apple Remote Management, so its adaptive web-native rendering never applies to the primary host;
  (b) **its web client does not support Safari** for direct connections (Safari lacks Basic Auth on
  WebSockets — WebKit bug #80362, still reproducing on iOS 26) — a **hard iPhone-PWA blocker**;
  (c) seamless clipboard is Chromium-only. Its one edge (opt-in WebRTC UDP) is Linux-host-only.
- **Selkies — Linux/X11 only** (macOS "planned"). Genuinely embeddable and WebRTC-based, so it's a
  strong **per-host upgrade for the secondary Linux box**, behind the same amux tab. (Note: its
  "60fps @ 1080p" claim was **refuted 1-2** in verification — do not rely on it.)
- **MeshCentral, Sunshine/Moonlight-web:** produced **no claims that survived verification** — their
  absence reflects *missing evidence*, not confirmed disqualification.

---

## Recommendation

**Go with Guacamole (≥1.6.1), embedded via `guacamole-common-js` + `json-auth`.** The criteria were
weighted toward UX, and since Guacamole and noVNC traverse the *identical* Apple VNC pipe on macOS,
their latency/quality should be near-identical — so the tiebreaker is **client UX vs sidecar
weight**, and the iPhone on-screen-keyboard gap is a *structural* noVNC limitation, not a bug that
will be fixed.

**But there's a cheap way to de-risk it:** noVNC + websockify is a ~half-day spike that answers the
one thing no source could (see open questions). Ship it first as a throwaway to *measure the real
thing*; if Apple's VNC pixels over Tailscale on the iPhone are unusable, then no gateway saves you
and the decision jumps straight to RustDesk — and you'll have learned that for a fraction of the cost
of standing up guacd+Tomcat.

---

## Open questions no source could answer (measure these)

1. **Real latency/frame-rate of macOS Screen Sharing's RFB stream through websockify/guacd over
   Tailscale on an iPhone** — and can macOS 14+'s *High Performance (H.264)* screen-sharing mode be
   reached by any third-party VNC client, or is it Apple-client-only? *(This is the single most
   decision-relevant unknown.)*
2. Does noVNC's Apple-DH auth still work against **current** macOS (Sequoia/Tahoe) Screen Sharing, or
   must the Mac be downgraded to legacy "VNC viewers may control screen with password" mode — and
   what security level does that imply on a Tailscale-only network?
3. Does RustDesk's web client V2 work end-to-end against a **self-hosted OSS (non-Pro)** server?
4. What is Guacamole's actual sidecar footprint on a personal Mac, and can `guacamole-lite` replace
   the Java webapp while keeping `guacamole-common-js` + amux-issued auth?

## Caveats on this report

All version facts are as of **2026-07-13** (Guacamole 1.6.x, noVNC 1.7.0, websockify 0.13.0,
KasmVNC 1.4.0, RustDesk web client V2 "Preview", Selkies v1.6.2). **No claim quantifying real
latency/frame-rate/codec quality on the macOS path survived verification** — the H.264-vs-raw-VNC
question was answered *structurally* (all gateways capped by Apple's RFB server), never
*empirically*. Guacamole's mobile evidence documents **feature existence, not polish** (known
qualifications: iOS needs a focused input for the native keyboard — hence the built-in OSK;
GUACAMOLE-447 external-keyboard keys on iOS Safari). Resource-footprint comparisons
(guacd+Tomcat vs websockify) were **not verified by any source**.
