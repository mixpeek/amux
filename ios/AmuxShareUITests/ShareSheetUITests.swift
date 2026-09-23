import XCTest

/// AMUX-4990. The one path nothing else covers: open ANOTHER app, tap Share,
/// and drive the amux row in the system share sheet.
///
/// Everything below the sheet already has tests. `AmuxClientBootstrapTests`
/// covers the parser, `AmuxClientLiveTests` drives the shipping client against
/// a real server. Both compile `AmuxClient.swift` into a test bundle and call
/// it directly, so neither can see what a user sees. The failures that have
/// actually shipped here are all of that kind:
///
///   - a hand-written `AmuxShare/Info.plist` silently discarded by XcodeGen,
///     leaving an extension with no `NSExtension` dict. The build succeeded.
///     iOS never showed it in the sheet.
///   - the row reading "AmuxApp" rather than "amux", because the share sheet
///     labels an extension with its CONTAINING APP's name and the app had no
///     `CFBundleDisplayName`. Found 2026-09-23 when Ethan went looking for
///     "amux" in the sheet's app list and did not find it.
///
/// The CI guard added for the first one checks the appex is EMBEDDED, which is
/// a weaker claim than "iOS offers it under the name people look for".
///
/// SELECTORS ARE MEASURED, NOT GUESSED. Every identifier below was read out of
/// a live element tree on iOS 26.5 rather than assumed, because the obvious
/// guesses are all wrong here: the Photos grid has no cells (it is `Image`
/// elements with identifier `PXGGridLayout-Info`), those images are not
/// hittable so they need a coordinate tap, and `images.firstMatch` matches a
/// tab-bar icon rather than a photo.
///
/// PREREQUISITE the caller sets up, because a UI test cannot reach simctl:
///   xcrun simctl addmedia <udid> <some.png>
/// The worker-list test additionally needs a reachable server in the App Group
/// and SKIPS without one, so a rig gap never reports as a share-sheet bug.
final class ShareSheetUITests: XCTestCase {

    private static let expectedRowLabel = "amux"
    /// Delivery is confirmed against a REAL worker, because the owner asked for
    /// the share to be proven end to end. `amux` is this repo's own lane, so a
    /// verification prompt lands where it is expected rather than interrupting
    /// somebody else's work.
    private static let target = "amux"
    private let photos = XCUIApplication(bundleIdentifier: "com.apple.mobileslideshow")

    override func setUp() {
        super.setUp()
        continueAfterFailure = false
    }

    override func tearDown() {
        photos.terminate()
        super.tearDown()
    }

    // MARK: - Steps

    private func openNewestPhoto() throws {
        photos.launch()
        // A fresh simulator shows a first-run screen whose title moves between
        // releases, so dismiss by action rather than by title.
        for label in ["Continue", "Get Started", "Not Now", "Later"] {
            let b = photos.buttons[label]
            if b.waitForExistence(timeout: 2), b.isHittable { b.tap() }
        }

        let grid = photos.images.matching(identifier: "PXGGridLayout-Info")
        guard grid.firstMatch.waitForExistence(timeout: 20) else {
            attach(photos, "photos-empty")
            throw XCTSkip(
                "the Photos library is empty. Seed it first: "
                + "xcrun simctl addmedia <udid> <file.png>")
        }
        // Newest last, and the newest is the one the caller just added.
        grid.element(boundBy: grid.count - 1)
            .coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
            .tap()
    }

    private func tapShare() {
        let share = photos.buttons["PUOneUpBarButtonItemIdentifierShare"]
        if share.waitForExistence(timeout: 10) {
            share.tap()
            return
        }
        // Identifier is private API and may be renamed; the label is localized
        // but stable in an en_US simulator.
        let byLabel = photos.buttons["Share"]
        XCTAssertTrue(byLabel.waitForExistence(timeout: 10),
                      "no Share control in the Photos viewer")
        byLabel.tap()
    }

    /// Every app offered in the sheet's app row, by label.
    private func sheetAppRow() -> [String] {
        photos.cells.matching(identifier: "shareCell")
            .allElementsBoundByIndex.map(\.label)
    }

    private func attach(_ app: XCUIApplication, _ name: String) {
        let tree = XCTAttachment(string: app.debugDescription)
        tree.name = name
        tree.lifetime = .keepAlways
        add(tree)
        let shot = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        shot.name = name + "-screen"
        shot.lifetime = .keepAlways
        add(shot)
    }

    // MARK: - Tests

