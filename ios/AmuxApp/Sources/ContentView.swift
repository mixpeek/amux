import SwiftUI
import WebKit

struct ContentView: View {
    @EnvironmentObject var serverManager: ServerManager
    @State private var isLoading = false
    @State private var canGoBack = false
    @State private var canGoForward = false
    @State private var showSettings = false
    @State private var webViewRef: WKWebView?
    @State private var loadError: String?
    @State private var webViewId = UUID()  // force recreation on retry

    var body: some View {
        ZStack(alignment: .top) {
            if let url = serverManager.serverURL {
                WebView(
                    url: url,
                    isLoading: $isLoading,
                    canGoBack: $canGoBack,
                    canGoForward: $canGoForward,
                    loadError: $loadError,
                    onNavigationAction: handleNavigation
                )
                .id(webViewId)
                .ignoresSafeArea()
            }

            if isLoading {
                ProgressView()
                    .progressViewStyle(.linear)
                    .frame(maxWidth: .infinity)
                    .tint(Color.accentColor)
            }

            // Error overlay — shown when navigation fails
            if let error = loadError {
                VStack(spacing: 20) {
                    Spacer()
                    Image(systemName: "wifi.exclamationmark")
                        .font(.system(size: 48))
                        .foregroundColor(.secondary)
                    Text("Can't reach server")
                        .font(.title3.bold())
                    Text(error)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .padding(.horizontal, 32)
                    if let url = serverManager.serverURL {
                        Text(url.absoluteString)
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                    HStack(spacing: 16) {
                        Button("Retry") {
                            loadError = nil
                            webViewId = UUID()
                        }
                        .buttonStyle(.borderedProminent)
                        Button("Switch Server") {
                            showSettings = true
                        }
                        .buttonStyle(.bordered)
                    }
                    .padding(.top, 8)
                    Spacer()
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color(uiColor: .systemBackground))
            }
        }
        .gesture(
            LongPressGesture(minimumDuration: 0.5)
                .onEnded { _ in showSettings = true }
        )
        .sheet(isPresented: $showSettings) {
            SettingsView()
                .environmentObject(serverManager)
        }
    }

    private func handleNavigation(_ action: WKNavigationAction) -> WKNavigationActionPolicy {
        guard let url = action.request.url,
              let serverHost = serverManager.serverURL?.host else {
            return .allow
        }
        if action.navigationType == .linkActivated && url.host != serverHost {
            UIApplication.shared.open(url)
            return .cancel
        }
        return .allow
    }
}

// MARK: - Settings Sheet

struct SettingsView: View {
    @EnvironmentObject var serverManager: ServerManager
    @Environment(\.dismiss) var dismiss
    @State private var showAddServer = false
    @State private var addError = false

    var body: some View {
        NavigationStack {
            settingsList
                .navigationTitle("Settings")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .navigationBarTrailing) {
                        Button("Done") { dismiss() }
                    }
                }
                .sheet(isPresented: $showAddServer) {
                    AddServerView(onAdd: { name, url in
                        if serverManager.addServer(name: name, urlString: url) {
                            showAddServer = false
                        } else {
                            addError = true
                        }
                    })
                }
        }
    }

    private var settingsList: some View {
        List {
            serversSection
            addSection
            resetSection
        }
        .onAppear { serverManager.checkAllServers() }
    }

    private var serversSection: some View {
        Section("Servers") {
            ForEach(serverManager.savedServers) { server in
                serverRow(server)
            }
            .onDelete(perform: serverManager.removeServer)
        }
    }

    private func serverRow(_ server: SavedServer) -> some View {
        HStack(spacing: 12) {
            Image(systemName: server.serverType.icon)
                .foregroundColor(.secondary)
                .frame(width: 24)
            VStack(alignment: .leading, spacing: 2) {
                Text(server.name).font(.headline)
                HStack(spacing: 6) {
                    Text(server.serverType.label)
                        .font(.caption2)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.secondary.opacity(0.15))
                        .clipShape(Capsule())
                    if let status = serverManager.serverStatus[server.url] {
                        Circle()
                            .fill(status == .online ? Color.green : Color.red)
                            .frame(width: 6, height: 6)
                    }
                    Text(server.url.replacingOccurrences(of: "https://", with: "").replacingOccurrences(of: "http://", with: ""))
                        .font(.caption2)
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer()
            if serverManager.serverURL?.absoluteString == server.url {
                Image(systemName: "checkmark.circle.fill").foregroundColor(.accentColor)
            }
        }
        .contentShape(Rectangle())
        .onTapGesture {
            serverManager.selectServer(server.url)
            dismiss()
        }
    }

    private var addSection: some View {
        Section {
            Button("Add Server") { showAddServer = true }
        }
    }

    private var resetSection: some View {
        Section {
            Button("Back to Setup", role: .destructive) {
                serverManager.resetServer()
                dismiss()
            }
        }
    }
}

// MARK: - Add Server Sheet

struct AddServerView: View {
    @Environment(\.dismiss) var dismiss
    var onAdd: (String, String) -> Void
    @State private var name = ""
    @State private var url = ""
    @State private var showError = false

    var body: some View {
        NavigationStack {
            Form {
                Section("Server Details") {
                    TextField("Name (e.g. Home Mac)", text: $name)
                    TextField("URL (e.g. https://amux.tail-xxxx.ts.net:8822)", text: $url)
                        .keyboardType(.URL)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                }
                if showError {
                    Section {
                        Text("Invalid URL. Must start with http:// or https://")
                            .foregroundColor(.red)
                            .font(.caption)
                    }
                }
            }
            .navigationTitle("Add Server")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .navigationBarLeading) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .navigationBarTrailing) {
                    Button("Add") {
                        showError = false
                        onAdd(name.isEmpty ? url : name, url)
                    }
                    .disabled(url.isEmpty)
                }
            }
        }
    }
}
