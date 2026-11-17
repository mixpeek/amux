import XCTest

/// Switching servers keeps each server's dashboard loaded (ServerPanes.swift).
final class ServerPanesTests: XCTestCase {
    let home = URL(string: "https://home.tail-1.ts.net:8824")!
    let cloud = URL(string: "https://sandbox.amux.io")!

    func testSwitchingBackResumesThePageThatWasOpen() {
        var p = ServerPanes()
        XCTAssertEqual(p.show(home, navigate: false).verdict, .opened)
        // A deep link moved the home pane to a file.
        let file = URL(string: "https://home.tail-1.ts.net:8824/#path=/notes.md")!
        XCTAssertEqual(p.show(file, navigate: true).verdict, .navigated)
        XCTAssertEqual(p.show(cloud, navigate: false).verdict, .opened)
        XCTAssertEqual(p.active?.url, cloud)
        // Back to home from the switcher (its saved address): the file is still the page.
        XCTAssertEqual(p.show(home, navigate: false).verdict, .resumed)
        XCTAssertEqual(p.active?.url, file, "a switch must not reload the pane")
        XCTAssertEqual(p.panes.map(\.key), ["https://home.tail-1.ts.net:8824", "https://sandbox.amux.io"])
    }

    func testTheSameServerIsOnePaneWhateverTheCaseOrPath() {
        var p = ServerPanes()
        p.show(URL(string: "https://Sandbox.AMUX.io/sign-in?t=1")!, navigate: false)
        XCTAssertEqual(p.show(cloud, navigate: false).verdict, .resumed)
        XCTAssertEqual(p.panes.count, 1)
        XCTAssertNotEqual(ServerPanes.key(of: URL(string: "https://sandbox.amux.io:8825")!), ServerPanes.key(of: cloud),
                          "another port is another server")
    }

    func testTheLeastRecentPaneClosesPastTheLimit() {
        var p = ServerPanes()
        let urls = (1...ServerPanes.limit + 1).map { URL(string: "https://s\($0).example.com")! }
        var closed: [String] = []
        for u in urls { if let c = p.show(u, navigate: false).closed { closed.append(c) } }
        XCTAssertEqual(p.panes.count, ServerPanes.limit)
        XCTAssertEqual(closed, ["https://s1.example.com"])
        XCTAssertEqual(p.active?.url, urls.last)
    }

    func testANonWebAddressChangesNothing() {
        var p = ServerPanes()
        p.show(home, navigate: false)
        XCTAssertEqual(p.show(URL(string: "amux://open?server=x")!, navigate: false).verdict, .refused)
        XCTAssertEqual(p.active?.url, home)
        p.close("https://home.tail-1.ts.net:8824")
        XCTAssertNil(p.active)
    }
}
