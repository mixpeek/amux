import SwiftUI
import WebKit
import AuthenticationServices

struct WebView: UIViewRepresentable {
    let url: URL
    @Binding var isLoading: Bool
    @Binding var canGoBack: Bool
    @Binding var canGoForward: Bool
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
        webView.scrollView.contentInsetAdjustmentBehavior = .automatic

        // Pull-to-refresh
        let refresh = UIRefreshControl()
        refresh.addTarget(context.coordinator, action: #selector(Coordinator.handleRefresh(_:)), for: .valueChanged)
        webView.scrollView.addSubview(refresh)
        context.coordinator.refreshControl = refresh
        context.coordinator.webView = webView

        webView.load(URLRequest(url: url))
        return webView
    }

    func updateUIView(_ webView: WKWebView, context: Context) {
        // Re-load if the URL changed (server switch)
        if webView.url?.host != url.host || webView.url?.port != url.port {
            webView.load(URLRequest(url: url))
        }
    }

    // MARK: - Coordinator
    class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate, ASWebAuthenticationPresentationContextProviding {
        var parent: WebView
        weak var webView: WKWebView?
        var refreshControl: UIRefreshControl?
        var authSession: ASWebAuthenticationSession?

        init(_ parent: WebView) {
            self.parent = parent
        }

        // ASWebAuthenticationPresentationContextProviding
        func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
            UIApplication.shared.connectedScenes
                .compactMap { $0 as? UIWindowScene }
                .flatMap { $0.windows }
                .first(where: { $0.isKeyWindow }) ?? ASPresentationAnchor()
        }

        // Accept self-signed certs (Tailscale local installs)
        func webView(_ webView: WKWebView,
                     didReceive challenge: URLAuthenticationChallenge,
                     completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
            guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
                  let serverTrust = challenge.protectionSpace.serverTrust else {
                completionHandler(.performDefaultHandling, nil)
                return
            }
            let host = challenge.protectionSpace.host
            let isTailscale = host.contains(".ts.net") || host.hasSuffix(".local") || host == "localhost"
            if isTailscale {
                completionHandler(.useCredential, URLCredential(trust: serverTrust))
            } else {
                completionHandler(.performDefaultHandling, nil)
            }
        }

        func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
                     decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
            // Intercept Apple OAuth redirects and use ASWebAuthenticationSession
            if let url = navigationAction.request.url,
               let host = url.host,
               host.contains("appleid.apple.com") {
                decisionHandler(.cancel)
                startAuthSession(url: url, webView: webView)
                return
            }
            decisionHandler(parent.onNavigationAction(navigationAction))
        }

        private func startAuthSession(url: URL, webView: WKWebView) {
            // Determine the callback scheme from the server URL
            let callbackScheme = "https"
            let serverHost = parent.url.host ?? "cloud.amux.io"

            let session = ASWebAuthenticationSession(
                url: url,
                callbackURLScheme: callbackScheme
            ) { [weak self] callbackURL, error in
                self?.authSession = nil
                if let callbackURL = callbackURL {
                    // Load the callback URL back in the WebView to complete the OAuth flow
                    webView.load(URLRequest(url: callbackURL))
                } else if let error = error as? ASWebAuthenticationSessionError,
                          error.code == .canceledLogin {
                    // User cancelled — reload the sign-in page
                    webView.load(URLRequest(url: self?.parent.url ?? url))
                }
            }
            session.presentationContextProvider = self
            session.prefersEphemeralWebBrowserSession = false
            authSession = session
            session.start()
        }

        func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
            parent.isLoading = true
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            parent.isLoading = false
            parent.canGoBack = webView.canGoBack
            parent.canGoForward = webView.canGoForward
            refreshControl?.endRefreshing()
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            parent.isLoading = false
            refreshControl?.endRefreshing()
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
