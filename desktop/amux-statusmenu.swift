import Cocoa

// The amux icon in the menu bar (owner, 2026-11-22: "make a menu bar icon and
// put the most appropriate dropdown items"), shown as the app icon. It replaces the "C:12% M:60%"
// text readout, whose click opened a popover of pinned notes.
//
// The icon carries the needs-you count beside it, the same number as the Dock
// badge. The menu is rebuilt each time it opens, top to bottom:
//   - what needs you: each waiting worker, a click opens it
//   - Open amux, New Window, New Tab
//   - Start or stop a recording (and pause it while one runs)
//   - Pins and Workspaces, the same submenus as the Dock menu
//   - the server it talks to, and Connect to Server…
//   - this Mac's CPU and memory, the readout the old item showed
//   - Settings…, Check for Updates…, Quit amux
// A quiet test copy makes the item but keeps it off the menu bar.

/// The amux app icon itself (owner, 2026-11-22: "Use the amux icon for the
/// menu bar"), at menu bar size. It keeps its colours rather than being a
/// template, so it reads as amux in a light or dark menu bar.
func amuxMenuBarIcon() -> NSImage {
    let src = NSApp.applicationIconImage ?? NSImage(named: NSImage.applicationIconName) ?? NSImage()
    let img = (src.copy() as? NSImage) ?? src
    img.size = NSSize(width: 18, height: 18)
    img.isTemplate = false
    img.accessibilityDescription = "amux"
    return img
}

final class AppStatusMenu: NSObject, NSMenuDelegate {
    let item: NSStatusItem
    let menu = NSMenu(title: "amux")
    weak var app: AppDelegate?
    /// The last /api/needs-input items: worker, card, question.
    private(set) var needs: [[String: Any]] = []
    private(set) var needsCount = 0
    /// The last /api/computer/status: running sandboxes, and why there are none.
    private(set) var computers: [[String: Any]] = []
    private(set) var computerNote = "Checking…"
    static let maxNeeds = 8

    init(app: AppDelegate) {
        self.app = app
        item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        super.init()
        item.isVisible = !desktopTestQuiet
        menu.delegate = self
        menu.autoenablesItems = false
        item.menu = menu
        if let b = item.button {
            b.image = amuxMenuBarIcon()
            b.imagePosition = .imageLeading
            b.toolTip = "amux"
        }
        rebuild()
        dlog("verdict=status_menu_shown visible=\(item.isVisible ? 1 : 0)")
    }

    func stop() {
        NSStatusBar.system.removeStatusItem(item)
    }

    /// The needs-you poll (AppDelegate.pollNeedsInput) passes each answer here.
    func setNeeds(count: Int, items: [[String: Any]]) {
        needsCount = count
        needs = items
        applyTitle()
    }

    /// "<worker> is using <App>" while a worker uses this Mac (AgentWatch), else nil.
    private(set) var agent: String?
    func setAgent(_ headline: String?) {
        guard headline != agent else { return }
        agent = headline
        applyTitle()
    }

    /// The count beside the icon, and a dot while a worker uses this Mac
    /// (never red: it must not read as recording).
    private func applyTitle() {
        guard let b = item.button else { return }
        b.title = (needsCount > 0 ? " \(needsCount)" : "") + (agent != nil ? " ●" : "")
        let needs = needsCount > 0 ? "\(needsCount) need\(needsCount == 1 ? "s" : "") you" : ""
        b.toolTip = "amux" + ([needs, agent ?? ""].filter { !$0.isEmpty }.map { ": " + $0 }.joined())
    }

    /// The computer-use sandboxes (amux computer), polled with needs-you.
    func setComputers(_ d: [String: Any]?) {
        guard let d = d else { computers = []; computerNote = "Server did not answer"; return }
        computers = ((d["sandboxes"] as? [[String: Any]]) ?? []).filter { ($0["state"] as? String) == "running" }
        let docker = d["docker"] as? [String: Any]
        if (docker?["up"] as? Bool) == false { computerNote = "Docker is not running" }
        else if computers.isEmpty { computerNote = "No sandbox running" }
        else { computerNote = "" }
    }

    func menuNeedsUpdate(_ menu: NSMenu) { rebuild() }

    @discardableResult
    private func add(_ title: String, _ action: Selector?, to m: NSMenu? = nil, key: String = "", enabled: Bool = true) -> NSMenuItem {
        let it = NSMenuItem(title: title, action: action, keyEquivalent: key)
        it.target = self
        it.isEnabled = enabled && action != nil
        (m ?? menu).addItem(it)
        return it
    }

