import Cocoa
import WebKit

// Settings (amux > Settings…, Cmd-,) and saved workspaces.
//
// A workspace (stored as a "layout") is the set of dashboard windows (what
// each shows, where it sits, on which display and Desktop, which ones are tabs
// together) plus the floating pins, under a name. Opening one replaces the
// current windows, reusing any already showing the same page. The open
// workspace keeps where you left it, so switching away and back returns to it
// as it was.

struct SavedPin: Codable {
    var paths: [String]
    var frame: String
    var opacity: Double
}

/// A saved window's tab name: the one it had when saved, or (a file saved
/// before names were kept) what its link shows.
func savedTabName(_ w: SavedWindow) -> String {
    let own = savedPageName(url: w.url, title: w.title)
    // A window split into panes (amux-panes.swift) is named by every pane, and
    // a pane by each of its tabs ("amux-admin + amux-app | amux-app + desktop"),
    // so the list shows everything Open will put back.
    guard let g = w.grid, !g.panes.isEmpty else { return own }
    func pane(_ url: String, _ title: String?, _ tabs: [SavedTab]?) -> String {
        ([savedPageName(url: url, title: title) + savedSubtab(url)] + (tabs ?? []).map { savedPageName(url: $0.url, title: $0.title) + savedSubtab($0.url) })
            .joined(separator: " + ")
    }
    let panes = ([pane(w.url, w.title, g.hostTabs)] + g.panes.map { pane($0.url, $0.title, $0.tabs) }).joined(separator: " | ")
    if let n = g.name, !n.isEmpty { return n + " (" + panes + ")" }
    return panes
}

/// A worker shown on another of its tabs than Terminal: " (Chat)".
func savedSubtab(_ url: String) -> String {
    guard let r = url.range(of: "#"), url[r.upperBound...].hasPrefix("peek=") else { return "" }
    let t = url[r.upperBound...].split(separator: "&").first { $0.hasPrefix("tab=") }.map { String($0.dropFirst(4)) } ?? ""
    return t.isEmpty || t == "terminal" ? "" : " (" + t.prefix(1).uppercased() + t.dropFirst() + ")"
}

func savedPageName(url: String, title: String?) -> String {
    if let t = title, !t.isEmpty { return t }
    guard let r = url.range(of: "#") else { return "amux" }
    let h = String(url[r.upperBound...]).split(separator: "&").first.map(String.init) ?? ""
    let v = h.split(separator: "=", maxSplits: 1).last.map(String.init) ?? h
    return v.removingPercentEncoding ?? v
}

/// A workspace change waiting for the server: save (replace or add), delete,
/// or rename. Replayed on the server's list, so it merges with other Macs'.
struct WorkspaceOp: Codable {
    var kind: String
    var name: String
    var newName: String? = nil
    var layout: Layout? = nil

    func apply(to list: inout [Layout]) {
        switch kind {
        case "save":
            guard let l = layout else { return }
            if let i = list.firstIndex(where: { $0.name == name }) { list[i] = l } else { list.append(l) }
        case "delete":
            list.removeAll { $0.name == name }
        case "rename":
            guard let n = newName, list.contains(where: { $0.name == name }) else { return }
            list.removeAll { $0.name == n }
            if let i = list.firstIndex(where: { $0.name == name }) { list[i].name = n }
        default:
            break
        }
    }
}

/// The server's copy of the open windows (prefs key desktop_windows).
struct ServerWindows: Codable {
    var schema: Int
    var windows: [SavedWindow]
    var savedBy: String?
    var savedAt: Double
    var server: String? = nil   // the sending Mac's address for this server (amux-rehome.swift)
}

/// The server's copy of every saved workspace (prefs key desktop_workspaces).
struct ServerWorkspaces: Codable {
    var schema: Int
    var layouts: [Layout]
    var savedBy: String?
    var savedAt: Double
}

struct Layout: Codable {
    var name: String
    var windows: [SavedWindow]
    var pins: [SavedPin]?
    var savedAt: Double
    /// The address the saving Mac reached the server at (originKey), so another
    /// Mac moves those tabs to its own address for it (amux-rehome.swift).
    /// Absent in a workspace saved before 2026-10-29.
    var server: String? = nil
}

// ── Layouts ──
extension AppDelegate {
    func snapshotPins() -> [SavedPin] {
        overlays.map { o in
            SavedPin(paths: o.files.map { $0.path },
                     frame: NSStringFromRect(o.contentWindow.frame),
                     opacity: Double(o.contentWindow.alphaValue))
        }
    }

    func saveLayout(named name: String, _ done: (() -> Void)? = nil) {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        snapshotWindows { [weak self] windows in
            guard let self = self else { return }
            var c = loadConfig()
            var layouts = c.layouts ?? []
            let l = Layout(name: trimmed, windows: windows, pins: self.snapshotPins(), savedAt: Date().timeIntervalSince1970,
                           server: self.serverURL.flatMap(originKey))
            if let i = layouts.firstIndex(where: { $0.name == trimmed }) { layouts[i] = l } else { layouts.append(l) }
            c.layouts = layouts
            saveConfig(c)
            self.config = c
            NSLog("amux-desktop: verdict=layout_saved name=%@ windows=%d pins=%d shows=%@", trimmed, windows.count, l.pins?.count ?? 0,
                  self.workspaceWindows(l).joined(separator: " | "))
            self.rebuildLayoutsMenu()
            self.syncWorkspaces(.init(kind: "save", name: trimmed, layout: l)) { _ in done?() }
        }
    }

    /// The saved windows with every tab, pane and pane tab on the saving Mac's
    /// server moved to this Mac's address for it (amux-rehome.swift). Logs how
    /// many moved, so a workspace that opens on the wrong server says so.
    func rehomed(_ windows: [SavedWindow], savedOn: String?, what: String) -> [SavedWindow] {
        let here = serverURL
        var links = 0, moved = 0
        func move(_ u: String) -> String {
            let r = rehomedURL(u, savedOn: savedOn, here: here)
            links += 1
            if r != u { moved += 1 }
            return r
        }
        func tab(_ t: SavedTab) -> SavedTab {
            var t = t
            t.url = move(t.url)
            t.split = t.split.map { Box(grid($0.value)) }
            return t
        }
        func grid(_ g: SavedGrid) -> SavedGrid {
            var g = g
            g.hostTabs = g.hostTabs?.map(tab)
            g.panes = g.panes.map { p in
                var p = p
                p.url = move(p.url)
                p.split = p.split.map { Box(grid($0.value)) }
                p.tabs = p.tabs?.map(tab)
                return p
            }
            return g
        }
        let out = windows.map { w -> SavedWindow in
            var w = w
            w.url = move(w.url)
            w.grid = w.grid.map(grid)
            return w
        }
        NSLog("amux-desktop: verdict=%@ what=%@ links=%d moved=%d saved_on=%@ here=%@", moved > 0 ? "workspace_rehomed" : "workspace_same_server",
              what, links, moved, savedOn ?? "unrecorded", here.flatMap(originKey) ?? "not_connected")
        return out
    }

