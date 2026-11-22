import Cocoa
import AVFoundation
import ScreenCaptureKit
import UserNotifications
import Carbon.HIToolbox

// What the owner sees while a worker uses this Mac (owner, 2026-11-22: "Do
// them all", from research/athena-mac-app-teardown.md):
//
// - AgentWatch: a small live preview at the bottom right ("<worker> is using
//   <App>", its latest screenshot, Stop and Watch) and an outline over the
//   exact window being driven. Both come from the server's activity feed
//   (GET /api/computer/mac/activity), polled each second while the driver
//   runs, and show for 8 seconds after the last step.
// - Both are left out of screen capture (sharingType = .none), so they never
//   land in a worker's screenshot or the owner's screen share.
// - QuickAsk: ⌥Space from any app opens a small panel to send a worker a
//   message, optionally with the window you were in and the text you had
//   selected.
// - Permissions: what this Mac has granted amux, for Settings.

// MARK: - Live preview and window outline

final class AgentWatch {
    static let shared = AgentWatch()
    weak var app: AppDelegate?
    private var timer: Timer?
    private var polling = false
    private(set) var row: [String: Any]?
    // Made on first use: a window that exists unused is still one on the
    // window server (and a quiet test copy's check counts it).
    lazy var preview = AgentPreview()
    lazy var outline = AgentOutline()
    private var _madePanels = false
    static let showFor: TimeInterval = 8

    func start(_ app: AppDelegate) {
        self.app = app
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.tick() }
    }

    private func tick() {
        guard let app = app, MacComputer.shared.running else { show(nil); return }
        guard !polling else { return }
        polling = true
        app.api("/api/computer/mac/activity?limit=5") { [weak self] obj in
            self?.polling = false
            let rows = ((obj as? [String: Any])?["rows"] as? [[String: Any]]) ?? []
            let now = Date().timeIntervalSince1970 * 1000
            let live = rows.first { ($0["verdict"] as? String) == "called"
                && now - (($0["ts"] as? Double) ?? 0) < AgentWatch.showFor * 1000 }
            self?.show(live)
        }
    }

    private var lastWorker = ""
    func show(_ r: [String: Any]?) {
        let was = row != nil
        row = r
        guard let r = r else {
            if was { dlog("verdict=agent_view_hidden worker=\(lastWorker)") }
            if _madePanels { preview.hide(); outline.hide() }
            app?.menuBar?.setAgent(nil)
            return
        }
        let worker = (r["worker"] as? String) ?? "a worker"
        let appName = ((r["apps"] as? String) ?? "an app").components(separatedBy: " (").first ?? "an app"
        let headline = "\(worker) is using \(appName)"
        if !was || worker != lastWorker { dlog("verdict=agent_view_shown worker=\(worker) app=\(appName)") }
        lastWorker = worker
        _madePanels = true
        preview.show(headline: headline, screenshot: r["screenshot"] as? String, worker: worker)
        if let pid = r["pid"] as? Int, let wid = r["window_id"] as? Int, let f = windowFrame(pid: pid, windowID: wid) {
            outline.show(frame: f, label: "amux: \(worker) is working in this window")
        } else {
            outline.hide()
        }
        app?.menuBar?.setAgent(headline)
    }

    /// A window's frame in Cocoa coordinates, from the window server.
    func windowFrame(pid: Int, windowID: Int) -> NSRect? {
        guard let list = CGWindowListCopyWindowInfo([.optionIncludingWindow], CGWindowID(windowID)) as? [[String: Any]],
              let info = list.first, (info[kCGWindowOwnerPID as String] as? Int) == pid,
              let b = info[kCGWindowBounds as String] as? [String: CGFloat] else { return nil }
        let top = NSScreen.screens.first?.frame.maxY ?? 0
        let r = NSRect(x: b["X"] ?? 0, y: top - (b["Y"] ?? 0) - (b["Height"] ?? 0), width: b["Width"] ?? 0, height: b["Height"] ?? 0)
        return r.width > 1 && r.height > 1 ? r : nil
    }

    var state: [String: Any] {
        guard _madePanels else { return ["preview": false, "outline": false, "made": false, "worker": lastWorker] }
        return ["preview": preview.panel.isVisible, "headline": preview.headline.stringValue,
                "has_image": preview.image.image != nil, "preview_capturable": preview.panel.sharingType != .none,
                "outline": outline.panel.isVisible, "outline_frame": NSStringFromRect(outline.panel.frame),
                "outline_capturable": outline.panel.sharingType != .none, "worker": lastWorker, "made": true]
    }
}

