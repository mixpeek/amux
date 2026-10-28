import AVFoundation
import XCTest

/// The native recorder (desktop/amux-recorder-core.swift) on the iOS runtime:
/// the same writers, segments, manifest, gaps and recovery the phone app uses.
/// The source is the synthetic tone (`sources=tone`), so no permission prompt
/// and nothing from the room; the microphone path itself is proven on the Mac
/// (desktop/test/recorder-live-e2e.py) and on a device.
final class RecorderCoreTests: XCTestCase {
    var spool: URL!

    override func setUp() {
        super.setUp()
        spool = FileManager.default.temporaryDirectory.appendingPathComponent("rec-core-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: spool, withIntermediateDirectories: true)
        setenv("AMUX_RECORDER_SPOOL", spool.path, 1)
        setenv("AMUX_RECORDER_SEGMENT_S", "1", 1)
    }

    override func tearDown() {
        let r = NativeRecorder.shared
        if r.isActive { r.stop() }
        try? FileManager.default.removeItem(at: spool)
        super.tearDown()
    }

    private func wait(_ s: Double) { RunLoop.current.run(until: Date().addingTimeInterval(s)) }

    private func manifest(_ id: String) -> RecManifest? {
        NativeRecorder.shared.loadManifest(spool.appendingPathComponent(id))
    }

    func testARecordingIsWrittenAsSegmentsWithAManifest() throws {
        let r = NativeRecorder.shared
        XCTAssertNil(r.start(sources: ["tone"]))
        let id = try XCTUnwrap(r.status()["id"] as? String)
        wait(3.6)
        r.stop()
        let m = try XCTUnwrap(manifest(id))
        XCTAssertEqual(m.state, "stopped")
        XCTAssertGreaterThanOrEqual(m.segments.count, 3, "one second segments, written while recording")
        for s in m.segments {
            let f = try AVAudioFile(forReading: spool.appendingPathComponent(id).appendingPathComponent(s.file))
            XCTAssertGreaterThan(f.length, 0, "\(s.file) decodes")
        }
        XCTAssertEqual(m.gaps.count, 0, "\(m.gaps)")
    }

    func testAPhoneCallInterruptionIsAGapNotSilence() throws {
        let r = NativeRecorder.shared
        XCTAssertNil(r.start(sources: ["tone"]))
        let id = try XCTUnwrap(r.status()["id"] as? String)
        wait(1.5)
        // The audio session was taken: the handler iOS calls, then no audio.
        r.interrupted(true)
        r.sources.compactMap { $0 as? ToneSource }.forEach { $0.stall(2) }
        wait(3.5)
        r.interrupted(false)
        wait(1)
        r.stop()
        let m = try XCTUnwrap(manifest(id))
        XCTAssertTrue(m.gaps.contains { $0.why == "interruption" && $0.dur_ms >= 1500 }, "\(m.gaps)")
    }

    func testARecordingTheSystemKilledIsRecoveredOnTheNextLaunch() throws {
        let r = NativeRecorder.shared
        XCTAssertNil(r.start(sources: ["tone"]))
        let id = try XCTUnwrap(r.status()["id"] as? String)
        wait(2.5)
        // What a killed process leaves: its folder as it was, mid-recording,
        // with an open segment. Copied, because this process must not die.
        let dead = spool.appendingPathComponent(id + "x")
        try FileManager.default.copyItem(at: spool.appendingPathComponent(id), to: dead)
        r.stop()
        var m = try XCTUnwrap(r.loadManifest(dead))
        m.id = id + "x"
        m.state = "recording"
        r.writeManifest(m, dead)
        r.recoverInterrupted()
        let back = try XCTUnwrap(r.loadManifest(dead))
        XCTAssertEqual(back.state, "stopped")
        XCTAssertTrue(back.recovered)
        XCTAssertGreaterThanOrEqual(back.segments.count, 2)
    }
}
