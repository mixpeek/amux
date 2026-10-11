import Cocoa
import WebKit

class DragBarView: NSView {
    override var mouseDownCanMoveWindow: Bool { false }

    override func draw(_ dirtyRect: NSRect) {
        let style = NSMutableParagraphStyle()
        style.alignment = .center
        let attrs: [NSAttributedString.Key: Any] = [
            .foregroundColor: NSColor.tertiaryLabelColor,
            .font: NSFont.systemFont(ofSize: 12),
            .paragraphStyle: style,
        ]
        let r = NSRect(x: 0, y: (bounds.height - 16) / 2, width: bounds.width, height: 16)
        "⠿".draw(in: r, withAttributes: attrs)
    }

    override func mouseDown(with event: NSEvent) {
        guard let target = window?.parent ?? window else { return }
        let startScreen = NSEvent.mouseLocation
        let startOrigin = target.frame.origin
        while true {
            guard let next = window?.nextEvent(matching: [.leftMouseDragged, .leftMouseUp]) else { break }
            if next.type == .leftMouseUp { break }
            let cur = NSEvent.mouseLocation
            target.setFrameOrigin(NSPoint(
                x: startOrigin.x + cur.x - startScreen.x,
                y: startOrigin.y + cur.y - startScreen.y
            ))
        }
    }
}

class ResizeGripView: NSView {
    override func draw(_ dirtyRect: NSRect) {
        NSColor.tertiaryLabelColor.setStroke()
        let path = NSBezierPath()
        for offset: CGFloat in [4, 8, 12] {
            path.move(to: NSPoint(x: bounds.maxX - offset, y: bounds.minY))
            path.line(to: NSPoint(x: bounds.maxX, y: bounds.minY + offset))
        }
        path.lineWidth = 1
        path.stroke()
    }

    override func mouseDown(with event: NSEvent) {
        guard let cw = window?.parent else { return }
        let startScreen = NSEvent.mouseLocation
        let startFrame = cw.frame
        let topY = startFrame.maxY
        let leftX = startFrame.origin.x

        while true {
            guard let next = window?.nextEvent(matching: [.leftMouseDragged, .leftMouseUp]) else { break }
            if next.type == .leftMouseUp { break }
            let cur = NSEvent.mouseLocation
            let newW = max(250, startFrame.width + (cur.x - startScreen.x))
            let newH = max(200, startFrame.height - (cur.y - startScreen.y))
            cw.setFrame(NSRect(x: leftX, y: topY - newH, width: newW, height: newH), display: true)
            if let ctrl = cw.delegate as? OverlayController {
                ctrl.repositionChildren()
            }
        }
    }
}

class OverlayWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

class OverlayController: NSObject, WKNavigationDelegate, NSWindowDelegate {
    let contentWindow: OverlayWindow
    let toolbarWindow: NSWindow
    let webView: WKWebView
    var files: [(path: String, content: String)]
    let resizeWindow: NSWindow
    var editMode = false
    var zoom = 100
    var zoomLabel: NSTextField!
    var editBtn: NSButton!
    var tabControl: NSSegmentedControl?
    var fileWatchers: [DispatchSourceFileSystemObject] = []

