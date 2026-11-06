import Foundation

/// Whether the app's web view is showing the amux dashboard, or a page it
/// reached from there that has no amux controls of its own: a public app at
/// /pub/<id>/, a file at /f/..., an HTML app at /pages/<name>, an app on the
/// apps port, or another site.
///
/// A link with target=_blank loads in the same web view (WebView's
/// createWebViewWith), so opening a public app from the Apps tab replaced the
/// dashboard. On 2026-11-05 a password-protected app left the owner with no
/// way back short of quitting the app: "If I don't have the password there's
/// no way to exit this screen". ContentView shows a back button whenever this
/// is false.
enum DashboardPage {
    /// Pages that are the dashboard's own way in. A person who is not signed in
    /// has no dashboard to go back to, so these get no back button.
    static let entryPaths: Set<String> = ["", "/", "/index.html", "/sign-in", "/login"]

    static func isDashboard(_ url: URL?, server: URL) -> Bool {
        // No page yet, or about:blank: the error overlay owns that state.
        guard let url, let scheme = url.scheme?.lowercased(), scheme == "http" || scheme == "https" else {
            return true
        }
        guard url.host?.lowercased() == server.host?.lowercased(), port(url) == port(server) else {
            return false
        }
        return entryPaths.contains(url.path)
    }

    /// The dashboard's root on `server`, dropping any deep-link path or hash.
    static func root(of server: URL) -> URL {
        var c = URLComponents()
        c.scheme = server.scheme
        c.host = server.host
        c.port = server.port
        c.path = "/"
        return c.url ?? server
    }

    private static func port(_ url: URL) -> Int? {
        url.port ?? (url.scheme?.lowercased() == "http" ? 80 : 443)
    }
}
