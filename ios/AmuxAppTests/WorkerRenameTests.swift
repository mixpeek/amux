import XCTest

/// A renamed worker keeps its place in the share sheet (2026-11-06: `workflows`
/// became `wexus` and a phone kept addressing the old name).
final class WorkerRenameTests: XCTestCase {
    private var suite: String!
    private var store: UserDefaults!

    override func setUp() {
        suite = "worker-rename-\(UUID().uuidString)"
        store = UserDefaults(suiteName: suite)
    }
    override func tearDown() { store.removePersistentDomain(forName: suite) }

    func testTheLastWorkerAndShareCountsMoveToTheNewName() {
        store.set("workflows", forKey: AmuxStore.lastWorkerKey)
        store.set(["workflows": 3, "wexus": 1, "rothco": 2], forKey: AmuxStore.shareCountsKey)
        store.set(["workflows": 200.0, "wexus": 100.0], forKey: AmuxStore.shareLastKey)

        let moved = AmuxStore.applyRenames(["workflows": "wexus"], in: store)

        XCTAssertEqual(moved, 2)
        XCTAssertEqual(store.string(forKey: AmuxStore.lastWorkerKey), "wexus")
        XCTAssertEqual(store.dictionary(forKey: AmuxStore.shareCountsKey) as? [String: Int],
                       ["wexus": 4, "rothco": 2])
        XCTAssertEqual(store.dictionary(forKey: AmuxStore.shareLastKey) as? [String: Double],
                       ["wexus": 200.0])
    }

    func testNamesThatWereNotRenamedAreLeftAlone() {
        store.set("rothco", forKey: AmuxStore.lastWorkerKey)
        store.set(["rothco": 2], forKey: AmuxStore.shareCountsKey)

        XCTAssertEqual(AmuxStore.applyRenames(["workflows": "wexus"], in: store), 0)
        XCTAssertEqual(store.string(forKey: AmuxStore.lastWorkerKey), "rothco")
        XCTAssertEqual(store.dictionary(forKey: AmuxStore.shareCountsKey) as? [String: Int], ["rothco": 2])
    }
}