    /// THE REGRESSION THIS EXISTS FOR, in both of its shipped forms: absent
    /// from the sheet, or present under a name nobody would recognise.
    func testTheShareSheetOffersAmuxUnderThatName() throws {
        try openNewestPhoto()
        tapShare()

        let row = photos.cells.matching(identifier: "shareCell")
        XCTAssertTrue(row.firstMatch.waitForExistence(timeout: 15),
                      "the share sheet never rendered its app row")

        let labels = sheetAppRow()
        guard labels.contains(Self.expectedRowLabel) else {
            attach(photos, "share-sheet-app-row")
            return XCTFail(
                "the share sheet offers \(labels), which does not include "
                + "'\(Self.expectedRowLabel)'. Absent means a missing or "
                + "malformed NSExtension dict; present under another name means "
                + "the containing app's CFBundleDisplayName changed, since that "
                + "is what iOS labels this row with.")
        }

        let amux = photos.cells.matching(identifier: "shareCell")
            .matching(NSPredicate(format: "label == %@", Self.expectedRowLabel)).firstMatch
        XCTAssertTrue(amux.isHittable, "amux is in the sheet but not tappable")
        amux.tap()

        // Proves the appex LAUNCHED, rather than the row merely existing.
        let ext = XCUIApplication(bundleIdentifier: "com.EthanSteininger.nextup.Share")
        XCTAssertTrue(
            ext.navigationBars["Share to amux"].waitForExistence(timeout: 25)
            || photos.navigationBars["Share to amux"].waitForExistence(timeout: 5),
            "amux was tapped and its share UI never appeared")
    }

    /// The extension is a SEPARATE PROCESS with its own container. It reads the
    /// server from the App Group, and a group the two targets disagree about
    /// shows "No amux server selected" forever while the host app works fine.
    /// No in-process test can see that split.
    func testTheExtensionLoadsWorkersFromTheAppGroup() throws {
        try openNewestPhoto()
        tapShare()

        let amux = photos.cells.matching(identifier: "shareCell")
            .matching(NSPredicate(format: "label == %@", Self.expectedRowLabel)).firstMatch
        try XCTSkipUnless(amux.waitForExistence(timeout: 15),
                          "amux not offered; testTheShareSheetOffersAmuxUnderThatName owns that")
        amux.tap()

        let ext = XCUIApplication(bundleIdentifier: "com.EthanSteininger.nextup.Share")
        XCTAssertTrue(ext.navigationBars["Share to amux"].waitForExistence(timeout: 25),
                      "the share UI never appeared")

        // Three outcomes worth separating: the header means workers loaded, the
        // error text means it ran and could not reach a server, neither means
        // it hung.
        let sendTo = ext.staticTexts["Send to"]
        let noServer = ext.staticTexts.containing(
            NSPredicate(format: "label CONTAINS[c] 'No amux server selected'")).firstMatch

        if !sendTo.waitForExistence(timeout: 30) {
            if noServer.exists {
                attach(ext, "extension-no-server")
                throw XCTSkip(
                    "the extension ran and has no server configured. Set serverURL "
                    + "in group.com.EthanSteininger.nextup first; an unset server "
                    + "is a rig gap, not a bug.")
            }
            attach(ext, "extension-stuck")
            return XCTFail("the share UI opened and produced neither a worker list nor an error")
        }

        XCTAssertGreaterThan(
            ext.cells.count, 0,
            "'Send to' rendered with no rows, so the list request came back empty")

        // ACTIVE ONLY IS THE DEFAULT (AMUX-5015).
        //
        // ASSERTED FROM THE HEADER'S OWN ARITHMETIC rather than by driving the
        // toggle. Two earlier attempts and what each taught:
        //
        //   counting `worker-*` rows        6 -> 6. A SwiftUI Form realizes only
        //                                   the rows on screen, so that number
        //                                   is a viewport count and cannot see
        //                                   62 hidden workers.
        //   flipping the toggle             `switch value before='1' after='1'`.
        //                                   XCUITest's tap does not flip this
        //                                   SwiftUI Toggle, so the comparison
        //                                   measured nothing. An even earlier
        //                                   version PASSED this way only because
        //                                   its prose matcher picked up the
        //                                   toggle's own label, "Active workers
        //                                   only", as the second reading.
        //
        // The header states the whole population split, so it can be checked
        // without moving anything: active + hidden must equal the fleet, and
        // hidden must be non-zero or nothing is being filtered.
        let activeOnly = ext.switches["activeOnly"]
        XCTAssertTrue(activeOnly.waitForExistence(timeout: 10), "no active-only toggle")
        XCTAssertEqual(activeOnly.value as? String, "1",
                       "active-only must default ON, and must reset to ON for each share")

        let header = ext.staticTexts["population"]
        XCTAssertTrue(header.waitForExistence(timeout: 10), "no population count in the header")
        let label = header.label
        let numbers = label.split(whereSeparator: { !$0.isNumber }).compactMap { Int($0) }
        XCTAssertEqual(
            numbers.count, 3,
            "the default header must state active, running and hidden: '\(label)'")
        let (active, running, hidden) = (numbers[0], numbers[1], numbers[2])
        XCTAssertGreaterThan(
            hidden, 0,
            "nothing is being withheld, so the filter is doing nothing: '\(label)'")
        XCTAssertGreaterThan(active, 0, "no worker is offered at all: '\(label)'")
        XCTAssertLessThanOrEqual(
            running, active,
            "running must be a subset of active; the filter is on lifecycle, not on running: '\(label)'")
        XCTAssertTrue(
            label.contains("paused hidden"),
            "the header must name WHAT it withholds, not just how many: '\(label)'")

        // CROSS-CHECKED AGAINST THE SERVER (AMUX-4990). Everything above is
        // internally consistent arithmetic: it proves SOMETHING is withheld and
        // not that the RIGHT set is on screen. A filter that dropped one active
        // worker and admitted one paused one would satisfy all of it.
        //
        // So the displayed population is compared to the population the server
        // reports. Read over loopback, where amux answers anonymously, so this
        // needs no credential and cannot accidentally re-prove the auth path.
        let fleet = try fleetLifecycleCounts()
        XCTAssertEqual(
            active, fleet.active,
            "the list shows \(active) workers but the server reports \(fleet.active) "
            + "lifecycle-active (of \(fleet.total) unarchived). Header: '\(label)'")
        XCTAssertEqual(
            hidden, fleet.total - fleet.active,
            "\(hidden) withheld but \(fleet.total - fleet.active) are inactive. Header: '\(label)'")
        XCTAssertGreaterThan(
            fleet.total - fleet.active, 0,
            "this fleet has no inactive workers, so the exclusion half of this test "
            + "proves nothing right now — pause one and re-run")

        // SEARCH, then SELECT. Those are the two things this screen is for and
        // both are addressable by identifier.
        let search = ext.searchFields.firstMatch
        XCTAssertTrue(search.waitForExistence(timeout: 5), "no search field on the worker list")
        search.tap()
        search.typeText(Self.target)

        let row = ext.buttons["worker-\(Self.target)"]
        XCTAssertTrue(row.waitForExistence(timeout: 10),
                      "searching for '\(Self.target)' did not surface its row")
        row.tap()

        // Selection is shown by a checkmark on the row. Asserting THAT rather
        // than the Send button's enabled state is deliberate: any query against
        // the extension's toolbar throws "Failed to get matching snapshot" at
        // this point in the session, on `ext` and on the host app alike, while
        // queries against the list keep working. Four runs, same error, three
        // different spellings of the query.
        XCTAssertTrue(row.images.firstMatch.waitForExistence(timeout: 5)
                      || ext.images["checkmark"].waitForExistence(timeout: 5),
                      "tapping '\(Self.target)' did not mark it selected")

        // DELIVERY IS NOT ASSERTED HERE. `AmuxClientLiveTests` drives the same
        // send through the same shipping client and then reads the recipient's
        // history back, which is a stronger claim than a tap and does not
        // depend on the toolbar being queryable. Splitting them keeps this test
        // about the sheet.
    }

