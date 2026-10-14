import AuthenticationServices
import SwiftUI

/// The amux Cloud part of the server picker: sign in, list your servers, open one,
/// sign out, delete the account. It has no prices and no way to buy anything
/// (App Store guideline 3.1.3(f)); people who have no server are told so.
struct CloudSignInSection: View {
    @EnvironmentObject var account: CloudAccount
    @EnvironmentObject var serverManager: ServerManager
    @State private var confirmDelete = false
    @State private var opening: String?

    var body: some View {
        VStack(spacing: 12) {
            if account.signedIn {
                signedIn
            } else {
                signedOut
            }
            if let message = account.message {
                Text(message)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .accessibilityIdentifier("cloud-message")
            }
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 24)
        .confirmationDialog("Delete your amux Cloud account?", isPresented: $confirmDelete, titleVisibility: .visible) {
            Button("Delete account and servers", role: .destructive) { Task { _ = await account.deleteAccount() } }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Your servers, their backups and your sign-in are removed. This cannot be undone.")
        }
    }

    private var signedOut: some View {
        VStack(spacing: 10) {
            Text("amux Cloud")
                .font(.headline)
            SignInWithAppleButton(.signIn) { req in
                req.requestedScopes = [.email]
            } onCompletion: { result in
                Task { await account.handleApple(result) }
            }
            .signInWithAppleButtonStyle(.white)
            .frame(height: 48)
            .accessibilityIdentifier("sign-in-with-apple")
            Button(action: { account.signInOnWeb() }) {
                Text(account.linkURL == nil ? "Sign in with email, Google or GitHub" : "Sign in with email to link your Apple ID")
                    .font(.headline)
                    .frame(maxWidth: .infinity, minHeight: 48)
                    .background(Color(uiColor: .secondarySystemGroupedBackground))
                    .foregroundColor(.primary)
                    .clipShape(RoundedRectangle(cornerRadius: 12))
            }
            .accessibilityIdentifier("sign-in-on-web")
        }
    }

    private var signedIn: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text(account.email ?? "amux Cloud").font(.headline)
                Spacer()
                Button("Sign out") { account.signOut() }.font(.footnote)
            }
            if account.servers.isEmpty {
                Text("No amux Cloud servers on this account.")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            ForEach(account.servers) { server in
                Button(action: { open(server) }) {
                    HStack {
                        Image(systemName: "server.rack")
                        VStack(alignment: .leading) {
                            Text(server.slug).font(.headline)
                            Text(server.status == "ready" ? (server.role == "member" ? "Shared with you" : "Your server")
                                 : "Being set up").font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        if opening == server.id { ProgressView() } else { Image(systemName: "chevron.right") }
                    }
                    .padding(12)
                    .frame(minHeight: 48)
                    .background(Color(uiColor: .secondarySystemGroupedBackground))
                    .clipShape(RoundedRectangle(cornerRadius: 12))
                }
                .disabled(server.status != "ready" || opening != nil)
                .accessibilityIdentifier("cloud-server-\(server.slug)")
            }
            Button("Delete account", role: .destructive) { confirmDelete = true }
                .font(.footnote)
                .accessibilityIdentifier("delete-account")
        }
        .task { await account.refresh() }
    }

    private func open(_ server: CloudServer) {
        opening = server.id
        Task {
            defer { opening = nil }
            do {
                let entry = try await account.entryURL(for: server)
                _ = serverManager.addServer(name: server.slug, urlString: server.url)
                serverManager.openCloudServer(base: server.url, entry: entry)
            } catch {
                account.message = "Could not open \(server.slug). Pull to refresh and try again."
            }
        }
    }
}
