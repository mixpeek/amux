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
        webView.scrollView.contentInsetAdjustmentBehavior = .automatic
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

        webView.load(URLRequest(url: url))
        logger.info("Loading URL: \(url.absoluteString)")
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        // Re-load if the URL changed (server switch)
        if webView.url?.host != url.host || webView.url?.port != url.port {
            webView.load(URLRequest(url: url))
        }
    }

    // MARK: - Coordinator
    class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate, WKScriptMessageHandler {
        var parent: WebView
        weak var webView: WKWebView?
        var refreshControl: UIRefreshControl?

        init(_ parent: WebView) {
            self.parent = parent
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
            logger.debug("Navigation started: \(webView.url?.absoluteString ?? "nil")")
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            parent.isLoading = false
            parent.loadError = nil
            parent.canGoBack = webView.canGoBack
            parent.canGoForward = webView.canGoForward
            refreshControl?.endRefreshing()
            logger.info("Navigation finished: \(webView.url?.absoluteString ?? "nil")")
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
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
        }

        @objc func handleRefresh(_ sender: UIRefreshControl) {
            webView?.reload()
        }
    }
}

// Exposed for back/forward control from ContentView
extension WebView {
    static func goBack(in webView: WKWebView?) { webView?.goBack() }
    static func goForward(in webView: WKWebView?) { webView?.goForward() }
}
