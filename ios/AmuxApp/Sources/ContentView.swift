import SwiftUI
import WebKit

struct ContentView: View {
    @EnvironmentObject var serverManager: ServerManager
    @State private var showSettings = false

    var body: some View {
        // Every open server keeps its web view (ServerPanes.swift); only the first is shown.
        // The others stay loaded underneath, hidden from touch and VoiceOver, so switching
        // back to one is instant and keeps the view the owner left from.
        ZStack {
            Color(red: 0.051, green: 0.067, blue: 0.09).ignoresSafeArea()
            ForEach(serverManager.panes.panes) { pane in
                let active = pane.key == serverManager.panes.active?.key
                ServerPaneView(pane: pane, isActive: active, showSettings: $showSettings)
                    .opacity(active ? 1 : 0)
                    .allowsHitTesting(active)
                    .accessibilityHidden(!active)
                    .zIndex(active ? 1 : 0)
            }
        }
        .animation(.easeInOut(duration: 0.2), value: serverManager.panes.active?.key)
        // The Settings long-press lives on the web view as a UIKit recognizer
        // (WebView.makeUIView). A SwiftUI LongPressGesture here sat over the
        // whole WKWebView and, on iPad, swallowed the page's taps: "I cant
        // navigate the amux app on my ipad ... nothing clicks" (MSG-90700).
        .sheet(isPresented: $showSettings) {
            SettingsView()
                .environmentObject(serverManager)
                .presentationDetents([.medium, .large])
                .presentationDragIndicator(.visible)
        }
    }
}

/// One server's dashboard and the native controls around it: the offline strip, the way
/// back from a page that is not the dashboard, and the error screen.
struct ServerPaneView: View {
    let pane: ServerPanes.Pane
    let isActive: Bool
    @Binding var showSettings: Bool
    @State private var isLoading = false
    @State private var canGoBack = false
    @State private var canGoForward = false
    @State private var offDashboard = false
    @State private var webViewRef: WKWebView?
    @State private var loadError: String?
    @State private var offline: OfflineCopy.Badge?
    @State private var webViewId = UUID()  // force recreation on retry

