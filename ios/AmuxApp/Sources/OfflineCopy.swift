import Foundation

/// What the app does when the dashboard cannot be fetched.
///
/// Owner, 2026-11-12, on a plane in airplane mode: "ios offline needs to work
/// perfectly". The app showed its full-screen "Can't reach server" over a
/// dashboard it had loaded many times, because every failed navigation went
/// straight to that screen.
///
/// WHY NOT THE SERVICE WORKER. The dashboard's sw.js is what makes the PWA
/// open offline in Safari, and it never runs here: WKWebView only starts
/// service workers for App-Bound Domains (Info.plist WKAppBoundDomains), a
/// static list of at most 10 domains fixed at build time. The servers this app
/// opens are chosen by the person (a *.ts.net name, a LAN address, a cloud
/// host), so no build-time list can name them. Turning the list on would also
/// switch off script injection on every other domain, which the native
/// bridges depend on.
///
/// WHAT WORKS INSTEAD. WebKit keeps the dashboard and its scripts in its own
/// HTTP cache: the shell has no Cache-Control and the assets are `no-cache`
/// with an ETag, which WebKit stores (it revalidates them online). Reloading
/// with `.returnCacheDataElseLoad` serves the page from that cache, and WebKit
/// serves the page's scripts and styles from the cache too while that load is
/// in progress. The dashboard then boots from its own offline stores (the
/// sessions and board caches in localStorage, peeks and the outbox in
/// IndexedDB), all of which belong to the server's origin and are untouched by
/// how the HTML arrived. Its existing online/offline handling does the rest.
enum OfflineCopy {
    /// What the offline badge shows. `savedAt` is when the page on screen was
    /// last loaded from the server (nil: a live page that just lost its
    /// connection); `since` is when the app noticed.
    struct Badge: Equatable {
        let savedAt: Date?
        let since: Date
    }

    enum Decision: Equatable {
        /// The dashboard is already on screen: keep it, mark it offline.
        case keepPage
        /// Nothing usable on screen, and this device has opened the dashboard
        /// before: load it from WebKit's cache.
        case loadSavedCopy
        /// Show the native "Can't reach server" screen.
        case showError
    }

    /// Errors that mean "nothing answered from here", as opposed to a server
    /// that answered badly. Only these fall back to the saved copy; a TLS or
    /// HTTP problem keeps the error screen, which names it.
    static let unreachableCodes: Set<Int> = [
        NSURLErrorNotConnectedToInternet,  // airplane mode
        NSURLErrorDataNotAllowed,          // cellular off for the app
        NSURLErrorInternationalRoamingOff,
        NSURLErrorCannotConnectToHost,     // server stopped
        NSURLErrorCannotFindHost,          // Tailscale DNS gone
        NSURLErrorDNSLookupFailed,
        NSURLErrorNetworkConnectionLost,
        NSURLErrorTimedOut,
    ]

    static func isUnreachable(_ error: NSError) -> Bool {
        error.domain == NSURLErrorDomain && unreachableCodes.contains(error.code)
    }

    /// - Parameters:
    ///   - showingDashboard: the web view has a dashboard page committed (a
    ///     failed reload or pull-to-refresh leaves it on screen).
    ///   - savedAt: when this device last loaded the dashboard from the server;
    ///     nil means it never has, so there is nothing cached to show.
    ///   - triedSavedCopy: the navigation that failed was the cache load itself.
    static func decide(unreachable: Bool, showingDashboard: Bool, savedAt: Date?,
                       triedSavedCopy: Bool) -> Decision {
        guard unreachable else { return .showError }
        if showingDashboard { return .keepPage }
        if savedAt == nil || triedSavedCopy { return .showError }
        return .loadSavedCopy
    }

    /// The page to ask the cache for: the dashboard root on that server, with
    /// the requested view's fragment. A one-time sign-in query is dropped,
    /// because the cache holds the clean `/` it redirected to.
    static func cachedURL(for requested: URL, server: URL) -> URL {
        let root = DashboardPage.root(of: server)
        guard let fragment = requested.fragment, !fragment.isEmpty,
              var c = URLComponents(url: root, resolvingAgainstBaseURL: false) else { return root }
        c.fragment = fragment
        return c.url ?? root
    }

    // MARK: - When this device last loaded the dashboard, per server

    static func key(for server: URL) -> String {
        "offlineCopySavedAt." + DashboardPage.root(of: server).absoluteString
    }

    static func savedAt(server: URL, defaults: UserDefaults) -> Date? {
        defaults.object(forKey: key(for: server)) as? Date
    }

    static func recordOnlineLoad(server: URL, at date: Date, defaults: UserDefaults) {
        defaults.set(date, forKey: key(for: server))
    }

    /// Forget it: the cache load found nothing, so the next offline launch
    /// goes straight to the error screen instead of trying again.
    static func forget(server: URL, defaults: UserDefaults) {
        defaults.removeObject(forKey: key(for: server))
    }

    /// The offline badge: "Offline · saved 3:12 PM", or with the day when it
    /// is older than today.
    static func label(savedAt: Date?, now: Date = Date(), calendar: Calendar = .current,
                      locale: Locale = .current) -> String {
        guard let savedAt else { return "Offline" }
        let f = DateFormatter()
        f.locale = locale
        f.calendar = calendar
        f.timeZone = calendar.timeZone
        f.dateStyle = calendar.isDate(savedAt, inSameDayAs: now) ? .none : .short
        f.timeStyle = .short
        return "Offline · saved " + f.string(from: savedAt)
    }

    static let noCopyMessage =
        "This device has no saved copy of the dashboard yet. Open it once while "
        + "connected, and after that it opens offline."
}
