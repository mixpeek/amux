import Cocoa
import WebKit
import Carbon.HIToolbox
import UserNotifications

// The native macOS app: dashboard windows and tabs on the system WebKit (no
// bundled browser), pinned notes as floating panels, the menu bar readout, a
// Dock badge and notifications for "needs you", a global hotkey and amux://
// links, all in ONE process. The pinned overlay (scripts/pinned-overlay.swift)
// and the menu bar item (scripts/amux-menubar.swift) are compiled into this
// binary with -D AMUX_APP; their standalone builds are unchanged.

// ── Persistent config ──
let configURL: URL = {
    // A second copy (tests, a second server) can keep its own state.
    if let p = ProcessInfo.processInfo.environment["AMUX_DESKTOP_CONFIG"], !p.isEmpty {
        return URL(fileURLWithPath: p)
    }
    let dir = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent(".amux")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir.appendingPathComponent("desktop-config.json")
}()

struct Connection: Codable {
    var name: String
    var url: String
}

/// Which server a page is on, as a color for its tab (Ethan, 2026-10-25: "put
/// a color code based on the location/connection (so if its localhost/desktop
/// vs sandbox, etc.) in the tabs"). This Mac is green, amux cloud (a sandbox
/// on *.amux.io) blue, and any other server keeps one color of its own, picked
/// from its host so it never changes between launches.
struct ConnectionBadge {
    let color: NSColor
    let name: String
    let host: String

    private static var cache: [String: ConnectionBadge] = [:]

    static func of(_ url: URL?) -> ConnectionBadge? {
        guard let url = url, let host = url.host, url.scheme == "https" || url.scheme == "http" else { return nil }
        let key = host + ":" + String(url.port ?? 0)
        if let hit = cache[key] { return hit }
        let named = loadConfig().connections.first { c in
            URL(string: c.url).map { $0.host == host && ($0.port ?? 0) == (url.port ?? 0) } ?? false
        }?.name
        let local = ["localhost", "127.0.0.1", "::1"].contains(host) || host.hasSuffix(".local")
            || host == ProcessInfo.processInfo.hostName
        let color: NSColor
        if local {
            color = .systemGreen
        } else if host == "amux.io" || host.hasSuffix(".amux.io") {
            color = .systemBlue
        } else {
            let palette: [NSColor] = [.systemOrange, .systemPurple, .systemPink, .systemTeal, .systemYellow, .systemIndigo, .systemBrown]
            var h: UInt32 = 5381
            for b in host.utf8 { h = (h &* 33) ^ UInt32(b) }
            color = palette[Int(h % UInt32(palette.count))]
        }
        let badge = ConnectionBadge(color: color, name: named ?? (local ? "This Mac" : host), host: host)
        cache[key] = badge
        return badge
    }

    /// The dot a tab shows beside its title.
    func dot() -> NSView {
        let v = NSView(frame: NSRect(x: 0, y: 0, width: 8, height: 8))
        v.wantsLayer = true
        v.layer?.cornerRadius = 4
        v.layer?.backgroundColor = color.cgColor
        v.toolTip = "Connected to \(name) (\(host))"
        v.identifier = NSUserInterfaceItemIdentifier("amux-connection-" + host)
        v.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([v.widthAnchor.constraint(equalToConstant: 8), v.heightAnchor.constraint(equalToConstant: 8)])
        return v
    }
}

struct SavedWindow: Codable {
    var url: String
    var frame: String
    var tabGroup: Int?
    // Added 2026-10-21 (amux-workspace.swift); optional so older files decode.
    var selected: Bool?        // the visible tab of its group
    var display: String?       // the monitor's UUID
    var displayName: String?
    var rel: String?           // frame as fractions of that display's usable area
    var space: String?         // the macOS Desktop's Space UUID
    var spaceIndex: Int?       // its number on that display (Desktop 1, 2, ...)
    var allDesktops: Bool?
    var minimized: Bool?
    var fullscreen: Bool?
    // The tab's name when it was saved ("mixpeek-general", "Board"), shown in
    // Workspaces and on the tab until its page loads.
    var title: String?
    // Added 2026-10-23 (amux-panes.swift): the window split into panes.
    var grid: SavedGrid?
    // Added 2026-10-25: a name the owner gave the tab (right-click > Rename Tab).
    var name: String?
}

struct Config: Codable {
    var connections: [Connection]
    var lastUrl: String
    // Added 2026-10-19; optional so older desktop-config.json files still decode.
    var windows: [SavedWindow]?
    var menuBar: Bool?
    var notifications: Bool?
    var dockBadge: Bool?
    var layouts: [Layout]?
    // "last" (default), "new", or "layout:<name>"
    var launch: String?
    var snapToGrid: Bool?
    var openAtLogin: Bool?
    // The workspace open now; switching away saves where you left it.
    var activeWorkspace: String?
    // false while the app runs; still false at launch means it did not quit cleanly.
    var cleanExit: Bool?
    // View > Zoom All Windows (2026-10-28). Unset means on.
    var zoomAllWindows: Bool?
}

/// One line in the app's log: NSLog, which reaches the file below too.
func dlog(_ s: String) { NSLog("amux-desktop: %@", s) }

/// Launched from Finder the app's stderr is /dev/null, and its NSLog lines did
/// not reach the unified log either: a whole session (2026-10-22) left no
/// amux-desktop line anywhere, so a drag that failed could not say where. Under
/// Finder they now go to ~/.amux/logs/desktop-app.log (kept under 5 MB, one
/// previous file). A terminal or a test harness keeps its own stderr.
func routeStderrToLogFile() {
    var st = stat(), nul = stat()
    guard fstat(2, &st) == 0, stat("/dev/null", &nul) == 0,
          (st.st_mode & S_IFMT) == S_IFCHR, st.st_rdev == nul.st_rdev else { return }
    let dir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".amux/logs")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let path = dir.appendingPathComponent("desktop-app.log").path
    if let size = (try? FileManager.default.attributesOfItem(atPath: path))?[.size] as? Int, size > 5 << 20 {
        try? FileManager.default.removeItem(atPath: path + ".1")
        try? FileManager.default.moveItem(atPath: path, toPath: path + ".1")
    }
    if freopen(path, "a", stderr) != nil { setvbuf(stderr, nil, _IOLBF, 0) }
    dlog("verdict=log_routed path=\(path)")
}

func loadConfig() -> Config {
    guard let data = try? Data(contentsOf: configURL),
          let config = try? JSONDecoder().decode(Config.self, from: data) else {
        return Config(connections: [
            Connection(name: "localhost", url: "https://localhost:8824")
        ], lastUrl: "https://localhost:8824")
    }
    return config
}

func saveConfig(_ config: Config) {
    let enc = JSONEncoder()
    enc.outputFormatting = [.prettyPrinted, .sortedKeys]
    if let data = try? enc.encode(config) {
        try? data.write(to: configURL, options: .atomic)
    }
}

func normalized(_ url: String) -> String {
    url.replacingOccurrences(of: "/+$", with: "", options: .regularExpression)
}

/// Self-signed certificates are trusted for local and private-network hosts,
/// which is where an amux server lives.
func isLocalHost(_ host: String) -> Bool {
    host == "localhost" || host == "127.0.0.1" || host.hasSuffix(".local")
        || host.hasPrefix("10.") || host.hasPrefix("192.168.")
        || (host.hasPrefix("172.") && (Int(host.split(separator: ".").dropFirst().first ?? "") ?? 0) / 16 == 1)
}


// ── Connect page HTML ──
func connectPageHTML(_ config: Config) -> String {
    let connsJson = (try? JSONEncoder().encode(config.connections))
        .flatMap { String(data: $0, encoding: .utf8) } ?? "[]"
    return """
    <!DOCTYPE html>
    <html>
    <head>
    <meta charset="utf-8">
    <style>
      * { box-sizing: border-box; margin: 0; padding: 0; }
      body {
        font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif;
        background: #0d1117; color: #e6edf3;
        display: flex; align-items: center; justify-content: center;
        min-height: 100vh;
      }
      .container { width: 380px; }
      h1 { font-size: 1.6rem; font-weight: 700; margin-bottom: 6px; letter-spacing: -0.02em; }
      .subtitle { color: #8b949e; font-size: 0.85rem; margin-bottom: 28px; }
      .input-row { display: flex; gap: 8px; margin-bottom: 20px; }
      input[type="text"] {
        flex: 1; padding: 10px 14px; border-radius: 8px;
        border: 1px solid #30363d; background: #161b22; color: #e6edf3;
        font-size: 0.9rem; outline: none;
      }
      input[type="text"]:focus { border-color: #58a6ff; }
      .btn {
        padding: 10px 20px; border-radius: 8px; border: none;
        background: #238636; color: #fff; font-size: 0.9rem; font-weight: 600; cursor: pointer;
      }
      .btn:hover { background: #2ea043; }
      .divider { height: 1px; background: #21262d; margin: 4px 0 16px; }
      .label { font-size: 0.75rem; color: #8b949e; text-transform: uppercase; letter-spacing: 0.04em; margin-bottom: 10px; }
      .conn-item {
        display: flex; align-items: center; gap: 10px; padding: 10px 12px;
        border-radius: 8px; cursor: pointer; border: 1px solid transparent;
      }
      .conn-item:hover { background: #161b22; border-color: #30363d; }
      .conn-info { flex: 1; min-width: 0; }
      .conn-name { font-size: 0.9rem; font-weight: 600; }
      .conn-url { font-size: 0.78rem; color: #8b949e; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
      .conn-last { font-size: 0.7rem; color: #3fb950; }
      .conn-remove { background: none; border: none; color: #8b949e; cursor: pointer; font-size: 0.85rem; padding: 4px; }
      .conn-remove:hover { color: #f85149; }
      .empty { color: #8b949e; font-size: 0.85rem; }
    </style>
    </head>
    <body>
    <div class="container">
      <h1>amux</h1>
      <div class="subtitle">Connect to an amux server</div>
      <div class="input-row">
        <input type="text" id="url" placeholder="https://localhost:8824"
          value="\(config.lastUrl)" autofocus
          onkeydown="if(event.key==='Enter')doConnect()">
        <button class="btn" onclick="doConnect()">Connect</button>
      </div>
      <div class="divider"></div>
      <div class="label">Recent connections</div>
      <div id="list"></div>
    </div>
    <script>
      const conns = \(connsJson);
      const lastUrl = "\(config.lastUrl)";
      function render() {
        const el = document.getElementById('list');
        if (!conns.length) { el.innerHTML = '<div class="empty">No saved connections</div>'; return; }
        el.innerHTML = conns.map((c, i) => {
          const isLast = c.url.replace(/\\/+$/, '') === lastUrl.replace(/\\/+$/, '');
          return '<div class="conn-item" onclick="go(\\'' + c.url + '\\')">' +
            '<div class="conn-info"><div class="conn-name">' + esc(c.name) + '</div>' +
            '<div class="conn-url">' + esc(c.url) + '</div></div>' +
            (isLast ? '<span class="conn-last">last used</span>' : '') +
            '<button class="conn-remove" onclick="event.stopPropagation();rm(' + i + ')" title="Remove">&#x2715;</button></div>';
        }).join('');
      }
      function esc(s) { return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/'/g,'&#39;'); }
      function doConnect() {
        let url = document.getElementById('url').value.trim();
        if (!url) url = 'https://localhost:8824';
        if (!/^https?:\\/\\//.test(url)) url = 'https://' + url;
        go(url);
      }
      function go(url) { window.location = 'amux-connect://' + btoa(url); }
      function rm(i) {
        conns.splice(i, 1);
        window.location = 'amux-remove://' + i;
        render();
      }
      render();
    </script>
    </body>
    </html>
    """
}

