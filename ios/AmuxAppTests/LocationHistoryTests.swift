import XCTest
import CoreLocation

/// AMUX-5458: the shipping location-history conversion and buffer, compiled in
/// from AmuxApp/Sources/LocationHistory.swift (no copies). Raw capture: every
/// fix is kept with every field.
final class LocationHistoryTests: XCTestCase {

    private func sample(_ id: String, ts: Double, lat: Double = 40.7, lon: Double = -74.0,
                        activity: String? = "walking") -> LocationSample {
        LocationSample(id: id, ts: ts, lat: lat, lon: lon, alt: nil, h_acc: 5, v_acc: nil,
                       speed: nil, course: nil, activity: activity, activity_conf: "high", source: "test")
    }

    func testEveryRawFixKeepsEveryFieldIncludingInvalidMarkers() throws {
        // Core Location's "invalid" markers (-1) on a poor, stale, simulated
        // fix: raw capture stores all of it exactly as delivered.
        let t = Date(timeIntervalSince1970: 1_790_000_000)
        let info = CLLocationSourceInformation(softwareSimulationState: true, andExternalAccessoryState: false)
        let loc = CLLocation(coordinate: CLLocationCoordinate2D(latitude: 40.7, longitude: -74.0),
                             altitude: 12, horizontalAccuracy: 450, verticalAccuracy: -1,
                             course: -1, courseAccuracy: -1, speed: -1, speedAccuracy: -1,
                             timestamp: t, sourceInfo: info)
        let s = LocationSample(raw: loc, receivedAt: 1_790_000_120, activity: "walking",
                               confidence: "high", source: "live", id: "x")
        XCTAssertEqual(s.ts, 1_790_000_000)
        XCTAssertEqual(s.h_acc, 450)
        XCTAssertEqual(s.v_acc, -1)
        XCTAssertEqual(s.alt, 12)
        XCTAssertEqual(s.speed, -1)
        XCTAssertEqual(s.speed_acc, -1)
        XCTAssertEqual(s.course, -1)
        XCTAssertEqual(s.course_acc, -1)
        XCTAssertEqual(s.simulated, true)
        XCTAssertEqual(s.accessory, false)
        XCTAssertEqual(s.age_s, 120)
        XCTAssertNotNil(s.ell_alt)
        // The upload body carries those keys by their server names.
        let json = try XCTUnwrap(try JSONSerialization.jsonObject(with: JSONEncoder().encode(s)) as? [String: Any])
        for key in ["ell_alt", "speed_acc", "course_acc", "simulated", "accessory", "age_s", "h_acc", "v_acc"] {
            XCTAssertNotNil(json[key], key)
        }
    }

    func testMotionSamplesRoundTripThroughTheBuffer() {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("motion-\(UUID().uuidString)/motion.jsonl")
        let buf = DurableBuffer<MotionSample>(url: url)
        let m = MotionSample(id: "m1", ts: 1, stationary: false, walking: true, running: false,
                             cycling: false, automotive: false, unknown: false, confidence: "high")
        XCTAssertTrue(buf.append([m]))
        XCTAssertEqual(DurableBuffer<MotionSample>(url: url).all(), [m])
    }

    func testActivityNamePrefersTheMostSpecificFlag() {
        XCTAssertEqual(LocationRules.activityName(automotive: true, cycling: false, running: false, walking: true, stationary: true), "automotive")
        XCTAssertEqual(LocationRules.activityName(automotive: false, cycling: false, running: false, walking: false, stationary: false), "unknown")
    }

    func testBufferKeepsEverythingUntilConfirmedAndSurvivesReopening() throws {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("loc-\(UUID().uuidString)/points.jsonl")
        let buf = DurableBuffer<LocationSample>(url: url)
        XCTAssertTrue(buf.append([sample("1", ts: 1), sample("2", ts: 2), sample("3", ts: 3)]))
        XCTAssertEqual(buf.count, 3)
        XCTAssertEqual(buf.first(2).map(\.id), ["1", "2"])
        // An upload confirmed 1 and 2; 3 stays, and a fresh instance (the app
        // relaunched) still sees it.
        buf.remove(ids: ["1", "2"])
        let reopened = DurableBuffer<LocationSample>(url: url)
        XCTAssertEqual(reopened.all().map(\.id), ["3"])
        // A torn last line from a crash does not hide the good ones.
        let h = try FileHandle(forWritingTo: url)
        try h.seekToEnd()
        try h.write(contentsOf: Data("{\"id\":\"broken\",\"ts\":".utf8))
        try h.close()
        XCTAssertEqual(reopened.all().map(\.id), ["3"])
        // And the next point after the torn line is not lost with it.
        reopened.append([sample("4", ts: 4)])
        XCTAssertEqual(reopened.all().map(\.id), ["3", "4"])
        XCTAssertEqual(reopened.count, 2, "the cached count follows appends and removals")
    }

    // MARK: - Energy (2026-10-31)

    func testFullDetailStandardUpdatesPauseWhenStillAndSaverRunsNone() throws {
        let c = try XCTUnwrap(LocationRules.standardUpdates(for: .full))
        // Raw while moving: navigation grade, no distance filter.
        XCTAssertEqual(c.desiredAccuracy, kCLLocationAccuracyBestForNavigation)
        XCTAssertEqual(c.distanceFilter, kCLDistanceFilterNone)
        XCTAssertEqual(c.activityType, .otherNavigation)
        // Still: iOS may stop the GPS; a wake-up restarts it.
        XCTAssertTrue(c.pausesAutomatically)
        XCTAssertTrue(c.allowsBackground)
        XCTAssertTrue(c.showsIndicator)
        XCTAssertNil(LocationRules.standardUpdates(for: .saver))
    }

