import Cocoa
import ServiceManagement

// The workspace: where every amux window lives and what it shows, kept across
// quits, crashes, reboots and display changes.
//
// - Each window's place is its display (by the monitor's UUID), its frame,
//   that frame as fractions of the display (for a resolution change), its
//   macOS Desktop (by the Space's UUID, then by its number on that display),
//   its tab group, and whether it is minimized or full screen.
// - The whole set autosaves within a few seconds of any change, so a crash or
//   power loss restores what was on screen moments before.
// - A window remembers its "home": where you last put it. A display that goes
//   away moves its windows to another one (macOS does that); when it comes
//   back the windows go home.
// - Snap to grid while dragging (with a preview of where the window lands),
//   and Window > Arrange for halves, thirds, quarters, tiling and stacking.

// ── Spaces (macOS Desktops) ──
// No public API names a window's Desktop or moves one there. The window
// server's own calls (the ones Mission Control and the Dock use) do both. They
// are looked up at run time, so a macOS without them restores on the current
// Desktop and logs why instead of failing to start.
enum Spaces {
    struct Info {
        let id: Int
        let uuid: String
        let display: String      // display UUID, or "Main" when Displays have separate Spaces is off
        let index: Int           // 1-based Desktop number on that display; 0 for a full-screen Space
        let fullscreen: Bool
        let current: Bool
    }

    private typealias MainConn = @convention(c) () -> Int32
    private typealias CopyDisplaySpaces = @convention(c) (Int32) -> Unmanaged<CFArray>?
    private typealias CopyWindowSpaces = @convention(c) (Int32, Int32, CFArray) -> Unmanaged<CFArray>?
    private typealias MoveWindows = @convention(c) (Int32, CFArray, Int) -> Void

    private static func sym<T>(_ name: String, _: T.Type) -> T? {
        guard let p = dlsym(UnsafeMutableRawPointer(bitPattern: -2), name) else { return nil }   // RTLD_DEFAULT
        return unsafeBitCast(p, to: T.self)
    }
    private static let mainConn = sym("CGSMainConnectionID", MainConn.self)
    private static let copyDisplaySpaces = sym("CGSCopyManagedDisplaySpaces", CopyDisplaySpaces.self)
    private static let copyWindowSpaces = sym("CGSCopySpacesForWindows", CopyWindowSpaces.self)
    private static let moveWindows = sym("CGSMoveWindowsToManagedSpace", MoveWindows.self)

    static var canRead: Bool { mainConn != nil && copyDisplaySpaces != nil && copyWindowSpaces != nil }
    static var canMove: Bool { canRead && moveWindows != nil }

    static func all() -> [Info] {
        guard let conn = mainConn?(), let arr = copyDisplaySpaces?(conn)?.takeRetainedValue() as? [[String: Any]] else { return [] }
        var out: [Info] = []
        for d in arr {
            let display = (d["Display Identifier"] as? String) ?? "Main"
            let cur = ((d["Current Space"] as? [String: Any])?["ManagedSpaceID"] as? Int) ?? -1
            var n = 0
            for s in (d["Spaces"] as? [[String: Any]]) ?? [] {
                guard let id = s["ManagedSpaceID"] as? Int else { continue }
                let fs = ((s["type"] as? Int) ?? 0) == 4
                if !fs { n += 1 }
                out.append(Info(id: id, uuid: (s["uuid"] as? String) ?? "", display: display,
                                index: fs ? 0 : n, fullscreen: fs, current: id == cur))
            }
        }
        return out
    }

    /// The Spaces a window is on: one normally, every Desktop for a window
    /// assigned to all of them, none while it is minimized.
    static func of(window number: Int) -> [Int] {
        guard number > 0, let conn = mainConn?(),
              let arr = copyWindowSpaces?(conn, 7, [number] as CFArray)?.takeRetainedValue() as? [Int] else { return [] }
        return arr
    }

    /// Moves the windows to a Space and reports whether the window server agrees it happened.
    @discardableResult
    static func move(windows numbers: [Int], to space: Int) -> Bool {
        let ids = numbers.filter { $0 > 0 }
        guard !ids.isEmpty, let conn = mainConn?(), let move = moveWindows else { return false }
        move(conn, ids as CFArray, space)
        return ids.allSatisfy { of(window: $0).contains(space) }
    }
}

// ── Displays ──
extension NSScreen {
    var displayID: CGDirectDisplayID {
        (deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value ?? 0
    }
    /// Stable across reboots and reconnects for the same monitor.
    var uuid: String {
        guard let u = CGDisplayCreateUUIDFromDisplayID(displayID)?.takeRetainedValue() else { return "" }
        return (CFUUIDCreateString(nil, u) as String?) ?? ""
    }
    static func with(uuid: String?) -> NSScreen? {
        guard let uuid = uuid, !uuid.isEmpty else { return nil }
        return screens.first { $0.uuid == uuid }
    }
}

func relRect(_ r: NSRect, in vf: NSRect) -> String {
    guard vf.width > 0, vf.height > 0 else { return "" }
    let f = { (v: CGFloat) in String(format: "%.4f", Double(v)) }
    return [f((r.minX - vf.minX) / vf.width), f((r.minY - vf.minY) / vf.height),
            f(r.width / vf.width), f(r.height / vf.height)].joined(separator: ",")
}

func fromRel(_ s: String?, in vf: NSRect) -> NSRect? {
    guard let p = s?.split(separator: ",").compactMap({ Double($0) }), p.count == 4 else { return nil }
    return NSRect(x: vf.minX + CGFloat(p[0]) * vf.width, y: vf.minY + CGFloat(p[1]) * vf.height,
                  width: CGFloat(p[2]) * vf.width, height: CGFloat(p[3]) * vf.height)
}

/// Where a saved frame goes on the displays connected now: its own display
/// at the same spot when it still fits there; scaled from its fractions when
/// that display's resolution changed; on the main display, scaled, when its
/// display is not connected.
func resolveFrame(_ s: SavedWindow) -> (frame: NSRect?, screen: NSScreen?) {
    let abs = NSRectFromString(s.frame)
    let home = NSScreen.with(uuid: s.display)
    let screen = home
        ?? (s.display == nil ? NSScreen.screens.max { $0.frame.intersection(abs).area < $1.frame.intersection(abs).area } : nil)
        ?? NSScreen.main ?? NSScreen.screens.first
    guard let scr = screen, abs.width > 100 else { return (nil, screen) }
    let vf = scr.visibleFrame
    if home != nil || s.display == nil, vf.insetBy(dx: -2, dy: -2).contains(abs) { return (abs, scr) }
    if let r = fromRel(s.rel, in: vf) { return (r.clamped(to: vf), scr) }
    return (abs.clamped(to: vf), scr)
}

extension NSRect {
    var area: CGFloat { isNull ? 0 : width * height }
    func clamped(to vf: NSRect) -> NSRect {
        var r = self
        r.size.width = min(max(r.width, 480), vf.width)
        r.size.height = min(max(r.height, 400), vf.height)
        r.origin.x = min(max(r.minX, vf.minX), vf.maxX - r.width)
        r.origin.y = min(max(r.minY, vf.minY), vf.maxY - r.height)
        return r
    }
}

// ── Capture ──
extension AppDelegate {
    /// Where the window is now, or where it belongs when that is a display
    /// that is not connected at the moment.
    func placement(of c: DashboardWindowController, spaces: [Spaces.Info], live: Bool = false) -> SavedWindow {
        if !live, let h = c.home, NSScreen.with(uuid: h.display) == nil, h.display != nil { return h }
        var p = SavedWindow(url: "", frame: "")
        guard let w = c.window else { return p }
        let fullscreen = w.styleMask.contains(.fullScreen)
        let frame = fullscreen ? (c.normalFrame ?? w.frame) : w.frame
        p.frame = NSStringFromRect(frame)
        let scr = w.screen ?? NSScreen.screens.max { $0.frame.intersection(frame).area < $1.frame.intersection(frame).area }
        if let scr = scr {
            p.display = scr.uuid
            p.displayName = scr.localizedName
            p.rel = relRect(frame, in: scr.visibleFrame)
        }
        if fullscreen { p.fullscreen = true }
        if w.isMiniaturized { p.minimized = true }
        let ids = Spaces.of(window: w.windowNumber)
        if ids.count > 1 {
            p.allDesktops = true
        } else if let id = ids.first, let sp = spaces.first(where: { $0.id == id }), !sp.fullscreen {
            c.lastSpace = sp
        }
        if !(p.allDesktops ?? false), let sp = c.lastSpace {
            p.space = sp.uuid
            p.spaceIndex = sp.index
        }
        return p
    }