final class AgentPreview: NSObject {
    let panel: NSPanel
    let headline = NSTextField(labelWithString: "")
    let image = NSImageView()
    private var worker = ""

    override init() {
        panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 300, height: 236), styleMask: [.borderless, .nonactivatingPanel],
                        backing: .buffered, defer: true)
        super.init()
        panel.level = .floating
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.sharingType = .none
        panel.hidesOnDeactivate = false
        panel.collectionBehavior = [.canJoinAllSpaces, .transient, .ignoresCycle]
        let bg = NSVisualEffectView()
        bg.material = .hudWindow
        bg.state = .active
        bg.wantsLayer = true
        bg.layer?.cornerRadius = 12
        bg.layer?.masksToBounds = true
        headline.font = .systemFont(ofSize: 12, weight: .semibold)
        headline.lineBreakMode = .byTruncatingTail
        image.imageScaling = .scaleProportionallyUpOrDown
        image.wantsLayer = true
        image.layer?.cornerRadius = 6
        image.layer?.masksToBounds = true
        let stop = NSButton(title: "Stop", target: self, action: #selector(stop(_:)))
        stop.bezelStyle = .rounded
        stop.toolTip = "Stop computer use for every worker (⌘⇧Esc)"
        let watch = NSButton(title: "Watch", target: self, action: #selector(watch(_:)))
        watch.bezelStyle = .rounded
        watch.toolTip = "Open this worker"
        let buttons = NSStackView(views: [watch, stop])
        let col = NSStackView(views: [headline, image, buttons])
        col.orientation = .vertical
        col.alignment = .leading
        col.spacing = 8
        col.edgeInsets = NSEdgeInsets(top: 10, left: 12, bottom: 10, right: 12)
        col.translatesAutoresizingMaskIntoConstraints = false
        bg.addSubview(col)
        NSLayoutConstraint.activate([
            col.leadingAnchor.constraint(equalTo: bg.leadingAnchor), col.trailingAnchor.constraint(equalTo: bg.trailingAnchor),
            col.topAnchor.constraint(equalTo: bg.topAnchor), col.bottomAnchor.constraint(equalTo: bg.bottomAnchor),
            image.widthAnchor.constraint(equalToConstant: 276), image.heightAnchor.constraint(equalToConstant: 150),
            headline.widthAnchor.constraint(equalToConstant: 276),
        ])
        panel.contentView = bg
    }

    func show(headline text: String, screenshot: String?, worker: String) {
        self.worker = worker
        headline.stringValue = text
        if let p = screenshot, let img = NSImage(contentsOfFile: p) { image.image = img }
        if !panel.isVisible, let s = NSScreen.main?.visibleFrame {
            panel.setFrameOrigin(NSPoint(x: s.maxX - panel.frame.width - 16, y: s.minY + 16))
        }
        panel.orderFrontRegardless()
    }

    func hide() { panel.orderOut(nil); image.image = nil }

    @objc func stop(_ sender: Any?) { MacComputer.shared.stopByOwner() }
    @objc func watch(_ sender: Any?) {
        let safe = worker.allSatisfy { $0.isLetter || $0.isNumber || "-_.".contains($0) }
        if safe, !worker.isEmpty { AmuxDesktop.delegate.openNotificationTab("#peek=" + worker, why: "agent_preview") }
    }
}

final class AgentOutline {
    let panel: NSPanel
    let label = NSTextField(labelWithString: "")