// What the dashboard is showing, as a deep link another window (or the next
// launch) can open: the open worker and its tab (#peek=x&tab=chat), an open
// card (#issue=), an open file or folder (#path=), a worker's browser
// (#browser=), or the current tab (#view=). Every form is one the dashboard's
// own deep-link router (_handleDeeplink) already opens.
let viewHashBody = """
  try {
    const peek = document.getElementById('peek-overlay');
    if (peek && peek.classList.contains('active') && typeof peekSession !== 'undefined' && peekSession) {
      let t = '';
      try { if (typeof _peekTab !== 'undefined' && _peekTab) t = '&tab=' + encodeURIComponent(_peekTab); } catch (e) {}
      return '#peek=' + encodeURIComponent(peekSession) + t;
    }
    const bd = document.getElementById('board-detail-overlay');
    if (bd && bd.classList.contains('active') && typeof boardDetailId !== 'undefined' && boardDetailId)
      return '#issue=' + encodeURIComponent(boardDetailId);
    // An open file is saved by its real path: the title can read as a
    // markdown heading, "name (new)" or "name • cached 3m".
    const fo = document.getElementById('file-overlay');
    if (fo && fo.classList.contains('active')) {
      let p = '';
      try { if (typeof _fileData !== 'undefined' && _fileData && !_fileData._isNew && _fileData.path) p = _fileData.path; } catch (e) {}
      p = p || fo.dataset.path || '';
      if (p.startsWith('/')) return '#path=' + encodeURIComponent(p);
    }
    const mo = document.getElementById('mdai-overlay');
    if (mo && mo.classList.contains('active')) {
      try { if (typeof _mdaiCur !== 'undefined' && _mdaiCur && _mdaiCur.abs) return '#path=' + encodeURIComponent(_mdaiCur.abs); } catch (e) {}
    }
    if (typeof activeView !== 'undefined' && activeView) {
      try { if (activeView === 'files' && _filesPath && _filesPath !== '/') return '#path=' + encodeURIComponent(_filesPath); } catch (e) {}
      try { if (activeView === 'browser' && _bwSession) return '#browser=' + encodeURIComponent(_bwSession); } catch (e) {}
      // A hidden tab's drawing waits in pendingOpen until the editor mounts.
      try { if (activeView === 'draw') { const d = _drawS(), dp = (d && (d.path || d.pendingOpen)) || ''; if (dp.startsWith('/')) return '#path=' + encodeURIComponent(dp); } } catch (e) {}
      return '#view=' + encodeURIComponent(activeView);
    }
  } catch (e) {}
  return '';
"""
let currentViewJS = "(() => {\n" + viewHashBody + "\n})()"

// Names the window after what it is showing, so the Window menu and the
// Dock's right-click list read "mixpeek-general" or "Board" rather than a
// column of identical "amux" entries. Posts only when the label changes.
let windowLabelJS = """
(() => {
  let last = null;
  const label = () => {
    try {
      const peek = document.getElementById('peek-overlay');
      if (peek && peek.classList.contains('active') && typeof peekSession !== 'undefined' && peekSession) return peekSession;
      if (typeof activeView !== 'undefined' && activeView) {
        const tab = document.getElementById('tab-' + activeView);
        const t = tab ? tab.textContent.replace(/[^\\p{L}\\p{N} &'-]/gu, ' ').replace(/\\s+/g, ' ').trim() : '';
        return t || activeView;
      }
    } catch (e) {}
    return '';
  };
  const view = () => {
""" + viewHashBody + """
  };
  let lastView = null;
  const tick = () => {
    const l = label(), v = view();
    if (l !== last || v !== lastView) {
      last = l; lastView = v;
      window.webkit.messageHandlers.amuxWindowLabel.postMessage({label: l, view: v});
    }
  };
  setInterval(tick, 1000);
  document.addEventListener('click', () => setTimeout(tick, 50), true);
  window.addEventListener('hashchange', tick);
})();
"""

/// A script message handler holds its target strongly; this breaks the cycle
/// between the web view's content controller and its window controller.
class WeakScriptHandler: NSObject, WKScriptMessageHandler {
    weak var target: WKScriptMessageHandler?
    init(_ target: WKScriptMessageHandler) { self.target = target }
    func userContentController(_ c: WKUserContentController, didReceive message: WKScriptMessage) {
        target?.userContentController(c, didReceive: message)
    }
}

// ── A dashboard window ──
class DashboardWindowController: NSWindowController, NSWindowDelegate, WKNavigationDelegate, WKUIDelegate, WKDownloadDelegate, WKScriptMessageHandler {
    var pageLabel = ""
    // What the page shows, posted by windowLabelJS; saved with the window.
    var viewHash = ""
    /// When this page was last on screen (the memory guard reloads only pages
    /// that have been out of sight a while).
    var lastShown = Date()
    var lastMemoryReload = Date.distantPast
    // The last amux page this window showed, kept while it shows the connect page.
    var lastServerURL: URL?

    /// The amux server this tab is on (scheme, host and port): the page it shows,
    /// or the last one it showed while it sits on the connect page.
    var serverOrigin: URL? {
        let page = webView.url.flatMap { ($0.scheme == "https" || $0.scheme == "http") ? $0 : nil } ?? lastServerURL
        guard let u = page, let scheme = u.scheme, let host = u.host else { return nil }
        let h = host.contains(":") ? "[\(host)]" : host
        return URL(string: "\(scheme)://\(h)" + (u.port.map { ":\($0)" } ?? ""))
    }
    /// A restored tab that is not showing waits to load (launch speed, 2026-10-25):
    /// the shown tabs load first, then these one at a time, or at once when picked.
    var deferredURL: URL?

    func loadDeferred(_ why: String) {
        guard let u = deferredURL else { return }
        deferredURL = nil
        dlog("verdict=deferred_tab_loaded why=\(why) url=\(u.fragment ?? "")")
        load(u)
    }

    func windowDidBecomeKey(_ notification: Notification) { loadDeferred("selected") }
    // Where you last put it (amux-workspace.swift).
    var home: SavedWindow?
    var lastSpace: Spaces.Info?
    var normalFrame: NSRect?
    // amux-panes.swift: the panes this window shows, or the window whose grid
    // this controller's page is a pane of (its own window is hidden then).
    var grid: PaneGrid?
    weak var paneHost: DashboardWindowController?
    let webView: WKWebView
    var titleObservation: NSKeyValueObservation?
    var themeObservation: NSKeyValueObservation?
    weak var app: AppDelegate?

