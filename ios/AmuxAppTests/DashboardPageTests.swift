import XCTest

/// The back button shows on every page that is not the dashboard (DashboardPage.swift).
final class DashboardPageTests: XCTestCase {
    let server = URL(string: "https://sandbox.amux.io")!

    func testThePublicAppThatTrappedTheOwnerIsNotTheDashboard() {
        let app = URL(string: "https://sandbox.amux.io/pub/a383d8fca9250a2a9328a1dbfa5a8551/")!
        XCTAssertFalse(DashboardPage.isDashboard(app, server: server))
    }

    func testEveryOtherPageWithoutAmuxControlsIsNotTheDashboard() {
        for s in ["https://sandbox.amux.io/f/home/amux/a.html",
                  "https://sandbox.amux.io/pages/demo",
                  "https://sandbox.amux.io:8825/app/abc/",
                  "https://example.com/"] {
            XCTAssertFalse(DashboardPage.isDashboard(URL(string: s)!, server: server), s)
        }
    }

    func testTheDashboardAndItsSignInGetNoButton() {
        for s in ["https://sandbox.amux.io/",
                  "https://sandbox.amux.io",
                  "https://sandbox.amux.io/#view=apps&open=abc",
                  "https://sandbox.amux.io:443/?x=1#peek=mtp-demo",
                  "https://SANDBOX.amux.io/sign-in"] {
            XCTAssertTrue(DashboardPage.isDashboard(URL(string: s)!, server: server), s)
        }
        XCTAssertTrue(DashboardPage.isDashboard(nil, server: server), "no page yet")
        XCTAssertTrue(DashboardPage.isDashboard(URL(string: "about:blank")!, server: server), "the error overlay owns about:blank")
    }

    func testAServerOnAPortKeepsItsPort() {
        let s = URL(string: "https://100.1.2.3:8824/#view=apps")!
        XCTAssertTrue(DashboardPage.isDashboard(URL(string: "https://100.1.2.3:8824/")!, server: s))
        XCTAssertFalse(DashboardPage.isDashboard(URL(string: "https://100.1.2.3:8825/")!, server: s), "the apps port is another page")
        XCTAssertEqual(DashboardPage.root(of: s).absoluteString, "https://100.1.2.3:8824/")
        XCTAssertEqual(DashboardPage.root(of: URL(string: "https://sandbox.amux.io/#path=/x")!).absoluteString, "https://sandbox.amux.io/")
    }
}
