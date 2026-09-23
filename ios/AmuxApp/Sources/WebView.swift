import SwiftUI
import WebKit
import os.log

private let logger = Logger(subsystem: "io.amux.app", category: "WebView")

struct WebView: UIViewRepresentable {
    let url: URL
    @Binding var isLoading: Bool
    @Binding var canGoBack: Bool
    @Binding var canGoForward: Bool
    @Binding var loadError: String?
    let onNavigationAction: (WKNavigationAction) -> WKNavigationActionPolicy

    func makeCoordinator() -> Coordinator {
        Coordinator(self)
    }

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.allowsInlineMediaPlayback = true
        config.dataDetectorTypes = []

        let webView = WKWebView(frame: .zero, configuration: config)
        webView.navigationDelegate = context.coordinator
        webView.uiDelegate = context.coordinator
        webView.allowsBackForwardNavigationGestures = true
        webView.customUserAgent = (webView.value(forKey: "userAgent") as? String ?? "") + " AmuxApp"
        // .never, NOT .automatic. The dashboard ships `viewport-fit=cover` and
        // 39 `env(safe-area-inset-*)` rules, so it already carves the notch and
        // the home indicator itself. `.automatic` makes UIKit inset the scroll
        // view by the SAME safe area, and the two stack: measured on an
        // iPhone 17 simulator, a dead band above the worker header and another
        // below the composer, both far larger than the 59pt/34pt insets that
        // explain them.
        //
        // The page is the right owner of that decision, because it is the only
        // side that knows which of its elements are pinned to an edge.
        webView.scrollView.contentInsetAdjustmentBehavior = .never
        webView.isOpaque = false
        webView.backgroundColor = UIColor(red: 0.051, green: 0.067, blue: 0.09, alpha: 1) // #0d1117

        // Pull-to-refresh
        let refresh = UIRefreshControl()
        refresh.addTarget(context.coordinator, action: #selector(Coordinator.handleRefresh(_:)), for: .valueChanged)
        webView.scrollView.addSubview(refresh)
        context.coordinator.refreshControl = refresh
        context.coordinator.webView = webView

        // Capture JS console.log/error/warn into os_log
        let script = WKUserScript(source: """
            (function() {
                const _log = console.log, _warn = console.warn, _err = console.error;
                function post(level, args) {
                    window.webkit.messageHandlers.consoleLog.postMessage(
                        { level: level, message: Array.from(args).map(String).join(' ') }
                    );
                }
                console.log = function() { post('log', arguments); _log.apply(console, arguments); };
                console.warn = function() { post('warn', arguments); _warn.apply(console, arguments); };
                console.error = function() { post('error', arguments); _err.apply(console, arguments); };
                window.addEventListener('error', function(e) {
                    post('error', ['Uncaught: ' + e.message + ' at ' + e.filename + ':' + e.lineno]);
                });
                window.addEventListener('unhandledrejection', function(e) {
                    post('error', ['Unhandled rejection: ' + (e.reason || e)]);
                });
            })();
            """, injectionTime: .atDocumentStart, forMainFrameOnly: false)
        config.userContentController.add(context.coordinator, name: "consoleLog")
        config.userContentController.addUserScript(script)

        context.coordinator.requestedURL = url
        webView.load(URLRequest(url: url))
        context.coordinator.armWatchdog(host: url.host)
        logger.info("Loading URL: \(url.absoluteString)")
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        // Reload only when the REQUESTED server changed.
        //
        // This used to compare the WEBVIEW'S CURRENT url against the target,
        // and that is an infinite loop whenever the server cannot be reached.
        // WebKit parks an unreachable navigation on `about:blank`, whose host
        // and port never match, so this reloaded; the reload flips isLoading,
        // SwiftUI re-renders, updateUIView runs again, and it reloads again.
        // Measured against an unreachable host: "Navigation started" ->
        // "Navigation finished: about:blank" repeating every ~3ms, 3327 commits
        // in 18 seconds.
        //
        // It is also why no error could ever appear. Each spurious didFinish
        // set loadError = nil and isLoading = false, so the overlay was cleared
        // thousands of times a second and the user saw an unexplained dark
        // screen while the phone burned battery.
        //
        // Comparing against what we ASKED for is stable: about:blank is not a
        // server switch, so it no longer triggers one.
        if context.coordinator.requestedURL != url {
            context.coordinator.requestedURL = url
            webView.load(URLRequest(url: url))
            context.coordinator.armWatchdog(host: url.host)
        }
    }

    // MARK: - Coordinator
    class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {
        var parent: WebView
        weak var webView: WKWebView?
        var refreshControl: UIRefreshControl?
        /// The URL we last ASKED the web view to load. Compared against the
        /// desired URL in updateUIView; see the note there for why the web
        /// view's own `url` is the wrong thing to compare.
        var requestedURL: URL?

        init(_ parent: WebView) {
            self.parent = parent
        }

        // MARK: - Load watchdog
        //
        // THE ERROR OVERLAY ONLY EXISTS FOR NAVIGATIONS THAT FAIL, and a
        // navigation can do neither. `loadError` is set exclusively by
        // `didFail` and `didFailProvisionalNavigation`, so a load that starts
        // and never resolves leaves `loadError` nil and the view blank.
        // ContentView then renders the WebView and nothing else, which is a
        // dark empty screen with no message and no retry.
        //
        // That is the reported symptom, and the owner's screenshot pins WHICH
        // variant it was: a thin linear bar under the status bar, which is
        // ContentView's `if isLoading { ProgressView(.linear) }`. isLoading
        // true with loadError nil means the navigation had started and never
        // finished or failed. Reproduced here against an unreachable host: two
        // minutes, no overlay, no progress, nothing.
        //
        // Deliberately distinct from the dashboard-side fix (a7eca7d3), which
        // handles a page that LOADED and then failed to read sessions. That
        // one cannot help here, because when the page never loads none of its
        // JavaScript runs.
        //
        // Armed where the load is ISSUED rather than only in
        // didStartProvisionalNavigation, because a request that never reaches
        // the network may not produce that callback either.
        static let loadTimeout: TimeInterval = 25
        private var watchdog: DispatchWorkItem?