    /// Every window, back to front, tab groups together in tab order.
    func snapshotNow() -> [SavedWindow] {
        let spaces = Spaces.all()
        let z = NSApp.orderedWindows
        var seen = Set<ObjectIdentifier>()
        var units: [(z: Int, items: [SavedWindow])] = []
        var groupN = 0
        for c in controllers {
            guard let w = c.window, !seen.contains(ObjectIdentifier(w)) else { continue }
            let tabWindows = w.tabbedWindows ?? [w]
            let members = tabWindows.compactMap { tw in controllers.first { $0.window === tw } }
            members.forEach { if let mw = $0.window { seen.insert(ObjectIdentifier(mw)) } }
            guard members.contains(where: { $0.window?.isVisible == true || $0.window?.isMiniaturized == true }) else { continue }
            let selected = w.tabGroup?.selectedWindow ?? w
            let lead = members.first { $0.window === selected } ?? members[0]
            let place = placement(of: lead, spaces: spaces)
            var group: Int? = nil
            if members.count > 1 { groupN += 1; group = groupN }
            let items = members.compactMap { m -> SavedWindow? in
                guard let url = m.savedURL() else { return nil }
                var s = place
                s.url = url
                let t = m.pageTitle
                if !t.isEmpty, t != "amux" { s.title = t }
                s.name = m.customName
                s.tabGroup = group
                if group != nil { s.selected = m.window === selected }
                s.grid = self.savedGrid(m)
                return s
            }
            if items.isEmpty { continue }
            let front = members.compactMap { m in m.window.flatMap { z.firstIndex(of: $0) } }.min() ?? Int.max
            units.append((front, items))
        }
        return units.sorted { $0.z > $1.z }.flatMap { $0.items }
    }
}

extension DashboardWindowController {
    /// The deep link this window reopens: the page it is on (or was on, while
    /// the server restarts and it shows the connect page) and its view.
    func savedURL() -> String? {
        // The tab's own server, so a tab on sandbox reopens on sandbox, not on the
        // default connection.
        let page = webView.url.flatMap { ($0.scheme == "https" || $0.scheme == "http") ? $0 : nil } ?? lastServerURL
        guard let base = page else { return nil }
        if !viewHash.isEmpty, let u = app?.url(withHash: viewHash, on: serverOrigin) { return u.absoluteString }
        return base.absoluteString
    }
}

// ── Restore ──
extension AppDelegate {
    /// After a page finishes loading, start the next restored tab that is waiting,
    /// so tabs behind others load one at a time instead of all at launch. A
    /// fallback timer keeps the queue moving if a page never finishes.
    func loadNextDeferred(after c: DashboardWindowController?) {
        guard let next = controllers.first(where: { $0.deferredURL != nil }) else { return }
        deferredTimer?.invalidate()
        deferredTimer = Timer.scheduledTimer(withTimeInterval: envSeconds("AMUX_DESKTOP_DEFER_STEP_S", 4), repeats: false) { [weak self] _ in
            self?.loadNextDeferred(after: nil)
        }
        next.loadDeferred(c == nil ? "timer" : "queue")
    }

    /// Opens saved windows where they were: display, frame, Desktop, tabs,
    /// minimized and full screen. Windows already showing the same page are
    /// reused rather than opened again, so switching workspaces keeps their
    /// state (scroll, terminal, a half-typed message).
    func openWindows(_ saved: [SavedWindow], reuse: [DashboardWindowController] = []) {
        guard serverURL != nil, !saved.isEmpty else {
            if reuse.isEmpty { openWindow(url: nil, frame: nil) }
            return
        }
        restoring = true
        defer { restoring = false }
        let spaces = Spaces.all()
        dissolveGrids(reuse)
        var pool = reuse
        var lastInGroup: [Int: DashboardWindowController] = [:]
        var selected: [Int: DashboardWindowController] = [:]
        var placed: [(DashboardWindowController, SavedWindow)] = []
        for s in saved {
            let (frame, screen) = resolveFrame(s)
            let c: DashboardWindowController
            if let i = pool.firstIndex(where: { $0.savedURL() == s.url }) {
                c = pool.remove(at: i)
                if let w = c.window {
                    if w.isMiniaturized { w.deminiaturize(nil) }
                    if w.styleMask.contains(.fullScreen) { w.toggleFullScreen(nil) }
                    if w.tabbedWindows != nil { w.moveTabToNewWindow(nil) }
                    if let f = frame { w.setFrame(f, display: false) }
                    w.orderFront(nil)
                }
            } else if s.tabGroup != nil, s.selected != true, s.grid == nil, let u = URL(string: s.url),
                      ProcessInfo.processInfo.environment["AMUX_DESKTOP_DEFER_TABS"] != "0" {
                // A tab behind another: it loads after the shown ones (loadNextDeferred).
                c = openWindow(url: nil, frame: frame, activate: false, load: false)
                c.deferredURL = u
                c.lastServerURL = u
                if let t = s.title, !t.isEmpty { c.pageLabel = t; c.updateTitle() }
            } else {
                c = openWindow(url: URL(string: s.url), frame: frame, activate: false)
                if let t = s.title, !t.isEmpty { c.pageLabel = t; c.updateTitle() }
            }
            c.home = s
            c.customName = s.name
            if s.name != nil { c.updateTitle() }
            c.home?.display = s.display ?? screen?.uuid
            c.home?.grid = nil
            if let g = s.grid { restoreGrid(g, in: c, pool: &pool) }
            if let g = s.tabGroup {
                if let prev = lastInGroup[g], let pw = prev.window, let cw = c.window {
                    pw.addTabbedWindow(cw, ordered: .above)
                }
                lastInGroup[g] = c
                if s.selected == true { selected[g] = c }
            }
            placed.append((c, s))
        }
        for c in pool { c.window?.close() }
        for (_, c) in selected { c.window?.tabGroup?.selectedWindow = c.window }

        // Desktops: move each window (a tab group as one) to its Space.
        var moved = 0, stayed = 0, missing = 0
        var done = Set<ObjectIdentifier>()
        for (c, s) in placed {
            guard let w = c.window, !done.contains(ObjectIdentifier(w)) else { continue }
            let group = (w.tabbedWindows ?? [w])
            group.forEach { done.insert(ObjectIdentifier($0)) }
            if s.allDesktops == true {
                group.forEach { $0.collectionBehavior.insert(.canJoinAllSpaces) }
                continue
            }
            guard s.space != nil || s.spaceIndex != nil else { continue }
            guard let target = resolveSpace(s, screen: w.screen, spaces: spaces) else { missing += 1; continue }
            if target.current { stayed += 1; c.lastSpace = target; continue }
            if Spaces.move(windows: group.map { $0.windowNumber }, to: target.id) {
                moved += 1
                c.lastSpace = target
            } else {
                missing += 1
            }
        }
        for (c, s) in placed {
            if s.minimized == true { c.window?.miniaturize(nil) }
            if s.fullscreen == true, let w = c.window, !w.styleMask.contains(.fullScreen) {
                c.normalFrame = w.frame
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { w.toggleFullScreen(nil) }
            }
        }
        // Focus the front-most window on the Desktop you are on, so restoring
        // does not pull you to another Desktop. Never a hidden tab: bringing
        // one forward makes it the visible tab of its group (a saved grid
        // came back showing Workers where Calendar had been).
        if let front = placed.reversed().first(where: { $0.0.window?.isOnActiveSpace == true && $0.1.minimized != true && $0.1.selected != false })?.0 {
            front.window?.makeKeyAndOrderFront(nil)
        }
        if controllers.contains(where: { $0.deferredURL != nil }) {
            // The queue also starts on its own if no shown page ever finishes.
            DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self] in
                if self?.deferredTimer == nil { self?.loadNextDeferred(after: nil) }
            }
        }
        NSLog("amux-desktop: verdict=windows_restored n_considered=%d desktops_moved=%d desktops_current=%d desktops_unavailable=%d spaces_readable=%d spaces_movable=%d displays=%d",
              saved.count, moved, stayed, missing, Spaces.canRead ? 1 : 0, Spaces.canMove ? 1 : 0, NSScreen.screens.count)
    }

    /// The Desktop a saved window belongs on: the same Space if it still
    /// exists, else the Desktop with the same number on the window's display.
    func resolveSpace(_ s: SavedWindow, screen: NSScreen?, spaces: [Spaces.Info]) -> Spaces.Info? {
        if let u = s.space, !u.isEmpty, let hit = spaces.first(where: { $0.uuid == u && !$0.fullscreen }) { return hit }
        guard let n = s.spaceIndex, n > 0 else { return nil }
        let disp = screen?.uuid ?? ""
        let onDisplay = spaces.filter { !$0.fullscreen && ($0.display == disp || $0.display == "Main") }
        return onDisplay.first { $0.index == n }
    }
}