    init(filePaths: [String], x: Int, y: Int, w: Int, h: Int, opacity: Double) {
        self.files = filePaths.map { p in
            let c = (try? String(contentsOf: URL(fileURLWithPath: p), encoding: .utf8)) ?? ""
            return (path: p, content: c)
        }

        let rect = NSRect(x: x, y: y, width: w, height: h)
        contentWindow = OverlayWindow(
            contentRect: rect,
            styleMask: [.borderless],
            backing: .buffered, defer: false
        )

        let config = WKWebViewConfiguration()
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")
        webView = WKWebView(frame: .zero, configuration: config)
        webView.translatesAutoresizingMaskIntoConstraints = false

        let tbH: CGFloat = 28
        let tbRect = NSRect(
            x: CGFloat(x), y: CGFloat(y) + CGFloat(h) - tbH,
            width: CGFloat(w), height: tbH
        )
        toolbarWindow = NSWindow(
            contentRect: tbRect,
            styleMask: [.borderless],
            backing: .buffered, defer: false
        )

        let gripSize: CGFloat = 20
        let gripRect = NSRect(
            x: CGFloat(x) + CGFloat(w) - gripSize,
            y: CGFloat(y),
            width: gripSize, height: gripSize
        )
        resizeWindow = NSWindow(
            contentRect: gripRect,
            styleMask: [.borderless],
            backing: .buffered, defer: false
        )

        super.init()

        contentWindow.delegate = self
        contentWindow.level = .floating
        contentWindow.isOpaque = false
        contentWindow.backgroundColor = .clear
        contentWindow.alphaValue = CGFloat(opacity)
        contentWindow.hasShadow = true
        contentWindow.ignoresMouseEvents = true

        let visual = NSVisualEffectView()
        visual.material = .hudWindow
        visual.blendingMode = .behindWindow
        visual.state = .active
        visual.translatesAutoresizingMaskIntoConstraints = false
        visual.wantsLayer = true
        visual.layer?.cornerRadius = 10
        visual.layer?.masksToBounds = true

        contentWindow.contentView = visual
        visual.addSubview(webView)
        NSLayoutConstraint.activate([
            webView.topAnchor.constraint(equalTo: visual.topAnchor),
            webView.bottomAnchor.constraint(equalTo: visual.bottomAnchor),
            webView.leadingAnchor.constraint(equalTo: visual.leadingAnchor),
            webView.trailingAnchor.constraint(equalTo: visual.trailingAnchor),
        ])

        webView.navigationDelegate = self
        webView.setValue(false, forKey: "drawsBackground")

        let togHandler = ToggleHandler(controller: self)
        let editHandler = EditHandler(controller: self)
        let zoomHandler = ZoomHandler(controller: self)
        webView.configuration.userContentController.add(togHandler, name: "toggle")
        webView.configuration.userContentController.add(editHandler, name: "edit")
        webView.configuration.userContentController.add(zoomHandler, name: "zoomChanged")

        toolbarWindow.level = .floating
        toolbarWindow.isOpaque = false
        toolbarWindow.backgroundColor = .clear
        toolbarWindow.hasShadow = false
        toolbarWindow.alphaValue = CGFloat(opacity)

        let tbVisual = NSVisualEffectView()
        tbVisual.material = .titlebar
        tbVisual.blendingMode = .behindWindow
        tbVisual.state = .active
        tbVisual.translatesAutoresizingMaskIntoConstraints = false
        tbVisual.wantsLayer = true
        tbVisual.layer?.cornerRadius = 8
        tbVisual.layer?.masksToBounds = true

        toolbarWindow.contentView = tbVisual
        setupToolbar(in: tbVisual)

        resizeWindow.level = .floating
        resizeWindow.isOpaque = false
        resizeWindow.backgroundColor = .clear
        resizeWindow.hasShadow = false
        resizeWindow.alphaValue = CGFloat(opacity)

        let gripView = ResizeGripView()
        gripView.translatesAutoresizingMaskIntoConstraints = false
        let gripContainer = NSView()
        gripContainer.wantsLayer = true
        resizeWindow.contentView = gripContainer
        gripContainer.addSubview(gripView)
        gripView.frame = gripContainer.bounds
        gripView.autoresizingMask = [.width, .height]

        contentWindow.addChildWindow(toolbarWindow, ordered: .above)
        contentWindow.addChildWindow(resizeWindow, ordered: .above)

        loadContent()
        contentWindow.orderFront(nil)
        toolbarWindow.orderFront(nil)
        resizeWindow.orderFront(nil)

        setupFileWatchers()
    }

    func setupToolbar(in container: NSView) {
        let stack = NSStackView()
        stack.translatesAutoresizingMaskIntoConstraints = false
        stack.orientation = .horizontal
        stack.spacing = 3
        stack.alignment = .centerY
        stack.edgeInsets = NSEdgeInsets(top: 0, left: 6, bottom: 0, right: 4)

        let sf = NSFont.systemFont(ofSize: 11)

        let closeBtn = makeBtn("\u{2715}", font: sf)
        closeBtn.target = self
        closeBtn.action = #selector(closeOverlay)
        closeBtn.contentTintColor = .secondaryLabelColor

        let zoomOutBtn = makeBtn("\u{2212}", font: sf)
        zoomOutBtn.target = self
        zoomOutBtn.action = #selector(zoomOutAction)

        zoomLabel = NSTextField(labelWithString: "100%")
        zoomLabel.font = NSFont.monospacedDigitSystemFont(ofSize: 10, weight: .regular)
        zoomLabel.textColor = .secondaryLabelColor
        zoomLabel.alignment = .center
        zoomLabel.setContentHuggingPriority(.defaultHigh, for: .horizontal)

        let zoomInBtn = makeBtn("+", font: sf)
        zoomInBtn.target = self
        zoomInBtn.action = #selector(zoomInAction)

        editBtn = makeBtn("Edit", font: sf)
        editBtn.target = self
        editBtn.action = #selector(toggleEditAction)

        let moreBtn = makeBtn("\u{22EF}", font: NSFont.systemFont(ofSize: 13))
        moreBtn.target = self
        moreBtn.action = #selector(showMoreMenu(_:))

        let spacer = NSView()
        spacer.translatesAutoresizingMaskIntoConstraints = false
        spacer.setContentHuggingPriority(.defaultLow, for: .horizontal)
        spacer.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)