    init(app: AppDelegate, frame: NSRect?, configuration: WKWebViewConfiguration? = nil) {
        self.app = app
        let cfg: WKWebViewConfiguration
        if let configuration = configuration {
            cfg = configuration
        } else {
            cfg = WKWebViewConfiguration()
            // Every window shares the default data store (and, since macOS 12,
            // one web content process pool), so you sign in once.
            cfg.websiteDataStore = .default()
            cfg.preferences.setValue(true, forKey: "developerExtrasEnabled")
            cfg.mediaTypesRequiringUserActionForPlayback = []
        }
        // A configuration handed over by window.open already carries the
        // script from the window that opened it.
        if !cfg.userContentController.userScripts.contains(where: { $0.source == windowLabelJS }) {
            cfg.userContentController.addUserScript(WKUserScript(source: windowLabelJS, injectionTime: .atDocumentEnd, forMainFrameOnly: true))
        }
        // WebKit's own user agent with the app's name after it. A custom agent of
        // only "Amux-Desktop/2.0" hid that this is WebKit, so the dashboard took
        // its CSS-zoom path, and at 110% the spreadsheet editor selected the
        // wrong cell (2026-10-26; the page's zoom-mode beacon read apple_webkit:false).
        cfg.applicationNameForUserAgent = "Amux-Desktop/2.0"
        webView = WKWebView(frame: .zero, configuration: cfg)
        webView.allowsMagnification = true
        webView.allowsBackForwardNavigationGestures = true

        let window = NSWindow(
            contentRect: frame ?? DashboardWindowController.defaultFrame(),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        // A saved frame is the whole window. As a content rect it gained a
        // title bar's height (32 pt) on every launch.
        if let frame = frame { window.setFrame(frame, display: false) }
        window.title = "amux"
        window.backgroundColor = NSColor(red: 13/255, green: 17/255, blue: 23/255, alpha: 1)
        window.minSize = NSSize(width: 480, height: 400)
        window.tabbingIdentifier = "amux"
        // The title bar takes the dashboard's own colour: the page sets
        // <meta name="theme-color"> to match its light or dark mode.
        window.titlebarAppearsTransparent = true
        window.isReleasedWhenClosed = false
        // amux saves and restores its own windows (with their Desktops);
        // AppKit's restoration would open a second, blank set.
        window.isRestorable = false
        window.collectionBehavior.insert(.fullScreenPrimary)
        window.contentView = webView
        super.init(window: window)
        window.delegate = self
        cfg.userContentController.removeScriptMessageHandler(forName: "amuxWindowLabel")
        cfg.userContentController.add(WeakScriptHandler(self), name: "amuxWindowLabel")
        // The page's Split button (app.js _nativeSplit): this page again in a
        // new pane on the right half.
        cfg.userContentController.removeScriptMessageHandler(forName: "amuxPane")
        cfg.userContentController.add(WeakScriptHandler(self), name: "amuxPane")
        // The Record tab drives the native recorder (amux-recorder.swift).
        cfg.userContentController.removeScriptMessageHandler(forName: "amuxRecorder")
        cfg.userContentController.add(WeakScriptHandler(self), name: "amuxRecorder")
        // Messages notifications (app.js _cvMaybeNotify): native, so they show
        // when the window is hidden and click through to the conversation.
        cfg.userContentController.removeScriptMessageHandler(forName: "amuxNotify")
        cfg.userContentController.add(WeakScriptHandler(self), name: "amuxNotify")
        // A zoom from the page (app.js _zoomShare): every other page follows it.
        cfg.userContentController.removeScriptMessageHandler(forName: "amuxZoom")
        cfg.userContentController.add(WeakScriptHandler(self), name: "amuxZoom")
        webView.navigationDelegate = self
        webView.uiDelegate = self
        themeObservation = webView.observe(\.themeColor, options: [.initial, .new]) { [weak self] wv, _ in
            self?.applyTheme(wv.themeColor)
        }
        titleObservation = webView.observe(\.title, options: [.new]) { [weak self] wv, _ in
            self?.updateTitle()
        }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func userContentController(_ c: WKUserContentController, didReceive message: WKScriptMessage) {
        if message.name == "amuxZoom" {
            if let n = message.body as? NSNumber { app?.zoomOthers(from: self, level: n.intValue) }
            return
        }
        if message.name == "amuxNotify" {
            if webView.url.map({ app?.isServerURL($0) ?? false }) ?? false, let d = message.body as? [String: Any] {
                app?.notifyMessage(d)
            }
            return
        }
        if message.name == "amuxRecorder" {
            // Only a page on this tab's own server drives the recorder.
            if webView.url.map({ app?.isServerURL($0) ?? false }) ?? false {
                app?.recorderMessage(message.body as? [String: Any] ?? [:], from: self)
            }
            return
        }
        if message.name == "amuxPane" {
            let d = message.body as? [String: Any]
            guard (d?["op"] as? String) == "split" else { return }
            // Only a page on this tab's own server; anything else splits the
            // page this tab is showing.
            let asked = (d?["url"] as? String).flatMap(URL.init(string:))
            let same = asked.map { u in
                u.scheme == webView.url?.scheme && u.host == webView.url?.host && u.port == webView.url?.port
            } ?? false
            app?.splitPane(self, url: same ? asked : webView.url)
            return
        }
        guard message.name == "amuxWindowLabel" else { return }
        if let d = message.body as? [String: Any] {
            pageLabel = (d["label"] as? String) ?? ""
            viewHash = (d["view"] as? String) ?? ""
        } else {
            pageLabel = (message.body as? String) ?? ""
        }
        updateTitle()
    }

    /// The page's own name ("mixpeek-general", "Board").
    /// A name the owner gave this tab (Ethan, 2026-10-25: "right click a tab
    /// and name it"). It wins over the page's own name until cleared, and is
    /// saved with the window and its workspace.
    var customName: String?

    var pageTitle: String {
        if let n = customName, !n.isEmpty { return n }
        let page = (webView.title ?? "").trimmingCharacters(in: .whitespaces)
        return !pageLabel.isEmpty ? pageLabel : (page.isEmpty ? "amux" : page)
    }

    func rename(_ name: String?) {
        let n = name?.trimmingCharacters(in: .whitespacesAndNewlines)
        customName = (n?.isEmpty ?? true) ? nil : n
        dlog("verdict=tab_renamed named=\(customName == nil ? 0 : 1) view=\(viewHash)")
        updateTitle()
        (paneHost ?? self).updateTitle()
        app?.saveWindowsSoon()
    }

    func updateTitle() {
        window?.title = pageTitle
        // The tab's dot says which server this page is on.
        if let w = window, let badge = ConnectionBadge.of(webView.url ?? lastServerURL),
           w.tab.accessoryView?.identifier?.rawValue != "amux-connection-" + badge.host {
            w.tab.accessoryView = badge.dot()
            w.tab.toolTip = pageTitle + " · " + badge.name
        }
        // A grid's window is named by its panes, each pane by its page.
        if let host = paneHost ?? (grid != nil ? self : nil), let g = host.grid {
            g.refreshStrips()
            host.window?.title = g.tree.leaves.compactMap { g.members[$0]?.pageTitle }.joined(separator: " | ")
        }
    }

    func applyTheme(_ color: NSColor?) {
        guard let window = window, let c = color?.usingColorSpace(.sRGB) else { return }
        window.backgroundColor = c
        let luminance = 0.2126 * c.redComponent + 0.7152 * c.greenComponent + 0.0722 * c.blueComponent
        // Dark page, dark title bar text and traffic-light chrome; and back.
        window.appearance = NSAppearance(named: luminance < 0.5 ? .darkAqua : .aqua)
    }

    static func defaultFrame() -> NSRect {
        let screen = NSScreen.main?.visibleFrame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        let width: CGFloat = min(1440, screen.width * 0.85)
        let height: CGFloat = min(900, screen.height * 0.85)
        return NSRect(x: screen.minX + (screen.width - width) / 2,
                      y: screen.minY + (screen.height - height) / 2,
                      width: width, height: height)
    }

    func load(_ url: URL) {
        webView.load(URLRequest(url: url, cachePolicy: .useProtocolCachePolicy, timeoutInterval: 15))
    }

    func showConnectPage() {
        webView.loadHTMLString(connectPageHTML(loadConfig()), baseURL: nil)
    }

    // ── NSWindowDelegate ──
    /// ⌘W in a window split into panes closes the focused pane's tab
    /// (amux-panes.swift); the window's close button still closes the window.
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        if let g = grid, g.all.count > 1, NSApp.currentEvent?.type == .keyDown,
           let id = g.focused ?? g.tree.leaves.first, let c = g.members[id] {
            app?.undockPane(c, to: .close)
            return false
        }
        return true
    }

    func windowWillClose(_ notification: Notification) {
        titleObservation = nil
        themeObservation = nil
        if grid != nil { app?.gridHostClosed(self) }
        app?.windowClosed(self)
    }

    func windowWillMove(_ notification: Notification) { app?.dragBegan(self) }
    // macOS also live-resizes a window it moves off an unplugged display;
    // only a resize that began with the mouse down is yours.
    var userResize = false
    func windowWillStartLiveResize(_ notification: Notification) { userResize = NSEvent.pressedMouseButtons & 1 != 0 }
    func windowDidEndLiveResize(_ notification: Notification) {
        if userResize { app?.resizeEnded(self) } else { app?.windowPlaced(self) }
        userResize = false
    }
    func windowDidMove(_ notification: Notification) {
        if NSEvent.pressedMouseButtons & 1 == 0 { app?.windowPlaced(self) }
    }
    func windowDidResize(_ notification: Notification) {
        if !(window?.inLiveResize ?? false) { app?.windowPlaced(self) }
    }
    func windowDidChangeScreen(_ notification: Notification) { app?.windowPlaced(self) }
    func windowDidMiniaturize(_ notification: Notification) { app?.saveWindowsSoon() }
    func windowDidDeminiaturize(_ notification: Notification) { app?.saveWindowsSoon() }
    func windowWillEnterFullScreen(_ notification: Notification) { normalFrame = window?.frame }
    func windowDidEnterFullScreen(_ notification: Notification) { app?.saveWindowsSoon() }
    func windowDidExitFullScreen(_ notification: Notification) { normalFrame = nil; app?.windowPlaced(self) }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        // Any amux server this tab shows, not only the default connection: a tab on
        // sandbox.amux.io must remember sandbox, or its new tabs and its saved place
        // fall back to the default server (Ethan, 2026-10-26).
        if let u = webView.url, u.scheme == "https" || u.scheme == "http" { lastServerURL = u }
        app?.loadNextDeferred(after: self)
    }

    // The tab bar's "+" button and File > New Tab.
    @objc override func newWindowForTab(_ sender: Any?) {
        app?.newTab(nextTo: self)
    }

    // ── WKNavigationDelegate ──
    func webView(_ webView: WKWebView,
                 didReceive challenge: URLAuthenticationChallenge,
                 completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        let host = challenge.protectionSpace.host
        if challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
           isLocalHost(host) || host == app?.serverURL?.host,
           let trust = challenge.protectionSpace.serverTrust {
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            completionHandler(.performDefaultHandling, nil)
        }
    }

    func webView(_ webView: WKWebView,
                 decidePolicyFor navigationAction: WKNavigationAction,
                 decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = navigationAction.request.url else {
            decisionHandler(.allow)
            return
        }
        if url.scheme == "amux-connect" {
            if let b64 = url.host, let data = Data(base64Encoded: b64),
               let serverUrl = String(data: data, encoding: .utf8) {
                app?.connect(to: serverUrl, in: self)
            }
            decisionHandler(.cancel)
            return
        }
        if url.scheme == "amux-remove" {
            if let idxStr = url.host, let idx = Int(idxStr) {
                var config = loadConfig()
                if idx < config.connections.count {
                    config.connections.remove(at: idx)
                    saveConfig(config)
                }
                showConnectPage()
            }
            decisionHandler(.cancel)
            return
        }
        if url.scheme == "amux" {
            app?.handleAmuxURL(url)
            decisionHandler(.cancel)
            return
        }
        // A download first: a page saving a file (an <a download> on a blob: or
        // data: URL, as the file viewer's Download does) is not a link to
        // another site. Checked after the link rule, a blob: URL is not a server
        // URL, so it went to macOS, which offered to find an app for
        // "blob:https://..." (Ethan, 2026-10-26 screenshot).
        if navigationAction.shouldPerformDownload {
            dlog("verdict=download_started url_scheme=\(url.scheme ?? "-")")
            decisionHandler(.download)
            return
        }
        // Links off the amux server open in the default browser. blob: and
        // data: belong to this page, never to another app.
        if navigationAction.navigationType == .linkActivated, !(app?.isServerURL(url) ?? true),
           url.scheme != "blob", url.scheme != "data" {
            NSWorkspace.shared.open(url)
            decisionHandler(.cancel)
            return
        }
        decisionHandler(.allow)
    }

    func webView(_ webView: WKWebView,
                 decidePolicyFor navigationResponse: WKNavigationResponse,
                 decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void) {
        let disposition = (navigationResponse.response as? HTTPURLResponse)?
            .value(forHTTPHeaderField: "Content-Disposition") ?? ""
        if !navigationResponse.canShowMIMEType || disposition.lowercased().hasPrefix("attachment") {
            decisionHandler(.download)
        } else {
            decisionHandler(.allow)
        }
    }

    func webView(_ webView: WKWebView, navigationAction: WKNavigationAction, didBecome download: WKDownload) {
        download.delegate = self
    }

    func webView(_ webView: WKWebView, navigationResponse: WKNavigationResponse, didBecome download: WKDownload) {
        download.delegate = self
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        app?.loadFailed(in: self, error: error)
    }

    // A crashed web content process leaves a blank window; reload it.
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        NSLog("amux-desktop: verdict=webcontent_terminated url=%@", webView.url?.absoluteString ?? "-")
        webView.reload()
    }

    // ── WKDownloadDelegate: into ~/Downloads, never overwriting ──
    func download(_ download: WKDownload, decideDestinationUsing response: URLResponse,
                  suggestedFilename: String, completionHandler: @escaping (URL?) -> Void) {
        // AMUX_DESKTOP_DOWNLOADS_DIR: where a test copy saves (download-e2e.py).
        let dir = ProcessInfo.processInfo.environment["AMUX_DESKTOP_DOWNLOADS_DIR"].map { URL(fileURLWithPath: $0) }
            ?? FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first!
        var dest = dir.appendingPathComponent(suggestedFilename)
        let base = dest.deletingPathExtension().lastPathComponent
        let ext = dest.pathExtension
        var n = 1
        while FileManager.default.fileExists(atPath: dest.path) {
            dest = dir.appendingPathComponent(ext.isEmpty ? "\(base) \(n)" : "\(base) \(n).\(ext)")
            n += 1
        }
        completionHandler(dest)
    }

    func downloadDidFinish(_ download: WKDownload) {
        dlog("verdict=download_finished")
        NSSound(named: "Glass")?.play()
    }

    func download(_ download: WKDownload, didFailWithError error: Error, resumeData: Data?) {
        dlog("verdict=download_failed error=\(error.localizedDescription)")
        let a = NSAlert()
        a.messageText = "The download did not finish"
        a.informativeText = error.localizedDescription
        a.runModal()
    }

    // ── WKUIDelegate ──
    // window.open, target=_blank and "Open Link in New Window": a page on the
    // amux server opens as another amux window; anything else in the browser.
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let url = navigationAction.request.url, let app = app {
            if app.isServerURL(url) {
                dlog("verdict=page_opened_window url=\(url.absoluteString) from=\(webView.url?.absoluteString ?? "-")")
                let c = app.openWindow(url: nil, frame: nil, configuration: configuration)
                return c.webView
            }
            // A blob: or data: URL opened in a new window (target=_blank) is a
            // file this page made: save it, rather than asking macOS for an app.
            if url.scheme == "blob" || url.scheme == "data" {
                dlog("verdict=download_started url_scheme=\(url.scheme ?? "-") via=new_window")
                webView.startDownload(using: navigationAction.request) { $0.delegate = self }
                return nil
            }
            NSWorkspace.shared.open(url)
        }
        return nil
    }

    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void) {
        let a = NSAlert()
        a.messageText = message
        a.addButton(withTitle: "OK")
        a.beginSheetModal(for: sheetWindow) { _ in completionHandler() }
    }

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        let a = NSAlert()
        a.messageText = message
        a.addButton(withTitle: "OK")
        a.addButton(withTitle: "Cancel")
        a.beginSheetModal(for: sheetWindow) { r in completionHandler(r == .alertFirstButtonReturn) }
    }

    func webView(_ webView: WKWebView, runJavaScriptTextInputPanelWithPrompt prompt: String,
                 defaultText: String?, initiatedByFrame frame: WKFrameInfo,
                 completionHandler: @escaping (String?) -> Void) {
        let a = NSAlert()
        a.messageText = prompt
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 280, height: 24))
        field.stringValue = defaultText ?? ""
        a.accessoryView = field
        a.addButton(withTitle: "OK")
        a.addButton(withTitle: "Cancel")
        a.beginSheetModal(for: sheetWindow) { r in completionHandler(r == .alertFirstButtonReturn ? field.stringValue : nil) }
    }

    // <input type="file">: uploads and attachments.
    func webView(_ webView: WKWebView, runOpenPanelWith parameters: WKOpenPanelParameters,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping ([URL]?) -> Void) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = parameters.allowsMultipleSelection
        panel.canChooseDirectories = parameters.allowsDirectories
        panel.canChooseFiles = true
        panel.beginSheetModal(for: sheetWindow) { r in completionHandler(r == .OK ? panel.urls : nil) }
    }

    // Voice and recording features ask for the microphone; allow it for the
    // amux server only (macOS still asks you once for the app itself).
    @available(macOS 12.0, *)
    func webView(_ webView: WKWebView, requestMediaCapturePermissionFor origin: WKSecurityOrigin,
                 initiatedByFrame frame: WKFrameInfo, type: WKMediaCaptureType,
                 decisionHandler: @escaping (WKPermissionDecision) -> Void) {
        decisionHandler(origin.host == app?.serverURL?.host ? .grant : .prompt)
    }
}

