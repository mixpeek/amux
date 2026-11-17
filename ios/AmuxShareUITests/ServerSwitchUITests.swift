import XCTest

/// Switching between two servers the way a person does it, on real servers: the Mac's own
/// amux (https://localhost:8824, which the simulator reaches) and the sandbox host.
///
/// Opens the first from the server picker, adds the second from the switcher (long press),
/// switches back with one tap, and opens a server from the Home Screen quick action. A switch
/// back to a server already open must show its page at once (ServerPanes.swift).
///
/// Run: TEST_RUNNER_SWITCH_E2E=1 xcodebuild test -scheme AmuxApp
///   -only-testing:AmuxShareUITests/ServerSwitchUITests  (uninstall the app first: it starts at the picker)
final class ServerSwitchUITests: XCTestCase {
    private let first = ProcessInfo.processInfo.environment["SWITCH_FIRST_URL"] ?? "https://localhost:8824"
    private let second = ProcessInfo.processInfo.environment["SWITCH_SECOND_URL"] ?? "https://sandbox.amux.io"

    override func setUp() { continueAfterFailure = false }

    private func shot(_ app: XCUIApplication, _ name: String) {
        let a = XCTAttachment(screenshot: app.screenshot())
        a.name = name
        a.lifetime = .keepAlways
        add(a)
    }

    /// The page on screen has rendered something (the hidden panes are out of the tree).
    private func waitForPage(_ app: XCUIApplication, timeout: TimeInterval) -> Bool {
        let page = app.webViews.firstMatch
        let end = Date().addingTimeInterval(timeout)
        while Date() < end {
            if page.exists && (page.staticTexts.count > 0 || page.buttons.count > 0 || page.textFields.count > 0) { return true }
            Thread.sleep(forTimeInterval: 0.05)
        }
        return false
    }

    private func openSwitcher(_ app: XCUIApplication) {
        app.webViews.firstMatch.press(forDuration: 1.0)
        XCTAssertTrue(app.buttons["Add Server"].waitForExistence(timeout: 5), "the switcher opens on a long press")
    }

    func testSwitchBetweenTwoServers() throws {
        guard ProcessInfo.processInfo.environment["SWITCH_E2E"] == "1" else {
            throw XCTSkip("set TEST_RUNNER_SWITCH_E2E=1 to run against real servers")
        }
        let app = XCUIApplication()
        app.launch()
        let byPlaceholder = { (p: String) in app.textFields.matching(NSPredicate(format: "placeholderValue == %@", p)).firstMatch }

        // 1. The first server, from the picker.
        let pickerURL = byPlaceholder("https://amux.tail-xxxx.ts.net:8824")
        XCTAssertTrue(pickerURL.waitForExistence(timeout: 10), "a fresh install opens on the picker")
        let name = byPlaceholder("Name (optional)")
        name.tap(); name.typeText("Mac")
        pickerURL.tap(); pickerURL.typeText(first + "\n")  // Go connects
        XCTAssertTrue(waitForPage(app, timeout: 30), "the first server's page renders")
        shot(app, "1-first-server")

        // 2. Add the second from the switcher: it opens as soon as it is added.
        openSwitcher(app)
        shot(app, "2-switcher")
        app.buttons["Add Server"].tap()
        let addName = app.textFields["Name (e.g. Home Mac)"]
        XCTAssertTrue(addName.waitForExistence(timeout: 5))
        addName.tap(); addName.typeText("Sandbox")
        let addURL = app.textFields["URL (e.g. https://amux.tail-xxxx.ts.net:8824)"]
        addURL.tap(); addURL.typeText(second + "\n")  // Go adds and opens it
        XCTAssertFalse(app.buttons["Add Server"].waitForExistence(timeout: 2) && app.buttons["Add Server"].isHittable,
                       "adding a server closes the switcher")
        XCTAssertTrue(waitForPage(app, timeout: 30), "the second server's page renders")
        shot(app, "3-second-server")
        XCTAssertFalse(app.buttons["back-to-dashboard"].exists, "a server's sign-in page is a way in, with no back button")

        // 3. Back to the first: one tap, and its page is already there.
        openSwitcher(app)
        let rows = app.buttons.matching(identifier: "server-row")
        XCTAssertEqual(rows.count, 2, "both servers are listed")
        let mac = rows.containing(NSPredicate(format: "label CONTAINS %@", "Mac")).firstMatch
        let tapped = Date()
        mac.tap()
        XCTAssertTrue(waitForPage(app, timeout: 5))
        let backMs = Int(Date().timeIntervalSince(tapped) * 1000)
        print("SWITCH_E2E switch_back_ms=\(backMs)")
        shot(app, "4-back-to-first")
        XCTAssertLessThan(backMs, 2500, "a server already open shows without a reload")

        // 4. The Home Screen quick action opens the other server.
        XCUIDevice.shared.press(.home)
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let icon = springboard.icons["amux"]
        XCTAssertTrue(icon.waitForExistence(timeout: 10), "the amux icon is on the Home Screen")
        icon.press(forDuration: 1.5)
        // Each item reads "<name>, <host>".
        let action = springboard.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Sandbox")).firstMatch
        XCTAssertTrue(action.waitForExistence(timeout: 5), "the quick actions list the saved servers")
        shot(app, "5-quick-actions")
        action.tap()
        XCTAssertTrue(app.wait(for: .runningForeground, timeout: 10))
        XCTAssertTrue(waitForPage(app, timeout: 10))
        shot(app, "6-quick-action-opened")
    }
}