    /// Replace the open windows and pins with the workspace's.
    func openLayout(named name: String) {
        var cfg = loadConfig()
        guard let l = (cfg.layouts ?? []).first(where: { $0.name == name }) else { return }
        cfg.activeWorkspace = name
        saveConfig(cfg)
        config = cfg
        saveWork?.cancel()
        closeAllPins()
        openWindows(rehomed(l.windows, savedOn: l.server, what: "workspace " + name), reuse: controllers)
        rebuildLayoutsMenu()
        for p in l.pins ?? [] where !p.paths.isEmpty {
            let f = NSRectFromString(p.frame)
            let o = OverlayController(filePaths: p.paths, x: Int(f.minX), y: Int(f.minY),
                                      w: Int(max(f.width, 200)), h: Int(max(f.height, 150)), opacity: p.opacity)
            overlays.append(o)
        }
        applyFreeze()
        NSLog("amux-desktop: verdict=layout_opened name=%@ windows=%d pins=%d", name, l.windows.count, l.pins?.count ?? 0)
        activateApp()
    }

    func deleteLayout(named name: String) {
        var c = loadConfig()
        c.layouts = (c.layouts ?? []).filter { $0.name != name }
        if c.launch == "layout:" + name { c.launch = nil }
        if c.activeWorkspace == name { c.activeWorkspace = nil }
        saveConfig(c)
        config = c
        rebuildLayoutsMenu()
        syncWorkspaces(.init(kind: "delete", name: name))
    }

    func renameLayout(_ old: String, to new: String) {
        let n = new.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !n.isEmpty, n != old else { return }
        var c = loadConfig()
        var layouts = (c.layouts ?? []).filter { $0.name != n }
        if let i = layouts.firstIndex(where: { $0.name == old }) { layouts[i].name = n }
        c.layouts = layouts
        if c.launch == "layout:" + old { c.launch = "layout:" + n }
        if c.activeWorkspace == old { c.activeWorkspace = n }
        saveConfig(c)
        config = c
        rebuildLayoutsMenu()
        syncWorkspaces(.init(kind: "rename", name: old, newName: n))
    }

    // ── Saved on the server ──
    // Workspaces live in the server's prefs under one key, so they outlast this
    // Mac and appear on the next one (owner, 2026-10-28: "i want it saved on the
    // server"). The server's list is the record: a change is applied to a fresh
    // read of it, then sent back, so two Macs saving keep each other's. This
    // Mac's file is the copy used while the server cannot be reached; a change
    // made then is marked and sent at the next sync.
    /// nil when this copy must not touch the server's: one started with its own
    /// config (the desktop tests) syncs only to a key it names, so a test never
    /// writes over the owner's workspaces.
    var workspacesKey: String? {
        let env = ProcessInfo.processInfo.environment
        let k = env["AMUX_DESKTOP_WORKSPACES_KEY"] ?? ""
        if !k.isEmpty { return k }
        return (env["AMUX_DESKTOP_CONFIG"] ?? "").isEmpty ? "desktop_workspaces" : nil
    }

    /// Records a change (if any) as pending, then syncs: reads the server's
    /// list, replays every pending change on it in order, keeps the result here
    /// and sends it. A pending change is dropped only once the server has
    /// confirmed a list that includes it, so neither a crash nor a lost
    /// connection loses it, and a Mac coming back online adds its changes to
    /// the other Macs' workspaces instead of replacing them.
    func syncWorkspaces(_ op: WorkspaceOp? = nil, _ done: ((Bool) -> Void)? = nil) {
        guard let key = workspacesKey else {
            NSLog("amux-desktop: verdict=workspaces_sync_off reason=private_config measured=false")
            done?(false)
            return
        }
        if let op = op {
            var c = loadConfig()
            c.workspacesPending = (c.workspacesPending ?? []) + [op]
            saveConfig(c)
            config = c
        }
        if workspacesSyncBusy {
            workspacesSyncWaiting.append { reached in done?(reached) }
            return
        }
        workspacesSyncBusy = true
        let finish: (Bool) -> Void = { [weak self] reached in
            done?(reached)
            guard let self = self else { return }
            self.workspacesSyncBusy = false
            let waiting = self.workspacesSyncWaiting
            self.workspacesSyncWaiting = []
            // Everyone who waited is served by one more sync, which also
            // carries any change recorded while this one ran.
            if !waiting.isEmpty { self.syncWorkspaces { r in waiting.forEach { $0(r) } } }
        }
        api("/api/prefs?key=" + (key.addingPercentEncoding(withAllowedCharacters: .urlQueryAllowed) ?? key)) { [weak self] obj in
            guard let self = self else { return }
            var c = loadConfig()
            let pending = c.workspacesPending ?? []
            guard let reply = obj as? [String: Any], reply["key"] as? String == key else {
                NSLog("amux-desktop: verdict=workspaces_server_unreachable measured=false key=%@ local=%d pending=%d", key, (c.layouts ?? []).count, pending.count)
                finish(false)
                return
            }
            let raw = (reply["value"] as? String) ?? ""
            let remote = raw.isEmpty ? nil : (try? JSONDecoder().decode(ServerWorkspaces.self, from: Data(raw.utf8)))?.layouts
            if !raw.isEmpty, remote == nil {
                // Unreadable (a newer app's format, or damaged): left as it is.
                NSLog("amux-desktop: verdict=workspaces_server_unreadable key=%@ bytes=%d pending=%d", key, raw.utf8.count, pending.count)
                finish(true)
                return
            }
            // The first sync to an empty server uploads this Mac's list.
            var list = remote ?? (c.layouts ?? [])
            // A Mac's first sync also keeps the workspaces only it has.
            let first = c.workspacesSyncedOnce != true
            if first, remote != nil {
                for l in c.layouts ?? [] where !list.contains(where: { $0.name == l.name }) { list.append(l) }
            }
            for p in pending { p.apply(to: &list) }
            let source = remote == nil ? "local_first_upload" : "server"
            c.layouts = list
            saveConfig(c)
            self.config = c
            self.rebuildLayoutsMenu()
            let enc = JSONEncoder()
            enc.outputFormatting = [.sortedKeys]
            let same = remote.flatMap { try? enc.encode($0) } == (try? enc.encode(list))
            let doc = ServerWorkspaces(schema: 1, layouts: list, savedBy: Host.current().localizedName, savedAt: Date().timeIntervalSince1970)
            func confirmed() {
                // Only the changes this list carried; any recorded since stay.
                var c2 = loadConfig()
                let left = Array((c2.workspacesPending ?? []).dropFirst(pending.count))
                c2.workspacesPending = left.isEmpty ? nil : left
                c2.workspacesSyncedOnce = true
                saveConfig(c2)
                self.config = c2
            }
            guard !same, let data = try? enc.encode(doc), let text = String(data: data, encoding: .utf8) else {
                confirmed()
                NSLog("amux-desktop: verdict=workspaces_synced key=%@ source=%@ layouts=%d applied=%d sent=0", key, source, list.count, pending.count)
                finish(true)
                return
            }
            self.api("/api/prefs", method: "POST", body: ["key": key, "value": text]) { r in
                let ok = (r as? [String: Any])?["ok"] as? Bool == true
                if ok { confirmed() }
                NSLog("amux-desktop: verdict=%@ key=%@ source=%@ layouts=%d applied=%d bytes=%d", ok ? "workspaces_synced" : "workspaces_send_failed", key, source, list.count, pending.count, data.count)
                finish(ok)
            }
        }
    }