    init() {
        panel = NSPanel(contentRect: .zero, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: true)
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.ignoresMouseEvents = true
        panel.level = .floating
        panel.sharingType = .none
        panel.collectionBehavior = [.canJoinAllSpaces, .transient, .ignoresCycle]
        let v = NSView()
        v.wantsLayer = true
        v.layer?.borderColor = NSColor.systemTeal.withAlphaComponent(0.9).cgColor
        v.layer?.borderWidth = 3
        v.layer?.cornerRadius = 10
        label.font = .systemFont(ofSize: 11, weight: .semibold)
        label.textColor = .white
        label.drawsBackground = true
        label.backgroundColor = NSColor.systemTeal.withAlphaComponent(0.9)
        label.translatesAutoresizingMaskIntoConstraints = false
        v.addSubview(label)
        NSLayoutConstraint.activate([label.topAnchor.constraint(equalTo: v.topAnchor, constant: 6),
                                     label.centerXAnchor.constraint(equalTo: v.centerXAnchor)])
        panel.contentView = v
    }

    func show(frame: NSRect, label text: String) {
        label.stringValue = "  \(text)  "
        panel.setFrame(frame.insetBy(dx: -3, dy: -3), display: true)
        panel.orderFrontRegardless()
    }

    func hide() { panel.orderOut(nil) }
}

// MARK: - Quick Ask (⌥Space)

final class QuickAsk: NSObject, NSTextFieldDelegate {
    static let shared = QuickAsk()
    /// Whether the panel exists yet; reading state must not make it.
    static private(set) var made = false
    let panel: NSPanel
    let field = NSTextField()
    let workers = NSPopUpButton()
    let attachWindow = NSButton(checkboxWithTitle: "The window you were in", target: nil, action: nil)
    let attachSelection = NSButton(checkboxWithTitle: "The text you selected", target: nil, action: nil)
    let status = NSTextField(labelWithString: "")
    /// The app in front when Quick Ask opened, and the text selected there.
    private(set) var previousPID: pid_t = 0
    private(set) var selection = ""
    private(set) var lastSent: [String: Any] = [:]
    /// A test copy names the window to attach (its own), never the owner's.
    var testWindowPID: pid_t?

