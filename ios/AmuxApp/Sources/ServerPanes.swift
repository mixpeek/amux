import Foundation

/// The servers whose dashboards stay loaded, most recent first.
///
/// Switching servers used to point the one web view at the new address, so every switch was a
/// cold start: a dark screen, the sign-in cookie check, the whole dashboard bundle, and coming
/// back to the first server started over again and lost the view the owner had left from.
/// Each server now keeps its own web view. Switching back to one already open shows it as it
/// was, with no reload. `limit` bounds how many stay loaded; the least recent is closed first.
struct ServerPanes: Equatable {
    struct Pane: Equatable, Identifiable {
        /// `scheme://host[:port]`, the server's identity (`key(of:)`).
        let key: String
        /// What this pane's web view is asked to load. Unchanged by a plain switch.
        var url: URL
        var id: String { key }
    }

    /// What `show` did, for the log line (`ios_server_switch verdict=`).
    enum Verdict: String {
        /// The server had no pane: one is created and loads.
        case opened
        /// The server's pane was open: it is brought forward as it was.
        case resumed
        /// The server's pane was open and was asked to load a new page (a link, a sign-in).
        case navigated
        /// Not an http(s) address; nothing changed.
        case refused
    }

    static let limit = 3

    private(set) var panes: [Pane] = []

    var active: Pane? { panes.first }

    /// The identity of a server: scheme, lowercased host and port. Paths, queries and hashes
    /// (a deep link, a sign-in link) belong to the same server.
    static func key(of url: URL) -> String? {
        guard let scheme = url.scheme?.lowercased(), scheme == "https" || scheme == "http",
              let host = url.host?.lowercased(), !host.isEmpty else { return nil }
        return url.port.map { "\(scheme)://\(host):\($0)" } ?? "\(scheme)://\(host)"
    }

    /// Shows `url`'s server. `navigate` false is a switch: an open pane keeps its page.
    /// `navigate` true loads `url` in the pane, for a deep link or a one-time sign-in link.
    /// Returns the verdict and the key of a pane closed to stay within `limit`, if any.
    @discardableResult
    mutating func show(_ url: URL, navigate: Bool) -> (verdict: Verdict, closed: String?) {
        guard let key = Self.key(of: url) else { return (.refused, nil) }
        var verdict = Verdict.opened
        var pane = Pane(key: key, url: url)
        if let i = panes.firstIndex(where: { $0.key == key }) {
            pane = panes.remove(at: i)
            verdict = .resumed
            if navigate && pane.url != url {
                pane.url = url
                verdict = .navigated
            }
        }
        panes.insert(pane, at: 0)
        var closed: String?
        if panes.count > Self.limit { closed = panes.removeLast().key }
        return (verdict, closed)
    }

    /// Closes a server's pane (the server was removed, or the app went back to setup).
    mutating func close(_ key: String) {
        panes.removeAll { $0.key == key }
    }

    mutating func closeAll() { panes.removeAll() }
}
