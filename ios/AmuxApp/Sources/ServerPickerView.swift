import SwiftUI
import AuthenticationServices

/// Where the cloud button sends people. One constant, because the app, the
/// site and the Cloudflare redirect all have to agree on it.
enum AmuxCloud {
    static let onboardingURL = URL(string: "https://amux.io/cloud/")!
    /// Sign in with Apple: cloud.amux.io checks Apple's token and answers with the account's
    /// servers, each with a two-minute sign-in link (cloud/start/worker.mjs, /api/apple/servers).
    static let appleServersURL = URL(string: "https://cloud.amux.io/api/apple/servers")!

    struct Server: Decodable { let name: String; let url: String; let login_url: String? }
    struct Answer: Decodable { let servers: [Server]?; let error: String? }

    static func servers(identityToken: String) async throws -> [Server] {
        var req = URLRequest(url: appleServersURL)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONEncoder().encode(["identity_token": identityToken])
        let (data, resp) = try await URLSession.shared.data(for: req)
        let answer = try? JSONDecoder().decode(Answer.self, from: data)
        if let servers = answer?.servers, !servers.isEmpty { return servers }
        let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
        throw NSError(domain: "amux.cloud", code: code, userInfo: [NSLocalizedDescriptionKey:
            answer?.error ?? "amux Cloud did not answer (HTTP \(code)). Try again in a minute."])
    }
}

struct ServerPickerView: View {
    @Environment(\.openURL) private var openURL
    @EnvironmentObject var serverManager: ServerManager
    @State private var customURL = ""
    @State private var customName = ""
    @State private var urlError = false
    @State private var appleBusy = false
    @State private var appleError: String?

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                // Header
                VStack(spacing: 12) {
                    Image(systemName: "square.stack.3d.up.fill")
                        .font(.system(size: 56))
                        .foregroundColor(.accentColor)
                        .padding(.top, 48)
                    Text("amux")
                        .font(.largeTitle.bold())
                    Text("Connect to your amux server")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                .padding(.bottom, 36)

                // Cloud option.
                //
                // This used to add https://cloud.amux.io as a server and select
                // it, which sent a first-run user straight into the black
                // screen: that origin has been unreachable for days and the
                // picker has no way to know. Onboarding for cloud now goes
                // through the web page, where a human schedules it.
                //
                // OPENS EXTERNALLY rather than loading in the app's WebView,
                // because the destination is a marketing page and Calendly, not
                // an amux server. Loading it in the dashboard WebView would
                // leave the app pointed at something it cannot talk to.
                VStack(spacing: 12) {
                    // Existing amux Cloud customers: Sign in with Apple finds their server and
                    // signs them in. The Apple ID's email must be the one they signed up with
                    // (once linked, a private relay address works too).
                    SignInWithAppleButton(.continue, onRequest: { $0.requestedScopes = [.email] },
                                          onCompletion: signedInWithApple)
                        .signInWithAppleButtonStyle(.black)
                        .frame(height: 50)
                        .clipShape(RoundedRectangle(cornerRadius: 12))
                        .disabled(appleBusy)
                        .overlay { if appleBusy { ProgressView() } }

                    if let appleError {
                        Text(appleError)
                            .font(.caption)
                            .foregroundColor(.red)
                            .multilineTextAlignment(.center)
                    }

                    Button(action: { openURL(AmuxCloud.onboardingURL) }) {
                        HStack {
                            Image(systemName: "cloud.fill")
                            Text("Get amux cloud")
                                .font(.headline)
                            Image(systemName: "arrow.up.right")
                                .font(.footnote.weight(.semibold))
                        }
                        .frame(maxWidth: .infinity)
                        .padding(14)
                        .background(Color.accentColor)
                        .foregroundColor(.white)
                        .clipShape(RoundedRectangle(cornerRadius: 12))
                    }

                    // The old caption said "Includes Sign in with Apple", which
                    // described the sign-in this button no longer performs.
                    Text("New to amux Cloud? amux.io sets up your server")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)

                // Divider
                HStack {
                    Rectangle().frame(height: 1).foregroundStyle(.quaternary)
                    Text("or self-hosted")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                        .layoutPriority(1)
                    Rectangle().frame(height: 1).foregroundStyle(.quaternary)
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)

                // Server URL form
                VStack(spacing: 16) {
                    VStack(alignment: .leading, spacing: 8) {
                        TextField("Name (optional)", text: $customName)
                            .padding(12)
                            .background(Color(uiColor: .secondarySystemGroupedBackground))
                            .clipShape(RoundedRectangle(cornerRadius: 10))

                        TextField("https://amux.tail-xxxx.ts.net:8824", text: $customURL)
                            .keyboardType(.URL)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .padding(12)
                            .background(Color(uiColor: .secondarySystemGroupedBackground))
                            .clipShape(RoundedRectangle(cornerRadius: 10))

                        Text("Find your Tailscale hostname in the Tailscale app. Port is 8824 by default.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 4)
                    }

                    if urlError {
                        Text("Please enter a valid URL starting with http:// or https://")
                            .foregroundColor(.red)
                            .font(.caption)
                    }

                    Button(action: {
                        urlError = false
                        let name = customName.isEmpty ? customURL : customName
                        if serverManager.addServer(name: name, urlString: customURL) {
                            serverManager.selectServer(customURL)
                        } else {
                            urlError = true
                        }
                    }) {
                        Text("Connect")
                            .font(.headline)
                            .frame(maxWidth: .infinity)
                            .padding(14)
                            .background(Color(uiColor: .secondarySystemGroupedBackground))
                            .foregroundColor(.primary)
                            .clipShape(RoundedRectangle(cornerRadius: 12))
                    }
                    .disabled(customURL.isEmpty)
                }
                .padding(.horizontal, 20)

                Spacer()
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(uiColor: .systemBackground))
        }
    }

    private func signedInWithApple(_ result: Result<ASAuthorization, Error>) {
        appleError = nil
        switch result {
        case .failure(let e):
            if (e as? ASAuthorizationError)?.code != .canceled { appleError = e.localizedDescription }
        case .success(let auth):
            guard let cred = auth.credential as? ASAuthorizationAppleIDCredential,
                  let data = cred.identityToken, let token = String(data: data, encoding: .utf8) else {
                appleError = "Apple did not return a sign-in token. Try again."
                return
            }
            appleBusy = true
            Task { @MainActor in
                defer { appleBusy = false }
                do {
                    let servers = try await AmuxCloud.servers(identityToken: token)
                    for s in servers { _ = serverManager.addServer(name: s.name, urlString: s.url) }
                    let first = servers[0]
                    serverManager.openServer(first.url, signInLink: first.login_url.flatMap(URL.init(string:)))
                } catch {
                    appleError = error.localizedDescription
                }
            }
        }
    }
}