// ── Autosave, crash and display-change handling ──
extension AppDelegate {
    func startWorkspaceTracking() {
        let nc = NotificationCenter.default
        nc.addObserver(forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main) { [weak self] _ in
            self?.screensChanged()
        }
        NSWorkspace.shared.notificationCenter.addObserver(forName: NSWorkspace.activeSpaceDidChangeNotification, object: nil, queue: .main) { [weak self] _ in
            self?.workspaceChanged()
        }
        // A window dragged to another Desktop in Mission Control sends no
        // window notification; a cheap signature check catches it.
        placementTimer = Timer.scheduledTimer(withTimeInterval: 4, repeats: true) { [weak self] _ in
            guard let self = self, !self.restoring, !self.terminating else { return }
            let sig = self.placementSignature()
            if sig != self.lastSignature { self.lastSignature = sig; self.saveWindowsSoon() }
        }
        // SIGTERM (a logout, a reboot, `kill`) quits the normal way, saving first.
        signal(SIGTERM, SIG_IGN)
        let src = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        src.setEventHandler { [weak self] in NSLog("amux-desktop: verdict=sigterm_quit"); self?.sigtermQuitting = true; NSApp.terminate(nil) }
        src.resume()
        sigtermSource = src
        // Another copy (tests, a second server) is driven by its pid, since an
        // amux:// link reaches whichever copy macOS picks.
        DistributedNotificationCenter.default().addObserver(forName: Notification.Name("io.amux.desktop.url.\(getpid())"), object: nil, queue: .main) { [weak self] n in
            if let s = n.object as? String, let u = URL(string: s) { self?.handleAmuxURL(u) }
        }
    }

    func placementSignature() -> String {
        controllers.compactMap { c -> String? in
            guard let w = c.window else { return nil }
            return "\(w.windowNumber):\(NSStringFromRect(w.frame)):\(Spaces.of(window: w.windowNumber)):\(w.isMiniaturized):\(c.viewHash):\(c.grid?.signature ?? "")"
        }.joined(separator: "|")
    }

    func workspaceChanged() {
        if !restoring { saveWindowsSoon() }
    }

    /// A move or resize sets the window's home, except one macOS makes because
    /// a display came or went. While the home display is unplugged only you
    /// (a drag, a resize, Arrange) can give the window a new home: macOS can
    /// report the move off an unplugged display before it reports the display
    /// change, so the grace period alone let that move overwrite the home.
    func windowPlaced(_ c: DashboardWindowController, user: Bool = false) {
        guard !restoring, !terminating else { return }
        let homeGone = c.home.map { $0.display != nil && NSScreen.with(uuid: $0.display) == nil } ?? false
        // Onto another display only by you: the app's own screen list can
        // still name a display macOS is already moving windows off.
        let sameDisplay = c.home?.display == nil || c.window?.screen?.uuid == c.home?.display
        if user || (Date() > screenGrace && !homeGone && sameDisplay), let w = c.window, !w.styleMask.contains(.fullScreen) {
            let before = c.home?.display
            c.home = placement(of: c, spaces: Spaces.all(), live: true)
            if before != nil, before != c.home?.display {
                NSLog("amux-desktop: verdict=window_home_display_changed window=%d from=%@ to=%@ by=%@ screens=%d",
                      w.windowNumber, before ?? "-", c.home?.display ?? "-", user ? "you" : "macos", NSScreen.screens.count)
            }
        }
        saveWindowsSoon()
    }

    func screensChanged() {
        screenGrace = Date().addingTimeInterval(3)
        let now = NSScreen.screens.map { "\($0.localizedName) \(NSStringFromRect($0.frame))" }
        NSLog("amux-desktop: verdict=displays_changed displays=%d [%@]", now.count, now.joined(separator: "; "))
        // Let macOS finish moving windows off a removed display first.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in self?.sendWindowsHome() }
    }

    /// Windows whose home display is connected again go back to it.
    func sendWindowsHome() {
        var n = 0
        for c in controllers {
            guard let h = c.home, let w = c.window, !w.styleMask.contains(.fullScreen),
                  let home = NSScreen.with(uuid: h.display) else { continue }
            let (frame, _) = resolveFrame(h)
            guard let f = frame else { continue }
            if w.screen?.uuid != home.uuid || abs(w.frame.minX - f.minX) > 2 || abs(w.frame.minY - f.minY) > 2 || abs(w.frame.width - f.width) > 2 {
                w.setFrame(f, display: true, animate: false)
                n += 1
            }
        }
        if n > 0 { NSLog("amux-desktop: verdict=display_windows_returned n=%d n_considered=%d", n, controllers.count) }
        saveWindowsSoon()
    }

    // ── Launch at login, so a reboot comes back to the workspace ──
    func applyOpenAtLogin() {
        guard #available(macOS 13.0, *) else { return }
        let want = config.openAtLogin ?? true
        let svc = SMAppService.mainApp
        guard Bundle.main.bundleURL.pathExtension == "app" else { return }
        do {
            if want, svc.status != .enabled { try svc.register() }
            if !want, svc.status == .enabled { try svc.unregister() }
        } catch {
            NSLog("amux-desktop: verdict=login_item_failed want=%d error=%@", want ? 1 : 0, error.localizedDescription)
        }
        NSLog("amux-desktop: login_item want=%d status=%d", want ? 1 : 0, svc.status.rawValue)
    }

    var openAtLoginStatus: String {
        guard #available(macOS 13.0, *) else { return "needs macOS 13" }
        switch SMAppService.mainApp.status {
        case .enabled: return "on"
        case .requiresApproval: return "waiting for approval in System Settings > General > Login Items"
        case .notRegistered: return "off"
        default: return "unavailable"
        }
    }
}

// ── Snap to grid and Arrange ──
// The grid is 12 columns by 8 rows of the display's usable area. A window
// dragged near a grid line or another amux window's edge snaps to it when you
// let go, and a translucent outline shows where while you drag. Hold Option
// while letting go to place it freely.
// What the drag code reads of the mouse. Under AMUX_DESKTOP_TEST=1 a test can
// set it (amux://test/pointer): a real drag needs synthetic mouse events, which
// macOS refuses an agent without the Accessibility grant. What that leaves
// untested is written in desktop/test/tab-snap-e2e.py.
enum Pointer {
    static var testOverride: (at: NSPoint, down: Bool)?
    static var location: NSPoint { testOverride?.at ?? NSEvent.mouseLocation }
    // The window server's own button state as well as AppKit's: during AppKit's
    // tab drag session the app receives no mouse events, and a real tab drag
    // (Ethan's recording, 2026-10-22 10:48) never got a preview or a zone.
    static var buttonDown: Bool {
        testOverride.map { $0.down }
            ?? (NSEvent.pressedMouseButtons & 1 != 0 || CGEventSource.buttonState(.combinedSessionState, button: .left))
    }
}
let desktopTestMode = ProcessInfo.processInfo.environment["AMUX_DESKTOP_TEST"] == "1"

let gridColumns: CGFloat = 12
let gridRows: CGFloat = 8
let snapDistance: CGFloat = 18

final class SnapPreview {
    let panel: NSPanel
    init() {
        panel = NSPanel(contentRect: .zero, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: true)
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.ignoresMouseEvents = true
        panel.level = .floating
        panel.collectionBehavior = [.moveToActiveSpace, .transient, .ignoresCycle]
        let v = NSView()
        v.wantsLayer = true
        v.layer?.backgroundColor = NSColor.controlAccentColor.withAlphaComponent(0.16).cgColor
        v.layer?.borderColor = NSColor.controlAccentColor.withAlphaComponent(0.75).cgColor
        v.layer?.borderWidth = 2
        v.layer?.cornerRadius = 10
        panel.contentView = v
    }
    func show(_ r: NSRect, below w: NSWindow) {
        panel.setFrame(r, display: true)
        if !panel.isVisible { panel.order(.below, relativeTo: w.windowNumber) }
    }
    /// For a tab being dragged out: there is no window under it yet.
    func showFront(_ r: NSRect) {
        panel.setFrame(r, display: true)
        if !panel.isVisible { panel.orderFrontRegardless() }
    }
    func hide() { panel.orderOut(nil) }
}

extension AppDelegate {
    var snapEnabled: Bool { config.snapToGrid ?? true }

    func gridLines(_ vf: NSRect, excluding w: NSWindow) -> (xs: [CGFloat], ys: [CGFloat]) {
        var xs = (0...Int(gridColumns)).map { vf.minX + vf.width * CGFloat($0) / gridColumns }
        var ys = (0...Int(gridRows)).map { vf.minY + vf.height * CGFloat($0) / gridRows }
        for c in controllers {
            guard let o = c.window, o !== w, o.isVisible, o.isOnActiveSpace, !o.isMiniaturized,
                  !(w.tabbedWindows?.contains(o) ?? false) else { continue }
            xs += [o.frame.minX, o.frame.maxX]
            ys += [o.frame.minY, o.frame.maxY]
        }
        return (xs, ys)
    }

