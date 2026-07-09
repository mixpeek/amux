import Cocoa
import WebKit

// ── Persistent config ──
let configURL: URL = {
    let dir = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent(".amux")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir.appendingPathComponent("desktop-config.json")
}()

struct Connection: Codable {
    var name: String
    var url: String
}

struct Config: Codable {
    var connections: [Connection]
    var lastUrl: String
}

func loadConfig() -> Config {
    guard let data = try? Data(contentsOf: configURL),
          let config = try? JSONDecoder().decode(Config.self, from: data) else {
        return Config(connections: [
            Connection(name: "localhost", url: "https://localhost:8822")
        ], lastUrl: "https://localhost:8822")
    }
    return config
}

func saveConfig(_ config: Config) {
    if let data = try? JSONEncoder().encode(config) {
        try? data.write(to: configURL)
    }
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
        font-size: 0.9rem; outline: none; font-family: inherit;
      }
      input:focus { border-color: #58a6ff; }
      input::placeholder { color: #484f58; }
      .btn {
        padding: 10px 20px; border-radius: 8px; border: none;
        background: #238636; color: #fff; font-size: 0.85rem;
        font-weight: 600; cursor: pointer; font-family: inherit;
      }
      .btn:hover { background: #2ea043; }
      .divider { border-top: 1px solid #21262d; margin: 4px 0 16px; }
      .label { font-size: 0.75rem; color: #8b949e; text-transform: uppercase;
        letter-spacing: 0.05em; margin-bottom: 8px; }
      .conn-item {
        display: flex; align-items: center; gap: 10px; padding: 10px 12px;
        background: #161b22; border: 1px solid #21262d; border-radius: 8px;
        cursor: pointer; margin-bottom: 4px; transition: border-color 0.15s;
      }
      .conn-item:hover { border-color: #58a6ff; }
      .conn-name { font-size: 0.88rem; font-weight: 600; }
      .conn-url { font-size: 0.72rem; color: #8b949e; overflow: hidden;
        text-overflow: ellipsis; white-space: nowrap; }
      .conn-info { flex: 1; min-width: 0; }
      .conn-remove {
        background: none; border: none; color: #484f58; cursor: pointer;
        font-size: 1rem; padding: 4px; opacity: 0; transition: opacity 0.15s;
      }
      .conn-item:hover .conn-remove { opacity: 1; }
      .conn-remove:hover { color: #f85149; }
      .conn-last { font-size: 0.65rem; color: #58a6ff; flex-shrink: 0; }
      .empty { color: #484f58; font-size: 0.82rem; text-align: center; padding: 20px 0; }
    </style>
    </head>
    <body>
    <div class="container">
      <h1>amux</h1>
      <div class="subtitle">Connect to an amux server</div>
      <div class="input-row">
        <input type="text" id="url" placeholder="https://localhost:8822"
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
        if (!url) url = 'https://localhost:8822';
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

// ── App delegate ──
class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow!
    var webView: WKWebView!
    var config: Config!

    func applicationDidFinishLaunching(_ notification: Notification) {
        config = loadConfig()

        let wkConfig = WKWebViewConfiguration()
        wkConfig.preferences.setValue(true, forKey: "developerExtrasEnabled")
        wkConfig.mediaTypesRequiringUserActionForPlayback = []

        webView = WKWebView(frame: .zero, configuration: wkConfig)
        webView.customUserAgent = "Amux-Desktop/1.0"
        webView.navigationDelegate = self

        let screen = NSScreen.main!.frame
        let width: CGFloat = min(1440, screen.width * 0.85)
        let height: CGFloat = min(900, screen.height * 0.85)
        let x = (screen.width - width) / 2
        let y = (screen.height - height) / 2

        window = NSWindow(
            contentRect: NSRect(x: x, y: y, width: width, height: height),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        window.title = "Amux"
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.backgroundColor = NSColor(red: 13/255, green: 17/255, blue: 23/255, alpha: 1)
        window.minSize = NSSize(width: 800, height: 500)
        window.contentView = webView
        window.makeKeyAndOrderFront(nil)

        showConnectPage()
    }

    func showConnectPage() {
        config = loadConfig()
        webView.loadHTMLString(connectPageHTML(config), baseURL: nil)
    }

    func connectToServer(_ urlString: String) {
        var url = urlString
        if !url.hasPrefix("http") { url = "https://" + url }
        config.lastUrl = url
        // Add to connections if new
        if !config.connections.contains(where: { $0.url.replacingOccurrences(of: "/+$", with: "", options: .regularExpression) == url.replacingOccurrences(of: "/+$", with: "", options: .regularExpression) }) {
            let name = URL(string: url)?.host ?? url
            config.connections.append(Connection(name: name, url: url))
        }
        saveConfig(config)
        loadWithRetry(url: url, attempts: 0)
    }

    func loadWithRetry(url: String, attempts: Int) {
        guard let nsUrl = URL(string: url) else { return }
        let request = URLRequest(url: nsUrl, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 3)
        webView.load(request)

        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
            guard let self = self else { return }
            if self.webView.isLoading { return }
            if let host = self.webView.url?.host, host == (URL(string: url)?.host ?? "localhost") { return }
            if attempts < 20 {
                self.loadWithRetry(url: url, attempts: attempts + 1)
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        return true
    }
}

// ── Navigation delegate ──
extension AppDelegate: WKNavigationDelegate {
    func webView(_ webView: WKWebView,
                 didReceive challenge: URLAuthenticationChallenge,
                 completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        // Trust localhost / private network self-signed certs
        if let host = challenge.protectionSpace.host.components(separatedBy: ":").first,
           (host == "localhost" || host == "127.0.0.1" || host.hasSuffix(".local")
            || host.hasPrefix("10.") || host.hasPrefix("192.168.")),
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

        // Handle connect page actions via custom URL scheme
        if url.scheme == "amux-connect" {
            if let b64 = url.host, let data = Data(base64Encoded: b64),
               let serverUrl = String(data: data, encoding: .utf8) {
                connectToServer(serverUrl)
            }
            decisionHandler(.cancel)
            return
        }

        if url.scheme == "amux-remove" {
            if let idxStr = url.host, let idx = Int(idxStr), idx < config.connections.count {
                config.connections.remove(at: idx)
                saveConfig(config)
                showConnectPage()
            }
            decisionHandler(.cancel)
            return
        }

        // Open external links in default browser
        if let host = url.host,
           host != "localhost" && host != "127.0.0.1" && !host.hasSuffix(".local"),
           navigationAction.navigationType == .linkActivated {
            NSWorkspace.shared.open(url)
            decisionHandler(.cancel)
            return
        }

        decisionHandler(.allow)
    }
}

// ── Launch ──
let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