    override init() {
        panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 520, height: 150), styleMask: [.titled, .closable, .nonactivatingPanel, .fullSizeContentView],
                        backing: .buffered, defer: true)
        super.init()
        QuickAsk.made = true
        panel.title = "Ask a worker"
        panel.titlebarAppearsTransparent = true
        panel.level = .floating
        panel.isFloatingPanel = true
        panel.hidesOnDeactivate = false
        panel.sharingType = .none
        field.placeholderString = "Ask or tell a worker…"
        field.font = .systemFont(ofSize: 15)
        field.delegate = self
        field.target = self
        field.action = #selector(send(_:))
        let sendBtn = NSButton(title: "Send", target: self, action: #selector(send(_:)))
        sendBtn.keyEquivalent = "\r"
        sendBtn.bezelStyle = .rounded
        status.textColor = .secondaryLabelColor
        status.font = .systemFont(ofSize: 11)
        let attach = NSStackView(views: [NSTextField(labelWithString: "Attach:"), attachWindow, attachSelection])
        let bottom = NSStackView(views: [workers, status, NSView(), sendBtn])
        let col = NSStackView(views: [field, attach, bottom])
        col.orientation = .vertical
        col.alignment = .leading
        col.spacing = 10
        col.edgeInsets = NSEdgeInsets(top: 30, left: 14, bottom: 12, right: 14)
        field.translatesAutoresizingMaskIntoConstraints = false
        bottom.translatesAutoresizingMaskIntoConstraints = false
        panel.contentView = col
        NSLayoutConstraint.activate([field.widthAnchor.constraint(equalToConstant: 492), bottom.widthAnchor.constraint(equalToConstant: 492)])
    }

    func toggle() { panel.isVisible ? panel.orderOut(nil) : show() }

    func show() {
        let front = NSWorkspace.shared.frontmostApplication
        previousPID = front?.bundleIdentifier == Bundle.main.bundleIdentifier ? previousPID : (front?.processIdentifier ?? 0)
        selection = selectedText() ?? ""
        attachSelection.isEnabled = !selection.isEmpty
        attachSelection.state = .off
        attachSelection.toolTip = MacComputer.shared.accessibility ? (selection.isEmpty ? "No text was selected" : String(selection.prefix(200)))
            : "Needs Accessibility (Settings > Permissions)"
        attachWindow.state = .off
        attachWindow.toolTip = front?.localizedName.map { "A screenshot of \($0)'s front window" }
        status.stringValue = ""
        loadWorkers()
        if let s = NSScreen.main?.visibleFrame {
            panel.setFrameOrigin(NSPoint(x: s.midX - panel.frame.width / 2, y: s.maxY - s.height / 3))
        }
        panel.makeKeyAndOrderFront(nil)
        panel.makeFirstResponder(field)
        dlog("verdict=quick_ask_shown previous_app=\(front?.bundleIdentifier ?? "-") selection_chars=\(selection.count)")
    }

    private func loadWorkers() {
        let remembered = UserDefaults.standard.string(forKey: "quickAskWorker") ?? ""
        AmuxDesktop.delegate.api("/api/sessions") { [weak self] obj in
            guard let self = self else { return }
            let names = ((obj as? [[String: Any]]) ?? []).compactMap { $0["name"] as? String }.sorted()
            self.workers.removeAllItems()
            self.workers.addItems(withTitles: names)
            if names.contains(remembered) { self.workers.selectItem(withTitle: remembered) }
            if names.isEmpty { self.status.stringValue = "No workers on this server" }
        }
    }

    /// The selected text in the app in front, through Accessibility.
    func selectedText() -> String? {
        guard AXIsProcessTrusted() else { return nil }
        let sys = AXUIElementCreateSystemWide()
        var focused: AnyObject?
        guard AXUIElementCopyAttributeValue(sys, kAXFocusedUIElementAttribute as CFString, &focused) == .success,
              let el = focused else { return nil }
        var sel: AnyObject?
        guard AXUIElementCopyAttributeValue(el as! AXUIElement, kAXSelectedTextAttribute as CFString, &sel) == .success else { return nil }
        return (sel as? String).flatMap { $0.isEmpty ? nil : $0 }
    }

    @objc func send(_ sender: Any?) {
        let text = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let worker = workers.titleOfSelectedItem, !worker.isEmpty else { status.stringValue = "Pick a worker"; return }
        guard !text.isEmpty else { status.stringValue = "Type a message"; return }
        UserDefaults.standard.set(worker, forKey: "quickAskWorker")
        var body = text
        var attached: [String] = []
        if attachSelection.state == .on, !selection.isEmpty {
            body += "\n\nThe text I had selected:\n" + selection
            attached.append("selection")
        }
        let finish = { [weak self] in
            guard let self = self else { return }
            let enc = worker.addingPercentEncoding(withAllowedCharacters: .alphanumerics.union(CharacterSet(charactersIn: "-_."))) ?? worker
            AmuxDesktop.delegate.api("/api/sessions/\(enc)/send", method: "POST", body: ["text": body]) { obj in
                let ok = (obj as? [String: Any]).map { $0["error"] == nil } ?? false
                self.lastSent = ["worker": worker, "text": body, "ok": ok, "attached": attached]
                dlog("verdict=quick_ask_sent worker=\(worker) ok=\(ok ? 1 : 0) chars=\(body.count) attached=\(attached.joined(separator: ","))")
                if ok { self.field.stringValue = ""; self.panel.orderOut(nil) } else { self.status.stringValue = "Not sent" }
            }
        }
        if attachWindow.state == .on {
            captureWindow(pid: testWindowPID ?? previousPID) { path in
                if let p = path { body += "\n\nThe window I was in: \(p)"; attached.append("window") }
                else { dlog("verdict=quick_ask_window_uncaptured pid=\(self.testWindowPID ?? self.previousPID)") }
                finish()
            }
        } else {
            finish()
        }
    }

    /// The front window of `pid`, as a PNG under <AMUX_HOME>/quick-ask.
    func captureWindow(pid: pid_t, _ done: @escaping (String?) -> Void) {
        guard #available(macOS 14.0, *), pid > 0 else { done(nil); return }
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: false) { content, listErr in
            // Its normal window first; a panel or a raised window when that is all it has.
            let mine = (content?.windows ?? []).filter { $0.owningApplication?.processID == pid && $0.frame.width > 40 && $0.frame.height > 40 }
            let win = mine.first { $0.windowLayer == 0 } ?? mine.first
            guard let w = win else {
                dlog("verdict=quick_ask_window_none pid=\(pid) windows=\(content?.windows.count ?? -1) mine=\(mine.count) error=\(listErr?.localizedDescription ?? "-")")
                DispatchQueue.main.async { done(nil) }; return
            }
            let cfg = SCStreamConfiguration()
            cfg.width = Int(w.frame.width * 2)
            cfg.height = Int(w.frame.height * 2)
            SCScreenshotManager.captureImage(contentFilter: SCContentFilter(desktopIndependentWindow: w), configuration: cfg) { img, capErr in
                if img == nil { dlog("verdict=quick_ask_window_capture_failed pid=\(pid) layer=\(w.windowLayer) error=\(capErr?.localizedDescription ?? "-")") }
                var out: String?
                if let img = img {
                    let dir = MacComputer.shared.home.appendingPathComponent("quick-ask")
                    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                    let f = dir.appendingPathComponent("window-\(Int(Date().timeIntervalSince1970 * 1000)).png")
                    if let data = NSBitmapImageRep(cgImage: img).representation(using: .png, properties: [:]), (try? data.write(to: f)) != nil {
                        out = f.path
                    }
                }
                DispatchQueue.main.async { done(out) }
            }
        }
    }

    /// A test copy has no selection of the owner's to read.
    func testSelection(_ s: String) { selection = s; attachSelection.isEnabled = !s.isEmpty }

    var state: [String: Any] {
        ["visible": panel.isVisible, "workers": workers.itemTitles, "worker": workers.titleOfSelectedItem ?? "",
         "capturable": panel.sharingType != .none, "last_sent": lastSent, "selection": selection]
    }
}

