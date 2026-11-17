import UIKit
import os.log

private let logger = Logger(subsystem: "io.amux.app", category: "QuickActions")

/// Home Screen quick actions: press and hold the amux icon and pick a server to open it.
///
/// The items are the saved servers, the most recently used first, rebuilt whenever the list
/// or the server on screen changes. A pick is handed to ServerManager, which switches to it
/// like a tap in Settings does.
enum QuickActions {
    static let type = "io.amux.app.server"
    static let picked = Notification.Name("amux.quickAction.server")
    /// iOS shows four app-defined items.
    static let limit = 4

    /// A pick made before the app's state existed (a cold launch), taken on the next look.
    private static var pending: String?

    static func items(servers: [SavedServer], recent: [String]) -> [UIApplicationShortcutItem] {
        let rank = { (s: SavedServer) -> Int in
            let key = URL(string: s.url).flatMap(ServerPanes.key(of:))
            return key.flatMap { recent.firstIndex(of: $0) } ?? recent.count
        }
        let ordered = servers.enumerated().sorted { a, b in
            (rank(a.element), a.offset) < (rank(b.element), b.offset)
        }.map(\.element)
        return ordered.prefix(limit).map { s in
            UIApplicationShortcutItem(type: type, localizedTitle: s.name,
                                      localizedSubtitle: URL(string: s.url)?.host,
                                      icon: UIApplicationShortcutIcon(systemImageName: s.serverType.icon),
                                      userInfo: ["url": s.url as NSString])
        }
    }

    /// Returns true when the item was a server and was handed on.
    @discardableResult
    static func handle(_ item: UIApplicationShortcutItem) -> Bool {
        guard item.type == type, let url = item.userInfo?["url"] as? String else { return false }
        logger.info("ios_quick_action verdict=picked host=\(URL(string: url)?.host ?? "-", privacy: .public)")
        pending = url
        NotificationCenter.default.post(name: picked, object: nil)
        return true
    }

    static func takePending() -> String? {
        defer { pending = nil }
        return pending
    }
}

final class AmuxAppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication, configurationForConnecting session: UISceneSession,
                     options: UIScene.ConnectionOptions) -> UISceneConfiguration {
        if let item = options.shortcutItem { QuickActions.handle(item) }
        let config = UISceneConfiguration(name: nil, sessionRole: session.role)
        config.delegateClass = AmuxSceneDelegate.self
        return config
    }
}

final class AmuxSceneDelegate: NSObject, UIWindowSceneDelegate {
    func windowScene(_ windowScene: UIWindowScene, performActionFor shortcutItem: UIApplicationShortcutItem,
                     completionHandler: @escaping (Bool) -> Void) {
        completionHandler(QuickActions.handle(shortcutItem))
    }
}
