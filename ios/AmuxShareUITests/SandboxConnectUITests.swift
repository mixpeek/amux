import XCTest

/// The sandbox host (https://sandbox.amux.io), connected the way a person does
/// it in the app: type the URL in the server picker, tap Connect, and land on
/// the host's sign-in page inside the app's web view. With credentials in the
/// environment (TEST_RUNNER_SANDBOX_USER / TEST_RUNNER_SANDBOX_PASS when run
/// through xcodebuild), it also signs in and waits for the dashboard.
///
/// The host sits behind a sign-in "door" (cloud/desktop/door.py) that answers
/// an unauthenticated request with a redirect to /_door/login, so reaching that
/// form proves DNS, TLS, the redirect and the page render inside WKWebView.
/// Signing in proves the cookie round trip and the dashboard itself.
///
/// Run: TEST_RUNNER_SANDBOX_E2E=1 [TEST_RUNNER_SANDBOX_USER=… TEST_RUNNER_SANDBOX_PASS=…]
///   xcodebuild test -scheme AmuxShareUITests -only-testing:AmuxShareUITests/SandboxConnectUITests
/// TEST_RUNNER_SANDBOX_URL=https://<name>.amux.io points it at an amux Cloud customer's server.
final class SandboxConnectUITests: XCTestCase {
    private let sandbox = ProcessInfo.processInfo.environment["SANDBOX_URL"] ?? "https://sandbox.amux.io"
    private var sandboxHost: String { URL(string: sandbox)?.host ?? "sandbox.amux.io" }

    override func setUp() { continueAfterFailure = false }

    private func shot(_ app: XCUIApplication, _ name: String) {
        let a = XCTAttachment(screenshot: app.screenshot())
        a.name = name
        a.lifetime = .keepAlways
        add(a)
    }

    /// A WKWebView field can take the tap without taking keyboard focus (the door's password
    /// field on a Cloud server did, 2026-10-19), and typeText then fails. Tap until the
    /// keyboard is up, then type.
    private func type(_ app: XCUIApplication, _ field: XCUIElement, _ text: String) {
        // The keyboard can already be up for the previous field, so ask the field itself.
        let focused = { (field.value(forKey: "hasKeyboardFocus") as? Bool) ?? false }
        for _ in 0..<4 where !focused() {
            field.tap()
            _ = app.keyboards.firstMatch.waitForExistence(timeout: 2)
            if !focused() { Thread.sleep(forTimeInterval: 1) }
        }
        field.typeText(text)
    }

