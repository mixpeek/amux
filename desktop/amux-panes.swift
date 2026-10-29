import Cocoa
import WebKit

// Panes: a tab dragged from the tab bar into an amux window splits THAT window
// into a grid, and the tab becomes a pane in it (Ethan, 2026-10-23: "i want it
// to be all within the same amux window rather than the mac desktop"). Desktop
// snap zones (amux-snapzones.swift) still place a tab dropped outside every
// window.
//
// A pane is the dragged tab's own controller and web view: the web view moves
// into the grid and the tab's window is hidden, so the page keeps its state
// (scroll, terminal, a half-typed message). The window the grid lives in is its
// host. Dividers resize. The layout is amux-panegrid.swift's tree, saved with
// the workspace.
//
// EACH PANE IS ITS OWN SET OF TABS (Ethan, 2026-10-24: "my thing remains
// standalone relative to the other one im next to but my thing can have its
// own tabs and the others can have their own tabs and i can do up to 4 panes
// per view"). A window's native tabs are its views; a view holds up to
// `maxPanes` panes; a pane holds any number of tabs in a strip across its top.
// A tab dropped on a pane's middle joins that pane's tabs, on an edge it splits
// the pane, and on a full view it always joins. One pane has focus (the last
// one clicked): ⌘T, ⌘W, ⌘1-9 and Window > Panes act on it, ⌘⌥ and an arrow
// moves focus, ⌘⇧↩ zooms it to the whole view and back. Dragging a strip tab
// moves that tab: to another pane, another view, or out of every window to
// become a window tab again. A tab not showing in its pane is out of the
// window, so its page reads as hidden and does its hidden-tab deferrals.

final class PaneGrid {
    var tree: PaneTree
    /// The tab each pane shows.
    var members: [Int: DashboardWindowController]
    /// Every tab of each pane in strip order; members[id] is one of them.
    var stacks: [Int: [DashboardWindowController]]
    /// The pane ⌘T, ⌘W and the Panes menu act on: the last one clicked.
    var focused: Int?
    /// A pane shown alone over the whole view (⌘⇧↩); the layout is kept.
    var zoomed: Int?
    var nextId: Int
    weak var host: DashboardWindowController?
    let container = NSView()
    var views: [Int: PaneView] = [:]
    private var clickMonitor: Any?

    init(host: DashboardWindowController) {
        self.host = host
        tree = .leaf(0)
        members = [0: host]
        stacks = [0: [host]]
        focused = 0
        nextId = 1
        container.autoresizingMask = [.width, .height]
        clickMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown) { [weak self] e in self?.noteClick(e); return e }
    }
    deinit { if let m = clickMonitor { NSEvent.removeMonitor(m) } }

    func id(of c: DashboardWindowController) -> Int? { stacks.first { $0.value.contains { $0 === c } }?.key }
    func tabs(_ id: Int) -> [DashboardWindowController] { stacks[id] ?? [] }
    /// Every page in the grid, pane by pane in reading order.
    var all: [DashboardWindowController] { tree.leaves.flatMap { tabs($0) } }

    /// A click in a pane gives it focus.
    func noteClick(_ e: NSEvent) {
        guard let w = e.window, w === host?.window else { return }
        if let hit = views.first(where: { $0.value.window === w && $0.value.convert($0.value.bounds, to: nil).contains(e.locationInWindow) }) { focus(hit.key) }
    }

    func focus(_ id: Int?) {
        focused = id
        let shown = views.values.filter { $0.window != nil }.count
        for (i, v) in views { v.isFocused = shown > 1 && i == id }
    }

    /// `c`, a tab of pane `id`, becomes the one it shows.
    func select(_ c: DashboardWindowController, in id: Int) {
        guard tabs(id).contains(where: { $0 === c }) else { return }
        members[id] = c
        views[id]?.attach(c.webView)
        views[id]?.reloadStrip()
        focus(id)
        host?.window?.makeFirstResponder(c.webView)
        host?.updateTitle()
        dlog("verdict=pane_tab_selected pane_tabs=\(tabs(id).count)")
        host?.app?.saveWindowsSoon()
    }

    func refreshStrips() { views.values.forEach { $0.reloadStrip() } }

    /// The tree with each split's fraction as the dividers have it now.
    func syncFractions() {
        guard let root = container.subviews.first else { return }
        func walk(_ t: PaneTree, _ v: NSView) -> PaneTree {
            guard case .split(let row, let f, let a, let b) = t, let sv = v as? GridSplitView, sv.subviews.count == 2 else { return t }
            return .split(row: row, fraction: sv.currentFraction ?? f, walk(a, sv.subviews[0]), walk(b, sv.subviews[1]))
        }
        tree = walk(tree, root)
    }

    /// Rebuilds the split views for the tree. Web views move into their panes;
    /// WebKit keeps the pages running while they do.
    func rebuild() {
        container.subviews.forEach { $0.removeFromSuperview() }
        if let z = zoomed, !tree.leaves.contains(z) { zoomed = nil }
        func make(_ t: PaneTree) -> NSView {
            switch t {
            case .leaf(let id):
                let v = views[id] ?? PaneView(grid: self, id: id)
                views[id] = v
                if let c = members[id] { v.attach(c.webView) }
                return v
            case .split(let row, let f, let a, let b):
                let s = GridSplitView(row: row, fraction: f)
                s.onResize = { [weak self] in self?.host?.app?.saveWindowsSoon() }
                s.addSubview(make(a))
                s.addSubview(make(b))
                return s
            }
        }
        let root = make(zoomed.map { .leaf($0) } ?? tree)
        root.frame = container.bounds
        root.autoresizingMask = [.width, .height]
        container.addSubview(root)
        let live = Set(tree.leaves)
        views = views.filter { live.contains($0.key) }
        if let f = focused, !live.contains(f) { focused = tree.leaves.first }
        refreshStrips()
        focus(focused)
    }

    /// Each pane's frame on screen, for drop zones.
    func paneFrames() -> [(id: Int, frame: NSRect)] {
        views.compactMap { id, v in
            guard let w = v.window else { return nil }
            return (id, w.convertToScreen(v.convert(v.bounds, to: nil)))
        }
    }

    var signature: String { syncFractions(); return tree.encoded }
    /// The layout with panes numbered in reading order, as it is saved.
    var readingOrder: String { syncFractions(); let l = tree.leaves; return tree.mapped { l.firstIndex(of: $0) ?? -1 }.encoded }
}