    /// What launch opens: the last windows (default), a saved layout, or one window.
    func launchWindows() {
        let launch = config.launch ?? "last"
        if launch.hasPrefix("layout:"), (config.layouts ?? []).contains(where: { "layout:" + $0.name == launch }) {
            openLayout(named: String(launch.dropFirst("layout:".count)))
        } else if launch == "new" {
            openWindow(url: nil, frame: nil)
        } else {
            launchedWithoutWindows = (config.windows ?? []).isEmpty
            // The last windows too: a Mac that opened a workspace before the
            // remap existed saved its tabs on its own localhost (2026-10-29), and
            // reopening them as saved kept every tab on an empty server.
            openWindows(rehomed(config.windows ?? [], savedOn: nil, what: "last windows"))
        }
        lastSignature = placementSignature()
    }

    // ── The open windows, on the server too (owner, 2026-10-29: "make sure the
    // tab names are also saved with the windows and ensure its all saved server
    // side") ──
    // Every autosave also sends the windows (each tab, its name, its panes and
    // their tabs and names, where it sits) to the server, at most every 10 s and
    // only when they changed. A Mac that starts with no windows of its own (a
    // new Mac, a cleared config) opens the server's once its first page has
    // the token to read them.
    var windowsKey: String? { workspacesKey.map { $0 == "desktop_workspaces" ? "desktop_windows" : $0 + "_windows" } }