// ── Global hotkey (Carbon: no Accessibility permission needed) ──
var hotKeyHandler: (() -> Void)?
func registerHotKey() {
    var ref: EventHotKeyRef?
    let id = EventHotKeyID(signature: OSType(0x616D7578), id: 1)   // 'amux'
    // Control-Option-Space toggles amux from any app.
    RegisterEventHotKey(UInt32(kVK_Space), UInt32(controlKey | optionKey), id, GetApplicationEventTarget(), 0, &ref)
    // Control-Option-R starts or stops a recording from any app (amux-recorder.swift).
    var recRef: EventHotKeyRef?
    RegisterEventHotKey(UInt32(kVK_ANSI_R), UInt32(controlKey | optionKey), EventHotKeyID(signature: OSType(0x616D7578), id: 2),
                        GetApplicationEventTarget(), 0, &recRef)
    var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
    InstallEventHandler(GetApplicationEventTarget(), { _, event, _ in
        var hk = EventHotKeyID()
        GetEventParameter(event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID), nil,
                          MemoryLayout<EventHotKeyID>.size, nil, &hk)
        DispatchQueue.main.async { if hk.id == 2 { recordHotKeyHandler?() } else { hotKeyHandler?() } }
        return noErr
    }, 1, &spec, nil, nil)
}

// ── App delegate ──
class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate, UNUserNotificationCenterDelegate {
    var config = loadConfig()
    var controllers: [DashboardWindowController] = []
    var overlays: [OverlayController] = []
    var menuBar: MenuBarController?
    var pins: [[String: Any]] = []
    var pinsMenu = NSMenu(title: "Pins")
    var needsKeys: Set<String>? = nil
    var needsTimer: Timer?
    var pinsTimer: Timer?
    var terminating = false
    var lastNeedsCount = 0
    var settings: SettingsWindowController?
    let layoutsMenu = NSMenu(title: "Workspaces")
    // amux-workspace.swift
    var restoring = false
    var screenGrace = Date.distantPast
    var placementTimer: Timer?
    var lastSignature = ""
    var sigtermSource: DispatchSourceSignal?
    var sigtermQuitting = false
    var dragTimer: Timer?
    var tabDragTimer: Timer?
    var tabDragMonitor: Any?
    var tearWatchTimer: Timer?
    var tabGroupSeen: [ObjectIdentifier: (tabs: Int, frame: NSRect)] = [:]
    var lastTabPlaced = Date.distantPast
    var actionGrid: PaneGrid? = nil
    lazy var snapPreview = SnapPreview()
    // What covered the pointer the last time a drop found no amux window (amux-panes.swift).
    var paneBlocker: String?

    func applyBadge() {
        let show = config.dockBadge ?? true
        NSApp.dockTile.badgeLabel = show && lastNeedsCount > 0 ? String(lastNeedsCount) : nil
    }

    @objc func toggleDockBadge(_ sender: Any?) {
        config = loadConfig()
        config.dockBadge = !(config.dockBadge ?? true)
        saveConfig(config)
        applyBadge()
    }

    var serverURL: URL? {
        config.connections.isEmpty ? nil : URL(string: normalized(config.lastUrl))
    }

    /// The saved connection `url` names (same scheme, host and port), if any.
    func knownConnection(_ url: URL) -> URL? {
        func key(_ u: URL) -> String? {
            guard let scheme = u.scheme?.lowercased(), scheme == "https" || scheme == "http", let host = u.host?.lowercased() else { return nil }
            return "\(scheme)://\(host):\(u.port ?? (scheme == "https" ? 443 : 80))"
        }
        guard let want = key(url) else { return nil }
        return config.connections.compactMap { URL(string: normalized($0.url)) }.first { key($0) == want }
    }