    var body: some View {
        ZStack(alignment: .top) {
            // Offline, a strip above the page says so and keeps Retry and
            // Switch Server one tap away. It sits ABOVE the web view rather
            // than over it: floated over the page it covered the dashboard's
            // own header buttons (measured on an iPhone 17 Pro simulator).
            VStack(spacing: 0) {
            if let badge = offline, loadError == nil {
                offlineStrip(badge)
            }
            WebView(
                    url: pane.url,
                    isActive: isActive,
                    isLoading: $isLoading,
                    canGoBack: $canGoBack,
                    canGoForward: $canGoForward,
                    loadError: $loadError,
                    offDashboard: $offDashboard,
                    offline: $offline,
                    onWebView: { webViewRef = $0 },
                    onNavigationAction: handleNavigation,
                    onLongPress: { showSettings = true },
                    onAppBridge: { _ in showSettings = true }
                )
                .id(webViewId)
                // With the strip shown it owns the status-bar area, so the
                // page starts below it and its safe-area padding drops to 0.
                .ignoresSafeArea(edges: (offline != nil && loadError == nil) ? .bottom : .all)
            }

            // A page opened from the dashboard (a public app, a shared file,
            // another site) replaces it in this web view and has no amux
            // controls of its own. A password-protected app left the owner
            // with no way out but quitting (2026-11-05), so any page that is
            // not the dashboard gets a way back to it.
            if offDashboard && loadError == nil {
                HStack {
                    Button(action: backToDashboard) {
                        Label("amux", systemImage: "chevron.left")
                            .font(.subheadline.weight(.semibold))
                            .padding(.horizontal, 12)
                            .frame(minHeight: 44)
                            .background(.regularMaterial, in: Capsule())
                    }
                    .accessibilityLabel("Back to amux")
                    .accessibilityIdentifier("back-to-dashboard")
                    Spacer()
                }
                .padding(.horizontal, 12)
                .padding(.top, 4)
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
                    Text(pane.url.absoluteString)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                    HStack(spacing: 16) {
                        Button("Retry") {
                            loadError = nil
                            offline = nil
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
    }

    private func offlineStrip(_ badge: OfflineCopy.Badge) -> some View {
        Menu {
            Button {
                // A reload goes to the server; if it still cannot reach it,
                // the page stays as it is (OfflineCopy.Decision.keepPage).
                webViewRef?.reload()
            } label: {
                Label("Retry", systemImage: "arrow.clockwise")
            }
            Button {
                showSettings = true
            } label: {
                Label("Switch Server", systemImage: "server.rack")
            }
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "wifi.slash")
                    .foregroundStyle(Color.orange)
                Text(OfflineCopy.label(savedAt: badge.savedAt))
                    .foregroundStyle(Color.white)
                    .lineLimit(1)
                Spacer(minLength: 8)
                Text("Retry")
                    .foregroundStyle(Color.accentColor)
                Image(systemName: "chevron.down")
                    .font(.caption2.weight(.bold))
                    .foregroundStyle(Color.accentColor)
            }
            .font(.footnote.weight(.semibold))
            .foregroundStyle(.primary)
            .padding(.horizontal, 16)
            .frame(maxWidth: .infinity, minHeight: 44)
            .contentShape(Rectangle())
        }
        .background(Color.orange.opacity(0.18).ignoresSafeArea(edges: .top))
        .background(Color(red: 0.051, green: 0.067, blue: 0.09).ignoresSafeArea(edges: .top))
        .accessibilityLabel(OfflineCopy.label(savedAt: badge.savedAt) + ". Retry or switch server")
        .accessibilityIdentifier("offline-badge")
    }

    /// Back to the newest dashboard page in this web view's history, which
    /// keeps the view the owner left from. A page opened from a deep link has
    /// none behind it, so that loads the dashboard instead.
    private func backToDashboard() {
        guard let web = webViewRef else { return }
        let server = pane.url
        if let item = web.backForwardList.backList.last(where: { DashboardPage.isDashboard($0.url, server: server) }) {
            web.go(to: item)
        } else {
            web.load(URLRequest(url: DashboardPage.root(of: server)))
        }
    }

    private func handleNavigation(_ action: WKNavigationAction) -> WKNavigationActionPolicy {
        guard let url = action.request.url,
              let serverHost = pane.url.host else {
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
                    // A server just added is the one wanted: it opens, and the sheet closes.
                    AddServerView(onAdd: { name, url in
                        guard serverManager.addServer(name: name, urlString: url) else { return false }
                        showAddServer = false
                        switchTo(url)
                        return true
                    })
                }
        }
    }

    private var settingsList: some View {
        List {
            serversSection
            addSection
            LocationSettingsSection()
            resetSection
        }
        .onAppear { serverManager.checkAllServers() }
    }

    private var serversSection: some View {
        Section("Servers") {
            ForEach(serverManager.savedServers) { server in
                serverRow(server)
                    .swipeActions {
                        Button(role: .destructive) { remove(server) } label: {
                            Label("Remove", systemImage: "trash")
                        }
                    }
                    .contextMenu {
                        Button { UIPasteboard.general.string = server.url } label: {
                            Label("Copy Address", systemImage: "doc.on.doc")
                        }
                        Button(role: .destructive) { remove(server) } label: {
                            Label("Remove", systemImage: "trash")
                        }
                    }
            }
        }
    }

    /// Switching is one tap: a tick under the finger, the sheet closes, and the server's page
    /// shows (at once when it is already open, ServerPanes.swift).
    private func switchTo(_ url: String) {
        UISelectionFeedbackGenerator().selectionChanged()
        serverManager.selectServer(url)
        dismiss()
    }

    private func remove(_ server: SavedServer) {
        guard let i = serverManager.savedServers.firstIndex(where: { $0.id == server.id }) else { return }
        serverManager.removeServer(at: IndexSet(integer: i))
    }

    private func statusColor(_ status: ServerStatus) -> Color {
        switch status {
        case .online: return .green
        case .offline: return .red
        case .checking: return .secondary
        }
    }

    private func serverRow(_ server: SavedServer) -> some View {
        let active = serverManager.isActive(server)
        return Button { if active { dismiss() } else { switchTo(server.url) } } label: {
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
                            .fill(statusColor(status))
                            .frame(width: 6, height: 6)
                            .accessibilityLabel(status == .online ? "online" : status == .offline ? "offline" : "checking")
                    }
                    Text(server.url.replacingOccurrences(of: "https://", with: "").replacingOccurrences(of: "http://", with: ""))
                        .font(.caption2)
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer()
            if active {
                Image(systemName: "checkmark.circle.fill").foregroundColor(.accentColor)
            }
        }
        .contentShape(Rectangle())
        }
        .foregroundStyle(.primary)
        .accessibilityAddTraits(active ? .isSelected : [])
        .accessibilityIdentifier("server-row")
    }

    private var addSection: some View {
        Section {
            Button { showAddServer = true } label: {
                Label("Add Server", systemImage: "plus")
            }
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
    /// Returns false when the address is not one the app can open.
    var onAdd: (String, String) -> Bool
    @State private var name = ""
    @State private var url = ""
    @State private var showError = false

    var body: some View {
        NavigationStack {
            Form {
                Section("Server Details") {
                    TextField("Name (e.g. Home Mac)", text: $name)
                    TextField("URL (e.g. https://amux.tail-xxxx.ts.net:8824)", text: $url)
                        .keyboardType(.URL)
                        .submitLabel(.go)
                        .onSubmit { if !url.isEmpty { showError = !onAdd(name.isEmpty ? url : name, url) } }
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
                        showError = !onAdd(name.isEmpty ? url : name, url)
                    }
                    .disabled(url.isEmpty)
                }
            }
        }
    }
}