    func sendWindowsSoon(_ windows: [SavedWindow]) {
        guard let key = windowsKey, !windows.isEmpty else { return }
        windowsSendWork?.cancel()
        let w = DispatchWorkItem { [weak self] in
            guard let self = self else { return }
            let enc = JSONEncoder()
            enc.outputFormatting = [.sortedKeys]
            guard let body = try? enc.encode(windows), body != self.windowsSent else { return }
            let doc = ServerWindows(schema: 1, windows: windows, savedBy: Host.current().localizedName, savedAt: Date().timeIntervalSince1970,
                                    server: self.serverURL.flatMap(originKey))
            guard let data = try? enc.encode(doc), let text = String(data: data, encoding: .utf8) else { return }
            self.api("/api/prefs", method: "POST", body: ["key": key, "value": text]) { [weak self] r in
                let ok = (r as? [String: Any])?["ok"] as? Bool == true
                if ok { self?.windowsSent = body }
                NSLog("amux-desktop: verdict=%@ key=%@ windows=%d bytes=%d", ok ? "windows_sent" : "windows_send_failed", key, windows.count, data.count)
            }
        }
        windowsSendWork = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 10, execute: w)
    }

    /// At most once a launch: the server's windows for a Mac that had none.
    func adoptServerWindows() {
        guard launchedWithoutWindows, let key = windowsKey else { return }
        launchedWithoutWindows = false
        api("/api/prefs?key=" + key) { [weak self] obj in
            guard let self = self, let raw = (obj as? [String: Any])?["value"] as? String, !raw.isEmpty,
                  let doc = try? JSONDecoder().decode(ServerWindows.self, from: Data(raw.utf8)), !doc.windows.isEmpty else {
                NSLog("amux-desktop: verdict=windows_server_none key=%@", key)
                return
            }
            NSLog("amux-desktop: verdict=windows_adopted_from_server key=%@ windows=%d saved_by=%@", key, doc.windows.count, doc.savedBy ?? "-")
            self.openWindows(self.rehomed(doc.windows, savedOn: doc.server, what: "server windows"), reuse: self.controllers)
        }
    }

    @objc func saveLayoutPrompt(_ sender: Any?) {
        let existing = (loadConfig().layouts ?? []).map { $0.name }
        var n = existing.count + 1
        while existing.contains("Workspace \(n)") { n += 1 }
        guard let name = askText(title: "Save Workspace",
                                 message: "Saves every open amux window: its tabs, how each is split into panes and every pane's tabs, what each one shows, where the window sits (display and Desktop), and the floating pins. It is kept on the server, so it also reaches your other Macs. A name that already exists is replaced.",
                                 value: "Workspace \(n)", ok: "Save") else { return }
        saveLayout(named: name)
    }

    @objc func openLayoutItem(_ sender: NSMenuItem) {
        openLayout(named: sender.representedObject as? String ?? sender.title)
    }

    func askText(title: String, message: String, value: String, ok: String) -> String? {
        activateApp()
        let a = NSAlert()
        a.messageText = title
        a.informativeText = message
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 300, height: 24))
        field.stringValue = value
        a.accessoryView = field
        a.addButton(withTitle: ok)
        a.addButton(withTitle: "Cancel")
        a.window.initialFirstResponder = field
        return a.runModal() == .alertFirstButtonReturn ? field.stringValue : nil
    }

    func rebuildLayoutsMenu() {
        layoutsMenu.removeAllItems()
        let save = NSMenuItem(title: "Save Workspace…", action: #selector(saveLayoutPrompt(_:)), keyEquivalent: "s")
        save.keyEquivalentModifierMask = [.command, .option]
        save.target = self
        layoutsMenu.addItem(save)
        let freeze = NSMenuItem(title: "Freeze Workspace", action: #selector(toggleFreeze(_:)), keyEquivalent: "f")
        freeze.keyEquivalentModifierMask = [.command, .option]
        freeze.target = self
        freeze.state = isFrozen ? .on : .off
        freeze.toolTip = "Lock this workspace's tabs, names and workers. Windows, panes and zoom still resize."
        layoutsMenu.addItem(freeze)
        let layouts = loadConfig().layouts ?? []
        if !layouts.isEmpty { layoutsMenu.addItem(.separator()) }
        let active = loadConfig().activeWorkspace
        for (i, l) in layouts.enumerated() {
            // ⌃⌘1 to ⌃⌘9 switch to the first nine.
            let item = NSMenuItem(title: l.name, action: #selector(openLayoutItem(_:)), keyEquivalent: i < 9 ? String(i + 1) : "")
            item.keyEquivalentModifierMask = [.control, .command]
            item.representedObject = l.name
            item.target = self
            item.state = l.name == active ? .on : .off
            item.toolTip = workspaceSummary(l)
            layoutsMenu.addItem(item)
        }
        layoutsMenu.addItem(.separator())
        let manage = NSMenuItem(title: "Manage Workspaces…", action: #selector(showSettings(_:)), keyEquivalent: "")
        manage.target = self
        layoutsMenu.addItem(manage)
    }

    /// Each window by its tab names and where it sits: "mixpeek-general, Board
    /// (left half)". A tab group is one window; its visible tab comes first.
    func workspaceWindows(_ l: Layout) -> [String] {
        var out: [String] = []
        var groups: [Int: Int] = [:]
        var names: [[String]] = [], spots: [String?] = []
        for w in l.windows {
            let name = savedTabName(w)
            if let g = w.tabGroup, let i = groups[g] {
                if w.selected == true { names[i].insert(name, at: 0); spots[i] = zoneName(rel: w.rel) } else { names[i].append(name) }
                continue
            }
            if let g = w.tabGroup { groups[g] = names.count }
            names.append([name]); spots.append(zoneName(rel: w.rel))
        }
        for (i, n) in names.enumerated() { out.append(n.joined(separator: ", ") + (spots[i].map { " (\($0))" } ?? "")) }
        return out
    }

    /// "5 windows · Desktop 1: 2, Desktop 3: 3 · 2 displays · 1 pin"
    func workspaceSummary(_ l: Layout) -> String {
        var parts = ["\(l.windows.count) window\(l.windows.count == 1 ? "" : "s")"]
        var desks: [Int: Int] = [:]
        for w in l.windows where w.selected != false { if let n = w.spaceIndex { desks[n, default: 0] += 1 } }
        if desks.count > 1 { parts.append(desks.keys.sorted().map { "Desktop \($0): \(desks[$0]!)" }.joined(separator: ", ")) }
        let displays = Set(l.windows.compactMap { $0.display }).count
        if displays > 1 { parts.append("\(displays) displays") }
        let pins = l.pins?.count ?? 0
        if pins > 0 { parts.append("\(pins) pin\(pins == 1 ? "" : "s")") }
        return parts.joined(separator: " · ") + "\n" + workspaceWindows(l).joined(separator: "\n")
    }

    /// amux > Settings… (⌘,), the Dock menu and the menu bar item: Settings in
    /// its own window, apart from every amux window (owner, 2026-11-27: "it
    /// should open up a modal that's outside of the amux app"). The window
    /// shows the server's Settings alone (app.js `?settings_window=1`): This
    /// device and This Mac first, then the server's tabs. With no server, or a
    /// server whose dashboard has no such mode, it shows the app's own page.
    @objc func showSettings(_ sender: Any?) {
        settings?.builtIn = false
        showNativeSettings(sender)
    }
    /// At most every 10 minutes, opening Settings also checks for an update.
    func checkUpdatesIfStale() {
        let u = Updater.shared
        if u.state == .idle || (u.state != .checking && u.state != .installing && -(u.checkedAt?.timeIntervalSinceNow ?? -1e9) > 600) { u.check() }
    }

    @objc func showNativeSettings(_ sender: Any?) {
        if settings == nil { settings = SettingsWindowController(app: self) }
        dlog("verdict=settings_window_open server=\(serverURL == nil ? 0 : 1)")
        settings?.reload()
        settings?.showWindow(nil)
        settings?.window?.makeKeyAndOrderFront(nil)
        activateApp()
        checkUpdatesIfStale()
        // Workspaces another Mac saved show up here: the list redraws when the
        // server's copy differs from this Mac's.
        let before = (try? JSONEncoder().encode(loadConfig().layouts ?? [])) ?? Data()
        syncWorkspaces { [weak self] _ in
            if (try? JSONEncoder().encode(loadConfig().layouts ?? [])) != before { self?.settings?.reload() }
        }
    }

    @objc func checkForUpdates(_ sender: Any?) {
        showSettings(sender)
        Updater.shared.check()
    }
}

// ── Settings window ──
// One window, apart from the dashboard windows. It loads the server's
// dashboard in its Settings-only mode, so the This Mac tab and every server tab
// are the same code the gear opens; the app's own page (settingsHTML) is the
// fallback when there is no server, it cannot be reached, or it predates the mode.
final class SettingsWindowController: NSWindowController, WKScriptMessageHandler, WKNavigationDelegate, WKUIDelegate {
    weak var app: AppDelegate?
    let webView: WKWebView
    /// The server whose Settings this window shows; nil while it shows the app's own page.
    private(set) var server: URL?
    /// The app's own page even with a server (a test of its buttons).
    var builtIn = false

    init(app: AppDelegate) {
        self.app = app
        let cfg = WKWebViewConfiguration()
        // The dashboard windows' store: the same sign-in.
        cfg.websiteDataStore = .default()
        cfg.applicationNameForUserAgent = "Amux-Desktop/2.0"
        webView = WKWebView(frame: .zero, configuration: cfg)
        QuietTestCopy.keepRendering(webView)
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 720, height: 720),
                         styleMask: [.titled, .closable, .miniaturizable, .resizable],
                         backing: .buffered, defer: false)
        w.title = "Settings"
        w.isReleasedWhenClosed = false
        w.minSize = NSSize(width: 480, height: 420)
        w.contentView = webView
        w.center()
        w.setFrameAutosaveName("amuxSettings")
        super.init(window: w)

        cfg.userContentController.add(WeakScriptHandler(self), name: "amuxSettings")
        cfg.userContentController.add(WeakScriptHandler(self), name: "amuxNotifyPrefs")
        webView.navigationDelegate = self
        webView.uiDelegate = self
        webView.setValue(false, forKey: "drawsBackground")
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// The page this window loads for `base`: its Settings and nothing else.
    static func pageURL(_ base: URL) -> URL? {
        guard var c = URLComponents(url: base, resolvingAgainstBaseURL: false) else { return nil }
        c.path = "/"
        c.query = "settings_window=1"
        c.fragment = nil
        return c.url
    }

    func isSettingsPage(_ url: URL?) -> Bool {
        guard let url = url, let app = app, app.isServerURL(url) else { return false }
        return URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.contains { $0.name == "settings_window" } ?? false
    }

    /// Run once the page has loaded (a test click must not land on the page a reload is replacing).
    var afterLoad: [() -> Void] = []
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        if server != nil {
            // An older server's dashboard has no Settings-only mode: it would
            // show a whole second amux here.
            webView.evaluateJavaScript("typeof _settingsWindowMode === 'function' && _settingsWindowMode()") { [weak self] r, _ in
                guard let self = self else { return }
                if (r as? Bool) == true {
                    dlog("verdict=settings_window_shown source=server")
                    self.runAfterLoad()
                } else {
                    self.fallBack("server_has_no_settings_window")
                }
            }
            return
        }
        runAfterLoad()
    }

    private func runAfterLoad() {
        let run = afterLoad
        afterLoad = []
        run.forEach { $0() }
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        loadFailed(error)
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        loadFailed(error)
    }

    /// A load another load replaced (-999) or our own cancel of a link out
    /// (WebKit 102) is not a failure: the page that replaced it reports itself.
    private func loadFailed(_ error: Error) {
        let e = error as NSError
        if e.code == NSURLErrorCancelled || (e.domain == "WebKitErrorDomain" && e.code == 102) { return }
        if server != nil { fallBack("load_failed error=\(e.code)") }
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        dlog("verdict=settings_window_process_gone")
        server = nil
        reload()
    }

    /// The app's own page, saying why.
    private func fallBack(_ why: String) {
        guard let app = app else { return }
        dlog("verdict=settings_window_shown source=app reason=\(why)")
        server = nil
        webView.loadHTMLString(settingsHTML(app), baseURL: nil)
    }

    /// Show the current state: the server's Settings when there is a server
    /// (loaded once; afterwards only told the app's new state), else the app's page.
    func reload() {
        guard let app = app else { return }
        if !builtIn, let base = app.serverURL, let url = SettingsWindowController.pageURL(base) {
            if server == base, isSettingsPage(webView.url) {
                app.tellMacSettings(webView)
                app.tellNotifications(webView)
                runAfterLoad()
                return
            }
            server = base
            webView.load(URLRequest(url: url))
            return
        }
        server = nil
        webView.loadHTMLString(settingsHTML(app), baseURL: nil)
    }

    func webView(_ webView: WKWebView, didReceive challenge: URLAuthenticationChallenge,
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

    /// Settings stays Settings: a link or a page change to anywhere else opens
    /// in an amux window (the server) or the default browser (elsewhere).
    func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
                 decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = navigationAction.request.url, navigationAction.targetFrame?.isMainFrame ?? false,
              server != nil, !isSettingsPage(url), url.scheme != "about", url.scheme != "blob", url.scheme != "data" else {
            decisionHandler(.allow)
            return
        }
        decisionHandler(.cancel)
        open(url)
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let url = navigationAction.request.url { open(url) }
        return nil
    }

    private func open(_ url: URL) {
        guard let app = app else { return }
        if url.scheme == "amux" { app.handleAmuxURL(url); return }
        if app.isServerURL(url) {
            dlog("verdict=settings_window_link to=server")
            let c = app.keyController ?? app.controllers.first(where: { app.isServerURL($0.webView.url ?? url) })
            if let c = c { c.webView.load(URLRequest(url: url)); c.window?.makeKeyAndOrderFront(nil) } else { _ = app.openWindow(url: url, frame: nil) }
            activateApp()
        } else {
            dlog("verdict=settings_window_link to=browser")
            NSWorkspace.shared.open(url)
        }
    }

    func userContentController(_ c: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let app = app, let m = message.body as? [String: Any] else { return }
        // A server page speaks only from that server; the app's own page from about:blank.
        if server != nil, !isSettingsPage(webView.url) { return }
        if message.name == "amuxNotifyPrefs" {
            if (m["op"] as? String) == "set", let on = m["on"] as? Bool { app.setNotifications(on, source: "settings") } else { app.tellNotifications(webView) }
            return
        }
        switch m["action"] as? String {
        case "state": app.tellMacSettings(webView)
        case "close": window?.performClose(nil)
        default: app.settingsAction(m)
        }
    }
}

/// The app's limits a person can change in Settings > This Mac > Memory and
/// storage. Each was an environment variable only (owner, 2026-11-26: "all
/// should be configurable in the ui"); the variable still wins when set.
struct AppLimit { let key: String, label: String, help: String, unit: String, fallback: Double }

extension AppDelegate {
    var appLimits: [AppLimit] {
        let ramMB = Double(ProcessInfo.processInfo.physicalMemory / 1_048_576)
        return [
            AppLimit(key: "AMUX_DESKTOP_WEB_BUDGET_MB", label: "Memory for all pages", help: "Past this, pages nobody has looked at for a while unload (they load again when shown).", unit: "MB", fallback: max(4096, ramMB / 8)),
            AppLimit(key: "AMUX_DESKTOP_DISCARD_HIDDEN_S", label: "Unload a hidden page after", help: "Only while all pages together are over the memory above. 0 never unloads.", unit: "seconds", fallback: 1800),
            AppLimit(key: "AMUX_DESKTOP_TAB_MB_LIMIT", label: "Refresh a hidden page over", help: "A page out of sight that grows past this reloads on the same view.", unit: "MB", fallback: 1536),
            AppLimit(key: "AMUX_DESKTOP_TAB_HIDDEN_S", label: "after it has been hidden for", help: "", unit: "seconds", fallback: 120),
            AppLimit(key: "AMUX_DESKTOP_SHOWN_TAB_MB_LIMIT", label: "Refresh a page on screen over", help: "Never while you are typing in it. 0 turns it off.", unit: "MB", fallback: 6144),
            AppLimit(key: "AMUX_DESKTOP_STORAGE_AUTO_MB", label: "Compact saved page data past", help: "At launch, and hourly while amux is in the background.", unit: "MB", fallback: 1024),
        ]
    }
}

// ── One handler for every Settings surface ──
// The dashboard's This Mac tab (app.js _macSettings*, through amuxSettings)
// and the fallback window send the same {action, name, value}; after each,
// every surface is told the new state, so two pages never disagree.
extension AppDelegate {
    func settingsAction(_ m: [String: Any]) {
        guard let action = m["action"] as? String else { return }
        let name = m["name"] as? String ?? ""
        var cfg = loadConfig()
        dlog("verdict=settings_action action=\(action) name=\(name)")
        let after: () -> Void = { [weak self] in self?.settingsChanged() }
        switch action {
        case "state": break
        case "open": openLayout(named: name)
        case "update", "save": saveLayout(named: name) { after() }; return
        case "delete": deleteLayout(named: name)
        case "rename":
            if let new = askText(title: "Rename Workspace", message: "", value: name, ok: "Rename") {
                renameLayout(name, to: new)
            }
        case "launch":
            cfg.launch = m["value"] as? String
            saveConfig(cfg)
            config = cfg
        case "splitDirection":
            cfg.splitDirection = m["value"] as? String
            saveConfig(cfg)
            config = cfg
            dlog("verdict=split_direction_set value=\(cfg.splitDirection ?? "right")")
        case "toggle":
            let on = m["value"] as? Bool ?? true
            switch name {
            case "menuBar": cfg.menuBar = on
            case "notifications": setNotifications(on, source: "settings"); cfg = config
            case "dockBadge": cfg.dockBadge = on
            case "snapToGrid": cfg.snapToGrid = on
            case "openAtLogin": cfg.openAtLogin = on
            case "computerUse": cfg.computerUse = on
            case "quickAsk": cfg.quickAsk = on; if !desktopTestQuiet { setQuickAskHotKey(on) }
            case "zoomAllWindows":
                if on != zoomAllWindows { toggleZoomAllWindows(nil) }
                cfg = loadConfig()
            default: break
            }
            saveConfig(cfg)
            config = cfg
            applyBadge()
            if name == "openAtLogin" { applyOpenAtLogin() }
            if name == "menuBar" { startMenuBarItem() }
            if name == "computerUse" { MacComputer.shared.apply(on) }
        case "server":
            (keyController ?? openWindow(url: nil, frame: nil)).showConnectPage()
        case "permission":
            MacPermissions.turnOn(name)
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) { after() }
            return
        case "compactStorage": compactWebStorage(why: "settings"); return
        case "limit":
            // A number sets it; anything else (an emptied field) goes back to the default.
            guard appLimits.contains(where: { $0.key == name }) else { dlog("verdict=settings_limit_unknown name=\(name)"); return }
            var l = cfg.limits ?? [:]
            let v = (m["value"] as? NSNumber)?.doubleValue ?? (m["value"] as? String).flatMap { Double($0.trimmingCharacters(in: .whitespaces)) }
            if let v = v, v >= 0 { l[name] = v } else { l[name] = nil }
            cfg.limits = l.isEmpty ? nil : l
            saveConfig(cfg)
            config = cfg
            dlog("verdict=settings_limit_set name=\(name) value=\(v.map { String($0) } ?? "default")")
        case "update-check": Updater.shared.check(); return
        case "update-install": Updater.shared.install(); return
        default:
            dlog("verdict=settings_action_unknown action=\(action)")
            return
        }
        after()
    }

    /// Every Settings surface redraws from the new state.
    func settingsChanged() {
        settings?.reload()
        for p in allPages { tellMacSettings(p.webView) }
    }

    func tellMacSettings(_ w: WKWebView) {
        guard w.url.map({ isServerURL($0) }) ?? false,
              let d = try? JSONSerialization.data(withJSONObject: macSettingsState()),
              let json = String(data: d, encoding: .utf8) else { return }
        w.evaluateJavaScript("typeof _macSettingsState === 'function' && _macSettingsState(\(json))", completionHandler: nil)
    }

    /// What the This Mac tab draws. Keys are the toggles' names in settingsAction.
    func macSettingsState() -> [String: Any] {
        let c = loadConfig()
        let df = DateFormatter()
        df.dateStyle = .medium
        df.timeStyle = .short
        let u = Updater.shared
        let built = u.commit.isEmpty ? "a build that does not record its commit" : "built from \(String(u.commit.prefix(9)))"
            + (u.builtAt.isEmpty ? "" : " on \(u.builtAt)")
        return [
            "workspaces": (c.layouts ?? []).map { l -> [String: Any] in
                ["name": l.name,
                 "summary": workspaceSummary(l).components(separatedBy: "\n")[0],
                 "shows": workspaceWindows(l),
                 "saved": df.string(from: Date(timeIntervalSince1970: l.savedAt)),
                 "open": c.activeWorkspace == l.name]
            },
            "launch": c.launch ?? "last",
            "split": c.splitDirection ?? "right",
            "toggles": [
                "computerUse": c.computerUse ?? false,
                "quickAsk": c.quickAsk ?? true,
                "openAtLogin": c.openAtLogin ?? true,
                "menuBar": c.menuBar ?? true,
                "notifications": c.notifications ?? true,
                "dockBadge": c.dockBadge ?? true,
                "snapToGrid": c.snapToGrid ?? true,
                "zoomAllWindows": c.zoomAllWindows ?? true,
            ],
            "computerUseNow": MacComputer.shared.stateLine,
            "openAtLoginNow": openAtLoginStatus,
            "spacesCanMove": Spaces.canMove,
            "permissions": MacPermissions.all(),
            "update": ["built": built, "state": "\(u.state)", "detail": u.state == .idle ? "" : u.detail,
                       "canInstall": u.state == .available || (u.state == .failed && !u.latest.isEmpty),
                       "busy": u.state == .checking || u.state == .installing],
            "server": serverURL?.absoluteString ?? "",
            "storage": webStorageState(),
            "limits": appLimits.map { d -> [String: Any] in
                let env = ProcessInfo.processInfo.environment[d.key].flatMap(Double.init)
                return ["key": d.key, "label": d.label, "help": d.help, "unit": d.unit, "default": d.fallback,
                        "value": appLimit(d.key, d.fallback), "set": c.limits?[d.key] != nil, "fromEnv": env != nil]
            },
        ]
    }
}