        let dragBar = DragBarView()
        dragBar.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            dragBar.widthAnchor.constraint(equalToConstant: 28),
            dragBar.heightAnchor.constraint(equalToConstant: 28),
        ])

        stack.addArrangedSubview(closeBtn)

        if files.count > 1 {
            let seg = NSSegmentedControl()
            seg.segmentCount = files.count
            seg.segmentStyle = .roundRect
            seg.controlSize = .small
            seg.font = NSFont.systemFont(ofSize: 10)
            for (i, f) in files.enumerated() {
                let name = URL(fileURLWithPath: f.path).lastPathComponent
                let label = name.count > 16 ? String(name.prefix(13)) + "..." : name
                seg.setLabel(label, forSegment: i)
                seg.setWidth(0, forSegment: i)
            }
            seg.selectedSegment = 0
            seg.target = self
            seg.action = #selector(tabChanged(_:))
            seg.setContentHuggingPriority(.defaultHigh, for: .horizontal)
            tabControl = seg
            stack.addArrangedSubview(seg)
        }

        for v in [zoomOutBtn, zoomLabel, zoomInBtn, editBtn, moreBtn, spacer, dragBar] as [NSView] {
            stack.addArrangedSubview(v)
        }

        container.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: container.topAnchor),
            stack.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: container.trailingAnchor),
        ])
    }

    func makeBtn(_ title: String, font: NSFont) -> NSButton {
        let btn = NSButton(title: title, target: nil, action: nil)
        btn.bezelStyle = .inline
        btn.font = font
        btn.isBordered = true
        btn.setContentHuggingPriority(.defaultHigh, for: .horizontal)
        return btn
    }

    @objc func closeOverlay() {
        NSApplication.shared.terminate(nil)
    }

    @objc func zoomOutAction() {
        zoom = max(60, zoom - 10)
        zoomLabel.stringValue = "\(zoom)%"
        webView.evaluateJavaScript("_zoom=\(zoom);applyZoom()")
    }

    @objc func zoomInAction() {
        zoom = min(200, zoom + 10)
        zoomLabel.stringValue = "\(zoom)%"
        webView.evaluateJavaScript("_zoom=\(zoom);applyZoom()")
    }

    @objc func toggleEditAction() {
        if !editMode {
            editMode = true
            contentWindow.ignoresMouseEvents = false
            contentWindow.makeKeyAndOrderFront(nil)
            editBtn.title = "Done"
            webView.evaluateJavaScript("enterEditMode()")
        } else {
            editMode = false
            contentWindow.ignoresMouseEvents = true
            editBtn.title = "Edit"
            webView.evaluateJavaScript("exitEditMode()")
        }
    }

    @objc func tabChanged(_ sender: NSSegmentedControl) {
        if editMode {
            editMode = false
            contentWindow.ignoresMouseEvents = true
            editBtn.title = "Edit"
        }
        webView.evaluateJavaScript("switchTab(\(sender.selectedSegment))")
    }

    func cancelEditMode() {
        editMode = false
        editBtn.title = "Edit"
        contentWindow.ignoresMouseEvents = true
    }

    @objc func showMoreMenu(_ sender: NSButton) {
        let menu = NSMenu()

        let themeLabel = NSMenuItem(title: "Theme", action: nil, keyEquivalent: "")
        themeLabel.isEnabled = false
        menu.addItem(themeLabel)

        for t in ["Dark", "Light", "Warm", "Ocean", "Rose", "Green"] {
            let item = NSMenuItem(title: t, action: #selector(setThemeAction(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = t.lowercased()
            menu.addItem(item)
        }

        menu.addItem(.separator())

        let scrollUp = NSMenuItem(title: "\u{25B2} Scroll Up", action: #selector(scrollUpAction), keyEquivalent: "")
        scrollUp.target = self
        menu.addItem(scrollUp)

        let scrollDown = NSMenuItem(title: "\u{25BC} Scroll Down", action: #selector(scrollDownAction), keyEquivalent: "")
        scrollDown.target = self
        menu.addItem(scrollDown)

        menu.addItem(.separator())

        let opLabel = NSMenuItem(title: "Opacity", action: nil, keyEquivalent: "")
        opLabel.isEnabled = false
        menu.addItem(opLabel)

        let opView = NSView(frame: NSRect(x: 0, y: 0, width: 160, height: 24))
        let slider = NSSlider(
            value: Double(contentWindow.alphaValue * 100),
            minValue: 20, maxValue: 100,
            target: self, action: #selector(opacityChanged(_:))
        )
        slider.frame = NSRect(x: 12, y: 0, width: 136, height: 24)
        opView.addSubview(slider)
        let sliderItem = NSMenuItem()
        sliderItem.view = opView
        menu.addItem(sliderItem)

        menu.popUp(positioning: nil, at: NSPoint(x: 0, y: sender.bounds.height), in: sender)
    }

    @objc func setThemeAction(_ sender: NSMenuItem) {
        guard let theme = sender.representedObject as? String else { return }
        webView.evaluateJavaScript("setTheme('\(theme)')")
    }

    @objc func scrollUpAction() {
        webView.evaluateJavaScript("window.scrollBy(0,-150)")
    }

    @objc func scrollDownAction() {
        webView.evaluateJavaScript("window.scrollBy(0,150)")
    }

    @objc func opacityChanged(_ sender: NSSlider) {
        let val = CGFloat(sender.doubleValue / 100.0)
        contentWindow.alphaValue = val
        toolbarWindow.alphaValue = val
    }

    func windowWillClose(_ notification: Notification) {
        NSApplication.shared.terminate(nil)
    }

    func windowDidResize(_ notification: Notification) {
        repositionChildren()
    }

    func repositionChildren() {
        let f = contentWindow.frame
        let tbH: CGFloat = 28
        toolbarWindow.setFrame(
            NSRect(x: f.origin.x, y: f.maxY - tbH, width: f.width, height: tbH),
            display: true
        )
        let gripSize: CGFloat = 20
        resizeWindow.setFrame(
            NSRect(x: f.maxX - gripSize, y: f.origin.y, width: gripSize, height: gripSize),
            display: true
        )
    }

    func setupFileWatchers() {
        for watcher in fileWatchers { watcher.cancel() }
        fileWatchers = []

        for f in files {
            let fd = open(f.path, O_EVTONLY)
            guard fd >= 0 else { continue }
            let source = DispatchSource.makeFileSystemObjectSource(
                fileDescriptor: fd,
                eventMask: [.write, .rename, .delete],
                queue: .main
            )
            source.setEventHandler { [weak self] in
                self?.loadContent()
            }
            source.setCancelHandler { close(fd) }
            source.resume()
            fileWatchers.append(source)
        }
    }

    func loadContent() {
        for i in 0..<files.count {
            files[i].content = (try? String(contentsOf: URL(fileURLWithPath: files[i].path), encoding: .utf8)) ?? ""
        }

        var fd = "["
        for (i, f) in files.enumerated() {
            let nm = escapeJS(URL(fileURLWithPath: f.path).lastPathComponent)
            let b = f.content.data(using: .utf8)?.base64EncodedString() ?? ""
            if i > 0 { fd += "," }
            fd += "{name:\"" + nm + "\",b64:\"" + b + "\"}"
        }
        fd += "]"

        let tb = ""

        var rc = ""
        for (i, f) in files.enumerated() {
            let body = renderMarkdown(f.content, tabIndex: i)
            let ac = i == 0 ? " active" : ""
            rc += "<div class=\"tab-content" + ac + "\" id=\"tc-" + String(i) + "\">" + body + "</div>"
        }

        let html = htmlTemplate()
            .replacingOccurrences(of: "/*FILEDATA*/[]", with: fd)
            .replacingOccurrences(of: "<!--TABBAR-->", with: tb)
            .replacingOccurrences(of: "<!--TABS-->", with: rc)

        webView.loadHTMLString(html, baseURL: URL(fileURLWithPath: files[0].path).deletingLastPathComponent())
    }

    func htmlTemplate() -> String {
        return """
        <!DOCTYPE html>
        <html>
        <head>
        <meta charset="utf-8">
        <style>
            :root {
                --fg: #e8e8e8; --fg-dim: rgba(255,255,255,0.45);
                --fg-muted: rgba(255,255,255,0.5); --fg-heading: rgba(255,255,255,0.9);
                --accent: #5ba3f5; --code-bg: rgba(0,0,0,0.25);
                --border: rgba(255,255,255,0.15); --table-header: rgba(255,255,255,0.05);
                --scrollbar: rgba(255,255,255,0.15);
            }
            [data-theme="light"] {
                --fg: #1a1a1a; --fg-dim: rgba(0,0,0,0.4);
                --fg-muted: rgba(0,0,0,0.45); --fg-heading: rgba(0,0,0,0.85);
                --accent: #2563eb; --code-bg: rgba(0,0,0,0.06);
                --border: rgba(0,0,0,0.12); --table-header: rgba(0,0,0,0.04);
                --scrollbar: rgba(0,0,0,0.12);
            }
            [data-theme="warm"] {
                --fg: #f5e6d3; --fg-dim: rgba(245,230,211,0.45);
                --fg-muted: rgba(245,230,211,0.5); --fg-heading: rgba(245,230,211,0.9);
                --accent: #e8a64c; --code-bg: rgba(0,0,0,0.2);
                --border: rgba(245,230,211,0.15); --table-header: rgba(245,230,211,0.05);
                --scrollbar: rgba(245,230,211,0.15);
            }
            [data-theme="ocean"] {
                --fg: #c8e6f5; --fg-dim: rgba(200,230,245,0.45);
                --fg-muted: rgba(200,230,245,0.5); --fg-heading: rgba(200,230,245,0.9);
                --accent: #38bdf8; --code-bg: rgba(0,20,40,0.3);
                --border: rgba(200,230,245,0.15); --table-header: rgba(200,230,245,0.05);
                --scrollbar: rgba(200,230,245,0.15);
            }
            [data-theme="rose"] {
                --fg: #f5d5e0; --fg-dim: rgba(245,213,224,0.45);
                --fg-muted: rgba(245,213,224,0.5); --fg-heading: rgba(245,213,224,0.9);
                --accent: #f472b6; --code-bg: rgba(30,0,15,0.25);
                --border: rgba(245,213,224,0.15); --table-header: rgba(245,213,224,0.05);
                --scrollbar: rgba(245,213,224,0.15);
            }
            [data-theme="green"] {
                --fg: #d4edda; --fg-dim: rgba(212,237,218,0.45);
                --fg-muted: rgba(212,237,218,0.5); --fg-heading: rgba(212,237,218,0.9);
                --accent: #34d399; --code-bg: rgba(0,20,10,0.25);
                --border: rgba(212,237,218,0.15); --table-header: rgba(212,237,218,0.05);
                --scrollbar: rgba(212,237,218,0.15);
            }

            * { margin: 0; padding: 0; box-sizing: border-box; }
            html, body { height: 100%; overflow-y: auto; }
            body {
                font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif;
                font-size: 14px; color: var(--fg);
                padding: 36px 16px 16px; line-height: 1.55;
            }

            .tab-bar {
                display: flex; gap: 2px; margin-bottom: 6px;
                overflow-x: auto; flex-shrink: 0;
            }
            .tab-bar button {
                font-size: 11px; padding: 3px 10px; border-radius: 4px 4px 0 0;
                border: 1px solid var(--border); border-bottom: none;
                background: rgba(255,255,255,0.05); color: var(--fg-muted);
                cursor: pointer; white-space: nowrap;
                max-width: 140px; overflow: hidden; text-overflow: ellipsis;
            }
            .tab-bar button.active { background: var(--code-bg); color: var(--fg); font-weight: 600; }
            .tab-content { display: none; }
            .tab-content.active { display: block; }

            h1 { font-size: 22px; font-weight: 700; margin-bottom: 2px; }
            h2 { font-size: 16px; font-weight: 700; margin: 18px 0 8px; color: var(--fg-heading); }
            h3 { font-size: 14px; font-weight: 700; margin: 14px 0 6px; color: var(--fg-heading); }
            h4 { font-size: 13px; font-weight: 600; margin: 12px 0 4px; }
            .date { font-size: 13px; color: var(--fg-dim); margin-bottom: 16px; }
            p { margin: 6px 0; }
            strong { font-weight: 700; }
            em { font-style: italic; }
            a { color: var(--accent); text-decoration: none; }
            a:hover { text-decoration: underline; }
            ul, ol { padding-left: 20px; margin: 4px 0; }
            ul { list-style: disc; }
            ol { list-style: decimal; }
            li { padding: 2px 0; }

            .task-item {
                display: flex; align-items: flex-start; gap: 8px;
                padding: 4px 0; list-style: none;
            }
            .task-item input[type=checkbox] {
                margin-top: 3px; accent-color: var(--accent); cursor: pointer;
                width: 16px; height: 16px;
            }
            .task-item.checked > span { text-decoration: line-through; opacity: 0.5; }
            blockquote {
                border-left: 3px solid var(--border);
                padding: 4px 0 4px 14px; margin: 8px 0;
                color: var(--fg-muted); font-style: italic;
            }
            code {
                background: var(--code-bg); padding: 1px 5px;
                border-radius: 3px; font-size: 0.86em;
                font-family: "SF Mono", Menlo, monospace;
            }
            pre {
                background: var(--code-bg); padding: 10px 12px;
                border-radius: 6px; overflow-x: auto; margin: 8px 0;
                font-size: 0.86em; line-height: 1.4;
            }
            pre code { background: none; padding: 0; }
            hr { border: none; border-top: 1px solid var(--border); margin: 12px 0; }
            table { border-collapse: collapse; margin: 8px 0; width: 100%; font-size: 0.93em; }
            th, td { border: 1px solid var(--border); padding: 5px 8px; text-align: left; }
            th { background: var(--table-header); font-weight: 600; }
            img { max-width: 100%; border-radius: 4px; margin: 4px 0; }

            .edit-area {
                width: 100%; min-height: 200px; background: var(--code-bg);
                border: 1px solid var(--border); border-radius: 6px;
                color: var(--fg); font-family: "SF Mono", Menlo, monospace;
                font-size: 13px; padding: 10px; resize: vertical; line-height: 1.5;
                display: none;
            }
            .edit-area:focus { outline: 1px solid var(--accent); }
            .edit-bar { display: none; gap: 6px; margin-top: 6px; }
            .edit-bar button {
                font-size: 12px; padding: 4px 10px; border-radius: 4px;
                border: 1px solid var(--border); background: rgba(255,255,255,0.05);
                color: var(--fg); cursor: pointer;
            }
            .edit-bar button.save { background: var(--accent); border-color: var(--accent); color: #fff; }
            .edit-bar button:hover { opacity: 0.85; }

            ::-webkit-scrollbar { width: 6px; }
            ::-webkit-scrollbar-track { background: transparent; }
            ::-webkit-scrollbar-thumb { background: var(--scrollbar); border-radius: 3px; }
        </style>
        </head>
        <body>
            <!--TABBAR-->
            <!--TABS-->
            <textarea class="edit-area" id="editArea"></textarea>
            <div class="edit-bar" id="editBar">
                <button class="save" onclick="saveEdit()">Save</button>
                <button onclick="cancelEditFromWeb()">Cancel</button>
            </div>
            <script>
                var _files = /*FILEDATA*/[];
                var _currentTab = 0;
                var _editMode = false;
                var _zoom = 100;

                try { _zoom = parseInt(localStorage.getItem('pinned_zoom')) || 100; } catch(e) {}
                try { _currentTab = parseInt(localStorage.getItem('pinned_tab')) || 0; } catch(e) {}
                document.body.style.fontSize = (_zoom / 100 * 14) + 'px';

                function b64decode(b64) {
                    try {
                        var raw = atob(b64);
                        var arr = new Uint8Array(raw.length);
                        for (var i = 0; i < raw.length; i++) arr[i] = raw.charCodeAt(i);
                        return new TextDecoder().decode(arr);
                    } catch(e) { return ''; }
                }
                function b64encode(str) {
                    try {
                        var bytes = new TextEncoder().encode(str);
                        var bin = '';
                        bytes.forEach(function(b) { bin += String.fromCharCode(b); });
                        return btoa(bin);
                    } catch(e) { return ''; }
                }

                function switchTab(idx) {
                    if (_editMode) exitEditMode();
                    _currentTab = idx;
                    document.querySelectorAll('.tab-content').forEach(function(el, i) {
                        el.classList.toggle('active', i === idx);
                    });
                    document.querySelectorAll('.tab-bar button').forEach(function(el, i) {
                        el.classList.toggle('active', i === idx);
                    });
                    try { localStorage.setItem('pinned_tab', '' + idx); } catch(e) {}
                }
                if (_currentTab > 0 && _currentTab < _files.length) switchTab(_currentTab);

                function setTheme(name) {
                    if (name === 'dark') document.documentElement.removeAttribute('data-theme');
                    else document.documentElement.setAttribute('data-theme', name);
                    try { localStorage.setItem('pinned_theme', name); } catch(e) {}
                }
                (function() {
                    var saved = 'dark';
                    try { saved = localStorage.getItem('pinned_theme') || 'dark'; } catch(e) {}
                    setTheme(saved);
                })();

                function handleToggle(tab, lineIdx, checked) {
                    window.webkit.messageHandlers.toggle.postMessage(
                        JSON.stringify({ tab: tab, line: lineIdx, checked: checked })
                    );
                }

                function enterEditMode() {
                    _editMode = true;
                    var content = b64decode(_files[_currentTab].b64);
                    var tc = document.getElementById('tc-' + _currentTab);
                    if (tc) tc.style.display = 'none';
                    var area = document.getElementById('editArea');
                    area.style.display = '';
                    area.value = content;
                    area.style.height = Math.max(200, area.scrollHeight) + 'px';
                    document.getElementById('editBar').style.display = 'flex';
                    area.focus();
                }
                function exitEditMode() {
                    _editMode = false;
                    document.querySelectorAll('.tab-content').forEach(function(el, i) {
                        el.style.display = i === _currentTab ? '' : 'none';
                        el.classList.toggle('active', i === _currentTab);
                    });
                    document.getElementById('editArea').style.display = 'none';
                    document.getElementById('editBar').style.display = 'none';
                }
                function saveEdit() {
                    var content = document.getElementById('editArea').value;
                    var b64 = b64encode(content);
                    _files[_currentTab].b64 = b64;
                    window.webkit.messageHandlers.edit.postMessage(
                        JSON.stringify({ tab: _currentTab, b64: b64 })
                    );
                }
                function cancelEditFromWeb() {
                    exitEditMode();
                    window.webkit.messageHandlers.edit.postMessage(
                        JSON.stringify({ action: 'cancel' })
                    );
                }

                function applyZoom() {
                    document.body.style.fontSize = (_zoom / 100 * 14) + 'px';
                    try { localStorage.setItem('pinned_zoom', '' + _zoom); } catch(e) {}
                    try { window.webkit.messageHandlers.zoomChanged.postMessage('' + _zoom); } catch(e) {}
                }

                document.addEventListener('keydown', function(e) {
                    if (e.metaKey && (e.key === '=' || e.key === '+')) { e.preventDefault(); _zoom = Math.min(200, _zoom + 10); applyZoom(); }
                    if (e.metaKey && e.key === '-') { e.preventDefault(); _zoom = Math.max(60, _zoom - 10); applyZoom(); }
                    if (e.metaKey && e.key === '0') { e.preventDefault(); _zoom = 100; applyZoom(); }
                });
            </script>
        </body>
        </html>
        """
    }

    func toggleCheckbox(tab: Int, lineIndex: Int, checked: Bool) {
        guard tab >= 0 && tab < files.count else { return }
        var lines = files[tab].content.components(separatedBy: "\n")
        guard lineIndex >= 0 && lineIndex < lines.count else { return }

        let line = lines[lineIndex]
        if checked {
            lines[lineIndex] = line
                .replacingOccurrences(of: "- [ ] ", with: "- [x] ")
                .replacingOccurrences(of: "- [ ]", with: "- [x]")
        } else {
            lines[lineIndex] = line
                .replacingOccurrences(of: "- [x] ", with: "- [ ] ")
                .replacingOccurrences(of: "- [X] ", with: "- [ ] ")
                .replacingOccurrences(of: "- [x]", with: "- [ ]")
                .replacingOccurrences(of: "- [X]", with: "- [ ]")
        }

        files[tab].content = lines.joined(separator: "\n")
        try? files[tab].content.write(toFile: files[tab].path, atomically: true, encoding: .utf8)
        loadContent()
    }

    func saveContent(tab: Int, content: String) {
        guard tab >= 0 && tab < files.count else { return }
        files[tab].content = content
        try? content.write(toFile: files[tab].path, atomically: true, encoding: .utf8)
        editMode = false
        editBtn.title = "Edit"
        contentWindow.ignoresMouseEvents = true
        loadContent()
    }

    func renderMarkdown(_ text: String, tabIndex: Int) -> String {
        var html = ""
        let lines = text.components(separatedBy: "\n")
        var inCodeBlock = false
        var codeBlockContent = ""
        var inTable = false
        var tableRows: [[String]] = []
        var inList = false
        var listType = ""

        let dateFormatter = DateFormatter()
        dateFormatter.dateFormat = "EEE, MMM d, yyyy"
        let dateStr = dateFormatter.string(from: Date())
        var titleRendered = false

        for (idx, rawLine) in lines.enumerated() {
            let line = rawLine

            if line.hasPrefix("```") {
                if inCodeBlock {
                    html += "<pre><code>" + escapeHTML(codeBlockContent) + "</code></pre>"
                    codeBlockContent = ""
                    inCodeBlock = false
                } else {
                    if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                    if inTable { html += renderTable(tableRows); inTable = false; tableRows = [] }
                    inCodeBlock = true
                }
                continue
            }

            if inCodeBlock {
                if !codeBlockContent.isEmpty { codeBlockContent += "\n" }
                codeBlockContent += line
                continue
            }

            let trimmed = line.trimmingCharacters(in: .whitespaces)

            if trimmed.contains("|") && trimmed.hasPrefix("|") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                let cells = trimmed.split(separator: "|", omittingEmptySubsequences: false)
                    .map { $0.trimmingCharacters(in: .whitespaces) }
                    .filter { !$0.isEmpty }
                let isSeparator = cells.allSatisfy { $0.allSatisfy { $0 == "-" || $0 == ":" || $0 == " " } }
                if isSeparator { continue }
                if !inTable { inTable = true; tableRows = [] }
                tableRows.append(cells)
                continue
            } else if inTable {
                html += renderTable(tableRows)
                inTable = false
                tableRows = []
            }

            if trimmed.isEmpty {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                continue
            }

            if trimmed.hasPrefix("# ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                let heading = String(trimmed.dropFirst(2))
                html += "<h1>" + inlineMarkdown(heading) + "</h1>"
                if !titleRendered {
                    html += "<div class=\"date\">" + dateStr + "</div>"
                    titleRendered = true
                }
            } else if trimmed.hasPrefix("## ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<h2>" + inlineMarkdown(String(trimmed.dropFirst(3))) + "</h2>"
            } else if trimmed.hasPrefix("### ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<h3>" + inlineMarkdown(String(trimmed.dropFirst(4))) + "</h3>"
            } else if trimmed.hasPrefix("#### ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<h4>" + inlineMarkdown(String(trimmed.dropFirst(5))) + "</h4>"
            } else if trimmed == "---" || trimmed == "***" || trimmed == "___" {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<hr>"
            } else if trimmed.hasPrefix("- [ ] ") || trimmed.hasPrefix("- [x] ") || trimmed.hasPrefix("- [X] ") {
                if !inList || listType != "ul" {
                    if inList { html += listType == "ol" ? "</ol>" : "</ul>" }
                    html += "<ul style=\"list-style:none;padding-left:0;\">"; inList = true; listType = "ul"
                }
                let isChecked = trimmed.hasPrefix("- [x]") || trimmed.hasPrefix("- [X]")
                let label = String(trimmed.dropFirst(6))
                html += "<li class=\"task-item\(isChecked ? " checked" : "")\">"
                html += "<input type=\"checkbox\"\(isChecked ? " checked" : "") onchange=\"handleToggle(\(tabIndex), \(idx), this.checked)\">"
                html += "<span>" + inlineMarkdown(label) + "</span></li>"
            } else if trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") {
                if !inList || listType != "ul" {
                    if inList { html += listType == "ol" ? "</ol>" : "</ul>" }
                    html += "<ul>"; inList = true; listType = "ul"
                }
                html += "<li>" + inlineMarkdown(String(trimmed.dropFirst(2))) + "</li>"
            } else if let _ = trimmed.range(of: #"^\d+\.\s"#, options: .regularExpression) {
                if !inList || listType != "ol" {
                    if inList { html += listType == "ol" ? "</ol>" : "</ul>" }
                    html += "<ol>"; inList = true; listType = "ol"
                }
                let text = trimmed.replacingOccurrences(of: #"^\d+\.\s*"#, with: "", options: .regularExpression)
                html += "<li>" + inlineMarkdown(text) + "</li>"
            } else if trimmed.hasPrefix("> ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<blockquote>" + inlineMarkdown(String(trimmed.dropFirst(2))) + "</blockquote>"
            } else if trimmed.hasPrefix("![") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                if let altEnd = trimmed.range(of: "]("),
                   let urlEnd = trimmed.range(of: ")", range: altEnd.upperBound..<trimmed.endIndex) {
                    let alt = String(trimmed[trimmed.index(trimmed.startIndex, offsetBy: 2)..<altEnd.lowerBound])
                    let src = String(trimmed[altEnd.upperBound..<urlEnd.lowerBound])
                    html += "<img src=\"" + escapeHTML(src) + "\" alt=\"" + escapeHTML(alt) + "\">"
                }
            } else {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<p>" + inlineMarkdown(trimmed) + "</p>"
            }
        }

        if inList { html += listType == "ol" ? "</ol>" : "</ul>" }
        if inCodeBlock { html += "<pre><code>" + escapeHTML(codeBlockContent) + "</code></pre>" }
        if inTable { html += renderTable(tableRows) }

        return html
    }

    func inlineMarkdown(_ text: String) -> String {
        var s = escapeHTML(text)

        let patterns: [(String, String, String)] = [
            (#"\*\*\*(.+?)\*\*\*"#, "<strong><em>", "</em></strong>"),
            (#"\*\*(.+?)\*\*"#, "<strong>", "</strong>"),
            (#"\*(.+?)\*"#, "<em>", "</em>"),
            (#"__(.+?)__"#, "<strong>", "</strong>"),
            (#"_(.+?)_"#, "<em>", "</em>"),
            (#"~~(.+?)~~"#, "<del>", "</del>"),
            (#"`(.+?)`"#, "<code>", "</code>"),
        ]

        for (pattern, open, close) in patterns {
            if let regex = try? NSRegularExpression(pattern: pattern) {
                let range = NSRange(s.startIndex..., in: s)
                s = regex.stringByReplacingMatches(in: s, range: range, withTemplate: open + "$1" + close)
            }
        }

        if let linkRegex = try? NSRegularExpression(pattern: #"\[(.+?)\]\((.+?)\)"#) {
            let range = NSRange(s.startIndex..., in: s)
            s = linkRegex.stringByReplacingMatches(in: s, range: range, withTemplate: "<a href=\"$2\" target=\"_blank\">$1</a>")
        }

        return s
    }

    func renderTable(_ rows: [[String]]) -> String {
        guard !rows.isEmpty else { return "" }
        var html = "<table>"
        for (i, row) in rows.enumerated() {
            html += "<tr>"
            let tag = i == 0 ? "th" : "td"
            for cell in row {
                html += "<\(tag)>" + inlineMarkdown(cell) + "</\(tag)>"
            }
            html += "</tr>"
        }
        html += "</table>"
        return html
    }

    func escapeHTML(_ s: String) -> String {
        s.replacingOccurrences(of: "&", with: "&amp;")
         .replacingOccurrences(of: "<", with: "&lt;")
         .replacingOccurrences(of: ">", with: "&gt;")
         .replacingOccurrences(of: "\"", with: "&quot;")
    }

    func escapeJS(_ s: String) -> String {
        var r = ""
        for c in s {
            switch c {
            case "\\": r += "\\\\"
            case "\"": r += "\\\""
            case "\n": r += "\\n"
            case "\r": r += "\\r"
            default: r.append(c)
            }
        }
        return r
    }
}

class ToggleHandler: NSObject, WKScriptMessageHandler {
    weak var controller: OverlayController?
    init(controller: OverlayController) { self.controller = controller }

    func userContentController(_ uc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? String,
              let data = body.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let tab = json["tab"] as? Int,
              let line = json["line"] as? Int,
              let checked = json["checked"] as? Bool
        else { return }
        DispatchQueue.main.async { self.controller?.toggleCheckbox(tab: tab, lineIndex: line, checked: checked) }
    }
}

class EditHandler: NSObject, WKScriptMessageHandler {
    weak var controller: OverlayController?
    init(controller: OverlayController) { self.controller = controller }

    func userContentController(_ uc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? String,
              let data = body.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }

        if let action = json["action"] as? String, action == "cancel" {
            DispatchQueue.main.async { self.controller?.cancelEditMode() }
            return
        }

        guard let tab = json["tab"] as? Int,
              let b64 = json["b64"] as? String,
              let contentData = Data(base64Encoded: b64),
              let content = String(data: contentData, encoding: .utf8)
        else { return }
        DispatchQueue.main.async { self.controller?.saveContent(tab: tab, content: content) }
    }
}

class ZoomHandler: NSObject, WKScriptMessageHandler {
    weak var controller: OverlayController?
    init(controller: OverlayController) { self.controller = controller }

    func userContentController(_ uc: WKUserContentController, didReceive message: WKScriptMessage) {
        if let val = message.body as? String, let z = Int(val) {
            DispatchQueue.main.async {
                self.controller?.zoom = z
                self.controller?.zoomLabel.stringValue = "\(z)%"
            }
        }
    }
}

@main
struct PinnedOverlayApp {
    static func main() {
        let args = CommandLine.arguments
        guard args.count >= 2 else {
            print("Usage: pinned-overlay <file> [file2 ...] [--x N] [--y N] [--width N] [--height N] [--opacity F]")
            exit(1)
        }

        var filePaths: [String] = []
        var x = 800, y = 200, w = 400, h = 500
        var opacity = 0.85

        var i = 1
        while i < args.count {
            switch args[i] {
            case "--x": i += 1; if i < args.count { x = Int(args[i]) ?? x }
            case "--y": i += 1; if i < args.count { y = Int(args[i]) ?? y }
            case "--width": i += 1; if i < args.count { w = Int(args[i]) ?? w }
            case "--height": i += 1; if i < args.count { h = Int(args[i]) ?? h }
            case "--opacity": i += 1; if i < args.count { opacity = Double(args[i]) ?? opacity }
            default:
                if !args[i].hasPrefix("--") { filePaths.append(args[i]) }
            }
            i += 1
        }

        guard !filePaths.isEmpty else {
            print("Error: at least one file path required")
            exit(1)
        }

        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)

        let _ = OverlayController(filePaths: filePaths, x: x, y: y, w: w, h: h, opacity: opacity)

        app.run()
    }
}
