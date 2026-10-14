import SwiftUI

@main
struct AmuxApp: App {
    @StateObject private var serverManager = ServerManager()
    @StateObject private var cloud = CloudAccount()
    @Environment(\.scenePhase) private var scenePhase

    init() {
        LocationRecorder.shared.resumeIfEnabled()
    }

    var body: some Scene {
        WindowGroup {
            Group {
                if serverManager.hasServer {
                    ContentView()
                        .environmentObject(serverManager)
                        .environmentObject(cloud)
                } else {
                    ServerPickerView()
                        .environmentObject(serverManager)
                        .environmentObject(cloud)
                }
            }
            .preferredColorScheme(.dark)
            // A saved amux Cloud server reopens through a fresh door link: the door
            // cookie lasts 12 hours, the app's sign-in lasts 30 days.
            .task {
                guard cloud.signedIn, let base = serverManager.savedCloudServer else { return }
                await cloud.refresh()
                if let server = cloud.servers.first(where: { $0.url == base }),
                   let entry = try? await cloud.entryURL(for: server) {
                    serverManager.openCloudServer(base: base, entry: entry)
                }
            }
        }
        // Shares queued while offline (ShareOutbox) go out when the app opens
        // or comes back to the foreground, as well as on the next share.
        .onChange(of: scenePhase) { phase in
            if phase == .active { Task { await ShareOutbox.drainShared() } }
        }
    }
}