/// ⌥Space opens Quick Ask from any app (Carbon: no permission needed).
var quickAskHotKeyRef: EventHotKeyRef?
func setQuickAskHotKey(_ on: Bool) {
    if let r = quickAskHotKeyRef { UnregisterEventHotKey(r); quickAskHotKeyRef = nil }
    guard on else { return }
    RegisterEventHotKey(UInt32(kVK_Space), UInt32(optionKey), EventHotKeyID(signature: OSType(0x616D7578), id: 4),
                        GetApplicationEventTarget(), 0, &quickAskHotKeyRef)
}

// MARK: - Permissions

enum MacPermissions {
    static var notifications: String = "unknown"

    /// Each permission: id, name, state (on / off / not set up), what it is for.
    static func all() -> [[String: String]] {
        let mic: String
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized: mic = "on"
        case .denied, .restricted: mic = "off"
        default: mic = "not set up"
        }
        return [
            ["id": "accessibility", "name": "Accessibility", "state": AXIsProcessTrusted() ? "on" : "off",
             "for": "Computer use acts in apps; Quick Ask reads the text you selected."],
            ["id": "screen", "name": "Screen Recording", "state": CGPreflightScreenCaptureAccess() ? "on" : "off",
             "for": "Computer use sees an app's window; Quick Ask attaches the window you were in."],
            ["id": "microphone", "name": "Microphone", "state": mic, "for": "Dictation and recordings."],
            ["id": "notifications", "name": "Notifications", "state": notifications, "for": "When a worker needs you."],
        ]
    }

    static func refreshNotifications(_ done: (() -> Void)? = nil) {
        UNUserNotificationCenter.current().getNotificationSettings { s in
            let v: String
            switch s.authorizationStatus {
            case .authorized, .provisional, .ephemeral: v = "on"
            case .denied: v = "off"
            default: v = "not set up"
            }
            DispatchQueue.main.async { notifications = v; done?() }
        }
    }

    /// Turn On: ask, or open the System Settings pane when macOS will not ask again.
    static func turnOn(_ id: String) {
        dlog("verdict=permission_turn_on id=\(id)")
        switch id {
        case "accessibility": MacComputer.shared.openPrivacyPane("accessibility")
        case "screen": MacComputer.shared.openPrivacyPane("screen")
        case "microphone":
            if AVCaptureDevice.authorizationStatus(for: .audio) == .notDetermined {
                AVCaptureDevice.requestAccess(for: .audio) { _ in }
            } else if let u = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone") {
                NSWorkspace.shared.open(u)
            }
        case "notifications":
            if let u = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension") { NSWorkspace.shared.open(u) }
        default: break
        }
    }
}
