import SwiftUI
import Combine
import os.log

private let logger = Logger(subsystem: "io.amux.app", category: "ServerManager")

class ServerManager: ObservableObject {
    /// One loaded dashboard per recently used server (ServerPanes.swift). The first is on screen.
    @Published private(set) var panes = ServerPanes()
    @Published var hasServer: Bool = false
    /// The page the server on screen was asked to load.
    var serverURL: URL? { panes.active?.url }
    @Published var serverStatus: [String: ServerStatus] = [:]  // url -> status

    @Published var savedServers: [SavedServer] {
        didSet {
            if let data = try? JSONEncoder().encode(savedServers) {
                AmuxStore.defaults.set(data, forKey: AmuxStore.savedServersKey)
            }
            refreshQuickActions()
        }
    }
    private var quickActionObserver: NSObjectProtocol?

    init() {
        AmuxStore.migrateFromStandardIfNeeded()
        if let data = AmuxStore.defaults.data(forKey: AmuxStore.savedServersKey),
           let servers = try? JSONDecoder().decode([SavedServer].self, from: data) {
            self.savedServers = servers
        } else {
            self.savedServers = []
        }

        if let urlString = AmuxStore.defaults.string(forKey: AmuxStore.serverURLKey),
           let url = URL(string: urlString) {
            self.panes.show(url, navigate: false)
            self.hasServer = true
        }
        quickActionObserver = NotificationCenter.default.addObserver(forName: QuickActions.picked, object: nil,
                                                                     queue: .main) { [weak self] _ in
            self?.takeQuickAction()
        }
        refreshQuickActions()
    }

    /// Opens the server picked from the Home Screen quick actions (QuickActions.swift), if any.
    func takeQuickAction() {
        guard let url = QuickActions.takePending() else { return }
        selectServer(url)
    }

    private func refreshQuickActions() {
        UIApplication.shared.shortcutItems = QuickActions.items(servers: savedServers,
                                                                recent: panes.panes.map(\.key))
    }

    func selectServer(_ urlString: String) {
        guard let url = URL(string: urlString) else { return }
        let (verdict, closed) = panes.show(url, navigate: false)
        guard verdict != .refused else {
            logger.warning("ios_server_switch verdict=refused url=\(urlString, privacy: .public)")
            return
        }
        hasServer = true
        AmuxStore.defaults.set(url.absoluteString, forKey: AmuxStore.serverURLKey)
        refreshQuickActions()
        logger.info("ios_server_switch verdict=\(verdict.rawValue, privacy: .public) host=\(url.host ?? "-", privacy: .public) open=\(self.panes.panes.count) closed=\(closed ?? "-", privacy: .public)")
    }

    /// Loads `url` in its server's pane (a deep link, a sign-in link), opening the pane if needed.
    private func navigate(_ url: URL) {
        let (verdict, _) = panes.show(url, navigate: true)
        logger.info("ios_server_switch verdict=\(verdict.rawValue, privacy: .public) host=\(url.host ?? "-", privacy: .public) link=1")
    }

    /// Opens a server through a one-time sign-in link (Sign in with Apple, via cloud.amux.io).
    /// The server's address is what is remembered; the link only loads once, and the door's
    /// cookie keeps the person signed in after that.
    func openServer(_ urlString: String, signInLink: URL?) {
        selectServer(urlString)
        if let signInLink { navigate(signInLink) }
    }

    /// Opens a copied file link (DeepLink.swift) on its server. An `amux://` link
    /// can come from any web page, so it opens only a server already saved or in
    /// use. An https link reached the app only because its domain's
    /// apple-app-site-association names this app (an amux server), so its server
    /// is opened, and saved if new; the server still asks for its own sign-in.
    @discardableResult
    func open(deepLink url: URL) -> Bool {
        guard let link = DeepLink.parse(url) else {
            logger.info("deep_link verdict=not_a_link")
            return false
        }
        let origin = link.server.absoluteString
        let known = savedServers.contains { DeepLink.origin(of: URL(string: $0.url) ?? link.server)?.absoluteString == origin }
            || serverURL.flatMap { DeepLink.origin(of: $0) }?.absoluteString == origin
        let universal = url.scheme?.lowercased() == "https"
        guard known || universal else {
            logger.info("deep_link verdict=unknown_server_refused host=\(link.server.host ?? "-", privacy: .public)")
            return false
        }
        if !known { _ = addServer(name: link.server.host ?? origin, urlString: origin) }
        selectServer(origin)
        navigate(link.load)
        logger.info("deep_link verdict=opened host=\(link.server.host ?? "-", privacy: .public)")
        return true
    }

