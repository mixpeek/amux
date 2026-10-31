import Foundation
import CoreLocation

/// Location history: the pure parts (AMUX-5458, docs/design/location-history.md).
///
/// RAW CAPTURE (Ethan, 2026-10-01): every fix Core Location delivers is kept,
/// with every field, exactly as delivered. Nothing is thinned or dropped on the
/// phone; the server computes a cleaned view at query time. Kept apart from the
/// recorder so the unit tests drive the exact shipping conversion and buffer.

/// One raw fix, in the server's ingest shape (`/api/map/location/points`).
/// Core Location's "invalid" markers (a negative accuracy, speed or course)
/// are stored as delivered.
struct LocationSample: Codable, Equatable, Identifiable {
    var id: String
    var ts: Double
    var lat: Double
    var lon: Double
    var alt: Double?
    var h_acc: Double?
    var v_acc: Double?
    var speed: Double?
    var course: Double?
    var activity: String?
    var activity_conf: String?
    var source: String
    var ell_alt: Double? = nil
    var speed_acc: Double? = nil
    var course_acc: Double? = nil
    var floor: Int? = nil
    var simulated: Bool? = nil
    var accessory: Bool? = nil
    /// How old the fix was when it reached the app, in seconds.
    var age_s: Double? = nil

    /// Every field of a `CLLocation`, unchanged.
    init(raw loc: CLLocation, receivedAt now: Double, activity: String?, confidence: String?,
         source: String, id: String = UUID().uuidString) {
        self.id = id
        ts = loc.timestamp.timeIntervalSince1970
        lat = loc.coordinate.latitude
        lon = loc.coordinate.longitude
        alt = loc.altitude
        h_acc = loc.horizontalAccuracy
        v_acc = loc.verticalAccuracy
        speed = loc.speed
        course = loc.course
        self.activity = activity
        activity_conf = confidence
        self.source = source
        speed_acc = loc.speedAccuracy
        course_acc = loc.courseAccuracy
        floor = loc.floor?.level
        if let info = loc.sourceInformation {
            simulated = info.isSimulatedBySoftware
            accessory = info.isProducedByAccessory
        }
        ell_alt = loc.ellipsoidalAltitude
        age_s = now - ts
    }

    init(id: String, ts: Double, lat: Double, lon: Double, alt: Double?, h_acc: Double?, v_acc: Double?,
         speed: Double?, course: Double?, activity: String?, activity_conf: String?, source: String) {
        self.id = id; self.ts = ts; self.lat = lat; self.lon = lon; self.alt = alt
        self.h_acc = h_acc; self.v_acc = v_acc; self.speed = speed; self.course = course
        self.activity = activity; self.activity_conf = activity_conf; self.source = source
    }
}

/// One raw Core Motion transition (`/api/map/location/motion`).
struct MotionSample: Codable, Equatable, Identifiable {
    var id: String
    var ts: Double
    var stationary: Bool
    var walking: Bool
    var running: Bool
    var cycling: Bool
    var automotive: Bool
    var unknown: Bool
    var confidence: String
}

/// An iOS visit (CLVisit), in the server's ingest shape (`/location/visits`).
struct VisitSample: Codable, Equatable, Identifiable {
    var id: String
    var arrival: Double
    var departure: Double?
    var lat: Double
    var lon: Double
    var h_acc: Double?
}

/// Full detail records every fix while moving; battery saver records only
/// significant changes and visits.
enum LocationMode: String, CaseIterable {
    case full
    case saver
}

/// The `CLLocationManager` settings for standard updates: the iOS 16 Full
/// detail path and the iOS 17+ stall backstop. Kept pure so a test pins them.
struct StandardUpdatesConfig: Equatable {
    var desiredAccuracy: CLLocationAccuracy
    var distanceFilter: CLLocationDistance
    var activityType: CLActivityType
    var pausesAutomatically: Bool
    var allowsBackground: Bool
    var showsIndicator: Bool
}

enum LocationRules {
    /// Points per upload request.
    static let batchSize = 1000