    /// The nearest snap for a moved window: whichever edge is closest to a line, size unchanged.
    func snapMove(_ f: NSRect, _ w: NSWindow) -> NSRect {
        guard let vf = w.screen?.visibleFrame else { return f }
        let (xs, ys) = gridLines(vf, excluding: w)
        func best(_ a: CGFloat, _ b: CGFloat, _ lines: [CGFloat]) -> CGFloat {
            var d: CGFloat = .infinity
            for l in lines {
                if abs(l - a) < abs(d) { d = l - a }
                if abs(l - b) < abs(d) { d = l - b }
            }
            return abs(d) <= snapDistance ? d : 0
        }
        return f.offsetBy(dx: best(f.minX, f.maxX, xs), dy: best(f.minY, f.maxY, ys))
    }

    /// A resized window: each edge snaps on its own.
    func snapResize(_ f: NSRect, _ w: NSWindow) -> NSRect {
        guard let vf = w.screen?.visibleFrame else { return f }
        let (xs, ys) = gridLines(vf, excluding: w)
        func near(_ v: CGFloat, _ lines: [CGFloat]) -> CGFloat {
            let l = lines.min { abs($0 - v) < abs($1 - v) } ?? v
            return abs(l - v) <= snapDistance ? l : v
        }
        let x0 = near(f.minX, xs), x1 = near(f.maxX, xs), y0 = near(f.minY, ys), y1 = near(f.maxY, ys)
        let r = NSRect(x: x0, y: y0, width: x1 - x0, height: y1 - y0)
        return r.width >= w.minSize.width && r.height >= w.minSize.height ? r : f
    }

    func dragBegan(_ c: DashboardWindowController) {
        // windowWillMove also fires for a programmatic move (Arrange, a
        // restore); only a drag with the button down snaps.
        guard Pointer.buttonDown, !restoring,
              let w = c.window, !w.styleMask.contains(.fullScreen) else { return }
        dlog("verdict=window_drag_tracking")
        dragTimer?.invalidate()
        let t = Timer(timeInterval: 1.0 / 30, repeats: true) { [weak self, weak c] t in
            guard let self = self, let c = c, let w = c.window else { t.invalidate(); return }
            let free = !self.snapEnabled || NSEvent.modifierFlags.contains(.option)
            // At a display edge the zone sets the size; elsewhere the grid nudges the edges.
            let zone = free ? nil : self.zoneAtCursor()
            let target = zone?.frame ?? self.snapMove(w.frame, w)
            if Pointer.buttonDown {
                if !free, target != w.frame { self.snapPreview.show(target, below: w) } else { self.snapPreview.hide() }
                return
            }
            t.invalidate()
            self.snapPreview.hide()
            if !free, target != w.frame {
                w.setFrame(target, display: true, animate: true)
                self.arranged(w, to: target)
                if let z = zone { dlog("verdict=window_snapped_zone zone=\(z.zone.name)") }
            } else {
                self.windowPlaced(c, user: true)
            }
        }
        RunLoop.main.add(t, forMode: .common)
        dragTimer = t
    }

    /// The snap zone under the cursor, on whichever display the cursor is on.
    func zoneAtCursor() -> (zone: SnapZone, frame: NSRect)? {
        let p = Pointer.location
        guard let scr = NSScreen.screens.first(where: { $0.frame.insetBy(dx: -1, dy: -1).contains(p) }),
              let z = snapZone(cursor: p, screen: scr.frame, visible: scr.visibleFrame) else { return nil }
        return (z, z.frame(in: scr.visibleFrame))
    }