/// One split. Holds the fraction it was built with until it has a size to apply it to.
final class GridSplitView: NSSplitView, NSSplitViewDelegate {
    var pending: Double?
    var onResize: (() -> Void)?
    init(row: Bool, fraction: Double) {
        pending = fraction
        super.init(frame: .zero)
        isVertical = row
        dividerStyle = .thin
        delegate = self
    }
    required init?(coder: NSCoder) { fatalError("not used") }

    var length: CGFloat { isVertical ? bounds.width : bounds.height }
    var currentFraction: Double? {
        guard pending == nil, subviews.count == 2, length > dividerThickness else { return pending }
        let first = isVertical ? subviews[0].frame.width : subviews[0].frame.height
        return Double(first / (length - dividerThickness))
    }

    override func layout() {
        super.layout()
        if let f = pending, subviews.count == 2, length > dividerThickness {
            pending = nil
            setPosition((length - dividerThickness) * CGFloat(f), ofDividerAt: 0)
        }
    }
    func splitView(_ s: NSSplitView, constrainMinCoordinate p: CGFloat, ofSubviewAt i: Int) -> CGFloat { 160 }
    func splitView(_ s: NSSplitView, constrainMaxCoordinate p: CGFloat, ofSubviewAt i: Int) -> CGFloat { length - 160 }
    func splitViewDidResizeSubviews(_ n: Notification) {
        if pending == nil, NSEvent.pressedMouseButtons & 1 != 0 { onResize?() }
    }
}

/// A pane: its strip of tabs and the page it shows under it.
final class PaneView: NSView {
    weak var grid: PaneGrid?
    let id: Int
    let header = PaneHeader()
    private let focusLine = NSView()
    private var items: [PaneTabItem] = []
    private let newButton = NSButton()
    private let zoomButton = NSButton()
    private let tabButton = NSButton()
    private let closeButton = NSButton()
    static let headerHeight: CGFloat = 26
    static let buttonsWidth: CGFloat = 104
    /// The pane's name: the page it shows.
    var title: String { controller?.pageTitle ?? "" }
    var isFocused = false { didSet { if isFocused != oldValue { paintHeader() } } }

