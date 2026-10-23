import Cocoa
import WebKit

// ── Status bar readout: polls CPU/MEM every 5s ──

struct SysStats {
    var cpuPct: Int = 0
    var memPct: Int = 0
    var cpuUser: Double = 0
    var cpuSys: Double = 0
}

func pollStats() -> SysStats {
    var s = SysStats()

    // CPU via host_statistics
    var cpuInfo = host_cpu_load_info_data_t()
    var count = mach_msg_type_number_t(MemoryLayout<host_cpu_load_info_data_t>.stride / MemoryLayout<integer_t>.stride)
    let kr = withUnsafeMutablePointer(to: &cpuInfo) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            host_statistics(mach_host_self(), HOST_CPU_LOAD_INFO, $0, &count)
        }
    }
    if kr == KERN_SUCCESS {
        let user = Double(cpuInfo.cpu_ticks.0)
        let sys  = Double(cpuInfo.cpu_ticks.1)
        let idle = Double(cpuInfo.cpu_ticks.2)
        let nice = Double(cpuInfo.cpu_ticks.3)
        let total = user + sys + idle + nice
        if total > 0 {
            s.cpuUser = (user + nice) / total * 100
            s.cpuSys  = sys / total * 100
            s.cpuPct  = Int((user + sys + nice) / total * 100)
        }
    }

    // Memory via host_statistics64
    var vmInfo = vm_statistics64_data_t()
    var vmCount = mach_msg_type_number_t(MemoryLayout<vm_statistics64_data_t>.stride / MemoryLayout<integer_t>.stride)
    let kr2 = withUnsafeMutablePointer(to: &vmInfo) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(vmCount)) {
            host_statistics64(mach_host_self(), HOST_VM_INFO64, $0, &vmCount)
        }
    }
    if kr2 == KERN_SUCCESS {
        let pageSize = UInt64(vm_kernel_page_size)
        let active = UInt64(vmInfo.active_count) * pageSize
        let wired  = UInt64(vmInfo.wire_count) * pageSize
        let compressed = UInt64(vmInfo.compressor_page_count) * pageSize
        let used = active + wired + compressed
        var totalMem: UInt64 = 0
        var size = MemoryLayout<UInt64>.size
        sysctlbyname("hw.memsize", &totalMem, &size, nil, 0)
        if totalMem > 0 {
            s.memPct = Int(used * 100 / totalMem)
        }
    }
    return s
}

// ── Menu bar controller ──

class MenuBarController: NSObject, WKNavigationDelegate, WKScriptMessageHandler {
    let statusItem: NSStatusItem
    let popover = NSPopover()
    let webView: WKWebView
    var files: [(path: String, content: String)] = []
    var fileWatchers: [DispatchSourceFileSystemObject] = []
    var pollTimer: Timer?
    var statsTimer: Timer?
    var currentStats = SysStats()
    let apiBase: String
    // Supplies the owner token for API calls. The app sets it (a remote server
    // refuses an anonymous request); the standalone readout talks to localhost.
    var authToken: ((@escaping (String?) -> Void) -> Void)?

    override convenience init() {
        self.init(apiBase: ProcessInfo.processInfo.environment["AMUX_URL"] ?? "https://localhost:8824")
    }