        func armWatchdog(host: String?) {
            watchdog?.cancel()
            let where_ = host ?? "the server"
            let item = DispatchWorkItem { [weak self] in
                guard let self else { return }
                // Only speak if nothing else already did. A real failure or a
                // successful load both resolve this more accurately.
                guard self.parent.loadError == nil, self.parent.isLoading || self.webView?.url == nil
                else { return }
                self.parent.isLoading = false
                self.parent.loadError =
                    "No response from \(where_) after \(Int(Coordinator.loadTimeout))s. "
                    + "The server may be down, or this device may have lost its VPN or "
                    + "Tailscale route to it."
                logger.error("Load watchdog fired after \(Int(Coordinator.loadTimeout))s for \(where_)")
            }
            watchdog = item
            DispatchQueue.main.asyncAfter(deadline: .now() + Coordinator.loadTimeout, execute: item)
        }

        func disarmWatchdog() {
            watchdog?.cancel()
            watchdog = nil
        }

        // JS console → os_log bridge
        func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            guard let body = message.body as? [String: String],
                  let level = body["level"], let msg = body["message"] else { return }
            switch level {
            case "error": logger.error("[js] \(msg)")
            case "warn":  logger.warning("[js] \(msg)")
            default:      logger.debug("[js] \(msg)")
            }
        }

        // Accept self-signed certs for self-hosted servers
        // (Tailscale, LAN, custom domains — anything that isn't a well-known public CA)
        func webView(_ webView: WKWebView,
                     didReceive challenge: URLAuthenticationChallenge,
                     completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
            guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
                  let serverTrust = challenge.protectionSpace.serverTrust else {
                completionHandler(.performDefaultHandling, nil)
                return
            }
            let host = challenge.protectionSpace.host
            // Trust self-signed certs for: Tailscale (.ts.net), local networks,
            // private IPs, and any non-cloud.amux.io host (user-configured servers)
            let isPublicAmux = host.hasSuffix("amux.io")
            if !isPublicAmux {
                completionHandler(.useCredential, URLCredential(trust: serverTrust))
            } else {
                completionHandler(.performDefaultHandling, nil)
            }
        }

        func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
                     decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
            // Allow all navigations — OAuth is handled in-place via the gateway JS
            // (window.open override converts popup OAuth to same-window navigation)
            decisionHandler(parent.onNavigationAction(navigationAction))
        }

        // Handle window.open — navigate in same webview instead of dropping
        func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                     for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
            if let url = navigationAction.request.url {
                webView.load(URLRequest(url: url))
            }
            return nil
        }

        func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
            parent.isLoading = true
            parent.loadError = nil
            armWatchdog(host: webView.url?.host ?? parent.url.host)
            logger.debug("Navigation started: \(webView.url?.absoluteString ?? "nil")")
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            disarmWatchdog()
            parent.isLoading = false
            // A navigation that "finishes" on about:blank did NOT reach the
            // server; WebKit parks unreachable loads there. Reporting success
            // is what left the screen blank and silent, so say it plainly and
            // reuse the overlay that already exists for unreachable servers.
            if webView.url.map({ $0.absoluteString == "about:blank" }) == true,
               parent.url.absoluteString != "about:blank" {
                parent.loadError =
                    "Could not load \(parent.url.host ?? "the server"). The server may be "
                    + "down, or this device may have lost its VPN or Tailscale route to it."
                logger.error("Navigation parked on about:blank for \(self.parent.url.absoluteString)")
                refreshControl?.endRefreshing()
                return
            }
            parent.loadError = nil
            parent.canGoBack = webView.canGoBack
            parent.canGoForward = webView.canGoForward
            refreshControl?.endRefreshing()
            logger.info("Navigation finished: \(webView.url?.absoluteString ?? "nil")")
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            disarmWatchdog()
            parent.isLoading = false
            refreshControl?.endRefreshing()
            let nsError = error as NSError
            // Don't show cancellation errors (e.g. user tapped a link before page loaded)
            if nsError.code != NSURLErrorCancelled {
                parent.loadError = error.localizedDescription
            }
            logger.error("Navigation failed: \(error.localizedDescription)")
        }

        func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
            disarmWatchdog()
            parent.isLoading = false
            refreshControl?.endRefreshing()
            let nsError = error as NSError
            if nsError.code != NSURLErrorCancelled {
                parent.loadError = error.localizedDescription
            }
            logger.error("Provisional navigation failed: \(error.localizedDescription) url=\(webView.url?.absoluteString ?? "nil")")
        }

        func webView(_ webView: WKWebView, didReceiveServerRedirectForProvisionalNavigation navigation: WKNavigation!) {
            logger.debug("Server redirect → \(webView.url?.absoluteString ?? "nil")")
        }

        func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
            logger.fault("WebContent process terminated — reloading")
            webView.reload()
            armWatchdog(host: webView.url?.host ?? parent.url.host)
        }

        @objc func handleRefresh(_ sender: UIRefreshControl) {
            webView?.reload()
            armWatchdog(host: webView?.url?.host ?? parent.url.host)
        }
    }
}

// Exposed for back/forward control from ContentView
extension WebView {
    static func goBack(in webView: WKWebView?) { webView?.goBack() }
    static func goForward(in webView: WKWebView?) { webView?.goForward() }
}