func htmlEscape(_ s: String) -> String {
    s.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
        .replacingOccurrences(of: ">", with: "&gt;").replacingOccurrences(of: "\"", with: "&quot;")
        .replacingOccurrences(of: "'", with: "&#39;")
}

func jsString(_ s: String) -> String {
    let data = (try? JSONSerialization.data(withJSONObject: [s])) ?? Data("[\"\"]".utf8)
    let arr = String(data: data, encoding: .utf8) ?? "[\"\"]"
    return htmlEscape(String(arr.dropFirst().dropLast()))
}

func settingsHTML(_ app: AppDelegate) -> String {
    let c = loadConfig()
    let layouts = c.layouts ?? []
    let launch = c.launch ?? "last"
    let df = DateFormatter()
    df.dateStyle = .medium
    df.timeStyle = .short
    let rows = layouts.isEmpty
        ? "<div class=\"empty\">No saved workspaces yet. Arrange your windows across your Desktops, then save them below.</div>"
        : layouts.map { l -> String in
            let n = jsString(l.name)
            let what = app.workspaceSummary(l).components(separatedBy: "\n")[0] + (c.activeWorkspace == l.name ? " · open now" : "")
            let shows = app.workspaceWindows(l).map(htmlEscape).joined(separator: "<br>")
            return """
            <div class="row">
              <div class="info"><div class="name">\(htmlEscape(l.name))</div>
                <div class="meta">\(what) · saved \(df.string(from: Date(timeIntervalSince1970: l.savedAt)))\(shows.isEmpty ? "" : "<br>" + shows)</div></div>
              <button class="primary" onclick="send('open',\(n))">Open</button>
              <button onclick="send('update',\(n))" title="Replace with the windows open now">Update</button>
              <button onclick="send('rename',\(n))">Rename</button>
              <button class="danger" onclick="send('delete',\(n))">Delete</button>
            </div>
            """
        }.joined()
    let split = c.splitDirection ?? "right"
    let splitOptions = [("right", "To the right"), ("below", "Below"), ("longer", "Along the pane's longer side")]
        .map { "<option value=\"\($0.0)\"\(split == $0.0 ? " selected" : "")>\($0.1)</option>" }.joined()
    let launchOptions = [("last", "Reopen the windows from last time"), ("new", "Open one new window")]
        .map { "<option value=\"\($0.0)\"\(launch == $0.0 ? " selected" : "")>\($0.1)</option>" }.joined()
        + layouts.map { l in
            let v = "layout:" + l.name
            return "<option value=\"\(htmlEscape(v))\"\(launch == v ? " selected" : "")>Open workspace: \(htmlEscape(l.name))</option>"
        }.joined()
    func toggle(_ key: String, _ on: Bool, _ label: String, _ hint: String) -> String {
        """
        <label class="toggle"><input type="checkbox" \(on ? "checked" : "") onchange="send('toggle','\(key)',this.checked)">
          <span><b>\(label)</b><br><small>\(hint)</small></span></label>
        """
    }
    let server = htmlEscape(app.serverURL?.absoluteString ?? "not connected")
    let u = Updater.shared
    let built = u.commit.isEmpty ? "a build that does not record its commit" : "built from \(htmlEscape(String(u.commit.prefix(9))))"
        + (u.builtAt.isEmpty ? "" : " on \(htmlEscape(u.builtAt))")
    let updStatus = u.state == .idle ? "Check whether a newer build is out." : htmlEscape(u.detail)
    let busy = u.state == .checking || u.state == .installing
    let updButtons = (u.state == .available || (u.state == .failed && !u.latest.isEmpty)
        ? "<button class=\"primary\" id=\"upd-install\" onclick=\"send('update-install','')\"\(busy ? " disabled" : "")>Update and Relaunch</button>" : "")
        + "<button id=\"upd-check\" onclick=\"send('update-check','')\"\(busy ? " disabled" : "")>Check for Updates</button>"
    return """
    <!DOCTYPE html><html><head><meta charset="utf-8">
    <style>
      :root { color-scheme: light dark; --bg:#f6f8fa; --card:#fff; --text:#1f2328; --dim:#59636e; --border:#d1d9e0; --accent:#0969da; --danger:#cf222e; }
      @media (prefers-color-scheme: dark) { :root { --bg:#0d1117; --card:#161b22; --text:#e6edf3; --dim:#8b949e; --border:#30363d; --accent:#2f81f7; --danger:#f85149; } }
      * { box-sizing: border-box; }
      body { margin:0; padding:24px; font: 13px -apple-system, BlinkMacSystemFont, sans-serif; background:var(--bg); color:var(--text); }
      h2 { font-size: 13px; text-transform: uppercase; letter-spacing: .04em; color: var(--dim); margin: 22px 0 8px; }
      h2:first-child { margin-top: 0; }
      .card { background: var(--card); border: 1px solid var(--border); border-radius: 10px; padding: 4px 14px; }
      .row { display:flex; align-items:center; gap:8px; padding:10px 0; border-bottom:1px solid var(--border); }
      .row:last-child { border-bottom: none; }
      .info { flex:1; min-width:0; }
      .name { font-weight:600; font-size:14px; }
      .meta { color: var(--dim); font-size: 12px; margin-top: 2px; overflow-wrap: anywhere; }
      .empty { color: var(--dim); padding: 12px 0; }
      button { font: inherit; padding: 5px 11px; border-radius: 6px; border: 1px solid var(--border); background: var(--card); color: var(--text); cursor: pointer; }
      button:hover { border-color: var(--accent); }
      button:disabled { opacity: .5; cursor: default; }
      button.primary { background: var(--accent); border-color: var(--accent); color: #fff; }
      button.danger { color: var(--danger); }
      .save { display:flex; gap:8px; padding: 12px 0; }
      input[type=text], select { flex:1; font: inherit; padding: 6px 10px; border-radius: 6px; border: 1px solid var(--border); background: var(--bg); color: var(--text); }
      .toggle { display:flex; gap:10px; align-items:flex-start; padding: 10px 0; border-bottom: 1px solid var(--border); cursor: pointer; }
      .toggle:last-child { border-bottom: none; }
      .toggle input { margin-top: 3px; }
      small { color: var(--dim); }
      kbd { font: 12px ui-monospace, monospace; border: 1px solid var(--border); border-radius: 4px; padding: 1px 5px; }
    </style></head><body>
    <h2>Workspaces</h2>
    <div class="card">
      \(rows)
      <div class="save"><input type="text" id="lname" placeholder="Name this workspace, e.g. Morning triage"
        onkeydown="if(event.key==='Enter')saveNew()"><button class="primary" onclick="saveNew()">Save current windows</button></div>
    </div>
    <h2>Computer use</h2>
    <div class="card">
      \(toggle("computerUse", c.computerUse ?? false, "Let workers use this Mac's apps", "In the background, with their own cursor: your pointer and keyboard stay yours. Each worker may use only the apps you list for it (AMUX_MAC_APPS in the dashboard's Scope tab); terminals, password managers and System Settings stay off limits. ⌘⇧Esc stops it from any app. Now: \(htmlEscape(MacComputer.shared.stateLine))."))
    </div>
    <h2>Permissions</h2>
    <div class="card" id="permissions">
      \(MacPermissions.all().map { p in
        let on = p["state"] == "on"
        return "<div class=\"row\" data-permission=\"\(p["id"] ?? "")\"><div class=\"info\"><div class=\"name\">\(htmlEscape(p["name"] ?? ""))</div><small>\(htmlEscape(p["for"] ?? ""))</small></div>"
            + "<span class=\"perm-state\" data-state=\"\(p["state"] ?? "")\">\(on ? "On" : p["state"] == "off" ? "Off" : "Not set up")</span>"
            + (on ? "" : " <button onclick=\"send('permission','\(p["id"] ?? "")',true)\">Turn On</button>") + "</div>"
      }.joined(separator: "\n      "))
    </div>
    <h2>Quick Ask</h2>
    <div class="card">
      \(toggle("quickAsk", c.quickAsk ?? true, "⌥Space asks a worker from any app", "A small panel to send a worker a message, with the window you were in or the text you selected attached if you want."))
    </div>
    <h2>When amux opens</h2>
    <div class="card"><div class="save"><select onchange="send('launch','',this.value)">\(launchOptions)</select></div>
      \(toggle("openAtLogin", c.openAtLogin ?? true, "Open amux at login", "So a restart comes back to your windows on their Desktops. Now: \(htmlEscape(app.openAtLoginStatus))."))
      <div class="row"><div class="info"><small>Each window goes back to its display and Desktop (\(Spaces.canMove ? "available on this Mac" : "not available on this macOS, so windows open on the current Desktop")). Windows on a display that is not connected open on the main display and move back when it returns.</small></div></div>
    </div>
    <h2>General</h2>
    <div class="card">
      \(toggle("menuBar", c.menuBar ?? true, "Show in menu bar", "The amux icon, with who needs you, new windows, recording, computer use, pins and workspaces."))
      \(toggle("notifications", c.notifications ?? true, "Notifications", "Each new item waiting on you, and Messages addressed to you. The dashboard's Settings > Alerts has the same switch."))
      \(toggle("dockBadge", c.dockBadge ?? true, "Show count on Dock icon", "The number of items waiting on you."))
      <div class="row"><div class="info"><div class="name">Split opens the new pane</div>
        <small>The Split button, "Open in split pane" and "Split this tab".</small></div>
        <select onchange="send('splitDirection','',this.value)">\(splitOptions)</select></div>
      \(toggle("snapToGrid", c.snapToGrid ?? true, "Snap windows to a grid", "While you drag, an outline shows where the window lands: a 12 by 8 grid of the screen or another amux window's edge. Hold Option to place it freely."))
    </div>
    <h2>Updates</h2>
    <div class="card"><div class="row"><div class="info"><div class="name">This app: \(built)</div>
      <div class="meta" id="upd-status" data-state="\(u.state)">\(updStatus)</div></div>\(updButtons)</div></div>
    <h2>Server</h2>
    <div class="card"><div class="row"><div class="info"><div class="name">\(server)</div>
      <div class="meta">Every window connects here.</div></div><button onclick="send('server','')">Change…</button></div></div>
    <h2>Shortcuts</h2>
    <div class="card"><div class="row"><div class="info">
      <kbd>⌃⌥Space</kbd> show or hide amux · <kbd>⌘N</kbd> new window · <kbd>⌘T</kbd> new tab ·
      <kbd>⇧⌘N</kbd> current view in a new window · <kbd>⌥⌘S</kbd> save workspace · <kbd>⌃⌘1</kbd>…<kbd>9</kbd> switch workspace ·
      <kbd>⌃⌥⌘←→↑↓</kbd> halves · <kbd>⌃⌥⌘U I J K</kbd> quarters · <kbd>⌃⌥⌘D F G</kbd> thirds · <kbd>⌃⌥⌘↩</kbd> fill ·
      <kbd>⌃⌥⌘T</kbd> tile this Desktop · <kbd>⌃⌥⌘S</kbd> stack as tabs · <kbd>⌃⌥⌘N</kbd> next display</div></div></div>
    <script>
      function send(action, name, value) { window.webkit.messageHandlers.amuxSettings.postMessage({action, name, value}); }
      function saveNew() { const v = document.getElementById('lname').value.trim(); if (v) send('save', v); }
    </script>
    </body></html>
    """
}