    func testBackoffDoublesFromAMinuteAndCapsAtHalfAnHour() {
        XCTAssertEqual(LocationRules.backoff(failures: 0), 0)
        XCTAssertEqual(LocationRules.backoff(failures: 1), 60)
        XCTAssertEqual(LocationRules.backoff(failures: 2), 120)
        XCTAssertEqual(LocationRules.backoff(failures: 5), 960)
        XCTAssertEqual(LocationRules.backoff(failures: 6), 1800)
        XCTAssertEqual(LocationRules.backoff(failures: 500), 1800)
    }

    func testOfflineFixesDoNotFireARequestEach() {
        // The pre-fix shape: offline, one fix a second, every fix tried again.
        // Now a failed attempt holds every automatic trigger for its backoff.
        let t0 = 1_000_000.0
        for s in stride(from: 1.0, to: 60, by: 1) {
            XCTAssertFalse(LocationRules.uploadDue(waiting: 2000, now: t0 + s, lastAttempt: t0,
                                                   failures: 1, interval: 0), "second \(s)")
        }
        XCTAssertTrue(LocationRules.uploadDue(waiting: 2000, now: t0 + 60, lastAttempt: t0,
                                              failures: 1, interval: 0))
    }

    func testUploadPacingByTrigger() {
        let t0 = 1_000_000.0
        // Nothing waiting: never.
        XCTAssertFalse(LocationRules.uploadDue(waiting: 0, now: t0 + 9999, lastAttempt: 0, failures: 0, interval: 0))
        // Background: five minutes between radio wake-ups.
        XCTAssertFalse(LocationRules.uploadDue(waiting: 10, now: t0 + 299, lastAttempt: t0, failures: 0,
                                               interval: LocationRules.backgroundUploadS))
        XCTAssertTrue(LocationRules.uploadDue(waiting: 10, now: t0 + 300, lastAttempt: t0, failures: 0,
                                              interval: LocationRules.backgroundUploadS))
        // Foreground: a minute.
        XCTAssertTrue(LocationRules.uploadDue(waiting: 10, now: t0 + 60, lastAttempt: t0, failures: 0,
                                              interval: LocationRules.foregroundUploadS))
        // A full batch goes at once while healthy.
        XCTAssertTrue(LocationRules.uploadDue(waiting: LocationRules.batchSize, now: t0 + 1, lastAttempt: t0,
                                              failures: 0, interval: LocationRules.backgroundUploadS))
        // Immediate triggers (backgrounding, a visit, foreground).
        XCTAssertTrue(LocationRules.uploadDue(waiting: 1, now: t0 + 1, lastAttempt: t0, failures: 0, interval: 0))
    }

    private func motion(_ ts: Double, walking: Bool = true, conf: String = "high", id: String = UUID().uuidString) -> MotionSample {
        MotionSample(id: id, ts: ts, stationary: !walking, walking: walking, running: false,
                     cycling: false, automotive: false, unknown: false, confidence: conf)
    }

    func testMotionFromLiveAndHistoryIsStoredOnce() {
        let stored = motion(100)
        // A history query and the first live update both repeat the current
        // activity; only the newer transitions are new.
        let fresh = LocationRules.newMotion([motion(200, walking: false), motion(100), motion(50)],
                                            afterTs: 100, lastStored: stored)
        XCTAssertEqual(fresh.map(\.ts), [200])
        // A revision of the current activity (confidence rose) is raw data: kept.
        let revised = LocationRules.newMotion([motion(100, conf: "medium")], afterTs: 100, lastStored: stored)
        XCTAssertEqual(revised.map(\.confidence), ["medium"])
        // After a relaunch only the timestamp is known: same start time is skipped.
        XCTAssertTrue(LocationRules.newMotion([motion(100, conf: "medium")], afterTs: 100, lastStored: nil).isEmpty)
        // Out of order in, oldest first out.
        XCTAssertEqual(LocationRules.newMotion([motion(300), motion(200)], afterTs: 0, lastStored: nil).map(\.ts), [200, 300])
    }

    func testFirstDecodesOnlyWhatItReturnsAndSkipsTornLines() throws {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("first-\(UUID().uuidString)/points.jsonl")
        let buf = DurableBuffer<LocationSample>(url: url)
        XCTAssertTrue(buf.first(5).isEmpty)
        XCTAssertTrue(buf.append((0..<10).map { sample("p\($0)", ts: Double($0)) }))
        // A torn line from a crash in the middle.
        let h = try FileHandle(forWritingTo: url)
        try h.seekToEnd(); try h.write(contentsOf: Data("{\"id\":\"torn".utf8)); try h.close()
        XCTAssertTrue(buf.append([sample("p10", ts: 10)]))
        XCTAssertEqual(buf.first(3).map(\.id), ["p0", "p1", "p2"])
        XCTAssertEqual(buf.first(100).map(\.id), buf.all().map(\.id))
        XCTAssertEqual(buf.first(100).count, 11)
        XCTAssertTrue(buf.first(0).isEmpty)
    }
}