    private func header(_ title: String) {
        if #available(macOS 14.0, *) { menu.addItem(.sectionHeader(title: title)) }
        else { add(title, nil, enabled: false) }
    }

    func rebuild() {
        guard let app = app else { return }
        menu.removeAllItems()

        // What needs you.
        if needsCount > 0 {
            header(needsCount == 1 ? "1 needs you" : "\(needsCount) need you")
            for (i, n) in needs.prefix(Self.maxNeeds).enumerated() {
                let worker = (n["worker"] as? String) ?? ""
                let card = (n["card"] as? String) ?? ""
                let q = ((n["question"] as? String) ?? (n["context"] as? String) ?? "")
                    .replacingOccurrences(of: "\n", with: " ")
                let who = worker.isEmpty ? (card.isEmpty ? "amux" : card) : worker
                let it = add(who, #selector(openNeed(_:)))
                it.tag = i
                it.representedObject = who   // the attributed title below replaces .title
                if !q.isEmpty {
                    it.toolTip = q
                    let short = q.count > 60 ? String(q.prefix(59)) + "…" : q
                    let s = NSMutableAttributedString(string: who, attributes: [.font: NSFont.menuFont(ofSize: 0)])
                    s.append(NSAttributedString(string: "  " + short, attributes: [.font: NSFont.menuFont(ofSize: 0), .foregroundColor: NSColor.secondaryLabelColor]))
                    it.attributedTitle = s
                }
            }
            if needsCount > min(needs.count, Self.maxNeeds) || needs.count > Self.maxNeeds {
                add("All \(needsCount) on the board…", #selector(openBoard(_:)))
            }
        } else {
            add("Nothing needs you", nil, enabled: false)
        }
        menu.addItem(.separator())

        if let a = agent { header(a) }
        add("Open amux", #selector(openAmux(_:)))
        let qa = add("Ask a Worker…", #selector(AppDelegate.showQuickAsk(_:)), key: " ")
        qa.keyEquivalentModifierMask = [.option]
        qa.target = app
        add("New Window", #selector(AppDelegate.newWindow(_:)), key: "").target = app
        add("New Tab", #selector(AppDelegate.newTabAction(_:))).target = app
        menu.addItem(.separator())

        let r = NativeRecorder.shared
        add(r.isActive ? "Stop Recording" : "Start Recording", #selector(AppDelegate.toggleRecording(_:))).target = app
        if r.isActive {
            add(r.state == "paused" ? "Resume Recording" : "Pause Recording", #selector(AppDelegate.pauseOrResumeRecording(_:))).target = app
        }
        add("Recordings", #selector(AppDelegate.openRecordTab(_:)), enabled: app.serverURL != nil).target = app
        // Computer use: each running sandbox, a click watches its desktop live.
        let cu = add("Computer Use", nil)
        cu.isEnabled = true
        let cm = NSMenu(title: "Computer Use")
        cm.autoenablesItems = false
        // This Mac's own apps (amux-computer.swift).
        let mc = MacComputer.shared
        add("This Mac: \(mc.stateLine)", nil, to: cm, enabled: false)
        let sw = add("Let Workers Use This Mac's Apps", #selector(AppDelegate.toggleComputerUse(_:)), to: cm, enabled: mc.binary != nil)
        sw.target = app
        sw.state = (app.config.computerUse ?? false) && !mc.stoppedByOwner ? .on : .off
        if mc.running && !mc.accessibility { add("Grant Accessibility…", #selector(AppDelegate.grantAccessibility(_:)), to: cm).target = app }
        if mc.running && !mc.screenRecording { add("Grant Screen Recording…", #selector(AppDelegate.grantScreenRecording(_:)), to: cm).target = app }
        if mc.running {
            let st = add("Stop Computer Use", #selector(AppDelegate.stopComputerUse(_:)), to: cm, key: "\u{1b}")
            st.keyEquivalentModifierMask = [.command, .shift]
            st.target = app
        }
        cm.addItem(.separator())
        add("Linux sandboxes", nil, to: cm, enabled: false)
        if !computerNote.isEmpty { add(computerNote, nil, to: cm, enabled: false) }
        for (i, c) in computers.enumerated() {
            let lane = (c["lane"] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? (c["name"] as? String) ?? "sandbox"
            let it = add("Watch \(lane)", #selector(watchComputer(_:)), to: cm, enabled: c["vnc_port"] is Int && serverIsLocal)
            it.tag = i
            if !serverIsLocal { it.toolTip = "The live view is on the server's own machine" }
        }
        cu.submenu = cm
        menu.addItem(.separator())

        let pins = add("Pins", nil)
        pins.isEnabled = true
        let pm = NSMenu(title: "Pins")
        for it in app.pinsMenu.items { pm.addItem(it.copy() as! NSMenuItem) }
        pins.submenu = pm
        let ws = add("Workspaces", nil)
        ws.isEnabled = true
        let wm = NSMenu(title: "Workspaces")
        for it in app.layoutsMenu.items { wm.addItem(it.copy() as! NSMenuItem) }
        ws.submenu = wm
        menu.addItem(.separator())

        let host = app.serverURL.map { u in app.config.connections.first { normalized($0.url) == normalized(u.absoluteString) }?.name ?? u.host ?? u.absoluteString }
        add(host.map { "Connected to \($0)" } ?? "Not connected", nil, enabled: false)
        add("Connect to Server…", #selector(AppDelegate.showConnect(_:))).target = app
        let s = pollStats()
        add("This Mac: CPU \(s.cpuPct)% · Memory \(s.memPct)%", nil, enabled: false)
        menu.addItem(.separator())

        add("Settings…", #selector(AppDelegate.showSettings(_:))).target = app
        add("Check for Updates…", #selector(AppDelegate.checkForUpdates(_:))).target = app
        add("Quit amux", #selector(NSApplication.terminate(_:))).target = NSApp
    }

    @objc func openNeed(_ sender: NSMenuItem) {
        guard let app = app, needs.indices.contains(sender.tag) else { return }
        let n = needs[sender.tag]
        let worker = (n["worker"] as? String) ?? ""
        dlog("verdict=status_menu_need_opened worker=\(worker.isEmpty ? "-" : worker)")
        if !worker.isEmpty, worker.allSatisfy({ $0.isLetter || $0.isNumber || "-_.".contains($0) }) {
            app.openNotificationTab("#peek=" + worker, why: "status_menu")
        } else {
            app.openCard((n["card"] as? String) ?? "")
        }
    }

    /// The sandboxes publish their noVNC port on the server's loopback only.
    var serverIsLocal: Bool {
        guard let h = app?.serverURL?.host?.lowercased() else { return false }
        return h == "localhost" || h == "127.0.0.1" || h == "::1"
    }

    @objc func watchComputer(_ sender: NSMenuItem) {
        guard let app = app, computers.indices.contains(sender.tag), let port = computers[sender.tag]["vnc_port"] as? Int,
              let u = URL(string: "http://127.0.0.1:\(port)/") else { return }
        dlog("verdict=status_menu_computer_watch lane=\((computers[sender.tag]["lane"] as? String) ?? "-") port=\(port)")
        app.openWindow(url: u, frame: nil)
    }

    @objc func openBoard(_ sender: Any?) {
        app?.openNotificationTab("#view=board", why: "status_menu")
    }

    @objc func openAmux(_ sender: Any?) {
        guard let app = app else { return }
        NSApp.unhide(nil)
        activateApp()
        if !app.controllers.contains(where: { $0.window?.isVisible == true }) { app.openWindow(url: nil, frame: nil) }
    }

    private func label(_ it: NSMenuItem) -> String { (it.representedObject as? String) ?? it.title }

    /// For amux://test/state: what the menu would show now.
    func snapshot() -> [String: Any] {
        rebuild()
        return ["visible": item.isVisible, "title": item.button?.title ?? "", "agent": agent ?? "", "has_icon": item.button?.image != nil,
                "items": menu.items.map { $0.isSeparatorItem ? "-" : label($0) },
                "computer": computerNote, "computers": computers.count,
                "enabled": menu.items.filter { !$0.isSeparatorItem }.map { $0.isEnabled }]
    }

    /// For amux://test/status-menu?title=: chooses an item, in the menu or a submenu, as a click does.
    func choose(_ title: String) -> Bool {
        rebuild()
        func pick(_ m: NSMenu) -> Bool {
            if let i = m.items.firstIndex(where: { label($0) == title && $0.action != nil && $0.isEnabled }) {
                m.performActionForItem(at: i)
                return true
            }
            return m.items.contains { $0.submenu.map(pick) ?? false }
        }
        return pick(menu)
    }
}
