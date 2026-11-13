import XCTest

/// What the app does when the dashboard cannot be fetched (OfflineCopy.swift).
/// Owner, 2026-11-12, in airplane mode: the app showed "Can't reach server"
/// over a dashboard it had loaded many times.
final class OfflineCopyTests: XCTestCase {
    let server = URL(string: "https://desktop.tail5ce8f5.ts.net:8824/")!
    let offline = NSError(domain: NSURLErrorDomain, code: NSURLErrorNotConnectedToInternet)

    private func freshDefaults() -> UserDefaults {
        let name = "OfflineCopyTests." + UUID().uuidString
        let d = UserDefaults(suiteName: name)!
        d.removePersistentDomain(forName: name)
        return d
    }

    // MARK: - The decision

    func testASavedCopyOpensOfflineInsteadOfTheErrorScreen() {
        let d = OfflineCopy.decide(unreachable: true, showingDashboard: false,
                                   savedAt: Date(), triedSavedCopy: false)
        XCTAssertEqual(d, .loadSavedCopy)
    }

    func testFirstLaunchOfflineHasNothingCachedSoTheErrorScreenShows() {
        let d = OfflineCopy.decide(unreachable: true, showingDashboard: false,
                                   savedAt: nil, triedSavedCopy: false)
        XCTAssertEqual(d, .showError)
    }

    func testACacheLoadThatAlsoFailsShowsTheErrorScreenOnce() {
        // WebKit's cache was cleared: the record said there was a copy and
        // the cache load went to the network and failed too. No loop.
        let d = OfflineCopy.decide(unreachable: true, showingDashboard: false,
                                   savedAt: Date(), triedSavedCopy: true)
        XCTAssertEqual(d, .showError)
    }

    func testAFailedReloadKeepsTheDashboardOnScreen() {
        // Pull-to-refresh or Retry while offline: the page is still there.
        for saved in [Date(), nil] as [Date?] {
            for tried in [true, false] {
                XCTAssertEqual(OfflineCopy.decide(unreachable: true, showingDashboard: true,
                                                  savedAt: saved, triedSavedCopy: tried), .keepPage)
            }
        }
    }

    func testAServerThatAnsweredBadlyStillShowsItsError() {
        // A bad certificate is not "offline"; hiding it behind a cached page
        // would leave the owner on stale data with no reason given.
        let tls = NSError(domain: NSURLErrorDomain, code: NSURLErrorServerCertificateUntrusted)
        XCTAssertFalse(OfflineCopy.isUnreachable(tls))
        XCTAssertEqual(OfflineCopy.decide(unreachable: false, showingDashboard: false,
                                          savedAt: Date(), triedSavedCopy: false), .showError)
        XCTAssertEqual(OfflineCopy.decide(unreachable: false, showingDashboard: true,
                                          savedAt: Date(), triedSavedCopy: false), .showError)
    }

    func testEveryWayOfBeingUnreachableCounts() {
        for code in [NSURLErrorNotConnectedToInternet, NSURLErrorCannotConnectToHost,
                     NSURLErrorCannotFindHost, NSURLErrorTimedOut, NSURLErrorNetworkConnectionLost,
                     NSURLErrorDataNotAllowed, NSURLErrorDNSLookupFailed, NSURLErrorInternationalRoamingOff] {
            XCTAssertTrue(OfflineCopy.isUnreachable(NSError(domain: NSURLErrorDomain, code: code)), "\(code)")
        }
        XCTAssertFalse(OfflineCopy.isUnreachable(NSError(domain: NSURLErrorDomain, code: NSURLErrorCancelled)))
        XCTAssertFalse(OfflineCopy.isUnreachable(NSError(domain: "WebKitErrorDomain", code: NSURLErrorTimedOut)))
    }

    // MARK: - The record of a saved copy

    func testAnOnlineLoadIsWhatMakesACopyAvailable() {
        let d = freshDefaults()
        XCTAssertNil(OfflineCopy.savedAt(server: server, defaults: d))
        let when = Date(timeIntervalSince1970: 1_800_000_000)
        OfflineCopy.recordOnlineLoad(server: server, at: when, defaults: d)
        XCTAssertEqual(OfflineCopy.savedAt(server: server, defaults: d), when)
        // The same server however its URL was written: a deep link, no slash.
        XCTAssertEqual(OfflineCopy.savedAt(server: URL(string: "https://desktop.tail5ce8f5.ts.net:8824/#peek=amux")!, defaults: d), when)
        XCTAssertEqual(OfflineCopy.savedAt(server: URL(string: "https://desktop.tail5ce8f5.ts.net:8824")!, defaults: d), when)
        // Another server, or another port on this one, has its own.
        XCTAssertNil(OfflineCopy.savedAt(server: URL(string: "https://cloud.amux.io/")!, defaults: d))
        XCTAssertNil(OfflineCopy.savedAt(server: URL(string: "https://desktop.tail5ce8f5.ts.net:8825/")!, defaults: d))
        OfflineCopy.forget(server: server, defaults: d)
        XCTAssertNil(OfflineCopy.savedAt(server: server, defaults: d))
    }

    func testTheCacheIsAskedForTheCleanDashboardWithTheViewKept() {
        XCTAssertEqual(OfflineCopy.cachedURL(for: URL(string: "https://h.ts.net:8824/?_token=abc#peek=amux")!,
                                             server: server).absoluteString,
                       "https://desktop.tail5ce8f5.ts.net:8824/#peek=amux")
        XCTAssertEqual(OfflineCopy.cachedURL(for: server, server: server).absoluteString,
                       "https://desktop.tail5ce8f5.ts.net:8824/")
    }

    func testTheBadgeSaysWhenTheCopyWasSaved() {
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "UTC")!
        let locale = Locale(identifier: "en_US")
        let now = Date(timeIntervalSince1970: 1_800_000_000)          // 2027-01-15 08:00 UTC
        let earlier = now.addingTimeInterval(-3600)
        let yesterday = now.addingTimeInterval(-86_400)
        let today = OfflineCopy.label(savedAt: earlier, now: now, calendar: cal, locale: locale)
        XCTAssertTrue(today.hasPrefix("Offline · saved 7:00"), today)
        XCTAssertFalse(today.contains("/"), "no date for today: \(today)")
        let older = OfflineCopy.label(savedAt: yesterday, now: now, calendar: cal, locale: locale)
        XCTAssertTrue(older.contains("1/14/27"), older)
        XCTAssertEqual(OfflineCopy.label(savedAt: nil), "Offline")
    }
}
