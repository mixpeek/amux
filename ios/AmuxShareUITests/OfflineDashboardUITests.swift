import XCTest

/// The dashboard opens offline from what this device last loaded, a send made
/// offline is queued, and it delivers when the server is back (OfflineCopy.swift).
/// Owner, 2026-11-12, on a plane: "ios offline needs to work perfectly".
///
/// The simulator has no airplane mode, so a rig stands in for the network:
/// scripts/ios-offline-rig.py runs a throwaway amux server plus a controller on
/// 127.0.0.1:18990 that starts and stops it, takes simulator screenshots and
/// reads the server's message history. Skips when the rig is not configured.
///   python3 scripts/ios-offline-rig.py <udid> &
///   TEST_RUNNER_OFFLINE_RIG=http://127.0.0.1:18990 \
///   TEST_RUNNER_OFFLINE_SERVER=https://127.0.0.1:18961/ \
///   xcodebuild test -scheme AmuxShareUITests -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
///     -only-testing:AmuxShareUITests/OfflineDashboardUITests
final class OfflineDashboardUITests: XCTestCase {
    override func setUp() { continueAfterFailure = false }

    private var rig = ""

    @discardableResult
    private func ctl(_ path: String) -> [String: Any] {
        guard let url = URL(string: rig + path) else { return [:] }
        let done = expectation(description: path)
        var out: [String: Any] = [:]
        var req = URLRequest(url: url)
        req.timeoutInterval = 60
        URLSession.shared.dataTask(with: req) { data, _, _ in
            if let data, let j = try? JSONSerialization.jsonObject(with: data) as? [String: Any] { out = j }
            done.fulfill()
        }.resume()
        wait(for: [done], timeout: 65)
        return out
    }

    private func shot(_ app: XCUIApplication, _ name: String) {
        let a = XCTAttachment(screenshot: app.screenshot())
        a.name = name
        a.lifetime = .keepAlways
        add(a)
        ctl("/shot?name=" + name)
    }

    private func launch(_ app: XCUIApplication, server: String) {
        app.launchArguments = ["-serverURL", server]
        app.launch()
    }

    private func text(_ app: XCUIApplication, containing s: String) -> XCUIElement {
        app.webViews.firstMatch.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS %@", s)).firstMatch
    }

    /// The dashboard reopens the worker it showed last; back to the list.
    private func closeWorker(_ app: XCUIApplication) {
        let close = app.webViews.firstMatch.buttons["Close worker"]
        if close.waitForExistence(timeout: 3), close.isHittable { close.tap(); sleep(1) }
    }

    private func openTab(_ app: XCUIApplication, _ name: String) {
        let tab = app.webViews.firstMatch.buttons.matching(NSPredicate(format: "label ENDSWITH %@", name)).firstMatch
        XCTAssertTrue(tab.waitForExistence(timeout: 10), "the \(name) tab")
        tab.tap()
        sleep(1)
    }