    func isServerURL(_ url: URL) -> Bool {
        guard let s = serverURL else { return false }
        return url.host == s.host && (url.port ?? (url.scheme == "https" ? 443 : 80)) == (s.port ?? (s.scheme == "https" ? 443 : 80))
    }

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSAppleEventManager.shared().setEventHandler(
            self, andSelector: #selector(handleURLEvent(_:withReply:)),
            forEventClass: AEEventClass(kInternetEventClass), andEventID: AEEventID(kAEGetURL))
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        routeStderrToLogFile()
        NSWindow.allowsAutomaticWindowTabbing = true
        buildMenuBar(self)
        NotificationCenter.default.addObserver(forName: .amuxOverlayClosed, object: nil, queue: .main) { [weak self] n in
            self?.overlays.removeAll { $0 === (n.object as AnyObject) }
        }
        // A layout link at launch replaces the windows anyway; skip opening them first.
        let launchLinks = CommandLine.arguments.dropFirst().compactMap { URL(string: $0) }.filter { $0.scheme == "amux" }
        if config.cleanExit == false {
            NSLog("amux-desktop: verdict=restore_after_unclean_exit windows=%d", config.windows?.count ?? 0)
        }
        if !launchLinks.contains(where: { $0.host == "layout" || $0.host == "workspace" }) { launchWindows() }
        config = loadConfig()
        config.cleanExit = false
        saveConfig(config)
        startWorkspaceTracking()
        startTabDragTracking()
        applyOpenAtLogin()
        hotKeyHandler = { [weak self] in self?.toggleVisible() }
        registerHotKey()
        setUpRecorder()
        startMenuBarItem()
        setUpNotifications()
        refreshPins()
        pinsTimer = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in self?.refreshPins() }
        pollNeedsInput()
        needsTimer = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in self?.pollNeedsInput() }
        DispatchQueue.main.asyncAfter(deadline: .now() + 90) { [weak self] in self?.reportResources() }
        resourcesTimer = Timer.scheduledTimer(withTimeInterval: 600, repeats: true) { [weak self] _ in self?.reportResources() }
        memoryGuardTimer = Timer.scheduledTimer(withTimeInterval: envSeconds("AMUX_DESKTOP_MEM_TICK_S", 60), repeats: true) { [weak self] _ in self?.guardTabMemory() }
        NSApp.activate(ignoringOtherApps: true)
        // `amux amux://worker/x` on the command line, once the pins are known.
        let links = CommandLine.arguments.dropFirst().compactMap { URL(string: $0) }.filter { $0.scheme == "amux" }
        if !links.isEmpty {
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in links.forEach { self?.handleAmuxURL($0) } }
        }
    }

    // ── Windows ──
    @discardableResult
    func openWindow(url: URL?, frame: NSRect?, configuration: WKWebViewConfiguration? = nil, activate: Bool = true, load: Bool = true) -> DashboardWindowController {
        let c = DashboardWindowController(app: self, frame: frame ?? cascadeFrame(), configuration: configuration)
        controllers.append(c)
        // Every new window or tab says so, so a tab nobody asked for is traceable.
        dlog("verdict=window_opened url=\(url?.absoluteString ?? (configuration != nil ? "page-window.open" : load ? "server" : "deferred")) windows=\(controllers.count)")
        if !load {
        } else if let url = url { c.load(url) } else if configuration == nil {
            if let s = serverURL { c.load(s) } else { c.showConnectPage() }
        }
        if activate {
            c.showWindow(nil)
            c.window?.makeKeyAndOrderFront(nil)
        } else {
            c.window?.orderFront(nil)
        }
        return c
    }

    func cascadeFrame() -> NSRect {
        guard let key = (NSApp.keyWindow ?? controllers.last?.window), key.isVisible else {
            return DashboardWindowController.defaultFrame()
        }
        return key.frame.offsetBy(dx: 28, dy: -28)
    }

    /// A new tab opens on the worker list (Ethan, 2026-10-22). The bare server
    /// address restores the page's last view, which is usually the worker the
    /// tab beside it already shows.
    /// A new tab opens on the server of the tab it was opened from (Ethan,
    /// 2026-10-26: "if i open a new tab from an active one that has a certain
    /// connection (like sandbox) it should open the tab in that"); only a tab on
    /// no server falls back to the default connection.
    func newTab(nextTo c: DashboardWindowController, hash: String = "#view=sessions") {
        let n = DashboardWindowController(app: self, frame: c.window?.frame, configuration: nil)
        controllers.append(n)
        let u = url(withHash: hash, on: c.serverOrigin)
        dlog("verdict=new_tab_server server=\(u.flatMap { $0.host } ?? "-") from_tab=\(c.serverOrigin != nil)")
        if let u = u { n.load(u) } else { n.showConnectPage() }
        if let w = c.window, let nw = n.window {
            w.addTabbedWindow(nw, ordered: .above)
            nw.makeKeyAndOrderFront(nil)
        }
    }

    func windowClosed(_ c: DashboardWindowController) {
        if terminating { return }
        controllers.removeAll { $0 === c }
        saveWindowsSoon()
    }

    var keyController: DashboardWindowController? {
        controllers.first { $0.window == NSApp.keyWindow } ?? controllers.last
    }

    /// The page you are in: the key window's, or in a grid (amux-panes.swift)
    /// the pane you last clicked. Reload, zoom and the like act on it.
    var keyPage: DashboardWindowController? {
        guard let c = keyController else { return nil }
        if let g = c.grid, let fr = c.window?.firstResponder as? NSView,
           let m = g.members.values.first(where: { fr === $0.webView || fr.isDescendant(of: $0.webView) }) { return m }
        return c
    }

    /// `hash` on server `base`, or on the default connection when `base` is nil.
    func url(withHash hash: String, on base: URL? = nil) -> URL? {
        guard let s = base ?? serverURL else { return nil }
        return URL(string: normalized(s.absoluteString) + "/" + hash)
    }

    /// Each window's view as a deep link and its place (amux-workspace.swift),
    /// so a relaunch reopens the same workers and tabs where they were.
    func snapshotWindows(_ done: @escaping ([SavedWindow]) -> Void) { done(snapshotNow()) }

    var saveWork: DispatchWorkItem?
    func saveWindowsSoon() {
        saveWork?.cancel()
        let w = DispatchWorkItem { [weak self] in
            self?.snapshotWindows { windows in
                guard let self = self, !self.terminating, !self.restoring else { return }
                var c = loadConfig().merging(windows: windows)
                c.cleanExit = false
                // The open workspace keeps where you left it.
                if let name = c.activeWorkspace, var ls = c.layouts, let i = ls.firstIndex(where: { $0.name == name }) {
                    ls[i].windows = windows
                    ls[i].pins = self.snapshotPins()
                    c.layouts = ls
                }
                self.config = c
                saveConfig(c)
            }
        }
        saveWork = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 1, execute: w)
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        if !recorderAllowsQuit(fromSignal: sigtermQuitting) { return .terminateCancel }
        snapshotWindows { [weak self] windows in
            guard let self = self else { return }
            self.terminating = true
            var c = loadConfig().merging(windows: windows)
            c.cleanExit = true
            if let name = c.activeWorkspace, var ls = c.layouts, let i = ls.firstIndex(where: { $0.name == name }) {
                ls[i].windows = windows
                ls[i].pins = self.snapshotPins()
                c.layouts = ls
            }
            self.config = c
            saveConfig(c)
            NSLog("amux-desktop: verdict=windows_saved_on_quit n=%d", windows.count)
            NSApp.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }

    // amux stays in the Dock and menu bar with no window open, like Mail.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !controllers.contains(where: { $0.window?.isVisible == true }) { openWindow(url: nil, frame: nil) }
        return true
    }

    func toggleVisible() {
        if NSApp.isActive, NSApp.keyWindow != nil {
            NSApp.hide(nil)
        } else {
            NSApp.activate(ignoringOtherApps: true)
            if !controllers.contains(where: { $0.window?.isVisible == true }) { openWindow(url: nil, frame: nil) }
        }
    }

    // ── Connecting ──
    func connect(to urlString: String, in c: DashboardWindowController) {
        var url = normalized(urlString)
        if !url.hasPrefix("http") { url = "https://" + url }
        config = loadConfig()
        config.lastUrl = url
        if !config.connections.contains(where: { normalized($0.url) == url }) {
            config.connections.append(Connection(name: URL(string: url)?.host ?? url, url: url))
        }
        saveConfig(config)
        if let u = URL(string: url) { c.load(u) }
        startMenuBarItem()
        refreshPins()
        needsKeys = nil
        pollNeedsInput()
    }

    // The server restarts on every deploy; retry a failed load for a minute
    // before showing the connect page.
    var retries: [ObjectIdentifier: Int] = [:]
    func loadFailed(in c: DashboardWindowController, error: Error) {
        let key = ObjectIdentifier(c)
        let n = retries[key, default: 0]
        guard let target = serverURL, n < 30 else {
            retries[key] = 0
            NSLog("amux-desktop: verdict=server_unreachable error=%@", error.localizedDescription)
            c.showConnectPage()
            return
        }
        retries[key] = n + 1
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak c] in
            guard let c = c else { return }
            c.load(c.webView.url ?? target)
        }
    }

    // ── amux:// links ──
    // amux://worker/<name>  amux://view/<tab>  amux://open#<hash>
    // amux://new-window     amux://pin/<id>     amux://settings
    // amux://workspace/<name>  amux://save-workspace/<name> (also layout/, save-layout/)
    // amux://arrange/<left|right|top|bottom|fill|center|tile|stack|next-display>  amux://quit
    @objc func handleURLEvent(_ event: NSAppleEventDescriptor, withReply reply: NSAppleEventDescriptor) {
        guard let s = event.paramDescriptor(forKeyword: keyDirectObject)?.stringValue, let url = URL(string: s) else { return }
        handleAmuxURL(url)
    }

    func handleAmuxURL(_ url: URL) {
        NSLog("amux-desktop: amux_url=%@ server=%@", url.absoluteString, serverURL?.absoluteString ?? "-")
        if handleTestURL(url) { return }
        NSApp.activate(ignoringOtherApps: true)
        let arg = url.path.trimmingCharacters(in: CharacterSet(charactersIn: "/")).removingPercentEncoding ?? ""
        let enc = arg.addingPercentEncoding(withAllowedCharacters: .alphanumerics.union(CharacterSet(charactersIn: "-_."))) ?? arg
        switch url.host ?? "" {
        case "worker":
            // Already open somewhere (any Desktop): go to it instead of opening a duplicate.
            if let c = controllers.first(where: { $0.viewHash.hasPrefix("#peek=" + enc) && ($0.viewHash.count == 6 + enc.count || $0.viewHash.dropFirst(6 + enc.count).hasPrefix("&")) }),
               let w = c.paneHost?.window ?? c.window {
                // A pane is shown in its grid's window; its own window is hidden.
                if w.isMiniaturized { w.deminiaturize(nil) }
                w.tabGroup?.selectedWindow = w
                w.makeKeyAndOrderFront(nil)
                // In a pane, maybe behind another of its tabs: that tab in front.
                if c.inGrid != nil { reveal(c); w.makeFirstResponder(c.webView) }
            } else {
                openWindow(url: self.url(withHash: "#peek=" + enc), frame: nil)
            }
        case "view": openWindow(url: self.url(withHash: "#view=" + enc), frame: nil)
        case "open":
            // amux://open?server=<origin>#path=<file> comes from a copied file link
            // opened in a browser (deep_links.rs, app.js _offerOpenInMacApp). Any web
            // page can fire an amux:// link, so only a server already among your
            // connections is honoured; anything else opens on the default server.
            let asked = URLComponents(url: url, resolvingAgainstBaseURL: false)?
                .queryItems?.first(where: { $0.name == "server" })?.value.flatMap(URL.init(string:))
            let server = asked.flatMap { knownConnection($0) }
            if asked != nil { dlog("verdict=\(server != nil ? "open_link_known_server" : "open_link_unknown_server_refused") server=\(asked?.host ?? "-")") }
            openWindow(url: self.url(withHash: url.fragment.map { "#" + $0 } ?? "", on: server), frame: nil)
        case "pin": if let id = Int(arg) { openPin(id: id) }
        case "layout", "workspace": openLayout(named: arg)
        case "save-layout", "save-workspace": saveLayout(named: arg)
        case "arrange":
            let actions: [String: Selector] = [
                "left": #selector(arrangeLeftHalf(_:)), "right": #selector(arrangeRightHalf(_:)),
                "top": #selector(arrangeTopHalf(_:)), "bottom": #selector(arrangeBottomHalf(_:)),
                "fill": #selector(arrangeFill(_:)), "center": #selector(arrangeCenter(_:)),
                "tile": #selector(arrangeTile(_:)), "stack": #selector(arrangeStack(_:)),
                "next-display": #selector(arrangeNextDisplay(_:)),
            ]
            if let sel = actions[arg] { perform(sel, with: nil) }
        case "quit": NSApp.terminate(nil)
        case "settings": showSettings(nil)
        default: openWindow(url: nil, frame: nil)
        }
    }

    // ── Resource report (Ethan, 2026-10-23: "macos app is still so much slower
    // than browser") ──
    // Every native tab is its own WebKit process running the whole dashboard,
    // and the page cannot see its own memory in WebKit. So the app measures
    // what each tab's process holds, with the machine's swap and memory
    // pressure, and sends it every 10 minutes as kind=desktop-resources
    // through a dashboard page (the page's beacon path needs no token). A
    // verdict of memory_pressure says the tabs are pushing this Mac into swap.
    var resourcesTimer: Timer?
    var memoryGuardTimer: Timer?
    var tabMenuMonitor: Any?
    var lastDuplicateWarned = Set<String>()
    weak var tabMenuTarget: DashboardWindowController?
    var deferredTimer: Timer?

    func envSeconds(_ key: String, _ fallback: Double) -> Double {
        ProcessInfo.processInfo.environment[key].flatMap(Double.init) ?? fallback
    }

    /// On screen: the selected tab of a visible window, or a pane of one.
    func isShown(_ c: DashboardWindowController) -> Bool {
        let host = c.paneHost?.window ?? c.window
        return (host?.isVisible ?? false) && (host?.tabGroup?.selectedWindow ?? host) === host
            && (c.paneHost == nil || c.webView.window != nil)
    }

    // ── Memory guard (Ethan, 2026-10-25: "reduce footprint ... with zero
    // degradation of capability") ──
    // A page normally settles at 150-500 MB, but the desktop-resources reports
    // show single tabs at 1.8, 3.2 and 6.3 GB. A page that has been out of
    // sight for a while and is over the limit is reloaded on the same URL: its
    // view, worker and tab come back from the URL, drafts from local storage,
    // and the shown page is never touched. Each reload is logged and reported.
    func guardTabMemory() {
        let limit = Int(envSeconds("AMUX_DESKTOP_TAB_MB_LIMIT", 1536))
        let grace = envSeconds("AMUX_DESKTOP_TAB_HIDDEN_S", 120)
        let now = Date()
        for c in controllers {
            if isShown(c) { c.lastShown = now; continue }
            let hidden = now.timeIntervalSince(c.lastShown)
            // At most one reload per page per cooldown: a page that grows straight
            // back is a leak to find (its view is in the log), not one to churn.
            guard hidden >= grace, now.timeIntervalSince(c.lastMemoryReload) >= envSeconds("AMUX_DESKTOP_TAB_RELOAD_COOLDOWN_S", 600),
                  let pid = webPID(c), let mb = footprintMB(pid), mb > limit,
                  let url = c.webView.url, isServerURL(url) else { continue }
            c.lastMemoryReload = now
            dlog("verdict=tab_reloaded_for_memory mb=\(mb) limit=\(limit) hidden_s=\(Int(hidden)) view=\(c.viewHash)")
            let payload: [String: Any] = ["kind": "desktop-tab-reloaded", "verdict": "tab_reloaded_for_memory", "measured": true,
                                          "n_considered": 1, "mb": mb, "limit_mb": limit, "hidden_s": Int(hidden), "view": c.viewHash]
            if let data = try? JSONSerialization.data(withJSONObject: payload), let json = String(data: data, encoding: .utf8),
               let r = controllers.first(where: { $0 !== c && ($0.webView.url.map(isServerURL) ?? false) }) {
                r.webView.evaluateJavaScript("fetch('/api/client-debug',{method:'POST',headers:{'Content-Type':'application/json'},keepalive:true,body:JSON.stringify(\(json))}).catch(()=>{}); 0", completionHandler: nil)
            }
            // The saved URL carries what the page had open (a file, a drawing, a
            // worker's tab); the web view's own URL may have dropped that hash.
            c.webView.load(URLRequest(url: c.savedURL().flatMap { URL(string: $0) } ?? url))
        }
    }

    func footprintMB(_ pid: pid_t) -> Int? {
        var info = rusage_info_v2()
        let r = withUnsafeMutablePointer(to: &info) {
            $0.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) { proc_pid_rusage(pid, RUSAGE_INFO_V2, $0) }
        }
        return r == 0 ? Int(info.ri_phys_footprint / 1_048_576) : nil
    }

    /// CPU seconds a process has used. proc_pid_rusage counts in mach time units,
    /// which are not nanoseconds on Apple silicon.
    func cpuSeconds(_ pid: pid_t) -> Double? {
        var info = rusage_info_v2()
        let r = withUnsafeMutablePointer(to: &info) {
            $0.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) { proc_pid_rusage(pid, RUSAGE_INFO_V2, $0) }
        }
        guard r == 0 else { return nil }
        var tb = mach_timebase_info_data_t()
        mach_timebase_info(&tb)
        let ticks = Double(info.ri_user_time + info.ri_system_time)
        return ticks * Double(tb.numer) / Double(tb.denom) / 1e9
    }

    /// One page's web process, when WebKit exposes it.
    func webPID(_ c: DashboardWindowController) -> pid_t? {
        let key = NSSelectorFromString("_webProcessIdentifier")
        guard c.webView.responds(to: key), let pid = (c.webView.value(forKey: "_webProcessIdentifier") as? NSNumber)?.int32Value, pid > 0 else { return nil }
        return pid
    }

    func reportResources() {
        let pidKey = NSSelectorFromString("_webProcessIdentifier")
        var seen = Set<pid_t>(), tabs: [[String: Any]] = [], total = 0, shown = 0
        for c in controllers {
            let w = c.window
            let visible = (w?.isVisible ?? false) && (w?.occlusionState.contains(.visible) ?? false)
            if visible { shown += 1 }
            var row: [String: Any] = ["shown": visible ? 1 : 0, "view": c.viewHash]
            if c.webView.responds(to: pidKey), let pid = (c.webView.value(forKey: "_webProcessIdentifier") as? NSNumber)?.int32Value, pid > 0 {
                if let mb = footprintMB(pid) {
                    row["mb"] = mb
                    if seen.insert(pid).inserted { total += mb }
                }
            }
            tabs.append(row)
        }
        var swap = xsw_usage(), size = MemoryLayout<xsw_usage>.size
        let swapMB = sysctlbyname("vm.swapusage", &swap, &size, nil, 0) == 0 ? Int(swap.xsu_used / 1_048_576) : -1
        var level: Int32 = 0, lsize = MemoryLayout<Int32>.size
        let pressure = sysctlbyname("kern.memorystatus_vm_pressure_level", &level, &lsize, nil, 0) == 0 ? Int(level) : -1
        let ramMB = Int(ProcessInfo.processInfo.physicalMemory / 1_048_576)
        let measured = !seen.isEmpty
        let payload: [String: Any] = [
            "kind": "desktop-resources", "measured": measured, "n_considered": tabs.count,
            "why_unmeasured": measured ? "" : "no tab exposed its web process",
            "tabs": tabs.count, "shown": shown, "web_processes": seen.count, "web_mb_total": total,
            "tab_mb": tabs.compactMap { $0["mb"] as? Int }, "ram_mb": ramMB, "swap_used_mb": swapMB,
            // Which page each figure is, so a 6 GB tab names its view (2026-10-25).
            "tab_views": tabs.filter { $0["mb"] != nil }.map { ($0["view"] as? String) ?? "" },
            "pressure_level": pressure, "app_mb": footprintMB(getpid()) ?? -1,
            "verdict": pressure >= 2 ? "memory_pressure" : (measured ? "ok" : "unmeasured"),
        ]
        NSLog("amux-desktop: desktop-resources tabs=%d web_mb_total=%d swap_used_mb=%d pressure=%d", tabs.count, total, swapMB, pressure)
        guard let data = try? JSONSerialization.data(withJSONObject: payload), let json = String(data: data, encoding: .utf8),
              let c = controllers.first(where: { $0.webView.url.map(isServerURL) ?? false }) else { return }
        c.webView.evaluateJavaScript("fetch('/api/client-debug',{method:'POST',headers:{'Content-Type':'application/json'},keepalive:true,body:JSON.stringify(\(json))}).catch(()=>{}); 0", completionHandler: nil)
    }

    // ── Native API calls (pins, the dock badge, the menu bar) ──
    // The API wants the owner token, which the dashboard shell hands each page
    // as window._AMUX_AUTH_TOKEN. URLSession shares none of WebKit's state, so
    // these calls borrow it from an open dashboard. Without it a remote server
    // answered every one of them 401 missing_credential (2026-10-22: about seven
    // a minute from one laptop, so pins and the badge never loaded).
    var apiToken: String?
    var unauthorized = UnauthorizedBackoff()

    func withAPIToken(_ go: @escaping (String?) -> Void) {
        if let t = apiToken { go(t); return }
        guard let c = controllers.first(where: { $0.webView.url.map(isServerURL) ?? false }) else { go(nil); return }
        c.webView.evaluateJavaScript("window._AMUX_AUTH_TOKEN || ''") { [weak self] v, _ in
            let t = (v as? String).flatMap { $0.isEmpty ? nil : $0 }
            self?.apiToken = t
            go(t)
        }
    }

    func api(_ path: String, method: String = "GET", _ done: @escaping (Any?) -> Void) {
        guard let s = serverURL, let url = URL(string: normalized(s.absoluteString) + path) else { done(nil); return }
        withAPIToken { [weak self] token in
            // A poll that would repeat a refused call waits out the backoff;
            // a new token (the owner signed in) goes through at once.
            if method == "GET", self?.unauthorized.holds(token) ?? false { done(nil); return }
            var req = URLRequest(url: url)
            req.httpMethod = method
            req.timeoutInterval = 8
            if let token = token { req.setValue("Bearer " + token, forHTTPHeaderField: "Authorization") }
            let session = URLSession(configuration: .ephemeral, delegate: InsecureDelegate(), delegateQueue: nil)
            session.dataTask(with: req) { data, resp, _ in
                let status = (resp as? HTTPURLResponse)?.statusCode ?? 0
                let obj = status == 401 ? nil : data.flatMap { try? JSONSerialization.jsonObject(with: $0) }
                DispatchQueue.main.async {
                    // A refused token is stale (the owner rotated it): drop it so
                    // the next call reads the dashboard's current one.
                    if status == 401 {
                        let wait = self?.unauthorized.refused(token) ?? 0
                        NSLog("amux-desktop: native_api_unauthorized verdict=native_api_backoff path=%@ had_token=%d wait_s=%d", path, token == nil ? 0 : 1, wait)
                        self?.apiToken = nil
                    } else if (200..<300).contains(status) {
                        self?.unauthorized.reset()
                    }
                    done(obj)
                }
                session.finishTasksAndInvalidate()
            }.resume()
        }
    }

    func refreshPins(then: (() -> Void)? = nil) {
        api("/api/pinned") { [weak self] obj in
            guard let self = self else { return }
            if let rows = obj as? [[String: Any]] { self.pins = rows }
            self.rebuildPinsMenu()
            then?()
        }
    }

    func rebuildPinsMenu() {
        pinsMenu.removeAllItems()
        var seenGroups = Set<String>()
        for p in pins {
            guard let id = p["id"] as? Int else { continue }
            if let g = p["group_id"] as? String, !g.isEmpty {
                if seenGroups.contains(g) { continue }
                seenGroups.insert(g)
            }
            let path = (p["file_path"] as? String) ?? ""
            var title = (p["title"] as? String) ?? ""
            if title.isEmpty { title = (path as NSString).lastPathComponent }
            if let g = p["group_id"] as? String, !g.isEmpty {
                let n = pins.filter { ($0["group_id"] as? String) == g }.count
                if n > 1 { title += " (\(n) tabs)" }
            }
            let item = NSMenuItem(title: title, action: #selector(pinMenuItem(_:)), keyEquivalent: "")
            item.target = self
            item.tag = id
            item.toolTip = path
            pinsMenu.addItem(item)
        }
        if pinsMenu.items.isEmpty {
            let none = NSMenuItem(title: "No pinned notes", action: nil, keyEquivalent: "")
            none.isEnabled = false
            pinsMenu.addItem(none)
        }
        pinsMenu.addItem(.separator())
        let all = NSMenuItem(title: "Show All Pins", action: #selector(showAllPins), keyEquivalent: "")
        all.target = self
        pinsMenu.addItem(all)
        let hide = NSMenuItem(title: "Close All Pins", action: #selector(closeAllPins), keyEquivalent: "")
        hide.target = self
        pinsMenu.addItem(hide)
        let manage = NSMenuItem(title: "Manage Pins…", action: #selector(managePins), keyEquivalent: "")
        manage.target = self
        pinsMenu.addItem(manage)
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        if menu === pinsMenu { refreshPins() }
    }

    @objc func pinMenuItem(_ sender: NSMenuItem) { openPin(id: sender.tag) }

    func openPin(id: Int, refreshed: Bool = false) {
        guard let row = pins.first(where: { ($0["id"] as? Int) == id }) else {
            if refreshed {
                NSLog("amux-desktop: verdict=pin_not_found id=%d n_considered=%d", id, pins.count)
            } else {
                refreshPins { [weak self] in self?.openPin(id: id, refreshed: true) }
            }
            return
        }
        var paths: [String] = []
        if let g = row["group_id"] as? String, !g.isEmpty {
            paths = pins.filter { ($0["group_id"] as? String) == g }
                .sorted { (($0["created_at"] as? String) ?? "") < (($1["created_at"] as? String) ?? "") }
                .compactMap { $0["file_path"] as? String }
        }
        if paths.isEmpty, let p = row["file_path"] as? String { paths = [p] }
        // Already open: bring it forward rather than stacking a duplicate.
        if let open = overlays.first(where: { $0.files.map { $0.path } == paths }) {
            open.contentWindow.orderFrontRegardless()
            open.toolbarWindow.orderFrontRegardless()
            return
        }
        func num(_ k: String, _ d: Double) -> Double {
            (row[k] as? Double) ?? (row[k] as? Int).map(Double.init) ?? d
        }
        let o = OverlayController(filePaths: paths,
                                  x: Int(num("pos_x", 800)), y: Int(num("pos_y", 200)),
                                  w: Int(num("width", 400)), h: Int(num("height", 500)),
                                  opacity: num("opacity", 0.85))
        overlays.append(o)
    }

    @objc func showAllPins() {
        var seenGroups = Set<String>()
        for p in pins {
            guard let id = p["id"] as? Int else { continue }
            if let g = p["group_id"] as? String, !g.isEmpty {
                if seenGroups.contains(g) { continue }
                seenGroups.insert(g)
            }
            openPin(id: id)
        }
    }

    @objc func closeAllPins() {
        for o in overlays { o.closeOverlay() }
    }

    @objc func managePins() {
        openWindow(url: url(withHash: "#view=files"), frame: nil)
    }

    // ── Menu bar readout (scripts/amux-menubar.swift) ──
    func startMenuBarItem() {
        menuBar?.stop()
        menuBar = nil
        guard config.menuBar ?? true, let s = serverURL else { return }
        // The standalone readout (make menubar) already owns a status item.
        if standaloneMenuBarRunning() { return }
        menuBar = MenuBarController(apiBase: normalized(s.absoluteString))
        menuBar?.authToken = { [weak self] go in
            guard let self = self else { go(nil); return }
            DispatchQueue.main.async { self.withAPIToken(go) }
        }
    }

    func standaloneMenuBarRunning() -> Bool {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/pgrep")
        p.arguments = ["-x", "amux-menubar"]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        do { try p.run() } catch { return false }
        p.waitUntilExit()
        return p.terminationStatus == 0
    }

    @objc func toggleMenuBarItem(_ sender: NSMenuItem) {
        config = loadConfig()
        config.menuBar = !(config.menuBar ?? true)
        saveConfig(config)
        startMenuBarItem()
    }

    // ── Needs you: Dock badge and notifications ──
    /// Notifications need a real .app bundle: UNUserNotificationCenter throws
    /// (and takes the app down) when the binary runs outside one.
    var canNotify: Bool { Bundle.main.bundleURL.pathExtension == "app" && Bundle.main.bundleIdentifier != nil }

    func setUpNotifications() {
        guard canNotify else {
            NSLog("amux-desktop: verdict=notifications_unavailable reason=not_in_app_bundle path=%@", Bundle.main.bundlePath)
            return
        }
        let c = UNUserNotificationCenter.current()
        c.delegate = self
        c.requestAuthorization(options: [.alert, .sound, .badge]) { granted, err in
            NSLog("amux-desktop: notifications granted=%d error=%@", granted ? 1 : 0, err?.localizedDescription ?? "-")
        }
    }

    func pollNeedsInput() {
        api("/api/needs-input") { [weak self] obj in
            guard let self = self else { return }
            guard let d = obj as? [String: Any] else {
                NSApp.dockTile.badgeLabel = nil
                return
            }
            self.lastNeedsCount = (d["count"] as? Int) ?? 0
            self.applyBadge()
            let items = (d["items"] as? [[String: Any]]) ?? []
            let keys = Set(items.compactMap { $0["key"] as? String })
            // The first poll only learns what is already waiting.
            if let before = self.needsKeys, self.config.notifications ?? true {
                for it in items {
                    guard let k = it["key"] as? String, !before.contains(k) else { continue }
                    self.notify(it)
                }
            }
            self.needsKeys = keys
        }
    }

    /// A Messages post addressed to the owner (the page decided; it only asks
    /// for one when the owner is not already looking at that conversation).
    func notifyMessage(_ d: [String: Any]) {
        guard canNotify else { return }
        let content = UNMutableNotificationContent()
        content.title = (d["title"] as? String) ?? "amux"
        content.body = (d["body"] as? String) ?? ""
        content.sound = .default
        let hash = (d["url"] as? String) ?? "#view=messages"
        content.userInfo = ["hash": hash.hasPrefix("#") ? hash : "#view=messages"]
        content.threadIdentifier = (d["tag"] as? String) ?? "messages"
        let req = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(req)
        NSLog("amux-desktop: verdict=message_notified tag=%@", content.threadIdentifier)
    }

    func notify(_ item: [String: Any]) {
        guard canNotify else { return }
        let content = UNMutableNotificationContent()
        let card = (item["card"] as? String) ?? ""
        content.title = "amux needs you" + (card.isEmpty ? "" : " · " + card)
        content.body = (item["question"] as? String) ?? (item["context"] as? String) ?? ""
        content.sound = .default
        // The worker too: a click opens it, in a tab of the window you have.
        content.userInfo = ["card": card, "worker": (item["worker"] as? String) ?? ""]
        let req = UNNotificationRequest(identifier: (item["key"] as? String) ?? UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(req)
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        NSApp.activate(ignoringOtherApps: true)
        if let hash = response.notification.request.content.userInfo["hash"] as? String, hash.hasPrefix("#messages=") || hash == "#view=messages" {
            openNotificationTab(hash, why: "message")
            completionHandler()
            return
        }
        if response.notification.request.content.userInfo["recorder_meeting"] != nil {
            meetingNotificationClicked()   // amux-recorder.swift
            completionHandler()
            return
        }
        let info = response.notification.request.content.userInfo
        let worker = (info["worker"] as? String) ?? ""
        let card = (info["card"] as? String) ?? ""
        if !worker.isEmpty, worker.allSatisfy({ $0.isLetter || $0.isNumber || "-_.".contains($0) }) {
            openNotificationTab("#peek=" + worker, why: "worker")
        } else {
            openCard(card)
        }
        completionHandler()
    }

    /// A notification click opens what it is about in a NEW TAB of the window
    /// you already have, on that window's server. It used to reuse only a
    /// visible key window and otherwise open a whole new window, which is what
    /// a click from another app (amux hidden, minimized, on another Space)
    /// always got (owner, 2026-10-28). A new window only when none exists.
    func openNotificationTab(_ hash: String, why: String) {
        let safe = hash.allSatisfy { $0.isLetter || $0.isNumber || "#=&:-_.%|@".contains($0) }
        guard safe else { return }
        NSApp.unhide(nil)
        NSApp.activate(ignoringOtherApps: true)
        let withWindow = controllers.filter { $0.window != nil }
        let host = withWindow.first { $0.window == NSApp.keyWindow }
            ?? withWindow.last { $0.window?.isVisible == true }
            ?? withWindow.last
        guard let c = host, let w = c.window else {
            dlog("verdict=notification_opened where=new_window why=\(why) reason=no_window")
            openWindow(url: url(withHash: hash), frame: nil)
            return
        }
        if w.isMiniaturized { w.deminiaturize(nil) }
        newTab(nextTo: c, hash: hash)
        dlog("verdict=notification_opened where=new_tab why=\(why) hash=\(hash)")
    }

    /// Open a dashboard hash in the key window, or a new one.
    func openHash(_ hash: String) {
        let safe = hash.allSatisfy { $0.isLetter || $0.isNumber || "#=&:-_.%|@".contains($0) }
        guard safe else { return }
        if let c = keyController, let w = c.window, w.isVisible || w.isMiniaturized {
            if w.isMiniaturized { w.deminiaturize(nil) }
            w.makeKeyAndOrderFront(nil)
            c.webView.evaluateJavaScript("location.hash = '\(hash)'") { [weak self] _, err in
                if err != nil { self?.openWindow(url: self?.url(withHash: hash), frame: nil) }
            }
        } else {
            openWindow(url: url(withHash: hash), frame: nil)
        }
    }

    /// A "needs you" notification opens its card on the board. An open window
    /// jumps to it (the dashboard's #issue= deep link handles hashchange);
    /// with none, a new window opens there. Without a card id it shows the board.
    func openCard(_ card: String) {
        let ok = !card.isEmpty && card.allSatisfy { $0.isLetter || $0.isNumber || $0 == "-" || $0 == "_" }
        let hash = ok ? "#issue=" + card : "#view=board"
        NSLog("amux-desktop: verdict=notification_opened card=%@ hash=%@", card.isEmpty ? "-" : card, hash)
        if let c = keyController, let w = c.window, w.isVisible || w.isMiniaturized {
            if w.isMiniaturized { w.deminiaturize(nil) }
            w.makeKeyAndOrderFront(nil)
            c.webView.evaluateJavaScript("location.hash = '\(hash)'") { [weak self] _, err in
                if err != nil { self?.openWindow(url: self?.url(withHash: hash), frame: nil) }
            }
        } else {
            openWindow(url: url(withHash: hash), frame: nil)
        }
    }

    @objc func toggleNotifications(_ sender: NSMenuItem) {
        config = loadConfig()
        config.notifications = !(config.notifications ?? true)
        saveConfig(config)
    }

    // ── Menu actions ──
    @objc func newWindow(_ sender: Any?) { openWindow(url: nil, frame: nil) }

    @objc func newTabAction(_ sender: Any?) {
        // A window split into panes: a tab in the focused pane (amux-panes.swift).
        if keyGrid != nil { paneNewTab(sender); return }
        if let c = keyController, c.window?.isVisible == true { newTab(nextTo: c) } else { openWindow(url: nil, frame: nil) }
    }

    /// The current worker or tab in its own window.
    @objc func openCurrentInNewWindow(_ sender: Any?) {
        guard let c = keyPage else { openWindow(url: nil, frame: nil); return }
        c.webView.evaluateJavaScript(currentViewJS) { [weak self] result, _ in
            guard let self = self else { return }
            let hash = (result as? String) ?? ""
            self.openWindow(url: hash.isEmpty ? c.webView.url : self.url(withHash: hash, on: c.serverOrigin), frame: nil)
        }
    }

    @objc func showConnect(_ sender: Any?) {
        (keyPage ?? openWindow(url: nil, frame: nil)).showConnectPage()
    }

    @objc func reload(_ sender: Any?) { keyPage?.webView.reload() }
    // Zoom goes through the dashboard's own zoom (root font size on WebKit).
    // WKWebView.pageZoom shrank the page's CSS viewport without the dashboard
    // re-measuring it, so the file viewer kept its old height and its bottom
    // ran off the window at 130% (Ethan, 2026-10-19: "cant scroll to the
    // bottom without zooming out"). pageZoom is only the fallback for a page
    // without the dashboard's zoom, such as the connect page.
    //
    // With View > Zoom All Windows on, every other window, tab and pane takes
    // the new level too (Ethan, 2026-10-28). The page's own zoom controls
    // report theirs through amuxZoom (app.js _zoomShare).
    func zoom(_ fn: String, fallback: @escaping (WKWebView) -> Void) {
        guard let page = keyPage else { return }
        let wv = page.webView
        wv.evaluateJavaScript("typeof \(fn) === 'function' ? (\(fn)(), _zoomLevel) : 'none'") { [weak self] r, _ in
            if let n = r as? NSNumber { self?.zoomOthers(from: page, level: n.intValue); return }
            fallback(wv)
            self?.zoomOthers(from: page, level: Int((wv.pageZoom * 100).rounded()))
        }
    }

    var zoomAllWindows: Bool { config.zoomAllWindows ?? true }

    @objc func toggleZoomAllWindows(_ sender: Any?) {
        config = loadConfig()
        config.zoomAllWindows = !zoomAllWindows
        saveConfig(config)
        dlog("verdict=zoom_all_toggled on=\(zoomAllWindows ? 1 : 0)")
        // Turning it on brings every window to the level of the one you are in.
        guard zoomAllWindows, let page = keyPage else { return }
        page.webView.evaluateJavaScript("typeof _zoomLevel === 'number' ? _zoomLevel : 0") { [weak self] r, _ in
            let n = (r as? NSNumber)?.intValue ?? 0
            self?.zoomOthers(from: page, level: n > 0 ? n : Int((page.webView.pageZoom * 100).rounded()))
        }
    }

    /// Every page the app shows: each window and every tab of each of its panes.
    var allPages: [DashboardWindowController] {
        var seen = Set<ObjectIdentifier>()
        var out: [DashboardWindowController] = []
        for c in controllers {
            for p in [c] + (c.grid?.stacks.values.flatMap { $0 } ?? []) where seen.insert(ObjectIdentifier(p)).inserted {
                out.append(p)
            }
        }
        return out
    }

    /// Puts every page but `src` at `level` percent: the dashboard's own zoom
    /// where the page has it, pageZoom elsewhere (the connect page).
    func zoomOthers(from src: DashboardWindowController, level: Int) {
        guard zoomAllWindows else { return }
        let lv = min(300, max(25, level))
        let others = allPages.filter { $0 !== src }
        var dashboard = 0, fallback = 0, pending = others.count
        func done() {
            pending -= 1
            if pending == 0 { dlog("verdict=zoom_all level=\(lv) pages=\(others.count) dashboard=\(dashboard) page_zoom=\(fallback)") }
        }
        if others.isEmpty { dlog("verdict=zoom_all level=\(lv) pages=0"); return }
        for p in others {
            p.webView.evaluateJavaScript("typeof _applyZoom === 'function' ? (_zoomLevel = \(lv), _applyZoom(), 'dashboard') : 'none'") { r, _ in
                if (r as? String) == "dashboard" { dashboard += 1 } else { p.webView.pageZoom = CGFloat(lv) / 100; fallback += 1 }
                done()
            }
        }
    }
    @objc func zoomIn(_ sender: Any?) { zoom("zoomIn") { $0.pageZoom = min(3, $0.pageZoom + 0.1) } }
    @objc func zoomOut(_ sender: Any?) { zoom("zoomOut") { $0.pageZoom = max(0.5, $0.pageZoom - 0.1) } }
    @objc func zoomReset(_ sender: Any?) { zoom("resetZoom") { $0.pageZoom = 1 } }

    func validateMenuItem(_ item: NSMenuItem) -> Bool {
        if item.action == #selector(toggleMenuBarItem(_:)) { item.state = (config.menuBar ?? true) ? .on : .off }
        if item.action == #selector(toggleNotifications(_:)) { item.state = (config.notifications ?? true) ? .on : .off }
        if item.action == #selector(toggleDockBadge(_:)) { item.state = (config.dockBadge ?? true) ? .on : .off }
        if item.action == #selector(toggleSnap(_:)) { item.state = snapEnabled ? .on : .off }
        if item.action == #selector(toggleZoomAllWindows(_:)) { item.state = zoomAllWindows ? .on : .off }
        // Window > Panes acts on a window split into panes; elsewhere its keys
        // (⌘1-9 among them) go on to the page.
        let paneActions: [Selector] = [#selector(paneCloseTab(_:)), #selector(paneZoom(_:)), #selector(paneNextTab(_:)),
            #selector(panePreviousTab(_:)), #selector(paneSelectTab(_:)), #selector(paneNewTab(_:)), #selector(paneFocusLeft(_:)),
            #selector(paneFocusRight(_:)), #selector(paneFocusUp(_:)), #selector(paneFocusDown(_:))]
        if let a = item.action, paneActions.contains(a) {
            if a == #selector(paneZoom(_:)) { item.state = keyGrid?.zoomed != nil ? .on : .off }
            return keyGrid != nil
        }
        return true
    }

    // ── Dock menu ──
    func applicationDockMenu(_ sender: NSApplication) -> NSMenu? {
        // macOS lists the open windows above these, by title.
        let m = NSMenu()
        m.addItem(withTitle: "New Window", action: #selector(newWindow(_:)), keyEquivalent: "").target = self
        m.addItem(withTitle: "New Tab", action: #selector(newTabAction(_:)), keyEquivalent: "").target = self
        let badge = NSMenuItem(title: "Show Count on Dock Icon", action: #selector(toggleDockBadge(_:)), keyEquivalent: "")
        badge.target = self
        badge.state = (config.dockBadge ?? true) ? .on : .off
        m.addItem(badge)
        let p = NSMenuItem(title: "Pins", action: nil, keyEquivalent: "")
        let copy = NSMenu(title: "Pins")
        for it in pinsMenu.items { copy.addItem(it.copy() as! NSMenuItem) }
        p.submenu = copy
        m.addItem(p)
        let l = NSMenuItem(title: "Workspaces", action: nil, keyEquivalent: "")
        let lcopy = NSMenu(title: "Workspaces")
        for it in layoutsMenu.items { lcopy.addItem(it.copy() as! NSMenuItem) }
        l.submenu = lcopy
        m.addItem(l)
        m.addItem(.separator())
        m.addItem(withTitle: "Settings…", action: #selector(showSettings(_:)), keyEquivalent: "").target = self
        return m
    }
}

extension Config {
    func merging(windows: [SavedWindow]) -> Config {
        var c = self
        c.windows = windows
        return c
    }
}

// ── Standard menu bar ──
func buildMenuBar(_ d: AppDelegate) {
    let mainMenu = NSMenu()
    func add(_ menu: NSMenu, _ title: String, _ action: Selector?, _ key: String,
             _ mods: NSEvent.ModifierFlags = [.command], target: AnyObject? = nil) {
        let item = menu.addItem(withTitle: title, action: action, keyEquivalent: key)
        item.keyEquivalentModifierMask = mods
        if let t = target { item.target = t }
    }

    let appMenuItem = NSMenuItem()
    let appMenu = NSMenu()
    add(appMenu, "About amux", #selector(NSApplication.orderFrontStandardAboutPanel(_:)), "")
    appMenu.addItem(.separator())
    add(appMenu, "Check for Updates…", #selector(AppDelegate.checkForUpdates(_:)), "", target: d)
    add(appMenu, "Settings…", #selector(AppDelegate.showSettings(_:)), ",", target: d)
    appMenu.addItem(.separator())
    add(appMenu, "Show in Menu Bar", #selector(AppDelegate.toggleMenuBarItem(_:)), "", target: d)
    add(appMenu, "Notify When amux Needs You", #selector(AppDelegate.toggleNotifications(_:)), "", target: d)
    add(appMenu, "Show Count on Dock Icon", #selector(AppDelegate.toggleDockBadge(_:)), "", target: d)
    appMenu.addItem(.separator())
    add(appMenu, "Hide amux", #selector(NSApplication.hide(_:)), "h")
    add(appMenu, "Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option])
    add(appMenu, "Show All", #selector(NSApplication.unhideAllApplications(_:)), "")
    appMenu.addItem(.separator())
    add(appMenu, "Quit amux", #selector(NSApplication.terminate(_:)), "q")
    appMenuItem.submenu = appMenu
    mainMenu.addItem(appMenuItem)

    let fileItem = NSMenuItem()
    let file = NSMenu(title: "File")
    add(file, "New Window", #selector(AppDelegate.newWindow(_:)), "n", target: d)
    add(file, "New Tab", #selector(AppDelegate.newTabAction(_:)), "t", target: d)
    add(file, "Open Current View in New Window", #selector(AppDelegate.openCurrentInNewWindow(_:)), "n", [.command, .shift], target: d)
    file.addItem(.separator())
    add(file, "Connect to Server…", #selector(AppDelegate.showConnect(_:)), "k", [.command, .shift], target: d)
    file.addItem(.separator())
    add(file, "Close", #selector(NSWindow.performClose(_:)), "w")
    fileItem.submenu = file
    mainMenu.addItem(fileItem)

    let editItem = NSMenuItem()
    let edit = NSMenu(title: "Edit")
    add(edit, "Undo", Selector(("undo:")), "z")
    add(edit, "Redo", Selector(("redo:")), "z", [.command, .shift])
    edit.addItem(.separator())
    add(edit, "Cut", #selector(NSText.cut(_:)), "x")
    add(edit, "Copy", #selector(NSText.copy(_:)), "c")
    add(edit, "Paste", #selector(NSText.paste(_:)), "v")
    add(edit, "Select All", #selector(NSText.selectAll(_:)), "a")
    editItem.submenu = edit
    mainMenu.addItem(editItem)

    let viewItem = NSMenuItem()
    let view = NSMenu(title: "View")
    add(view, "Reload", #selector(AppDelegate.reload(_:)), "r", target: d)
    view.addItem(.separator())
    add(view, "Zoom In", #selector(AppDelegate.zoomIn(_:)), "+", target: d)
    add(view, "Zoom Out", #selector(AppDelegate.zoomOut(_:)), "-", target: d)
    add(view, "Actual Size", #selector(AppDelegate.zoomReset(_:)), "0", target: d)
    add(view, "Zoom All Windows", #selector(AppDelegate.toggleZoomAllWindows(_:)), "", target: d)
    view.addItem(.separator())
    add(view, "Show Tab Bar", #selector(NSWindow.toggleTabBar(_:)), "t", [.command, .shift])
    add(view, "Show All Tabs", #selector(NSWindow.toggleTabOverview(_:)), "\\", [.command, .shift])
    add(view, "Toggle Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control])
    viewItem.submenu = view
    mainMenu.addItem(viewItem)

    let pinsItem = NSMenuItem()
    d.pinsMenu.delegate = d
    d.rebuildPinsMenu()
    pinsItem.submenu = d.pinsMenu
    mainMenu.addItem(pinsItem)

    let windowItem = NSMenuItem()
    let window = NSMenu(title: "Window")
    add(window, "Minimize", #selector(NSWindow.miniaturize(_:)), "m")
    add(window, "Zoom", #selector(NSWindow.zoom(_:)), "")
    window.addItem(.separator())
    add(window, "Show Previous Tab", #selector(NSWindow.selectPreviousTab(_:)), "\t", [.control, .shift])
    add(window, "Show Next Tab", #selector(NSWindow.selectNextTab(_:)), "\t", [.control])
    add(window, "Merge All Windows", #selector(NSWindow.mergeAllWindows(_:)), "")
    add(window, "Move Tab to New Window", #selector(NSWindow.moveTabToNewWindow(_:)), "")
    window.addItem(.separator())
    let arrangeItem = NSMenuItem(title: "Arrange", action: nil, keyEquivalent: "")
    arrangeItem.submenu = buildArrangeMenu(d)
    window.addItem(arrangeItem)
    // Panes (amux-panes.swift): each acts on the focused pane of the window in front.
    let panesItem = NSMenuItem(title: "Panes", action: nil, keyEquivalent: "")
    let panes = NSMenu(title: "Panes")
    add(panes, "New Tab in Pane", #selector(AppDelegate.paneNewTab(_:)), "t", [.command, .option], target: d)
    add(panes, "Close Tab in Pane", #selector(AppDelegate.paneCloseTab(_:)), "w", [.command, .option], target: d)
    add(panes, "Zoom Pane", #selector(AppDelegate.paneZoom(_:)), "\r", [.command, .shift], target: d)
    panes.addItem(.separator())
    add(panes, "Next Tab in Pane", #selector(AppDelegate.paneNextTab(_:)), "]", [.command, .option], target: d)
    add(panes, "Previous Tab in Pane", #selector(AppDelegate.panePreviousTab(_:)), "[", [.command, .option], target: d)
    for n in 1...9 {
        add(panes, n == 9 ? "Last Tab in Pane" : "Tab \(n) in Pane", #selector(AppDelegate.paneSelectTab(_:)), "\(n)", target: d)
        panes.items.last?.tag = n
    }
    panes.addItem(.separator())
    let arrow = { (k: Int) in String(Character(UnicodeScalar(k)!)) }
    add(panes, "Focus Pane on the Left", #selector(AppDelegate.paneFocusLeft(_:)), arrow(NSLeftArrowFunctionKey), [.command, .option], target: d)
    add(panes, "Focus Pane on the Right", #selector(AppDelegate.paneFocusRight(_:)), arrow(NSRightArrowFunctionKey), [.command, .option], target: d)
    add(panes, "Focus Pane Above", #selector(AppDelegate.paneFocusUp(_:)), arrow(NSUpArrowFunctionKey), [.command, .option], target: d)
    add(panes, "Focus Pane Below", #selector(AppDelegate.paneFocusDown(_:)), arrow(NSDownArrowFunctionKey), [.command, .option], target: d)
    panesItem.submenu = panes
    window.addItem(panesItem)
    let layoutsItem = NSMenuItem(title: "Workspaces", action: nil, keyEquivalent: "")
    d.rebuildLayoutsMenu()
    layoutsItem.submenu = d.layoutsMenu
    window.addItem(layoutsItem)
    windowItem.submenu = window
    mainMenu.addItem(windowItem)

    NSApplication.shared.mainMenu = mainMenu
    NSApplication.shared.windowsMenu = window
}

// ── Launch ──
@main
struct AmuxDesktop {
    static let delegate = AppDelegate()
    static func main() {
        let app = NSApplication.shared
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        app.run()
    }
}