    /// Standard updates for a mode, or nil when the mode runs none.
    ///
    /// ENERGY (2026-10-31, owner: "very efficient with energy/service use best
    /// practices"): `pausesAutomatically` is true. It was false, which kept
    /// navigation-grade GPS on at about 1 Hz for as long as the phone sat
    /// still, while the design doc and the Settings text promise "close to
    /// nothing while still". With it on, iOS stops the updates when the phone
    /// stays put and the recorder restarts them on the next significant-change
    /// or visit wake-up (the same services that already relaunch the app). The
    /// iOS 17+ live stream pauses itself and is unaffected. Accuracy and the
    /// distance filter stay raw: every fix while moving is the owner's call
    /// (Ethan, 2026-10-01, raw capture).
    static func standardUpdates(for mode: LocationMode) -> StandardUpdatesConfig? {
        switch mode {
        case .full:
            return StandardUpdatesConfig(desiredAccuracy: kCLLocationAccuracyBestForNavigation,
                                         distanceFilter: kCLDistanceFilterNone,
                                         activityType: .otherNavigation,
                                         pausesAutomatically: true,
                                         allowsBackground: true,
                                         showsIndicator: true)
        case .saver:
            return nil   // significant changes and visits only
        }
    }

    // MARK: Upload pacing
    //
    // ENERGY (2026-10-31): every upload powers up the cellular radio, which
    // then stays in a high-power tail for several seconds after the request.
    // Before this, a failed upload left `lastUpload` unchanged, so with no
    // network every new fix (about one a second in Full detail) fired another
    // request and re-read the whole buffer. Now an upload waits for its
    // interval since the last ATTEMPT, a failure backs off exponentially, and
    // the background interval is longer than the foreground one so a moving
    // phone wakes the radio a few times an hour instead of sixty. Nothing is
    // lost by waiting: points are on disk before any upload is tried.

    /// Seconds between uploads while amux is on screen.
    static let foregroundUploadS: Double = 60
    /// Seconds between uploads while amux records in the background.
    static let backgroundUploadS: Double = 300
    /// The longest a failing upload waits before trying again.
    static let maxBackoffS: Double = 1800

    /// How long to wait after `failures` failed uploads in a row.
    static func backoff(failures: Int) -> Double {
        guard failures > 0 else { return 0 }
        return min(60 * pow(2, Double(min(failures, 16) - 1)), maxBackoffS)
    }

    /// Whether an automatic upload should run now. `interval` is the pacing for
    /// the trigger (foreground, background, or 0 for "now": backgrounding, a
    /// visit, coming to the foreground). A full batch goes at once unless the
    /// server has been failing; a backoff holds every automatic trigger.
    static func uploadDue(waiting: Int, now: Double, lastAttempt: Double,
                          failures: Int, interval: Double) -> Bool {
        guard waiting > 0 else { return false }
        let since = now - lastAttempt
        if failures > 0 { return since >= backoff(failures: failures) }
        if waiting >= batchSize { return true }
        return since >= interval
    }

    /// Motion updates not stored yet, oldest first. Live updates and history
    /// queries overlap (the first live update repeats the current activity),
    /// so both go through this to store each once. An update with the same
    /// start time as the last stored one is kept when its flags or confidence
    /// changed (Core Motion revises an activity in place): raw capture.
    static func newMotion(_ items: [MotionSample], afterTs last: Double,
                          lastStored: MotionSample?) -> [MotionSample] {
        var out: [MotionSample] = []
        var cursorTs = last
        var cursor = lastStored
        for m in items.sorted(by: { $0.ts < $1.ts }) {
            if m.ts < cursorTs { continue }
            if m.ts == cursorTs {
                // Same start time: keep only a revision of a reading we know.
                guard let c = cursor, !sameMotion(c, m) else { continue }
            }
            out.append(m)
            cursorTs = m.ts
            cursor = m
        }
        return out
    }

    /// Same reading, ignoring the id.
    static func sameMotion(_ a: MotionSample, _ b: MotionSample) -> Bool {
        a.ts == b.ts && a.stationary == b.stationary && a.walking == b.walking && a.running == b.running
            && a.cycling == b.cycling && a.automotive == b.automotive && a.unknown == b.unknown
            && a.confidence == b.confidence
    }

    /// Haversine distance in metres.
    static func distance(_ aLat: Double, _ aLon: Double, _ bLat: Double, _ bLon: Double) -> Double {
        let r = 6_371_000.0
        let p1 = aLat * .pi / 180, p2 = bLat * .pi / 180
        let dp = (bLat - aLat) * .pi / 180, dl = (bLon - aLon) * .pi / 180
        let h = sin(dp / 2) * sin(dp / 2) + cos(p1) * cos(p2) * sin(dl / 2) * sin(dl / 2)
        return 2 * r * asin(min(1, sqrt(h)))
    }

