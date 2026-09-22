import SwiftUI
import Combine
import os.log

private let logger = Logger(subsystem: "io.amux.app", category: "ServerManager")

class ServerManager: ObservableObject {
    @Published var serverURL: URL?
    @Published var hasServer: Bool = false
    @Published var serverStatus: [String: ServerStatus] = [:]  // url -> status

    @Published var savedServers: [SavedServer] {
        didSet {
            if let data = try? JSONEncoder().encode(savedServers) {
                AmuxStore.defaults.set(data, forKey: AmuxStore.savedServersKey)
            }
        }
    }

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
            self.serverURL = url
            self.hasServer = true
        }
    }

    func selectServer(_ urlString: String) {
        guard let url = URL(string: urlString) else { return }
        serverURL = url
        hasServer = true
        AmuxStore.defaults.set(url.absoluteString, forKey: AmuxStore.serverURLKey)
        logger.info("Switched to server: \(urlString)")
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
        }
        self.savedServers.remove(atOffsets: offsets)
    }

    func resetServer() {
        serverURL = nil
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