    init(grid: PaneGrid, id: Int) {
        self.grid = grid
        self.id = id
        super.init(frame: .zero)
        header.wantsLayer = true
        focusLine.wantsLayer = true
        for (b, sym, tip, act) in [(newButton, "plus", "New tab in this pane (⌘T)", #selector(newTabHere)),
                                   (zoomButton, "arrow.up.left.and.arrow.down.right", "Zoom this pane (⌘⇧↩)", #selector(toggleZoom)),
                                   (tabButton, "rectangle.on.rectangle", "Move to a window tab", #selector(toTab)),
                                   (closeButton, "xmark", "Close pane", #selector(closePane))] {
            b.isBordered = false
            b.image = NSImage(systemSymbolName: sym, accessibilityDescription: tip)
            b.imagePosition = .imageOnly
            b.contentTintColor = .secondaryLabelColor
            b.toolTip = tip
            b.target = self
            b.action = act
            header.addSubview(b)
        }
        header.addSubview(focusLine)
        addSubview(header)
        header.tabAt = { [weak self] x in self?.items.firstIndex { $0.frame.minX <= x && x < $0.frame.maxX } }
        header.onClick = { [weak self] i in self?.selectTab(i) }
        header.onDrag = { [weak self] p, i in self?.dragged(to: p, tab: i) }
        header.onDrop = { [weak self] p, i in self?.dropped(at: p, tab: i) }
        paintHeader()
    }
    required init?(coder: NSCoder) { fatalError("not used") }

    private func paintHeader() {
        header.layer?.backgroundColor = NSColor.black.withAlphaComponent(isFocused ? 0.38 : 0.28).cgColor
        focusLine.layer?.backgroundColor = (isFocused ? NSColor.controlAccentColor : NSColor.clear).cgColor
    }

    /// Only the shown tab's page is in the pane; the others are out of the
    /// window, so WebKit hides them.
    func attach(_ wv: WKWebView) {
        for v in subviews where v is WKWebView && v !== wv { v.removeFromSuperview() }
        if wv.superview !== self { wv.removeFromSuperview(); addSubview(wv) }
        needsLayout = true
    }

    var tabs: [DashboardWindowController] { grid?.tabs(id) ?? [] }

    func reloadStrip() {
        let t = tabs
        while items.count > t.count { items.removeLast().removeFromSuperview() }
        while items.count < t.count {
            let it = PaneTabItem()
            it.onClose = { [weak self, weak it] in if let s = self, let it = it, let i = s.items.firstIndex(where: { $0 === it }) { s.closeTab(i) } }
            it.onRename = { [weak self, weak it] in
                if let s = self, let it = it, let i = s.items.firstIndex(where: { $0 === it }), i < s.tabs.count { s.grid?.host?.app?.promptRename(s.tabs[i]) }
            }
            items.append(it)
            header.addSubview(it)
        }
        for (i, c) in t.enumerated() { items[i].set(title: c.pageTitle, active: c === controller, closable: true) }
        zoomButton.contentTintColor = grid?.zoomed == id ? .controlAccentColor : .secondaryLabelColor
        needsLayout = true
    }

    override func layout() {
        super.layout()
        let h = PaneView.headerHeight
        header.frame = NSRect(x: 0, y: bounds.height - h, width: bounds.width, height: h)
        focusLine.frame = NSRect(x: 0, y: 0, width: bounds.width, height: 2)
        var x = bounds.width - 26
        for b in [closeButton, tabButton, zoomButton, newButton] { b.frame = NSRect(x: x, y: 3, width: 22, height: 20); x -= 26 }
        let room = max(0, bounds.width - PaneView.buttonsWidth - 6)
        let w = items.isEmpty ? 0 : min(200, floor(room / CGFloat(items.count)))
        for (i, it) in items.enumerated() { it.frame = NSRect(x: 4 + CGFloat(i) * w, y: 2, width: max(0, w - 2), height: h - 4) }
        for v in subviews where v is WKWebView { v.frame = NSRect(x: 0, y: 0, width: bounds.width, height: max(0, bounds.height - h)) }
    }

    var controller: DashboardWindowController? { grid?.members[id] }
    func tab(_ i: Int?) -> DashboardWindowController? {
        guard let i = i else { return controller }
        let t = tabs
        return i >= 0 && i < t.count ? t[i] : nil
    }

    func selectTab(_ i: Int?) { if let c = tab(i), let g = grid { g.select(c, in: id) } }
    func closeTab(_ i: Int?) { if let c = tab(i) { c.app?.undockPane(c, to: .close) } }
    @objc func newTabHere() { if let g = grid, let app = g.host?.app { app.newTab(inPane: id, of: g) } }
    @objc func toggleZoom() { if let g = grid, let app = g.host?.app { app.zoomPane(id, of: g) } }
    @objc func toTab() { if let c = controller { c.app?.undockPane(c, to: .tab) } }
    /// The pane and every tab in it.
    @objc func closePane() { for c in tabs.reversed() { c.app?.undockPane(c, to: .close) } }

    /// A drop that would put `c` back where it is: its own pane's middle, or
    /// any edge of its own pane when it is that pane's only tab.
    func isNoop(_ t: PaneDropTarget, _ c: DashboardWindowController) -> Bool {
        t.host === grid?.host && t.paneId == id && (t.edge == .center || tabs.count == 1)
    }

    func dragged(to p: NSPoint, tab i: Int?) {
        guard let c = tab(i), let app = c.app else { return }
        if let t = app.paneDropTarget(at: p, moving: c), !isNoop(t, c) {
            app.snapPreview.showFront(t.preview)
        } else {
            app.snapPreview.hide()
        }
    }

    func dropped(at p: NSPoint, tab i: Int?) {
        guard let c = tab(i), let app = c.app, let hostWindow = grid?.host?.window else { return }
        app.snapPreview.hide()
        if let t = app.paneDropTarget(at: p, moving: c) {
            if isNoop(t, c) { return }
            app.undockPane(c, to: .move(t))
            return
        }
        // Out of every window, or onto this window's tab bar: a tab again.
        let content = hostWindow.convertToScreen(hostWindow.contentLayoutRect)
        if !hostWindow.frame.contains(p) || p.y > content.maxY { app.undockPane(c, to: .tab) }
    }
    // The pane's header drag as a test drives it (amux://test/pane-drag): its shown tab.
    func dragged(to p: NSPoint) { dragged(to: p, tab: nil) }
    func dropped(at p: NSPoint) { dropped(at: p, tab: nil) }
}

/// One tab in a pane's strip: its page's name and a close button.
final class PaneTabItem: NSView {
    private let label = NSTextField(labelWithString: "")
    private let close = NSButton()
    var onClose: (() -> Void)?
    var onRename: (() -> Void)?
    private var active = false

    // Right-click a strip tab to name it, as a window tab can be.
    override func menu(for event: NSEvent) -> NSMenu? {
        guard onRename != nil else { return nil }
        let m = NSMenu()
        let r = NSMenuItem(title: "Rename Tab…", action: #selector(renameTapped), keyEquivalent: "")
        r.target = self
        m.addItem(r)
        return m
    }
    @objc private func renameTapped() { onRename?() }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = 5
        label.font = .systemFont(ofSize: 11, weight: .medium)
        label.lineBreakMode = .byTruncatingTail
        close.isBordered = false
        close.image = NSImage(systemSymbolName: "xmark", accessibilityDescription: "Close tab")
        close.imagePosition = .imageOnly
        close.contentTintColor = .tertiaryLabelColor
        close.toolTip = "Close tab"
        close.target = self
        close.action = #selector(closeTapped)
        addSubview(label)
        addSubview(close)
    }
    required init?(coder: NSCoder) { fatalError("not used") }

    func set(title: String, active: Bool, closable: Bool) {
        label.stringValue = title
        toolTip = title
        self.active = active
        label.textColor = active ? .labelColor : .secondaryLabelColor
        layer?.backgroundColor = active ? NSColor.white.withAlphaComponent(0.14).cgColor : NSColor.clear.cgColor
        close.isHidden = !closable
        needsLayout = true
    }
    override func layout() {
        super.layout()
        close.frame = NSRect(x: bounds.width - 18, y: (bounds.height - 14) / 2, width: 14, height: 14)
        label.frame = NSRect(x: 8, y: (bounds.height - 15) / 2, width: max(0, bounds.width - 30), height: 15)
    }
    @objc func closeTapped() { onClose?() }
}

/// The strip's press: a click picks a tab, a drag moves it (the shown tab when
/// the press is not on one) until the button comes up.
final class PaneHeader: NSView {
    var tabAt: ((CGFloat) -> Int?)?
    var onClick: ((Int?) -> Void)?
    var onDrag: ((NSPoint, Int?) -> Void)?
    var onDrop: ((NSPoint, Int?) -> Void)?
    override func mouseDown(with e: NSEvent) {
        guard let w = window else { return }
        let start = NSEvent.mouseLocation
        let i = tabAt?(convert(e.locationInWindow, from: nil).x)
        var dragging = false
        while let ev = w.nextEvent(matching: [.leftMouseDragged, .leftMouseUp]) {
            let p = NSEvent.mouseLocation
            if ev.type == .leftMouseUp { break }
            if !dragging, hypot(p.x - start.x, p.y - start.y) > 6 { dragging = true; NSCursor.closedHand.push() }
            if dragging { onDrag?(p, i) }
        }
        if dragging { NSCursor.pop(); onDrop?(NSEvent.mouseLocation, i) } else if i != nil { onClick?(i) }
    }
    override func resetCursorRects() { addCursorRect(bounds, cursor: .openHand) }
}

struct SavedTab: Codable {
    var url: String
    var title: String?
    var name: String?   // the owner's name for the tab, when given
}

struct SavedPane: Codable {
    var url: String
    var title: String?
    var name: String?
    /// The pane's other tabs, after this one, and which tab it shows
    /// (0 is this one). Absent in a file saved before panes had tabs.
    var tabs: [SavedTab]?
    var active: Int?
}

struct SavedGrid: Codable {
    var tree: String          // PaneTree.encoded; 0 is the window's own page
    var panes: [SavedPane]    // 1, 2, ... in reading order
    /// The other tabs of the window's own page's pane, and which it shows.
    var hostTabs: [SavedTab]?
    var hostActive: Int?
    var zoomed: Int?          // in the saved numbering
    var name: String?         // the whole tab's name (windowName)
}

struct PaneDropTarget {
    let host: DashboardWindowController
    let paneId: Int?          // nil: the window shows one page, no grid yet
    let edge: PaneEdge
    let name: String
    let preview: NSRect
}

enum PaneDestination {
    case tab, close
    case move(PaneDropTarget)
}

extension DashboardWindowController {
    var isGridHost: Bool { grid != nil }
    /// Where sheets (alerts, file pickers) attach: a pane's own window is hidden.
    var sheetWindow: NSWindow { paneHost?.window ?? window! }
    /// The grid this page is in, as a pane or as its host.
    var inGrid: PaneGrid? { (paneHost ?? self).grid }
}

extension AppDelegate {
    /// The amux window at `p`, front-most, skipping `excluding` (a tab being
    /// dragged, which follows the pointer) and the drop preview. nil when another
    /// app's window is on top there. Read from the window server's front-to-back
    /// list, skipping:
    /// - every layer above amux's own windows: the system draws overlays there
    ///   that cover the whole screen. The Dock keeps a full-screen window at
    ///   layer 20, the thumbnail AppKit draws under the pointer during a tab
    ///   drag sits at the dragging layer (Ethan's recording, 2026-10-24 07:39),
    ///   and a locked display puts a "Display Shield" on top (2026-10-23). Each
    ///   answered for every point. Another app's ordinary window over amux
    ///   still blocks;
    /// - this app's own windows that are not dashboard windows (drag images,
    ///   panels).
    /// What blocked the last miss is kept in `paneBlocker` for the drag's log line.
    func amuxWindow(at p: NSPoint, excluding: [NSWindow] = []) -> DashboardWindowController? {
        guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]],
              let top = NSScreen.screens.first?.frame.maxY else { return nil }
        let q = CGPoint(x: p.x, y: top - p.y)   // the window server's y runs down from the main display's top
        let skip = Set(excluding.map { $0.windowNumber } + [snapPreview.panel.windowNumber])
        let ours = controllers.compactMap { $0.window?.isVisible == true ? $0.window?.level.rawValue : nil }.max() ?? 0
        let me = Int(getpid())
        for w in list {
            guard let n = w[kCGWindowNumber as String] as? Int, !skip.contains(n),
                  (w[kCGWindowLayer as String] as? Int ?? 0) <= ours,
                  let b = w[kCGWindowBounds as String] as? NSDictionary, let r = CGRect(dictionaryRepresentation: b), r.contains(q) else { continue }
            if (w[kCGWindowAlpha as String] as? Double ?? 1) == 0 { continue }
            if let c = controllers.first(where: { $0.window?.windowNumber == n && $0.window?.isVisible == true }) { return c }
            if (w[kCGWindowOwnerPID as String] as? Int) == me { continue }
            paneBlocker = "\(w[kCGWindowOwnerName as String] as? String ?? "?") layer=\(w[kCGWindowLayer as String] as? Int ?? 0) \(Int(r.width))x\(Int(r.height))"
            return nil
        }
        paneBlocker = "no window"
        return nil
    }

    /// Where a tab (`moving`, when it is already a pane's tab) dropped at `p`
    /// would go: the pane under it in an amux window's content, and which half,
    /// or its middle to join its tabs. A window that already has `maxPanes`
    /// only takes joins. nil over a tab bar or away from every amux window.
    func paneDropTarget(at p: NSPoint, excluding: [NSWindow] = [], moving: DashboardWindowController? = nil) -> PaneDropTarget? {
        guard snapEnabled, !NSEvent.modifierFlags.contains(.option),
              let host = amuxWindow(at: p, excluding: excluding), let w = host.window, !w.styleMask.contains(.fullScreen) else { return nil }
        let content = w.convertToScreen(w.contentLayoutRect)
        guard content.contains(p) else { return nil }
        var paneId: Int? = nil
        var rect = content
        var full = false
        if let g = host.grid {
            // A split adds a pane unless the tab moving is its pane's only tab.
            let samePane = moving.flatMap { g.id(of: $0) }
            let freesOne = samePane.map { g.tabs($0).count == 1 } ?? false
            full = g.tree.leaves.count >= maxPanes && !freesOne
            guard let hit = g.paneFrames().first(where: { $0.frame.contains(p) }) else { return nil }
            paneId = hit.id
            rect = hit.frame
        }
        guard rect.width > 0, rect.height > 0 else { return nil }
        let d = paneDropEdge(u: (p.x - rect.minX) / rect.width, v: (p.y - rect.minY) / rect.height, width: rect.width, height: rect.height, full: full)
        return PaneDropTarget(host: host, paneId: paneId, edge: d.edge, name: d.name, preview: paneDropRect(rect, edge: d.edge).integral)
    }

    /// `c` becomes a pane of `host`'s window, beside `paneId` on `edge`, or
    /// (`.center`) a tab of that pane.
    @discardableResult
    func dockPane(_ c: DashboardWindowController, into host: DashboardWindowController, at paneId: Int?, edge: PaneEdge, zone: String = "") -> Bool {
        guard c !== host, c.grid == nil, let hw = host.window, let cw = c.window else {
            dlog("verdict=pane_dock_refused reason=\(c === host ? "same_window" : c.grid != nil ? "dragged_tab_has_grid" : "no_window")")
            return false
        }
        if host.grid == nil {
            let g = PaneGrid(host: host)
            host.grid = g
            g.container.frame = hw.contentView?.bounds ?? .zero
            hw.contentView = g.container
        }
        guard let g = host.grid else { return false }
        let target = paneId.flatMap { g.stacks[$0] != nil ? $0 : nil } ?? g.focused ?? g.tree.leaves.first ?? 0
        var edge = edge
        if edge != .center && g.tree.leaves.count >= maxPanes {
            dlog("verdict=pane_split_joined reason=grid_full panes=\(g.tree.leaves.count)")
            edge = .center
        }
        // Out of its tab group and off screen; its page moves into the grid.
        // Tab groups change here by the app, not a drag: the tear watcher must not read it as one.
        lastTabPlaced = Date()
        if (cw.tabbedWindows?.count ?? 1) > 1 { cw.tabGroup?.removeWindow(cw) }
        cw.orderOut(nil)
        cw.contentView = NSView()
        c.paneHost = host
        g.syncFractions()
        let into: Int
        if edge == .center {
            into = target
            g.stacks[into, default: []].append(c)
        } else {
            into = g.nextId
            g.nextId += 1
            g.stacks[into] = [c]
            g.tree = g.tree.inserting(into, at: target, edge: edge)
        }
        g.members[into] = c
        g.focused = into
        g.rebuild()
        host.updateTitle()
        hw.tabGroup?.selectedWindow = hw
        hw.makeKeyAndOrderFront(nil)
        hw.makeFirstResponder(c.webView)
        if edge == .center {
            dlog("verdict=tab_joined_pane panes=\(g.tree.leaves.count) pane_tabs=\(g.tabs(into).count) zone=\(zone.isEmpty ? "center" : zone) grid=\(g.tree.encoded)")
        } else {
            dlog("verdict=tab_docked_as_pane panes=\(g.tree.leaves.count) edge=\(edge.rawValue) zone=\(zone.isEmpty ? edge.rawValue : zone) grid=\(g.tree.encoded)")
        }
        saveWindowsSoon()
        return true
    }

    /// Takes `c` out of its pane: back to a tab of that window, closed, or into
    /// another pane. A pane left with no tabs closes and its neighbour takes the
    /// space; a window left with one page is a plain window again.
    func undockPane(_ c: DashboardWindowController, to dest: PaneDestination) {
        guard let g = c.paneHost?.grid ?? c.grid, let cid = g.id(of: c) else { return }
        // A move's target, by its page: removing this tab can renumber the grid
        // or collapse it to a plain window. A target pane whose shown tab is `c`
        // is named by another of its tabs.
        var targetPage: DashboardWindowController? = nil
        if case .move(let t) = dest {
            if let pid = t.paneId, let tg = t.host.grid {
                targetPage = tg.members[pid] !== c ? tg.members[pid] : tg.tabs(pid).first { $0 !== c }
            } else {
                targetPage = t.host
            }
        }
        // The host's own page: another page takes over the window first, so the
        // window, its place and its tab stay.
        if g.host === c {
            guard let heir = g.all.first(where: { $0 !== c }) else { return }
            rehost(g, to: heir)
        }
        guard let host = g.host, let hw = host.window else { return }
        lastTabPlaced = Date()
        g.syncFractions()
        var left = g.tabs(cid)
        let at = left.firstIndex { $0 === c } ?? 0
        left.removeAll { $0 === c }
        if left.isEmpty {
            g.stacks[cid] = nil
            g.members[cid] = nil
            g.views[cid]?.removeFromSuperview()
            g.tree = g.tree.removing(cid) ?? .leaf(g.stacks.keys.first ?? 0)
        } else {
            g.stacks[cid] = left
            if g.members[cid] === c { g.members[cid] = left[min(at, left.count - 1)] }
        }
        c.paneHost = nil
        c.webView.removeFromSuperview()
        c.window?.contentView = c.webView
        if g.tree.leaves.count <= 1 && g.all.count <= 1 {
            host.grid = nil
            g.container.removeFromSuperview()
            host.webView.removeFromSuperview()
            hw.contentView = host.webView
        } else {
            g.rebuild()
        }
        host.updateTitle()
        let panesLeft = host.grid?.tree.leaves.count ?? 1
        switch dest {
        case .tab:
            if let cw = c.window, cw !== hw, !(hw.tabbedWindows?.contains(cw) ?? false) { hw.addTabbedWindow(cw, ordered: .above) }
            c.window?.makeKeyAndOrderFront(nil)
            dlog("verdict=pane_to_tab panes_left=\(panesLeft)")
        case .close:
            c.window?.close()
            dlog("verdict=pane_closed panes_left=\(panesLeft)")
        case .move(let t):
            let page = targetPage ?? t.host
            let into = page.paneHost ?? page
            if !dockPane(c, into: into, at: into.grid?.id(of: page), edge: t.edge, zone: t.name) {
                if let cw = c.window, !(hw.tabbedWindows?.contains(cw) ?? false) { hw.addTabbedWindow(cw, ordered: .above) }
                c.window?.makeKeyAndOrderFront(nil)
            }
        }
        saveWindowsSoon()
    }

    /// `heir` (a pane) takes over the grid's window: its hidden window takes the
    /// host window's place and tab, and the grid moves into it. The old host
    /// stays a pane until the caller removes it.
    func rehost(_ g: PaneGrid, to heir: DashboardWindowController) {
        guard let old = g.host, let ow = old.window, let nw = heir.window else { return }
        nw.setFrame(ow.frame, display: false)
        nw.contentView = g.container
        ow.contentView = NSView()
        ow.addTabbedWindow(nw, ordered: .above)
        ow.tabGroup?.selectedWindow = nw
        nw.makeKeyAndOrderFront(nil)
        heir.paneHost = nil
        heir.grid = g
        old.grid = nil
        old.paneHost = heir
        g.host = heir
        for m in g.all where m !== heir { m.paneHost = heir }
        heir.home = old.home
        dlog("verdict=pane_grid_rehosted panes=\(g.tree.leaves.count)")
    }

    /// A grid's window closed: its panes close with it.
    func gridHostClosed(_ c: DashboardWindowController) {
        guard let g = c.grid else { return }
        c.grid = nil
        for m in g.all where m !== c {
            m.paneHost = nil
            m.window?.close()
        }
        dlog("verdict=pane_grid_closed panes=\(g.tree.leaves.count) tabs=\(g.all.count)")
    }

    // ── Pane actions (Window > Panes, the strip's buttons) ──

    /// The grid of the window in front, when it has one. `actionGrid` names it
    /// for one call from the test hook, whose window is never key.
    var keyGrid: PaneGrid? { actionGrid ?? keyController?.grid }

    /// The Split button: the page `c` shows, a second time, in a new pane to
    /// the right of `c`'s pane, as if a tab of it were dropped on that edge.
    /// A full grid takes it as a tab of the pane instead (dockPane).
    func splitPane(_ c: DashboardWindowController, url: URL?) {
        let host = c.paneHost ?? c
        let paneId = host.grid?.id(of: c)
        let n = DashboardWindowController(app: self, frame: host.window?.frame, configuration: nil)
        controllers.append(n)
        if let u = url { n.load(u) } else { n.showConnectPage() }
        let ok = dockPane(n, into: host, at: paneId, edge: .right, zone: "split_button")
        dlog("verdict=\(ok ? "split_button_pane" : "split_button_refused") panes=\(host.grid?.tree.leaves.count ?? 1) view=\(url?.fragment ?? "")")
        if !ok { n.window?.makeKeyAndOrderFront(nil) }
    }

    /// A new tab in pane `id`, on the worker list like any new tab.
    func newTab(inPane id: Int, of g: PaneGrid) {
        guard let host = g.host else { return }
        let n = DashboardWindowController(app: self, frame: host.window?.frame, configuration: nil)
        controllers.append(n)
        // On the server of the tab showing in that pane, like any new tab.
        let from = g.members[id]?.serverOrigin ?? host.serverOrigin
        if let u = url(withHash: "#view=sessions", on: from) { n.load(u) } else { n.showConnectPage() }
        dockPane(n, into: host, at: id, edge: .center, zone: "new_tab")
    }

    /// Pane `id` alone over the whole view, or the layout back.
    func zoomPane(_ id: Int, of g: PaneGrid) {
        g.syncFractions()
        g.zoomed = g.zoomed == id ? nil : id
        g.rebuild()
        if let c = g.members[id] { g.host?.window?.makeFirstResponder(c.webView) }
        dlog("verdict=pane_zoom zoomed=\(g.zoomed == nil ? 0 : 1) panes=\(g.tree.leaves.count)")
        saveWindowsSoon()
    }

    /// Focus moves to the pane beside the focused one, toward `edge`.
    func focusPane(toward edge: PaneEdge, of g: PaneGrid) {
        let frames = g.paneFrames()
        guard let from = frames.first(where: { $0.id == g.focused })?.frame ?? frames.first?.frame else { return }
        let c = NSPoint(x: from.midX, y: from.midY)
        let ahead = frames.filter { f in
            switch edge {
            case .left: return f.frame.maxX <= from.minX + 1
            case .right: return f.frame.minX >= from.maxX - 1
            case .top: return f.frame.minY >= from.maxY - 1
            case .bottom: return f.frame.maxY <= from.minY + 1
            case .center: return false
            }
        }
        guard let next = ahead.min(by: { hypot($0.frame.midX - c.x, $0.frame.midY - c.y) < hypot($1.frame.midX - c.x, $1.frame.midY - c.y) }),
              let m = g.members[next.id] else { return }
        g.select(m, in: next.id)
    }

    /// The page `c` shown wherever it is: its pane's tab, its view in front.
    func reveal(_ c: DashboardWindowController) {
        guard let g = c.inGrid, let id = g.id(of: c) else { return }
        if g.zoomed != nil && g.zoomed != id { g.zoomed = nil; g.rebuild() }
        if g.members[id] !== c { g.select(c, in: id) } else { g.focus(id) }
    }

    @objc func paneNewTab(_ sender: Any?) {
        guard let g = keyGrid, let id = g.focused ?? g.tree.leaves.first else { return }
        newTab(inPane: id, of: g)
    }
    @objc func paneCloseTab(_ sender: Any?) {
        guard let g = keyGrid, let id = g.focused ?? g.tree.leaves.first, let c = g.members[id] else { return }
        undockPane(c, to: .close)
    }
    @objc func paneZoom(_ sender: Any?) {
        guard let g = keyGrid, let id = g.focused ?? g.tree.leaves.first else { return }
        zoomPane(id, of: g)
    }
    @objc func paneFocusLeft(_ sender: Any?) { if let g = keyGrid { focusPane(toward: .left, of: g) } }
    @objc func paneFocusRight(_ sender: Any?) { if let g = keyGrid { focusPane(toward: .right, of: g) } }
    @objc func paneFocusUp(_ sender: Any?) { if let g = keyGrid { focusPane(toward: .top, of: g) } }
    @objc func paneFocusDown(_ sender: Any?) { if let g = keyGrid { focusPane(toward: .bottom, of: g) } }
    /// ⌘1-9: that tab of the focused pane; ⌘9 is its last.
    @objc func paneSelectTab(_ sender: Any?) {
        guard let item = sender as? NSMenuItem, let g = keyGrid, let id = g.focused ?? g.tree.leaves.first else { return }
        let t = g.tabs(id)
        guard !t.isEmpty else { return }
        let n = item.tag == 9 ? t.count - 1 : item.tag - 1
        if n >= 0 && n < t.count { g.select(t[n], in: id) }
    }
    @objc func paneNextTab(_ sender: Any?) { stepPaneTab(1) }
    @objc func panePreviousTab(_ sender: Any?) { stepPaneTab(-1) }
    func stepPaneTab(_ d: Int) {
        guard let g = keyGrid, let id = g.focused ?? g.tree.leaves.first, let c = g.members[id] else { return }
        let t = g.tabs(id)
        guard t.count > 1, let i = t.firstIndex(where: { $0 === c }) else { return }
        g.select(t[(i + d + t.count) % t.count], in: id)
    }

    /// The window a tab tore off into, finished by a drop on a pane zone.
    func finishPaneDrop(_ drop: PaneDropTarget, at p: NSPoint, from group: [NSWindow], source: NSRect, pressed: NSWindow? = nil) {
        let alone = group.filter { w in controllers.contains { $0.window === w } && w.isVisible && (w.tabbedWindows?.count ?? 1) == 1 }
        // The window the tab became, when AppKit tore it off. Dropped on a
        // window's own page AppKit cancels the drag instead and the tab stays
        // in the bar (Ethan, 2026-10-24 08:07: zone=pane, then
        // tab_drag_not_torn_off, five times): then the pressed tab is the one.
        let tornOff = alone.first(where: { abs($0.frame.minX - source.minX) > 1 || abs($0.frame.maxY - source.maxY) > 1 })
        guard let tw = tornOff ?? pressed, let torn = controllers.first(where: { $0.window === tw }) else {
            dlog("verdict=tab_drag_not_torn_off zone=pane pressed=\(pressed == nil ? 0 : 1)")
            return
        }
        if tornOff == nil { dlog("verdict=tab_pane_drop_from_bar") }
        // Dropped on the window it came from: that window is now whichever of
        // its tabs is showing.
        var host = drop.host
        if host === torn {
            // Still in the bar (no tear): the tab beside it takes the window.
            let others = group.filter { w in w !== tw && controllers.contains { $0.window === w } }
            guard let w = others.first(where: { $0.isVisible && ($0.tabGroup?.selectedWindow === $0 || $0.tabbedWindows == nil) })
                    ?? (tornOff == nil ? others.first : nil),
                  let h = controllers.first(where: { $0.window === w }) else { dlog("verdict=pane_dock_refused reason=no_host"); return }
            host = h
        }
        let again = paneDropTarget(at: p, excluding: [tw])
        let t = again?.host === host ? again! : drop
        dockPane(torn, into: host, at: t.host === host ? t.paneId : nil, edge: t.edge, zone: t.name)
    }

    /// Saved with the workspace: the grid's tree (the host's page is 0, the
    /// panes 1... in reading order) and each pane's tabs.
    func savedGrid(_ c: DashboardWindowController) -> SavedGrid? {
        guard let g = c.grid, g.host === c, let hostLeaf = g.id(of: c) else { return nil }
        g.syncFractions()
        let others = g.tree.leaves.filter { $0 != hostLeaf }
        var index: [Int: Int] = [hostLeaf: 0]
        for (i, id) in others.enumerated() { index[id] = i + 1 }
        func tab(_ m: DashboardWindowController) -> SavedTab? {
            guard let url = m.savedURL() else { return nil }
            let t = m.window?.title ?? ""
            return SavedTab(url: url, title: t.isEmpty || t == "amux" ? nil : t, name: m.customName)
        }
        var panes: [SavedPane] = []
        for id in others {
            let stack = g.tabs(id).compactMap(tab)
            guard stack.count == g.tabs(id).count, let first = stack.first else { return nil }
            let active = g.tabs(id).firstIndex { $0 === g.members[id] } ?? 0
            panes.append(SavedPane(url: first.url, title: first.title, name: first.name, tabs: stack.count > 1 ? Array(stack.dropFirst()) : nil, active: active == 0 ? nil : active))
        }
        // The host's pane: the host first, its other tabs after.
        let hostStack = [c] + g.tabs(hostLeaf).filter { $0 !== c }
        let hostTabs = hostStack.dropFirst().compactMap(tab)
        guard hostTabs.count == hostStack.count - 1 else { return nil }
        let hostActive = hostStack.firstIndex { $0 === g.members[hostLeaf] } ?? 0
        // A page twice in one window (Ethan's 2026-10-25 screenshot: one worker
        // in both panes) is saved as it is, but said, so the next one is traceable.
        let urls = [c.savedURL()].compactMap { $0 } + hostTabs.map { $0.url } + panes.flatMap { [$0.url] + ($0.tabs ?? []).map { $0.url } }
        let dups = Set(urls.filter { u in urls.filter { $0 == u }.count > 1 })
        if !dups.isEmpty, dups != lastDuplicateWarned {
            lastDuplicateWarned = dups
            dlog("verdict=pane_grid_duplicate_page pages=\(dups.sorted().joined(separator: ",")) grid=\(g.tree.encoded)")
        }
        return SavedGrid(tree: g.tree.mapped { index[$0] ?? 0 }.encoded, panes: panes,
                         hostTabs: hostTabs.isEmpty ? nil : hostTabs, hostActive: hostActive == 0 ? nil : hostActive,
                         zoomed: g.zoomed.flatMap { index[$0] }, name: c.windowName)
    }

    /// Rebuilds a saved grid in `host`'s window, reusing open controllers
    /// showing the same pages.
    func restoreGrid(_ s: SavedGrid, in host: DashboardWindowController, pool: inout [DashboardWindowController]) {
        guard let tree = PaneTree.decode(s.tree), let hw = host.window, tree.leaves.contains(0),
              Set(tree.leaves) == Set(0...s.panes.count) else {
            dlog("verdict=pane_grid_restore_skipped reason=bad_tree tree=\(s.tree)")
            return
        }
        func page(_ url: String, _ title: String?, _ name: String? = nil) -> DashboardWindowController {
            let c: DashboardWindowController
            if let j = pool.firstIndex(where: { $0.savedURL() == url }) {
                c = pool.remove(at: j)
            } else {
                c = openWindow(url: URL(string: url), frame: hw.frame, activate: false)
                if let t = title, !t.isEmpty { c.pageLabel = t; c.updateTitle() }
            }
            if let cw = c.window {
                if (cw.tabbedWindows?.count ?? 1) > 1 { cw.tabGroup?.removeWindow(cw) }
                cw.orderOut(nil)
                cw.contentView = NSView()
            }
            c.paneHost = host
            if let n = name, !n.isEmpty { c.customName = n }
            return c
        }
        let g = PaneGrid(host: host)
        let hostStack = [host] + (s.hostTabs ?? []).map { page($0.url, $0.title, $0.name) }
        g.stacks[0] = hostStack
        g.members[0] = hostStack[min(s.hostActive ?? 0, hostStack.count - 1)]
        for (i, p) in s.panes.enumerated() {
            let stack = [page(p.url, p.title, p.name)] + (p.tabs ?? []).map { page($0.url, $0.title, $0.name) }
            g.stacks[i + 1] = stack
            g.members[i + 1] = stack[min(p.active ?? 0, stack.count - 1)]
        }
        g.tree = tree
        g.nextId = s.panes.count + 1
        g.zoomed = s.zoomed
        host.grid = g
        g.container.frame = hw.contentView?.bounds ?? .zero
        hw.contentView = g.container
        g.rebuild()
        host.windowName = s.name
        host.updateTitle()
        dlog("verdict=pane_grid_restored panes=\(tree.leaves.count) tabs=\(g.all.count) grid=\(s.tree)")
    }

    /// Before a workspace switch: every grid back to plain hidden windows, so
    /// their pages can be matched by URL like any other window.
    func dissolveGrids(_ cs: [DashboardWindowController]) {
        for h in cs {
            guard let g = h.grid, g.host === h, let hw = h.window else { continue }
            for m in g.all where m !== h {
                m.webView.removeFromSuperview()
                m.window?.contentView = m.webView
                m.paneHost = nil
            }
            h.webView.removeFromSuperview()
            hw.contentView = h.webView
            h.grid = nil
            h.windowName = nil
        }
    }
}