    func addServer(name: String, urlString: String) -> Bool {
        guard let _ = URL(string: urlString), urlString.hasPrefix("http") else { return false }
        let normalized = urlString.hasSuffix("/") ? String(urlString.dropLast()) : urlString
        if !savedServers.contains(where: { $0.url == normalized }) {
            savedServers.append(SavedServer(name: name, url: normalized))
            logger.info("Added server: \(name) at \(normalized)")
        }
        return true
    }

    func removeServer(at offsets: IndexSet) {
        for i in offsets {
            logger.info("Removed server: \(self.savedServers[i].name)")
            if let url = URL(string: savedServers[i].url), let key = ServerPanes.key(of: url) {
                panes.close(key)
            }
        }
        self.savedServers.remove(atOffsets: offsets)
        // The server on screen was removed: show the next open one, or go back to setup.
        if let next = panes.active {
            AmuxStore.defaults.set(DashboardPage.root(of: next.url).absoluteString, forKey: AmuxStore.serverURLKey)
        } else if hasServer {
            resetServer()
        }
    }

    /// Whether `server` (a saved server's address) is the one on screen.
    func isActive(_ server: SavedServer) -> Bool {
        guard let url = URL(string: server.url) else { return false }
        return ServerPanes.key(of: url) != nil && ServerPanes.key(of: url) == panes.active?.key
    }

    func resetServer() {
        panes.closeAll()
        hasServer = false
        AmuxStore.defaults.removeObject(forKey: AmuxStore.serverURLKey)
        logger.info("Reset to server picker")
    }

    /// Ping all saved servers to check connectivity
    func checkAllServers() {
        for server in savedServers {
            checkServer(server.url)
        }
    }

    func checkServer(_ urlString: String) {
        guard let url = URL(string: urlString + "/api/release-notes") else { return }
        if serverStatus[urlString] == nil { serverStatus[urlString] = .checking }
        var req = URLRequest(url: url, timeoutInterval: 5)
        req.httpMethod = "GET"
        let session = URLSession(configuration: .ephemeral, delegate: TrustAllDelegate(), delegateQueue: nil)
        session.dataTask(with: req) { [weak self] _, response, error in
            let status: ServerStatus
            if let http = response as? HTTPURLResponse, http.statusCode < 500 {
                status = .online
            } else if error != nil {
                status = .offline
            } else {
                status = .offline
            }
            DispatchQueue.main.async {
                self?.serverStatus[urlString] = status
            }
            logger.debug("Server \(urlString) → \(status == .online ? "online" : "offline")")
        }.resume()
    }
}

enum ServerStatus {
    case online, offline, checking
}

/// Allows self-signed certs for health checks (Tailscale/local servers)
private class TrustAllDelegate: NSObject, URLSessionDelegate {
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                    completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        if let trust = challenge.protectionSpace.serverTrust {
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            completionHandler(.performDefaultHandling, nil)
        }
    }
}

struct SavedServer: Codable, Identifiable {
    var id: String { url }
    let name: String
    let url: String

    var serverType: ServerType {
        if url.contains("cloud.amux.io") { return .cloud }
        if url.contains(".ts.net") { return .tailscale }
        return .custom
    }
}

enum ServerType {
    case cloud, tailscale, custom

    var icon: String {
        switch self {
        case .cloud: return "cloud.fill"
        case .tailscale: return "network"
        case .custom: return "server.rack"
        }
    }

    var label: String {
        switch self {
        case .cloud: return "Cloud"
        case .tailscale: return "Tailscale"
        case .custom: return "Self-hosted"
        }
    }
}