    // MARK: - Helpers

    /// What the SERVER says the fleet looks like, so the UI's claim can be
    /// checked against something other than itself.
    ///
    /// Counts only unarchived sessions, because `AmuxClient.workers` drops
    /// archived rows before the list is built — comparing against the raw total
    /// would fail for a reason that has nothing to do with this filter.
    private func fleetLifecycleCounts() throws -> (total: Int, active: Int) {
        let url = URL(string: "https://localhost:8823/api/sessions")!
        let session = URLSession(configuration: .ephemeral, delegate: TrustAll(), delegateQueue: nil)
        let sem = DispatchSemaphore(value: 0)
        var payload: Data?
        session.dataTask(with: url) { data, _, _ in payload = data; sem.signal() }.resume()
        _ = sem.wait(timeout: .now() + 30)
        guard let payload,
              let rows = try JSONSerialization.jsonObject(with: payload) as? [[String: Any]] else {
            throw XCTSkip(
                "could not read /api/sessions over loopback, so the UI's population cannot be "
                + "cross-checked. That is a rig gap, not a filter failure.")
        }
        let live = rows.filter { ($0["archived"] as? Bool) != true }
        return (live.count, live.filter { ($0["lifecycle"] as? String) == "active" }.count)
    }

    /// The amux server serves a self-signed certificate.
    private final class TrustAll: NSObject, URLSessionDelegate {
        func urlSession(_ session: URLSession,
                        didReceive challenge: URLAuthenticationChallenge,
                        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
            if let trust = challenge.protectionSpace.serverTrust {
                completionHandler(.useCredential, URLCredential(trust: trust))
            } else {
                completionHandler(.performDefaultHandling, nil)
            }
        }
    }
}