    init(apiBase: String) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)

        let config = WKWebViewConfiguration()
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")
        webView = WKWebView(frame: NSRect(x: 0, y: 0, width: 420, height: 560), configuration: config)

        self.apiBase = apiBase

        super.init()

        webView.navigationDelegate = self
        webView.setValue(false, forKey: "drawsBackground")
        webView.configuration.userContentController.add(self, name: "toggle")
        webView.configuration.userContentController.add(self, name: "edit")
        webView.configuration.userContentController.add(self, name: "switchTab")

        let vc = NSViewController()
        vc.view = webView
        popover.contentViewController = vc
        popover.contentSize = NSSize(width: 420, height: 560)
        popover.behavior = .transient
        popover.animates = true

        if let button = statusItem.button {
            button.target = self
            button.action = #selector(togglePopover(_:))
            updateStatusText()
        }

        // Poll stats every 5s for the status bar text
        statsTimer = Timer.scheduledTimer(withTimeInterval: 5.0, repeats: true) { [weak self] _ in
            self?.currentStats = pollStats()
            self?.updateStatusText()
        }
        currentStats = pollStats()
        updateStatusText()

        // Fetch pinned files from the API, then set up watchers
        fetchPinnedFiles()

        // Re-fetch pinned list every 30s
        pollTimer = Timer.scheduledTimer(withTimeInterval: 30.0, repeats: true) { [weak self] _ in
            self?.fetchPinnedFiles()
        }
    }

    /// Remove the status item (the desktop app re-creates it on reconnect).
    func stop() {
        pollTimer?.invalidate()
        statsTimer?.invalidate()
        for w in fileWatchers { w.cancel() }
        fileWatchers = []
        popover.performClose(nil)
        NSStatusBar.system.removeStatusItem(statusItem)
    }

    func updateStatusText() {
        let s = currentStats
        let text = "C:\(s.cpuPct)% M:\(s.memPct)%"
        if let button = statusItem.button {
            let attrs: [NSAttributedString.Key: Any] = [
                .font: NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .medium),
                .foregroundColor: NSColor.labelColor,
            ]
            button.attributedTitle = NSAttributedString(string: text, attributes: attrs)
        }
    }

    @objc func togglePopover(_ sender: AnyObject?) {
        if popover.isShown {
            popover.performClose(sender)
        } else {
            if let button = statusItem.button {
                loadContent()
                popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
            }
        }
    }

    func fetchPinnedFiles() {
        guard let url = URL(string: apiBase + "/api/pinned") else { return }
        guard let authToken = authToken else { requestPinnedFiles(url, token: nil); return }
        authToken { [weak self] token in self?.requestPinnedFiles(url, token: token) }
    }

    func requestPinnedFiles(_ url: URL, token: String?) {
        var req = URLRequest(url: url)
        req.timeoutInterval = 5
        if let token = token { req.setValue("Bearer " + token, forHTTPHeaderField: "Authorization") }
        let session = URLSession(configuration: .ephemeral, delegate: InsecureDelegate(), delegateQueue: nil)
        session.dataTask(with: req) { [weak self] data, _, _ in
            guard let data = data,
                  let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
            else { return }
            var paths: [String] = []
            for item in arr {
                if let fp = item["file_path"] as? String {
                    paths.append(fp)
                }
            }
            // Dedupe while preserving order
            var seen = Set<String>()
            paths = paths.filter { seen.insert($0).inserted }

            DispatchQueue.main.async {
                self?.updateFiles(paths)
            }
        }.resume()
    }

    func updateFiles(_ paths: [String]) {
        // Only rebuild if paths changed
        let oldPaths = files.map { $0.path }
        if oldPaths == paths { return }

        files = paths.map { p in
            let c = (try? String(contentsOf: URL(fileURLWithPath: p), encoding: .utf8)) ?? "(file not found)"
            return (path: p, content: c)
        }
        setupFileWatchers()
        if popover.isShown { loadContent() }
    }

    func setupFileWatchers() {
        for w in fileWatchers { w.cancel() }
        fileWatchers = []

        for f in files {
            let fd = open(f.path, O_EVTONLY)
            guard fd >= 0 else { continue }
            let source = DispatchSource.makeFileSystemObjectSource(
                fileDescriptor: fd, eventMask: [.write, .rename, .delete], queue: .main
            )
            source.setEventHandler { [weak self] in
                self?.reloadFileContents()
            }
            source.setCancelHandler { close(fd) }
            source.resume()
            fileWatchers.append(source)
        }
    }

    func reloadFileContents() {
        for i in 0..<files.count {
            files[i].content = (try? String(contentsOf: URL(fileURLWithPath: files[i].path), encoding: .utf8)) ?? ""
        }
        if popover.isShown { loadContent() }
    }

    func loadContent() {
        if files.isEmpty {
            webView.loadHTMLString(emptyHtml(), baseURL: nil)
            return
        }

        var fd = "["
        for (i, f) in files.enumerated() {
            let nm = escapeJS(URL(fileURLWithPath: f.path).lastPathComponent)
            let b64 = f.content.data(using: .utf8)?.base64EncodedString() ?? ""
            if i > 0 { fd += "," }
            fd += "{name:\"" + nm + "\",b64:\"" + b64 + "\"}"
        }
        fd += "]"

        var tabBar = ""
        if files.count > 1 {
            tabBar += "<div class=\"tab-bar\">"
            for (i, f) in files.enumerated() {
                let nm = URL(fileURLWithPath: f.path).lastPathComponent
                    .replacingOccurrences(of: ".md", with: "")
                    .replacingOccurrences(of: ".mdai", with: "")
                let cls = i == 0 ? " active" : ""
                tabBar += "<button class=\"\(cls)\" onclick=\"switchTab(\(i))\">\(escapeHTML(nm))</button>"
            }
            tabBar += "</div>"
        }

        var tabs = ""
        for (i, f) in files.enumerated() {
            let body = renderMarkdown(f.content, tabIndex: i)
            let cls = i == 0 ? " active" : ""
            tabs += "<div class=\"tab-content\(cls)\" id=\"tc-\(i)\">\(body)</div>"
        }

        let html = htmlTemplate()
            .replacingOccurrences(of: "/*FILEDATA*/[]", with: fd)
            .replacingOccurrences(of: "<!--TABBAR-->", with: tabBar)
            .replacingOccurrences(of: "<!--TABS-->", with: tabs)

        webView.loadHTMLString(html, baseURL: files.isEmpty ? nil : URL(fileURLWithPath: files[0].path).deletingLastPathComponent())
    }

    func userContentController(_ uc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let body = message.body as? String,
              let data = body.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }

        if message.name == "toggle" {
            guard let tab = json["tab"] as? Int,
                  let line = json["line"] as? Int,
                  let checked = json["checked"] as? Bool
            else { return }
            toggleCheckbox(tab: tab, lineIndex: line, checked: checked)
        } else if message.name == "edit" {
            if let action = json["action"] as? String, action == "cancel" { return }
            guard let tab = json["tab"] as? Int,
                  let b64 = json["b64"] as? String,
                  let contentData = Data(base64Encoded: b64),
                  let content = String(data: contentData, encoding: .utf8)
            else { return }
            saveContent(tab: tab, content: content)
        }
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
        loadContent()
    }

    // ── Markdown renderer (same as pinned-overlay) ──

    func renderMarkdown(_ text: String, tabIndex: Int) -> String {
        var html = ""
        let lines = text.components(separatedBy: "\n")
        var inCodeBlock = false
        var codeBlockContent = ""
        var inTable = false
        var tableRows: [[String]] = []
        var inList = false
        var listType = ""

        for (idx, line) in lines.enumerated() {
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
                let isSep = cells.allSatisfy { $0.allSatisfy { $0 == "-" || $0 == ":" || $0 == " " } }
                if isSep { continue }
                if !inTable { inTable = true; tableRows = [] }
                tableRows.append(cells)
                continue
            } else if inTable {
                html += renderTable(tableRows); inTable = false; tableRows = []
            }

            if trimmed.isEmpty {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                continue
            }

            if trimmed.hasPrefix("# ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<h1>" + inlineMarkdown(String(trimmed.dropFirst(2))) + "</h1>"
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
                let t = trimmed.replacingOccurrences(of: #"^\d+\.\s*"#, with: "", options: .regularExpression)
                html += "<li>" + inlineMarkdown(t) + "</li>"
            } else if trimmed.hasPrefix("> ") {
                if inList { html += listType == "ol" ? "</ol>" : "</ul>"; inList = false }
                html += "<blockquote>" + inlineMarkdown(String(trimmed.dropFirst(2))) + "</blockquote>"
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
            s = linkRegex.stringByReplacingMatches(in: s, range: range, withTemplate: "<a href=\"$2\">$1</a>")
        }
        return s
    }

    func renderTable(_ rows: [[String]]) -> String {
        guard !rows.isEmpty else { return "" }
        var html = "<table>"
        for (i, row) in rows.enumerated() {
            html += "<tr>"
            let tag = i == 0 ? "th" : "td"
            for cell in row { html += "<\(tag)>" + inlineMarkdown(cell) + "</\(tag)>" }
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

    func emptyHtml() -> String {
        return """
        <!DOCTYPE html><html><head><meta charset="utf-8">
        <style>body{font-family:-apple-system,system-ui;background:#1e1e1e;color:#aaa;display:flex;
        align-items:center;justify-content:center;height:100vh;text-align:center;font-size:13px;}
        </style></head><body><div>No pinned files.<br>Pin a file from the amux Files tab.</div></body></html>
        """
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
                --border: rgba(255,255,255,0.12); --table-header: rgba(255,255,255,0.05);
                --scrollbar: rgba(255,255,255,0.15);
            }
            * { margin: 0; padding: 0; box-sizing: border-box; }
            html, body { height: 100%; overflow-y: auto; background: #1e1e1e; }
            body {
                font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif;
                font-size: 13px; color: var(--fg);
                padding: 12px 14px 14px; line-height: 1.5;
            }
            .tab-bar {
                display: flex; gap: 2px; margin-bottom: 8px; position: sticky; top: 0;
                background: #1e1e1e; padding: 4px 0 6px; z-index: 10;
                border-bottom: 1px solid var(--border);
            }
            .tab-bar button {
                font-size: 11px; padding: 4px 12px; border-radius: 6px;
                border: 1px solid var(--border);
                background: transparent; color: var(--fg-muted);
                cursor: pointer; white-space: nowrap;
            }
            .tab-bar button.active { background: var(--accent); color: #fff; border-color: var(--accent); font-weight: 600; }
            .tab-bar button:hover:not(.active) { background: rgba(255,255,255,0.06); }
            .tab-content { display: none; }
            .tab-content.active { display: block; }

            h1 { font-size: 18px; font-weight: 700; margin-bottom: 2px; }
            h2 { font-size: 14px; font-weight: 700; margin: 14px 0 6px; color: var(--fg-heading);
                 border-bottom: 1px solid var(--border); padding-bottom: 4px; }
            h3 { font-size: 12px; font-weight: 600; margin: 10px 0 4px; color: var(--fg-dim); text-transform: uppercase; letter-spacing: 0.5px; }
            h4 { font-size: 12px; font-weight: 600; margin: 8px 0 4px; }
            p { margin: 4px 0; }
            strong { font-weight: 700; }
            em { font-style: italic; }
            a { color: var(--accent); text-decoration: none; }
            ul, ol { padding-left: 16px; margin: 2px 0; }
            li { padding: 1px 0; }

            .task-item {
                display: flex; align-items: flex-start; gap: 6px;
                padding: 3px 0; list-style: none;
            }
            .task-item input[type=checkbox] {
                margin-top: 2px; accent-color: var(--accent); cursor: pointer;
                width: 14px; height: 14px;
            }
            .task-item.checked > span { text-decoration: line-through; opacity: 0.4; }
            blockquote {
                border-left: 2px solid var(--border);
                padding: 2px 0 2px 10px; margin: 6px 0;
                color: var(--fg-muted); font-style: italic;
            }
            code {
                background: var(--code-bg); padding: 1px 4px;
                border-radius: 3px; font-size: 0.88em;
                font-family: "SF Mono", Menlo, monospace;
            }
            pre {
                background: var(--code-bg); padding: 8px 10px;
                border-radius: 5px; overflow-x: auto; margin: 6px 0;
                font-size: 0.85em; line-height: 1.35;
            }
            pre code { background: none; padding: 0; }
            hr { border: none; border-top: 1px solid var(--border); margin: 10px 0; }
            table { border-collapse: collapse; margin: 6px 0; width: 100%; font-size: 0.88em; }
            th, td { border: 1px solid var(--border); padding: 3px 6px; text-align: left; }
            th { background: var(--table-header); font-weight: 600; font-size: 0.9em; }
            td:first-child { white-space: nowrap; }

            ::-webkit-scrollbar { width: 5px; }
            ::-webkit-scrollbar-track { background: transparent; }
            ::-webkit-scrollbar-thumb { background: var(--scrollbar); border-radius: 3px; }
        </style>
        </head>
        <body>
            <!--TABBAR-->
            <!--TABS-->
            <script>
                var _files = /*FILEDATA*/[];
                var _currentTab = 0;

                function switchTab(idx) {
                    _currentTab = idx;
                    document.querySelectorAll('.tab-content').forEach(function(el, i) {
                        el.classList.toggle('active', i === idx);
                    });
                    document.querySelectorAll('.tab-bar button').forEach(function(el, i) {
                        el.classList.toggle('active', i === idx);
                    });
                }

                function handleToggle(tab, lineIdx, checked) {
                    try {
                        window.webkit.messageHandlers.toggle.postMessage(
                            JSON.stringify({ tab: tab, line: lineIdx, checked: checked })
                        );
                    } catch(e) {}
                }
            </script>
        </body>
        </html>
        """
    }
}

// Allow self-signed certs for the amux API
class InsecureDelegate: NSObject, URLSessionDelegate {
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        if challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
           let trust = challenge.protectionSpace.serverTrust {
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            completionHandler(.performDefaultHandling, nil)
        }
    }
}

// ── Entry point ──

#if !AMUX_APP
@main
struct AmuxMenuBarApp {
    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)

        let _ = MenuBarController()

        app.run()
    }
}
#endif