    func testTheDashboardOpensOfflineQueuesASendAndCatchesUp() throws {
        let env = ProcessInfo.processInfo.environment
        guard let r = env["OFFLINE_RIG"], let server = env["OFFLINE_SERVER"] else {
            throw XCTSkip("set TEST_RUNNER_OFFLINE_RIG and TEST_RUNNER_OFFLINE_SERVER to run")
        }
        rig = r
        XCTAssertTrue(["up", "already"].contains(ctl("/start")["result"] as? String ?? ""), "the throwaway server runs")
        let app = XCUIApplication()
        let badge = app.descendants(matching: .any)["offline-badge"]

        // 1. Online: the dashboard loads, which is what WebKit keeps.
        launch(app, server: server)
        // The dashboard reopens the view it showed last (a worker, a tab).
        XCTAssertTrue(app.webViews.firstMatch.buttons.matching(NSPredicate(format: "label ENDSWITH 'Workers'"))
            .firstMatch.waitForExistence(timeout: 60), "online dashboard loads")
        closeWorker(app)
        openTab(app, "Workers")
        XCTAssertTrue(text(app, containing: "offline-demo").waitForExistence(timeout: 30), "online dashboard shows the worker")
        XCTAssertFalse(badge.exists, "no offline strip while online")
        openTab(app, "Board")
        sleep(2)
        shot(app, "10-online-board")
        openTab(app, "Workers")
        sleep(3)
        shot(app, "10-online")

        // 2. Server gone, cold launch: the saved copy, not "Can't reach server".
        ctl("/stop")
        app.terminate()
        launch(app, server: server)
        XCTAssertTrue(badge.waitForExistence(timeout: 40), "the offline strip shows")
        XCTAssertFalse(app.staticTexts["Can't reach server"].exists, "no native error screen")
        closeWorker(app)
        XCTAssertTrue(text(app, containing: "offline-demo").waitForExistence(timeout: 20), "cached worker shows offline")
        sleep(2)
        shot(app, "11-offline-cold-launch")

        // 3. Background and foreground: still the dashboard, still offline.
        XCUIDevice.shared.press(.home)
        sleep(2)
        app.activate()
        XCTAssertTrue(badge.waitForExistence(timeout: 20), "still marked offline after foreground")
        XCTAssertTrue(text(app, containing: "offline-demo").exists, "dashboard survives background/foreground")
        shot(app, "12-offline-foreground")

        // The board reads from what was cached too.
        closeWorker(app)
        openTab(app, "Board")
        let column = app.webViews.firstMatch.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS[c] 'backlog'")).firstMatch
        XCTAssertTrue(column.waitForExistence(timeout: 15), "the board renders offline")
        sleep(1)
        shot(app, "13-offline-board")
        openTab(app, "Workers")

        // 4. Queue a send to the worker while offline.
        let token = "plane-\(Int(Date().timeIntervalSince1970) % 100000)"
        // Open it from its card menu (the first card's ⋯ is offline-demo's).
        let menu = app.webViews.firstMatch.buttons.matching(NSPredicate(format: "label == '⋯'")).firstMatch
        XCTAssertTrue(menu.waitForExistence(timeout: 10), "the worker's menu")
        menu.tap()
        let peek = text(app, containing: "Peek terminal")
        XCTAssertTrue(peek.waitForExistence(timeout: 10), "Peek terminal in the menu")
        peek.tap()
        let web = app.webViews.firstMatch
        let composer = web.textViews.matching(NSPredicate(format: "label == 'Message the worker'")).firstMatch
        XCTAssertTrue(composer.waitForExistence(timeout: 20), "the worker's composer opens offline")
        sleep(2)   // the worker view slides in; a tap mid-animation does not focus
        // The dashboard's "Server unreachable" toast covers the right half of
        // the composer for a few seconds, so tap its left edge.
        let left = composer.coordinate(withNormalizedOffset: CGVector(dx: 0.12, dy: 0.5))
        left.tap()
        if !(composer.value(forKey: "hasKeyboardFocus") as? Bool ?? false) { sleep(2); left.tap() }
        composer.typeText("sent from the plane " + token)
        let sends = web.buttons.matching(NSPredicate(format: "label == 'Send'"))
        let send = sends.allElementsBoundByIndex.first(where: { $0.isHittable })
        XCTAssertNotNil(send, "a visible Send button")
        send?.tap()
        sleep(3)
        shot(app, "14-offline-send-queued")
        XCTAssertTrue(text(app, containing: "saved offline").exists, "the send waits in the outbox")
        XCTAssertEqual(ctl("/history?token=" + token)["count"] as? Int, 0, "nothing reached the server while it was down")

        // 5. Server back: the strip clears and the queued send delivers.
        XCTAssertEqual(ctl("/start")["result"] as? String ?? "", "up")
        let gone = NSPredicate(format: "exists == false")
        expectation(for: gone, evaluatedWith: badge)
        waitForExpectations(timeout: 45)
        // Delivered exactly once: the stopped stub worker holds it as its next
        // prompt, so the server's message history is the record, not the pane.
        var count = 0
        for _ in 0..<45 {
            count = ctl("/history?token=" + token)["count"] as? Int ?? 0
            if count > 0 { break }
            sleep(2)
        }
        let pendingGone = NSPredicate(format: "exists == false")
        expectation(for: pendingGone, evaluatedWith: text(app, containing: "saved offline"))
        waitForExpectations(timeout: 30)
        sleep(3)
        XCTAssertEqual(ctl("/history?token=" + token)["count"] as? Int, 1, "delivered once, not duplicated")
        shot(app, "15-reconnected")
        XCTAssertEqual(count, 1, "the queued send reached the server after reconnect")
    }
}
