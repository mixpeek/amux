import AuthenticationServices
import Foundation
import Security
import SwiftUI
import UIKit
import os.log

private let logger = Logger(subsystem: "io.amux.app", category: "CloudAccount")

/// Signs in to amux Cloud and opens the servers the person already has
/// (docs/design/cloud-self-service.md 8.3). The app sells nothing and never
/// creates an account: Sign in with Apple finds an account linked to that Apple
/// ID, and an Apple ID nobody has linked is sent through web sign-in once.
struct CloudServer: Codable, Identifiable, Equatable {
    let id: String
    let slug: String
    let url: String
    let status: String
    let plan: String
    let role: String?
}

@MainActor
final class CloudAccount: NSObject, ObservableObject {
    static let base = URL(string: "https://cloud.amux.io")!
    private static let tokenKey = "io.amux.cloud.token"

    @Published private(set) var servers: [CloudServer] = []
    @Published private(set) var email: String?
    @Published var message: String?
    @Published private(set) var linkURL: URL?
    @Published private(set) var busy = false

    var signedIn: Bool { Keychain.read(Self.tokenKey) != nil }

    override init() {
        super.init()
        if signedIn { Task { await refresh() } }
    }

    // MARK: Sign in with Apple

    func handleApple(_ result: Result<ASAuthorization, Error>) async {
        message = nil
        guard case .success(let auth) = result,
              let cred = auth.credential as? ASAuthorizationAppleIDCredential,
              let idData = cred.identityToken, let idToken = String(data: idData, encoding: .utf8) else {
            if case .failure(let e) = result, (e as? ASAuthorizationError)?.code == .canceled { return }
            message = "Sign in with Apple did not finish."
            return
        }
        let code = cred.authorizationCode.flatMap { String(data: $0, encoding: .utf8) }
        var body: [String: String] = ["identity_token": idToken]
        if let code { body["authorization_code"] = code }
        busy = true
        defer { busy = false }
        do {
            let (data, status) = try await request("POST", "/api/apple/signin", json: body, authorized: false)
            let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            if status == 200, let token = obj["token"] as? String {
                Keychain.write(Self.tokenKey, token)
                linkURL = nil
                await refresh()
            } else if status == 404, obj["link_required"] as? Bool == true {
                message = obj["message"] as? String ?? "No amux Cloud server for this Apple ID."
                linkURL = (obj["link_url"] as? String).flatMap(URL.init(string:))
            } else {
                message = "Sign in with Apple could not be verified."
            }
        } catch {
            message = "Could not reach amux Cloud: \(error.localizedDescription)"
        }
    }

    // MARK: Web sign-in (email, Google, GitHub), which also links an Apple ID

    private var webSession: ASWebAuthenticationSession?

    func signInOnWeb() {
        message = nil
        let start = linkURL ?? URL(string: "\(Self.base.absoluteString)/signin?from=ios")!
        let session = ASWebAuthenticationSession(url: start, callbackURLScheme: "amux") { [weak self] url, error in
            Task { @MainActor in
                guard let self else { return }
                if let error, (error as? ASWebAuthenticationSessionError)?.code == .canceledLogin { return }
                guard let url, url.host == "signed-in",
                      let token = URLComponents(url: url, resolvingAgainstBaseURL: false)?
                        .queryItems?.first(where: { $0.name == "token" })?.value else {
                    self.message = "Sign-in did not finish."
                    return
                }
                Keychain.write(Self.tokenKey, token)
                self.linkURL = nil
                await self.refresh()
            }
        }
        session.presentationContextProvider = self
        session.prefersEphemeralWebBrowserSession = false
        webSession = session
        session.start()
    }

    // MARK: Servers

    func refresh() async {
        guard signedIn else { servers = []; email = nil; return }
        do {
            let (data, status) = try await request("GET", "/api/me")
            if status == 401 { signOut(); message = "Your sign-in expired. Please sign in again."; return }
            let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            email = obj["email"] as? String
            let list = (obj["servers"] as? [[String: Any]]) ?? []
            let json = try JSONSerialization.data(withJSONObject: list)
            servers = (try? JSONDecoder().decode([CloudServer].self, from: json)) ?? []
        } catch {
            message = "Could not reach amux Cloud: \(error.localizedDescription)"
        }
    }

    /// A one-minute sign-in link for the server's door, fetched with the app's token.
    func entryURL(for server: CloudServer) async throws -> URL {
        let (data, status) = try await request("GET", "/api/go-url/\(server.id)")
        guard status == 200,
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let s = obj["url"] as? String, let url = URL(string: s), url.scheme == "https" else {
            throw URLError(.userAuthenticationRequired)
        }
        return url
    }

    func deleteAccount() async -> Bool {
        do {
            let (_, status) = try await request("POST", "/api/account/delete", json: ["confirm": "delete"])
            if status == 202 {
                signOut()
                message = "Your account is being deleted. A receipt will be emailed when it finishes."
                return true
            }
            message = "Deletion did not start (HTTP \(status))."
        } catch {
            message = "Could not reach amux Cloud: \(error.localizedDescription)"
        }
        return false
    }

    func signOut() {
        Keychain.delete(Self.tokenKey)
        servers = []
        email = nil
    }

    private func request(_ method: String, _ path: String, json: [String: String]? = nil,
                         authorized: Bool = true) async throws -> (Data, Int) {
        var req = URLRequest(url: Self.base.appendingPathComponent(String(path.dropFirst())), timeoutInterval: 20)
        req.httpMethod = method
        if let json {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try JSONSerialization.data(withJSONObject: json)
        }
        if authorized, let token = Keychain.read(Self.tokenKey) {
            req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }
        let (data, resp) = try await URLSession.shared.data(for: req)
        let status = (resp as? HTTPURLResponse)?.statusCode ?? 0
        logger.info("\(method) \(path) -> \(status)")
        return (data, status)
    }
}

extension CloudAccount: ASWebAuthenticationPresentationContextProviding {
    nonisolated func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        MainActor.assumeIsolated {
            UIApplication.shared.connectedScenes
                .compactMap { $0 as? UIWindowScene }
                .flatMap { $0.windows }
                .first { $0.isKeyWindow } ?? ASPresentationAnchor()
        }
    }
}

/// The app token lives in the Keychain, never in UserDefaults.
enum Keychain {
    static func read(_ key: String) -> String? {
        let q: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrAccount as String: key,
                                kSecReturnData as String: true, kSecMatchLimit as String: kSecMatchLimitOne]
        var out: AnyObject?
        guard SecItemCopyMatching(q as CFDictionary, &out) == errSecSuccess, let d = out as? Data else { return nil }
        return String(data: d, encoding: .utf8)
    }

    static func write(_ key: String, _ value: String) {
        delete(key)
        let q: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrAccount as String: key,
                                kSecValueData as String: Data(value.utf8),
                                kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly]
        SecItemAdd(q as CFDictionary, nil)
    }

    static func delete(_ key: String) {
        SecItemDelete([kSecClass as String: kSecClassGenericPassword, kSecAttrAccount as String: key] as CFDictionary)
    }
}
