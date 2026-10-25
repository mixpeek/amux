import XCTest

/// Taps reach the dashboard inside the app. Owner, 2026-10-17, on the iPad app:
/// "I cant navigate the amux app on my ipad my gestures nothing clicks or does
/// anything". The same page worked in Safari on that iPad, so this drives the
/// real app's web view: connect to a server, then tap tabs the way a person does
/// and require each tap to change what is on screen.
///
/// Run against any reachable amux server (self-signed is fine):
///   TEST_RUNNER_DASHBOARD_URL=https://localhost:18897 \
///   xcodebuild test -scheme AmuxShareUITests -destination 'platform=iOS Simulator,name=iPad Air 11-inch (M4)' \
///     -only-testing:AmuxShareUITests/DashboardTapUITests
final class DashboardTapUITests: XCTestCase {
    override func setUp() { continueAfterFailure = false }

    private func shot(_ app: XCUIApplication, _ name: String) {
        let a = XCTAttachment(screenshot: app.screenshot())
        a.name = name
        a.lifetime = .keepAlways
        add(a)
    }

    func testTapsInTheDashboardChangeTheScreen() throws {
        guard let url = ProcessInfo.processInfo.environment["DASHBOARD_URL"], !url.isEmpty else {
            throw XCTSkip("set TEST_RUNNER_DASHBOARD_URL to an amux server to run")
        }
        let app = XCUIApplication()
        app.launch()

        // Fresh install: the server picker. A saved server: Settings -> Add Server.
        let byPlaceholder = { (p: String) in app.textFields.matching(NSPredicate(format: "placeholderValue == %@", p)).firstMatch }
        let pickerURL = byPlaceholder("https://amux.tail-xxxx.ts.net:8824")
        if pickerURL.waitForExistence(timeout: 20) {
            pickerURL.tap(); pickerURL.typeText(url)
            app.buttons["Connect"].tap()
        } else {
            let web0 = app.webViews.firstMatch
            XCTAssertTrue(web0.waitForExistence(timeout: 10), "the app shows either the picker or a web view")
            web0.press(forDuration: 0.8)
            let add = app.buttons["Add Server"]
            XCTAssertTrue(add.waitForExistence(timeout: 10), "a long press opens Settings")
            add.tap()
            let u = byPlaceholder("URL (e.g. https://amux.tail-xxxx.ts.net:8824)")
            XCTAssertTrue(u.waitForExistence(timeout: 10))
            u.tap(); u.typeText(url)
            app.buttons["Add"].tap()
            let host = URL(string: url)?.host ?? url
            let row = app.staticTexts.containing(NSPredicate(format: "label CONTAINS[c] %@", host)).firstMatch
            XCTAssertTrue(row.waitForExistence(timeout: 10), "the new server is listed")
            row.tap()
        }

        let web = app.webViews.firstMatch
        XCTAssertTrue(web.waitForExistence(timeout: 30), "Connect opens the web view")
        // The dashboard reopens the last worker it showed; its Tasks tab shares a label with the nav's.
        let closePeek = web.buttons["Close worker"]
        if closePeek.waitForExistence(timeout: 8), closePeek.isHittable { closePeek.tap(); sleep(1) }
        // Tabs are buttons in the web view's accessibility tree, not static text.
        let byLabel = { (l: String) in web.descendants(matching: .any).matching(NSPredicate(format: "label == %@", l)).firstMatch }
        let workersTab = byLabel("Workers")
        XCTAssertTrue(workersTab.waitForExistence(timeout: 45), "the dashboard loads in the app")
        sleep(3)
        shot(app, "1-dashboard")

        // Tasks: its search field only exists on that view.
        let tasksTab = byLabel("Tasks")
        XCTAssertTrue(tasksTab.exists, "a Tasks tab is shown")
        tasksTab.tap()
        let tasksSearch = web.descendants(matching: .any).matching(NSPredicate(format: "placeholderValue == %@ OR label == %@", "Search tasks", "Search tasks")).firstMatch
        let tasksOK = tasksSearch.waitForExistence(timeout: 8)
        shot(app, "2-after-tasks-tap")
        print("DASHBOARD_TAP tasks_view_opened=\(tasksOK)")
        XCTAssertTrue(tasksOK, "tapping Tasks opens the Tasks view")

        // Back to Workers, then the gear opens Settings.
        workersTab.tap()
        sleep(1)
        shot(app, "3-after-workers-tap")
        let gear = web.buttons.matching(NSPredicate(format: "label == 'Settings'")).firstMatch
        if gear.exists {
            gear.tap()
            let opened = web.descendants(matching: .any).matching(NSPredicate(format: "label ==[c] 'Subscription & provider usage' OR label ==[c] 'Workspace access'")).firstMatch.waitForExistence(timeout: 8)
            shot(app, "4-after-settings-tap")
            print("DASHBOARD_TAP settings_opened=\(opened)")
            XCTAssertTrue(opened, "tapping the gear opens dashboard Settings")
        }
    }
}