    /// A tab dragged out of a tab bar is a drag session, not a window move:
    /// windowWillMove never fires, so dragBegan never sees it. From a press in
    /// a tab bar to the release this previews the zone under the cursor, then
    /// places the window the tab became: in that zone, or snapped to the grid.
    func startTabDragTracking() {
        tabDragMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown) { [weak self] e in
            self?.tabPressed(e)
            return e
        }
        tabMenuMonitor = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { [weak self] e in
            (self?.tabRightClicked(e) ?? false) ? nil : e
        }
        // The fallback that needs no press: a window that leaves a tab group
        // while the button is down is a tab being dragged out, however AppKit
        // delivered (or did not deliver) the press.
        let t = Timer(timeInterval: 0.2, repeats: true) { [weak self] _ in self?.watchTabGroups() }
        RunLoop.main.add(t, forMode: .common)
        tearWatchTimer = t
    }

    func watchTabGroups() {
        var now: [ObjectIdentifier: (tabs: Int, frame: NSRect)] = [:]
        var leavers: [(NSWindow, NSRect)] = []
        for c in controllers {
            guard let w = c.window, w.isVisible else { continue }
            let id = ObjectIdentifier(w), n = w.tabbedWindows?.count ?? 1
            now[id] = (n, w.frame)
            if let was = tabGroupSeen[id], was.tabs > 1, n == 1 { leavers.append((w, was.frame)) }
        }
        tabGroupSeen = now
        guard !leavers.isEmpty, !(tabDragTimer?.isValid ?? false), Date().timeIntervalSince(lastTabPlaced) > 2,
              snapEnabled, !NSEvent.modifierFlags.contains(.option) else { return }
        // Which of the windows left alone is the torn tab is decided at the
        // release: the one that moved off the group's frame. The tab left
        // behind (or every one, when the tab joined another window) did not.
        dlog("verdict=tab_tear_seen_by_watch left_alone=\(leavers.count) button_down=\(Pointer.buttonDown ? 1 : 0)")
        followTornTab(leavers.map { $0.0 }, source: leavers[0].1)
    }

    /// Previews the zone under the cursor until the button is released, then
    /// places `w` (the window a tab became) there, or snaps it to the grid.
    func followTornTab(_ candidates: [NSWindow], source: NSRect) {
        tabDragTimer?.invalidate()
        var zone: (zone: SnapZone, frame: NSRect)? = nil
        var pane: PaneDropTarget? = nil
        var at = Pointer.location
        var sawDown = false
        let t = Timer(timeInterval: 1.0 / 30, repeats: true) { [weak self] t in
            guard let self = self else { t.invalidate(); return }
            if !Pointer.buttonDown, !sawDown {
                // The tear was seen only after the drop: AppKit's tab drag gives
                // the app no events, and the press monitor missed the press
                // (Ethan, 2026-10-23 21:00: "im dragging tabs nothing is
                // happening"). The pointer is where the tab was dropped, and
                // AppKit puts the window the tab became with its title bar under
                // it. A tab moved by the menu or a shortcut lands elsewhere and
                // is not docked.
                let torn = candidates.filter { w in
                    (abs(w.frame.minX - source.minX) > 1 || abs(w.frame.maxY - source.maxY) > 1)
                        && w.frame.insetBy(dx: -8, dy: -8).contains(at) && at.y >= w.frame.maxY - 80
                }
                if !torn.isEmpty { pane = self.paneDropTarget(at: at, excluding: torn) }
                if let p = pane { dlog("verdict=tab_pane_drop_found_at_release zone=\(p.name)") }
            }
            if Pointer.buttonDown {
                sawDown = true
                at = Pointer.location
                // Over an amux window's content: a pane of that window. The torn
                // window follows the pointer, so it is not the one under it.
                let moving = candidates.filter { abs($0.frame.minX - source.minX) > 1 || abs($0.frame.maxY - source.maxY) > 1 }
                pane = self.paneDropTarget(at: at, excluding: moving)
                let out = !source.insetBy(dx: -8, dy: -8).contains(at)
                zone = pane == nil && out ? self.zoneAtCursor() : nil
                if let p = pane { self.snapPreview.showFront(p.preview) } else if let z = zone { self.snapPreview.showFront(z.frame) } else { self.snapPreview.hide() }
                return
            }
            t.invalidate()
            self.snapPreview.hide()
            self.lastTabPlaced = Date()
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
                if let p = pane { self?.finishPaneDrop(p, at: at, from: candidates, source: source) }
                else { self?.placeTornTab(from: candidates, source: source, zone: zone) }
            }
        }
        RunLoop.main.add(t, forMode: .common)
        tabDragTimer = t
    }

    func tabPressed(_ e: NSEvent) {
        guard let w = e.window, controllers.contains(where: { $0.window === w }),
              let group = w.tabbedWindows, group.count > 1, !w.styleMask.contains(.fullScreen),
              e.locationInWindow.y >= (w.contentView?.frame.maxY ?? .infinity) else { return }
        let source = w.frame
        // The tab under the press: a drop on a window's own page is cancelled
        // by AppKit and the tab goes back to the bar, so docking it needs to
        // know which tab it was.
        let pressed = tabWindow(at: e.locationInWindow, in: w, group: group)
        dlog("verdict=tab_press_tracked tabs=\(group.count) button_down=\(Pointer.buttonDown ? 1 : 0) tab=\(pressed.map { g in controllers.first { $0.window === g }?.pageTitle ?? "?" } ?? "-")")
        tabDragTimer?.invalidate()
        // The last place the drag was seen with the button down: a real drop
        // lands there, and AppKit may already have split the tab off (and
        // moved its window under the pointer) by the release.
        var pane: PaneDropTarget? = nil
        var at = Pointer.location
        let t = Timer(timeInterval: 1.0 / 30, repeats: true) { [weak self] t in
            guard let self = self else { t.invalidate(); return }
            // Dragging the bar itself moves the whole window; dragBegan has that.
            let windowDrag = self.dragTimer?.isValid ?? false
            let free = !self.snapEnabled || NSEvent.modifierFlags.contains(.option) || windowDrag
            if Pointer.buttonDown {
                at = Pointer.location
                // Into an amux window's content (this one too): a pane there.
                let moving = group.filter { abs($0.frame.minX - source.minX) > 1 || abs($0.frame.maxY - source.maxY) > 1 }
                pane = free ? nil : self.paneDropTarget(at: at, excluding: moving)
            }
            let out = !source.insetBy(dx: -8, dy: -8).contains(at)
            let zone = free || !out || pane != nil ? nil : self.zoneAtCursor()
            if Pointer.buttonDown {
                if let p = pane { self.snapPreview.showFront(p.preview) } else if let z = zone { self.snapPreview.showFront(z.frame) } else if !windowDrag { self.snapPreview.hide() }
                return
            }
            t.invalidate()
            if !windowDrag { self.snapPreview.hide() }
            // Over an amux window with no pane: say what was on top, so a miss names its cause.
            let over = self.controllers.contains { $0.window?.isVisible == true && $0.window!.frame.contains(at) }
            let blocker = pane == nil && over && !free ? " blocker=\(self.paneBlocker ?? "-")" : ""
            dlog("verdict=tab_press_released out=\(out ? 1 : 0) free=\(free ? 1 : 0) window_drag=\(windowDrag ? 1 : 0) zone=\(pane.map { "pane " + $0.name } ?? zone?.zone.name ?? "none")\(blocker)")
            if let p = pane {
                self.lastTabPlaced = Date()
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
                    self?.finishPaneDrop(p, at: at, from: group, source: source, pressed: pressed)
                }
                return
            }
            guard out, !free else { return }
            // AppKit splits the tab off at the drop, so the watcher sees the
            // split just after this release: it must not place the window a
            // second time (11:24, Ethan's drag: right half, then grid).
            self.lastTabPlaced = Date()
            // AppKit makes the window the tab became key once the drop lands.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
                self?.placeTornTab(from: group, source: source, zone: zone)
            }
        }
        RunLoop.main.add(t, forMode: .common)
        tabDragTimer = t
    }

    /// A right-click on a tab: its own menu, with Rename Tab (Ethan,
    /// 2026-10-25: "right click a tab and name it"). AppKit offers no way to add
    /// to the tab bar's menu, so this replaces it, keeping the items it had
    /// that matter: move to a new window, close, close the others.
    func tabRightClicked(_ e: NSEvent) -> Bool {
        guard let w = e.window, controllers.contains(where: { $0.window === w }), !w.styleMask.contains(.fullScreen),
              e.locationInWindow.y >= (w.contentView?.frame.maxY ?? .infinity) else { return false }
        let group = w.tabbedWindows ?? [w]
        guard group.count > 1, let tw = tabWindow(at: e.locationInWindow, in: w, group: group),
              let c = controllers.first(where: { $0.window === tw }) else { return false }
        tabMenuTarget = c
        let m = NSMenu()
        func add(_ title: String, _ sel: Selector) { let i = NSMenuItem(title: title, action: sel, keyEquivalent: ""); i.target = self; m.addItem(i) }
        add("Rename Tab…", #selector(renameTargetTab(_:)))
        if c.customName != nil { add("Use Page Name", #selector(clearTargetTabName(_:))) }
        m.addItem(.separator())
        add("Move Tab to New Window", #selector(moveTargetTab(_:)))
        add("Close Tab", #selector(closeTargetTab(_:)))
        add("Close Other Tabs", #selector(closeOtherTabs(_:)))
        NSMenu.popUpContextMenu(m, with: e, for: w.contentView?.superview ?? w.contentView!)
        return true
    }
    @objc func renameTargetTab(_ sender: Any?) { if let c = tabMenuTarget { promptRename(c) } }
    @objc func clearTargetTabName(_ sender: Any?) { tabMenuTarget?.rename(nil) }
    @objc func moveTargetTab(_ sender: Any?) { tabMenuTarget?.window?.moveTabToNewWindow(nil) }
    @objc func closeTargetTab(_ sender: Any?) { tabMenuTarget?.window?.performClose(nil) }
    @objc func closeOtherTabs(_ sender: Any?) {
        guard let w = tabMenuTarget?.window else { return }
        for o in (w.tabbedWindows ?? []) where o !== w { o.performClose(nil) }
    }

    /// Ask for a tab's name; empty puts the page's own name back.
    func promptRename(_ c: DashboardWindowController) {
        let a = NSAlert()
        a.messageText = "Rename Tab"
        a.informativeText = "Leave it empty to use the page's own name."
        let f = NSTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        f.stringValue = c.customName ?? c.pageTitle
        a.accessoryView = f
        a.addButton(withTitle: "Rename")
        a.addButton(withTitle: "Cancel")
        a.window.initialFirstResponder = f
        let win = (c.paneHost ?? c).window
        let done: (NSApplication.ModalResponse) -> Void = { r in if r == .alertFirstButtonReturn { c.rename(f.stringValue) } }
        if let win = win { a.beginSheetModal(for: win, completionHandler: done) } else { done(a.runModal()) }
    }

    /// Which tab of `group` is at `p` (window coordinates) in `w`'s tab bar:
    /// from AppKit's own tab buttons when it has them, else the bar split
    /// evenly. Tabs are laid out in tabbedWindows order.
    func tabWindow(at p: NSPoint, in w: NSWindow, group: [NSWindow]) -> NSWindow? {
        guard group.count > 1 else { return nil }
        let buttons = tabButtonFrames(in: w)
        if buttons.count == group.count, let i = buttons.firstIndex(where: { $0.minX <= p.x && p.x < $0.maxX }) { return group[i] }
        let plus: CGFloat = 28   // the bar's + button
        let width = (w.frame.width - plus) / CGFloat(group.count)
        guard width > 0 else { return nil }
        let i = Int(p.x / width)
        dlog("verdict=tab_index_by_geometry buttons=\(buttons.count) tabs=\(group.count) kinds=\(Set(tabButtons(in: w).map { $0.name }).sorted().joined(separator: ","))")
        return i >= 0 && i < group.count ? group[i] : nil
    }

    /// AppKit's tab buttons in `w`'s tab bar, left to right, in window coordinates.
    func tabButtonFrames(in w: NSWindow) -> [NSRect] { tabButtons(in: w).map { $0.frame } }

    /// The views behind tabButtonFrames, with their class names (for the log).
    func tabButtons(in w: NSWindow) -> [(name: String, frame: NSRect)] {
        guard let frameView = w.contentView?.superview else { return [] }
        var buttons: [(name: String, frame: NSRect)] = []
        func walk(_ v: NSView) {
            let name = String(describing: type(of: v))
            // The bar's + button is a tab-bar button too; it opens a tab, it is not one.
            if (name.contains("TabButton") || name == "NSTabBarItemView") && !name.contains("New") && !name.contains("Add") {
                buttons.append((name, v.convert(v.bounds, to: nil))); return
            }
            v.subviews.forEach(walk)
        }
        walk(frameView)
        return buttons.sorted { $0.frame.minX < $1.frame.minX }
    }

    func placeTornTab(from group: [NSWindow], source: NSRect, zone: (zone: SnapZone, frame: NSRect)?) {
        // The window the tab became: from the group, on its own now, and moved
        // off the group's frame (AppKit puts it where it was dropped). Never by
        // focus: the tab left behind when a tab joins another window can be key,
        // and placing it moved the wrong window (tab-snap-e2e case 9).
        let alone = group.filter { w in controllers.contains { $0.window === w } && w.isVisible && (w.tabbedWindows?.count ?? 1) == 1 }
        let moved = alone.first { abs($0.frame.minX - source.minX) > 1 || abs($0.frame.maxY - source.maxY) > 1 }
        guard let k = moved,
              let c = controllers.first(where: { $0.window === k }) else {
            dlog("verdict=tab_drag_not_torn_off")
            return
        }
        lastTabPlaced = Date()
        let target = zone?.frame ?? (snapEnabled ? snapMove(k.frame, k) : k.frame)
        if target != k.frame {
            k.setFrame(target, display: true, animate: true)
            arranged(k, to: target)
        } else {
            windowPlaced(c, user: true)
        }
        dlog("verdict=tab_torn_off_snapped zone=\(zone?.zone.name ?? "grid")")
    }

    /// amux://test/... links, honoured only under AMUX_DESKTOP_TEST=1.
    func handleTestURL(_ url: URL) -> Bool {
        guard desktopTestMode, url.host == "test" else { return false }
        let q = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems ?? []
        func num(_ k: String) -> CGFloat { CGFloat(Double(q.first { $0.name == k }?.value ?? "") ?? 0) }
        func press(_ inTabBar: Bool) {
            guard let w = arrangeTarget, let cv = w.contentView else { return }
            var p = inTabBar ? NSPoint(x: cv.frame.midX, y: cv.frame.maxY + 8) : NSPoint(x: cv.frame.midX, y: cv.frame.midY)
            // ?title= presses that tab's own button in AppKit's tab bar.
            if inTabBar, let t = q.first(where: { $0.name == "title" })?.value, let group = w.tabbedWindows,
               let i = group.firstIndex(where: { tw in controllers.first { $0.window === tw }?.pageTitle == t }) {
                let b = tabButtonFrames(in: w)
                if b.count == group.count { p = NSPoint(x: b[i].midX, y: b[i].midY) }
                dlog("verdict=test_press_tab title=\(t) buttons=\(b.count) tabs=\(group.count)")
            }
            if let e = NSEvent.mouseEvent(with: .leftMouseDown, location: p, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                          windowNumber: w.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1) { tabPressed(e) }
        }
        switch url.path {
        case "/pointer": Pointer.testOverride = (NSPoint(x: num("x"), y: num("y")), q.contains { $0.name == "down" && $0.value == "1" })
        case "/pointer-clear": Pointer.testOverride = nil
        case "/recorder": recorderTestHook(q)
        case "/press-tab": press(true)          // the mouse-down that starts a tab drag
        case "/press-content": press(false)     // a click in the page, which must not
        // What AppKit does with a tab dropped outside every tab bar.
        // ?x=&y= puts the new window's title bar at that point, as AppKit
        // does with a tab dropped there.
        case "/tear-tab":
            let w = arrangeTarget
            w?.moveTabToNewWindow(nil)
            if let w = w, q.contains(where: { $0.name == "x" }) { w.setFrameTopLeftPoint(NSPoint(x: num("x") - w.frame.width / 2, y: num("y") + 14)) }
        // A real drop: the button comes up and AppKit splits the tab off in the same instant.
        // ?title= drops that tab of the group rather than the one showing.
        case "/drop-tab":
            if let o = Pointer.testOverride { Pointer.testOverride = (o.at, false) }
            let want = q.first { $0.name == "title" }?.value
            let tab = want.flatMap { t in (arrangeTarget?.tabbedWindows ?? []).first { tw in controllers.first { $0.window === tw }?.pageTitle == t } }
            if let t = want, tab == nil {
                // Dropping the showing tab instead would pass a test about a different tab.
                dlog("verdict=test_drop_tab_title_missing title=\(t) tabs=\((arrangeTarget?.tabbedWindows ?? []).compactMap { tw in controllers.first { $0.window === tw }?.pageTitle })")
            }
            (tab ?? arrangeTarget)?.moveTabToNewWindow(nil)
        // The test copy above every app, so the window under the pointer is its
        // own: macOS no longer lets a background app activate itself, and the
        // owner's real amux window covered the test window (2026-10-23).
        case "/activate":
            NSApp.activate(ignoringOtherApps: true)
            for c in controllers { c.window?.level = .floating }
            arrangeTarget?.orderFrontRegardless()
        // What AppKit does with a tab dropped on another window's tab bar.
        // The visible tab of a tab group, never whichever window is in front
        // (that made tab-snap-e2e case 9 flaky: a lone window joined instead).
        case "/join-tab":
            let ws = controllers.compactMap { $0.window }
            if let w = ws.first(where: { $0.isVisible && ($0.tabbedWindows?.count ?? 1) > 1 && $0.tabGroup?.selectedWindow === $0 }),
               let other = ws.first(where: { o in o !== w && o.isVisible && !(w.tabbedWindows?.contains(o) ?? false) }) {
                other.addTabbedWindow(w, ordered: .above)
            }
        // The tab bar's + button, on the window Arrange acts on.
        case "/new-tab":
            if let w = arrangeTarget, let c = controllers.first(where: { $0.window === w }) { c.newWindowForTab(nil) }
        // A pane's buttons and its header drag (amux-panes.swift), by the pane's
        // place in reading order in the window Arrange acts on.
        case "/pane-tab", "/pane-close", "/pane-drag":
            // The window holding a grid (a test has one), shown in its tab group.
            guard let h = controllers.first(where: { $0.grid?.host === $0 }), let g = h.grid else { break }
            h.window?.tabGroup?.selectedWindow = h.window
            let i = Int(num("i")), leaves = g.tree.leaves
            guard i >= 0, i < leaves.count, let v = g.views[leaves[i]] else { break }
            if url.path == "/pane-tab" { v.toTab() } else if url.path == "/pane-close" { v.closePane() }
            // After the window server has the grid's tab in front, as it is for a header you can see.
            else { let p = NSPoint(x: num("x"), y: num("y")); DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { v.dragged(to: p); v.dropped(at: p) } }
        // A pane's tab strip and the Panes menu (amux-panes.swift), on the
        // window holding a grid: i is the pane in reading order, t a tab in its strip.
        case "/pane-tab-select", "/pane-tab-close", "/pane-tab-drag", "/pane-focus", "/pane-action":
            guard let h = controllers.first(where: { $0.grid?.host === $0 }), let g = h.grid else { break }
            h.window?.tabGroup?.selectedWindow = h.window
            let leaves = g.tree.leaves, i = Int(num("i")), t = Int(num("t"))
            let v = i >= 0 && i < leaves.count ? g.views[leaves[i]] : nil
            switch url.path {
            case "/pane-tab-select": v?.selectTab(t)
            case "/pane-tab-close": v?.closeTab(t)
            case "/pane-focus": if i >= 0 && i < leaves.count { g.focus(leaves[i]) }
            case "/pane-tab-drag":
                let p = NSPoint(x: num("x"), y: num("y"))
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { v?.dragged(to: p, tab: t); v?.dropped(at: p, tab: t) }
            default:
                // The same actions the menu items send, as a key press reaches them.
                let item = NSMenuItem(); item.tag = Int(num("n"))
                actionGrid = g
                defer { actionGrid = nil }
                switch q.first(where: { $0.name == "a" })?.value ?? "" {
                case "new": paneNewTab(item)
                case "close": paneCloseTab(item)
                case "zoom": paneZoom(item)
                case "next": paneNextTab(item)
                case "prev": panePreviousTab(item)
                case "tab": paneSelectTab(item)
                case "left": paneFocusLeft(item)
                case "right": paneFocusRight(item)
                case "up": paneFocusUp(item)
                case "down": paneFocusDown(item)
                default: break
                }
            }
        // A divider dragged to fraction f (the first split of the window's grid).
        case "/pane-divider":
            guard let g = controllers.first(where: { $0.grid?.host === $0 })?.grid,
                  let sv = g.container.subviews.first as? GridSplitView else { break }
            sv.setPosition((sv.length - sv.dividerThickness) * num("f"), ofDividerAt: 0)
            saveWindowsSoon()
        case "/window-drag":
            if let w = arrangeTarget, let c = controllers.first(where: { $0.window === w }) { dragBegan(c) }
        // Through Window > Arrange > Snap to Grid While Dragging, as a person turns it off.
        case "/snap": if snapEnabled != (q.first { $0.name == "on" }?.value != "0") { toggleSnap(nil) }
        // Click a button in Settings by id, as a person does (desktop/test/update-e2e.py).
        case "/settings-click":
            showSettings(nil)
            let id = q.first { $0.name == "id" }?.value ?? ""
            // JSON-quoted for JavaScript (jsString also escapes for HTML attributes).
            let lit = String(data: (try? JSONSerialization.data(withJSONObject: [id])) ?? Data("[\"\"]".utf8), encoding: .utf8)?.dropFirst().dropLast() ?? "\"\""
            settings?.afterLoad.append { [weak self] in
                self?.settings?.webView.evaluateJavaScript("(function(){const b=document.getElementById(\(lit));if(!b)return 'missing';if(b.disabled)return 'disabled';b.click();return 'clicked'})()") { r, e in
                    dlog("verdict=test_settings_click id=\(id) result=\(r ?? "-") error=\(e.map { "\($0)" } ?? "-")")
                }
            }
        // Name a tab, as Rename Tab does (empty clears it).
        case "/rename-tab":
            let want = "#" + (q.first { $0.name == "view" }?.value ?? "")
            // A tab still waiting to load has no view yet; its saved URL names it.
            controllers.first { $0.viewHash == want || $0.savedURL().flatMap { URL(string: $0)?.fragment }.map { "#" + $0 } == want }?
                .rename(q.first { $0.name == "name" }?.value)
        // A notification click, the handler a real click runs (notify-click-e2e.py).
        case "/notification-click":
            if let w = q.first(where: { $0.name == "worker" })?.value { openNotificationTab("#peek=" + w, why: "worker") }
            else if let h = q.first(where: { $0.name == "hash" })?.value { openNotificationTab(h, why: "message") }
        case "/hide": NSApp.hide(nil)
        // Run a script in the shown tab's page (download-e2e.py starts a page
        // download with it). Test mode only, like every hook here.
        case "/js":
            if let code = q.first(where: { $0.name == "code" })?.value,
               let c = controllers.first(where: { isShown($0) }) ?? controllers.first {
                c.webView.evaluateJavaScript(code) { _, err in if let err = err { dlog("verdict=test_js_failed error=\(err.localizedDescription)") } }
            }
        // Run JS in pane `i` of the key window's grid (or its only page) and
        // write what it returns, as a string, to <state file>.js.
        case "/js-pane":
            let i = Int(q.first { $0.name == "i" }?.value ?? "") ?? 0
            let host = controllers.first { $0.grid != nil && isShown($0) } ?? controllers.first { isShown($0) }
            let target = host?.grid.flatMap { g in i >= 0 && i < g.tree.leaves.count ? g.members[g.tree.leaves[i]] : nil } ?? host
            if let code = q.first(where: { $0.name == "code" })?.value, let c = target,
               let path = ProcessInfo.processInfo.environment["AMUX_DESKTOP_TEST_STATE"] {
                c.webView.callAsyncJavaScript(code, arguments: [:], in: nil, in: .page) { r in
                    let out: String
                    switch r { case .success(let v): out = "\(v)"; case .failure(let e): out = "error: " + e.localizedDescription }
                    try? out.data(using: .utf8)?.write(to: URL(fileURLWithPath: path + ".js"), options: .atomic)
                }
            }
        // View > Zoom In / Zoom Out / Actual Size / Zoom All Windows (?op=in|out|reset|all).
        case "/zoom":
            switch q.first(where: { $0.name == "op" })?.value {
            case "in": zoomIn(nil)
            case "out": zoomOut(nil)
            case "reset": zoomReset(nil)
            case "all": toggleZoomAllWindows(nil)
            default: break
            }
        // Every page's zoom, in allPages order, to <state file>.zoom:
        // {"all": on, "levels": [dashboard level, or pageZoom percent as a negative]}.
        case "/zoom-levels":
            guard let path = ProcessInfo.processInfo.environment["AMUX_DESKTOP_TEST_STATE"] else { break }
            let pages = allPages
            var levels = [Int](repeating: 0, count: pages.count), left = pages.count
            for (i, c) in pages.enumerated() {
                c.webView.evaluateJavaScript("typeof _zoomLevel === 'number' ? _zoomLevel : 0") { [weak self] r, _ in
                    let n = (r as? NSNumber)?.intValue ?? 0
                    levels[i] = n > 0 ? n : -Int((c.webView.pageZoom * 100).rounded())
                    left -= 1
                    if left == 0, let data = try? JSONSerialization.data(withJSONObject: ["all": self?.zoomAllWindows ?? false, "levels": levels]) {
                        try? data.write(to: URL(fileURLWithPath: path + ".zoom"), options: .atomic)
                    }
                }
            }
        // A click on the Split button of the focused pane (or the only page).
        case "/split":
            let host = controllers.first { $0.grid != nil && isShown($0) } ?? controllers.first { isShown($0) }
            if let c = host?.grid.flatMap({ g in g.focused.flatMap { g.members[$0] } }) ?? host {
                c.webView.evaluateJavaScript("document.getElementById('peek-split-toggle').click()")
            }
        // Show the tab whose page is on #<view> (perf-bench.py times the switch).
        case "/select-view":
            let want = "#" + (q.first { $0.name == "view" }?.value ?? "")
            if let c = controllers.first(where: { $0.viewHash == want || $0.webView.url?.fragment.map { "#" + $0 } == want
                                                  || $0.savedURL()?.hasSuffix(want) == true }), let w = c.window {
                w.tabGroup?.selectedWindow = w
                w.orderFrontRegardless()
            }
        case "/state":
            let ws = controllers.compactMap { c -> [String: Any]? in
                guard let w = c.window, w.isVisible else { return nil }
                var d: [String: Any] = ["title": w.title, "frame": NSStringFromRect(w.frame), "tabs": (w.tabbedWindows ?? [w]).count, "key": w.isKeyWindow, "number": w.windowNumber,
                                        "view": c.viewHash, "selected": (w.tabGroup?.selectedWindow ?? w) === w,
                                        "connection": w.tab.accessoryView?.toolTip ?? "",
                                        "server": c.serverOrigin?.absoluteString ?? "", "saved": c.savedURL() ?? ""]
                if let g = c.grid {
                    d["grid"] = g.readingOrder
                    d["panes"] = g.tree.leaves.compactMap { g.members[$0]?.pageTitle }
                    let frames = Dictionary(g.paneFrames().map { ($0.id, $0.frame) }, uniquingKeysWith: { a, _ in a })
                    d["pane_frames"] = g.tree.leaves.map { NSStringFromRect(frames[$0] ?? .zero) }
                    d["pane_views"] = g.tree.leaves.map { g.members[$0]?.viewHash ?? "" }
                    d["pane_tabs"] = g.tree.leaves.map { g.tabs($0).map { $0.pageTitle } }
                    d["pane_active"] = g.tree.leaves.map { id in g.tabs(id).firstIndex { $0 === g.members[id] } ?? -1 }
                    d["pane_shown"] = g.tree.leaves.map { g.views[$0]?.window != nil }
                    d["focused"] = g.focused.flatMap { g.tree.leaves.firstIndex(of: $0) } ?? -1
                    d["zoomed"] = g.zoomed.flatMap { g.tree.leaves.firstIndex(of: $0) } ?? -1
                }
                return d
            }
            let st: [String: Any] = ["preview": snapPreview.panel.isVisible,
                                     "hidden_panes": controllers.filter { $0.paneHost != nil && $0.window?.isVisible == false }.count,
                                     // What a drop at the pointer hits: the window server's front window there.
                                     "pointer_window": NSWindow.windowNumber(at: Pointer.location, belowWindowWithWindowNumber: 0),
                                     "pointer_amux": amuxWindow(at: Pointer.location)?.window?.title ?? "",
                                     "content_frame": arrangeTarget.map { NSStringFromRect($0.convertToScreen($0.contentLayoutRect)) } ?? "", "preview_frame": NSStringFromRect(snapPreview.panel.frame),
                                     "tracking": tabDragTimer?.isValid ?? false,
                                     "visible_frame": NSStringFromRect(arrangeTarget?.screen?.visibleFrame ?? NSScreen.main?.visibleFrame ?? .zero),
                                     "screen_frame": NSStringFromRect(arrangeTarget?.screen?.frame ?? NSScreen.main?.frame ?? .zero),
                                     "windows": ws,
                                     // Every page, shown or not: its web process and what it costs.
                                     "pages": controllers.map { c -> [String: Any] in
                                         let pid = webPID(c)
                                         return ["view": c.viewHash, "shown": isShown(c), "pid": pid ?? 0,
                                                 "mb": pid.flatMap { footprintMB($0) } ?? -1, "cpu_s": pid.flatMap { cpuSeconds($0) } ?? -1]
                                     },
                                     "app_mb": footprintMB(getpid()) ?? -1, "app_cpu_s": cpuSeconds(getpid()) ?? -1,
                                     "update": ["state": "\(Updater.shared.state)", "detail": Updater.shared.detail,
                                                "commit": Updater.shared.commit, "latest": Updater.shared.latest]]
            if let path = ProcessInfo.processInfo.environment["AMUX_DESKTOP_TEST_STATE"],
               let data = try? JSONSerialization.data(withJSONObject: st, options: [.sortedKeys]) {
                try? data.write(to: URL(fileURLWithPath: path), options: .atomic)
            }
        default: return false
        }
        NSLog("amux-desktop: test_hook=%@", url.absoluteString)
        return true
    }

    func resizeEnded(_ c: DashboardWindowController) {
        guard let w = c.window else { return }
        if restoring { return }
        if snapEnabled, !NSEvent.modifierFlags.contains(.option), !w.styleMask.contains(.fullScreen) {
            let r = snapResize(w.frame, w)
            if r != w.frame { w.setFrame(r, display: true, animate: true); arranged(w, to: r); return }
        }
        windowPlaced(c, user: true)
    }

    // ── Window > Arrange ──
    /// The window Arrange acts on: the key window, or with amux in the
    /// background (an amux:// link), its front-most window on this Desktop.
    var arrangeTarget: NSWindow? {
        if let k = NSApp.keyWindow, controllers.contains(where: { $0.window === k }) { return k }
        return NSApp.orderedWindows.first { w in
            controllers.contains { $0.window === w } && w.isVisible && w.isOnActiveSpace && !w.isMiniaturized
        }
    }

    /// x, y, width, height as fractions of the display's usable area.
    func place(_ fx: CGFloat, _ fy: CGFloat, _ fw: CGFloat, _ fh: CGFloat) {
        guard let w = arrangeTarget, let vf = w.screen?.visibleFrame else { return }
        if w.styleMask.contains(.fullScreen) { return }
        let r = NSRect(x: vf.minX + vf.width * fx, y: vf.minY + vf.height * fy, width: vf.width * fw, height: vf.height * fh).integral
        w.setFrame(r, display: true, animate: true)
        arranged(w, to: r)
    }

    @objc func arrangeLeftHalf(_ s: Any?) { place(0, 0, 0.5, 1) }
    @objc func arrangeRightHalf(_ s: Any?) { place(0.5, 0, 0.5, 1) }
    @objc func arrangeTopHalf(_ s: Any?) { place(0, 0.5, 1, 0.5) }
    @objc func arrangeBottomHalf(_ s: Any?) { place(0, 0, 1, 0.5) }
    @objc func arrangeTopLeft(_ s: Any?) { place(0, 0.5, 0.5, 0.5) }
    @objc func arrangeTopRight(_ s: Any?) { place(0.5, 0.5, 0.5, 0.5) }
    @objc func arrangeBottomLeft(_ s: Any?) { place(0, 0, 0.5, 0.5) }
    @objc func arrangeBottomRight(_ s: Any?) { place(0.5, 0, 0.5, 0.5) }
    @objc func arrangeLeftThird(_ s: Any?) { place(0, 0, 1.0 / 3, 1) }
    @objc func arrangeCenterThird(_ s: Any?) { place(1.0 / 3, 0, 1.0 / 3, 1) }
    @objc func arrangeRightThird(_ s: Any?) { place(2.0 / 3, 0, 1.0 / 3, 1) }
    @objc func arrangeFill(_ s: Any?) { place(0, 0, 1, 1) }
    @objc func arrangeCenter(_ s: Any?) { place(0.15, 0.1, 0.7, 0.8) }

    /// The key window onto the next display, at the same relative spot.
    @objc func arrangeNextDisplay(_ s: Any?) {
        guard let w = arrangeTarget, let cur = w.screen, NSScreen.screens.count > 1,
              let i = NSScreen.screens.firstIndex(of: cur) else { return }
        let next = NSScreen.screens[(i + 1) % NSScreen.screens.count]
        if let r = fromRel(relRect(w.frame, in: cur.visibleFrame), in: next.visibleFrame) {
            let f = r.clamped(to: next.visibleFrame)
            w.setFrame(f, display: true, animate: true)
            arranged(w, to: f)
        }
    }

    /// Arrange is you placing the window: it becomes its home. Read once the
    /// move has finished: an animated setFrame returns before the window has
    /// moved, and the home it recorded was the display it was leaving.
    func arranged(_ w: NSWindow, to target: NSRect? = nil) {
        let delay = target.map { w.animationResizeTime($0) + 0.1 } ?? 0.4
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self, weak w] in
            guard let self = self, let w = w else { return }
            for c in self.controllers where c.window === w || (w.tabbedWindows?.contains { $0 === c.window } ?? false) {
                self.windowPlaced(c, user: true)
            }
        }
    }

    /// The windows on this Desktop and display (a tab group counts once).
    func windowsHere(_ scr: NSScreen) -> [NSWindow] {
        var seen = Set<ObjectIdentifier>()
        var out: [NSWindow] = []
        let z = NSApp.orderedWindows
        for w in z where controllers.contains(where: { $0.window === w }) {
            guard w.isVisible, w.isOnActiveSpace, !w.isMiniaturized, !w.styleMask.contains(.fullScreen),
                  w.screen == scr, !seen.contains(ObjectIdentifier(w)) else { continue }
            (w.tabbedWindows ?? [w]).forEach { seen.insert(ObjectIdentifier($0)) }
            out.append(w)
        }
        return out
    }

    /// Tiles every window on this Desktop and display into an even grid,
    /// front-most window top left.
    @objc func arrangeTile(_ s: Any?) {
        guard let scr = arrangeTarget?.screen ?? NSScreen.main else { return }
        let ws = windowsHere(scr)
        guard !ws.isEmpty else { return }
        let vf = scr.visibleFrame
        let n = ws.count
        var cols = Int(ceil(sqrt(Double(n) * Double(vf.width / vf.height) / 1.6)))
        cols = max(1, min(n, cols))
        let rows = Int(ceil(Double(n) / Double(cols)))
        let gap: CGFloat = 6
        let cw = (vf.width - gap * CGFloat(cols + 1)) / CGFloat(cols)
        let ch = (vf.height - gap * CGFloat(rows + 1)) / CGFloat(rows)
        for (i, w) in ws.enumerated() {
            let r = i / cols, col = i % cols
            // The last row's windows share its width.
            let inRow = r == rows - 1 ? n - cols * (rows - 1) : cols
            let rw = (vf.width - gap * CGFloat(inRow + 1)) / CGFloat(inRow)
            let width = r == rows - 1 ? rw : cw
            let x = vf.minX + gap + CGFloat(col) * (width + gap)
            let y = vf.maxY - gap - CGFloat(r + 1) * ch - CGFloat(r) * gap
            let f = NSRect(x: x, y: y, width: width, height: ch).integral
            w.setFrame(f, display: true, animate: true)
            arranged(w, to: f)
        }
        NSLog("amux-desktop: verdict=windows_tiled n=%d cols=%d rows=%d", n, cols, rows)
    }

    /// Every window on this Desktop and display as tabs of the key window.
    @objc func arrangeStack(_ s: Any?) {
        guard let lead = arrangeTarget, let scr = lead.screen else { return }
        var last = lead
        for w in windowsHere(scr) where w !== lead && !(lead.tabbedWindows?.contains(w) ?? false) {
            last.addTabbedWindow(w, ordered: .above)
            last = w
        }
        lead.makeKeyAndOrderFront(nil)
    }

    @objc func toggleSnap(_ s: Any?) {
        config = loadConfig()
        config.snapToGrid = !(config.snapToGrid ?? true)
        saveConfig(config)
    }
}

func buildArrangeMenu(_ d: AppDelegate) -> NSMenu {
    let m = NSMenu(title: "Arrange")
    let ctlOptCmd: NSEvent.ModifierFlags = [.control, .option, .command]
    func key(_ c: Int) -> String { String(Character(UnicodeScalar(c)!)) }
    let items: [(String, Selector, String)] = [
        ("Left Half", #selector(AppDelegate.arrangeLeftHalf(_:)), key(NSLeftArrowFunctionKey)),
        ("Right Half", #selector(AppDelegate.arrangeRightHalf(_:)), key(NSRightArrowFunctionKey)),
        ("Top Half", #selector(AppDelegate.arrangeTopHalf(_:)), key(NSUpArrowFunctionKey)),
        ("Bottom Half", #selector(AppDelegate.arrangeBottomHalf(_:)), key(NSDownArrowFunctionKey)),
        ("-", #selector(AppDelegate.arrangeFill(_:)), ""),
        ("Top Left", #selector(AppDelegate.arrangeTopLeft(_:)), "u"),
        ("Top Right", #selector(AppDelegate.arrangeTopRight(_:)), "i"),
        ("Bottom Left", #selector(AppDelegate.arrangeBottomLeft(_:)), "j"),
        ("Bottom Right", #selector(AppDelegate.arrangeBottomRight(_:)), "k"),
        ("-", #selector(AppDelegate.arrangeFill(_:)), ""),
        ("Left Third", #selector(AppDelegate.arrangeLeftThird(_:)), "d"),
        ("Center Third", #selector(AppDelegate.arrangeCenterThird(_:)), "f"),
        ("Right Third", #selector(AppDelegate.arrangeRightThird(_:)), "g"),
        ("-", #selector(AppDelegate.arrangeFill(_:)), ""),
        ("Fill", #selector(AppDelegate.arrangeFill(_:)), "\r"),
        ("Center", #selector(AppDelegate.arrangeCenter(_:)), "c"),
        ("Move to Next Display", #selector(AppDelegate.arrangeNextDisplay(_:)), "n"),
        ("-", #selector(AppDelegate.arrangeFill(_:)), ""),
        ("Tile Windows on This Desktop", #selector(AppDelegate.arrangeTile(_:)), "t"),
        ("Stack Windows on This Desktop as Tabs", #selector(AppDelegate.arrangeStack(_:)), "s"),
    ]
    for (title, sel, k) in items {
        if title == "-" { m.addItem(.separator()); continue }
        let it = m.addItem(withTitle: title, action: sel, keyEquivalent: k)
        it.keyEquivalentModifierMask = ctlOptCmd
        it.target = d
    }
    m.addItem(.separator())
    let snap = m.addItem(withTitle: "Snap to Grid While Dragging", action: #selector(AppDelegate.toggleSnap(_:)), keyEquivalent: "")
    snap.target = d
    return m
}