    /// One word for Core Motion's flags, most specific first.
    static func activityName(automotive: Bool, cycling: Bool, running: Bool,
                             walking: Bool, stationary: Bool) -> String {
        if automotive { return "automotive" }
        if cycling { return "cycling" }
        if running { return "running" }
        if walking { return "walking" }
        if stationary { return "stationary" }
        return "unknown"
    }
}

/// A durable JSON-lines buffer. Points are written here BEFORE any upload is
/// attempted, and leave only when the server has confirmed them, so no network,
/// a killed app or a failed request never loses one.
final class DurableBuffer<T: Codable & Identifiable> where T.ID == String {
    let url: URL
    private let lock = NSLock()
    private var cachedCount: Int?

    init(url: URL) {
        self.url = url
        try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(),
                                                 withIntermediateDirectories: true)
    }

    /// Returns true only when every item reached the file, so a caller can
    /// count what was really stored rather than what it asked for.
    @discardableResult
    func append(_ items: [T]) -> Bool {
        guard !items.isEmpty else { return true }
        let enc = JSONEncoder()
        var blob = Data()
        for item in items {
            guard let line = try? enc.encode(item) else { return false }
            blob.append(line)
            blob.append(0x0A)
        }
        lock.lock(); defer { lock.unlock() }
        do {
            if let h = try? FileHandle(forUpdating: url) {
                defer { try? h.close() }
                let end = try h.seekToEnd()
                // A crash can leave a torn last line. Start on a fresh line so the
                // next good point is not glued onto it and lost with it.
                if end > 0 {
                    try h.seek(toOffset: end - 1)
                    if let last = try h.read(upToCount: 1), last.first != 0x0A {
                        blob.insert(0x0A, at: 0)
                    }
                    _ = try h.seekToEnd()
                }
                try h.write(contentsOf: blob)
            } else {
                try blob.write(to: url, options: .atomic)
            }
        } catch {
            cachedCount = nil
            return false
        }
        if let c = cachedCount { cachedCount = c + items.count }
        return true
    }

    /// Everything buffered, oldest first. A line that does not decode (a torn
    /// write from a crash) is skipped rather than blocking the rest.
    func all() -> [T] {
        lock.lock(); defer { lock.unlock() }
        return readLocked()
    }

    /// The oldest `n` items. Decodes only as many lines as it needs, so a
    /// large offline backlog costs one batch of decoding per upload rather
    /// than the whole file (2026-10-31, energy).
    func first(_ n: Int) -> [T] {
        guard n > 0 else { return [] }
        lock.lock(); defer { lock.unlock() }
        guard let data = try? Data(contentsOf: url, options: .mappedIfSafe), !data.isEmpty else { return [] }
        let dec = JSONDecoder()
        var out: [T] = []
        out.reserveCapacity(min(n, 1024))
        for line in data.split(separator: 0x0A) {
            if let item = try? dec.decode(T.self, from: Data(line)) {
                out.append(item)
                if out.count == n { break }
            }
        }
        return out
    }

    var count: Int {
        lock.lock(); defer { lock.unlock() }
        if let c = cachedCount { return c }
        let c = readLocked().count
        cachedCount = c
        return c
    }

    /// Drop the items the server confirmed. Rewritten atomically.
    func remove(ids: Set<String>) {
        guard !ids.isEmpty else { return }
        lock.lock(); defer { lock.unlock() }
        let keep = readLocked().filter { !ids.contains($0.id) }
        let enc = JSONEncoder()
        var blob = Data()
        for item in keep {
            if let line = try? enc.encode(item) {
                blob.append(line)
                blob.append(0x0A)
            }
        }
        try? blob.write(to: url, options: .atomic)
        cachedCount = keep.count
    }

    private func readLocked() -> [T] {
        guard let data = try? Data(contentsOf: url), !data.isEmpty else { return [] }
        let dec = JSONDecoder()
        return data.split(separator: 0x0A).compactMap { try? dec.decode(T.self, from: Data($0)) }
    }
}