    func testSandboxConnectsFromTheServerPicker() throws {
        // Opt-in: this reaches a live host on the internet, so CI and ordinary
        // test runs never depend on it. TEST_RUNNER_SANDBOX_E2E=1 turns it on.
        guard ProcessInfo.processInfo.environment["SANDBOX_E2E"] == "1" else {
            throw XCTSkip("set TEST_RUNNER_SANDBOX_E2E=1 to run against https://sandbox.amux.io")
        }
        let app = XCUIApplication()
        let started = Date()
        app.launch()

        // Two real ways in. A fresh install opens on the server picker. A
        // device that already saved a server opens it, and when that server is
        // unreachable offers Switch Server -> Settings -> Add Server, then the
        // new row is tapped to switch to it. The test takes whichever the app
        // shows and says which.
        let byPlaceholder = { (p: String) in app.textFields.matching(NSPredicate(format: "placeholderValue == %@", p)).firstMatch }
        let pickerURL = byPlaceholder("https://amux.tail-xxxx.ts.net:8824")
        let switchServer = app.buttons["Switch Server"]
        let addServer = app.buttons["Add Server"]
        let deadline = Date().addingTimeInterval(40)
        let savedSandbox = app.webViews.firstMatch.staticTexts[sandboxHost]
        while !pickerURL.exists && !addServer.exists && !savedSandbox.exists && Date() < deadline {
            if switchServer.exists { shot(app, "0-saved-server-unreachable"); switchServer.tap() }
            _ = pickerURL.waitForExistence(timeout: 2)
        }
        if savedSandbox.exists {
            // Already saved and selected from an earlier run: the app reopens it.
            print("SANDBOX_E2E route=saved-sandbox-reopened")
        } else if pickerURL.exists {
            print("SANDBOX_E2E route=server-picker")
            shot(app, "1-server-picker")
            let name = byPlaceholder("Name (optional)")
            name.tap(); name.typeText("Sandbox")
            pickerURL.tap(); pickerURL.typeText(sandbox)
            app.buttons["Connect"].tap()
        } else {
            XCTAssertTrue(addServer.exists, "Settings offers Add Server")
            print("SANDBOX_E2E route=settings-add-server")
            shot(app, "1-settings")
            addServer.tap()
            let name = byPlaceholder("Name (e.g. Home Mac)")
            XCTAssertTrue(name.waitForExistence(timeout: 10), "the Add Server form opens")
            name.tap(); name.typeText("Sandbox")
            let url = byPlaceholder("URL (e.g. https://amux.tail-xxxx.ts.net:8824)")
            url.tap(); url.typeText(sandbox)
            shot(app, "1b-add-server-filled")
            app.buttons["Add"].tap()
            let row = app.staticTexts["Sandbox"]
            XCTAssertTrue(row.waitForExistence(timeout: 10), "the new server is listed")
            row.tap()
        }

        // The door's sign-in form, rendered inside the app's web view.
        let web = app.webViews.firstMatch
        XCTAssertTrue(web.waitForExistence(timeout: 30), "Connect opens the web view")
        let password = web.secureTextFields.matching(NSPredicate(format: "label == 'Password'")).firstMatch
        XCTAssertTrue(password.waitForExistence(timeout: 30),
                      "the sandbox sign-in page renders in the app (no blank screen, no error overlay)")
        XCTAssertFalse(app.staticTexts.containing(NSPredicate(format: "label CONTAINS[c] 'No response from'")).firstMatch.exists,
                       "the load watchdog did not fire")
        let toSignIn = Date().timeIntervalSince(started)
        shot(app, "2-sandbox-sign-in")
        print("SANDBOX_E2E reached_sign_in_s=\(String(format: "%.1f", toSignIn))")

        let env = ProcessInfo.processInfo.environment
        guard let user = env["SANDBOX_USER"], let pass = env["SANDBOX_PASS"], !user.isEmpty, !pass.isEmpty else {
            throw XCTSkip("sign-in page reached in \(Int(toSignIn))s; SANDBOX_USER/SANDBOX_PASS not set, so the signed-in half was not run")
        }

        // Wrong password first: the door must refuse and stay on the form.
        let username = web.textFields.matching(NSPredicate(format: "label == 'Username'")).firstMatch
        type(app, username, user)
        type(app, password, "definitely-not-the-password")
        web.buttons.matching(NSPredicate(format: "label == 'Sign in'")).firstMatch.tap()
        XCTAssertTrue(web.secureTextFields.matching(NSPredicate(format: "label == 'Password'")).firstMatch.waitForExistence(timeout: 20), "a wrong password stays on the sign-in page")
        shot(app, "3-wrong-password")

        // Real sign-in, then the dashboard.
        // The door keeps the username after a refusal. Retype it only when it differs: the
        // long-press "Select All" menu otherwise swallowed the taps on the password field.
        let user2 = web.textFields.matching(NSPredicate(format: "label == 'Username'")).firstMatch
        if (user2.value as? String) != user {
            user2.tap()
            user2.press(forDuration: 1.0)
            if app.menuItems["Select All"].waitForExistence(timeout: 2) { app.menuItems["Select All"].tap() }
            user2.typeText(user)
        }
        let pw = web.secureTextFields.matching(NSPredicate(format: "label == 'Password'")).firstMatch
        type(app, pw, pass)
        web.buttons.matching(NSPredicate(format: "label == 'Sign in'")).firstMatch.tap()
        // The tab reads "▦ Workers", so match on containing, not equality (2026-10-19).
        let hasWorkers = NSPredicate(format: "label CONTAINS 'Workers'")
        let workers = web.staticTexts.containing(hasWorkers).firstMatch
        XCTAssertTrue(workers.waitForExistence(timeout: 45), "signing in lands on the dashboard")
        shot(app, "4-dashboard")

        // Survives a relaunch: the session cookie is kept.
        app.terminate()
        app.launch()
        XCTAssertTrue(app.webViews.firstMatch.staticTexts.containing(hasWorkers).firstMatch.waitForExistence(timeout: 45),
                      "after a relaunch the app reopens the sandbox still signed in")
        shot(app, "5-after-relaunch")
    }
}
