import Foundation
import CoreLocation
import CoreMotion
import UIKit
import os

private let log = Logger(subsystem: "io.amux.app", category: "Location")

/// Records the owner's location history and uploads it to their own amux server
/// (AMUX-5458, docs/design/location-history.md). Off until the owner turns it on.
///
/// - iOS 17+: `CLLocationUpdate.liveUpdates` held by a `CLBackgroundActivitySession`,
///   which keeps updates flowing in the background and slows them down when still.
/// - iOS 16: `CLLocationManager` standard updates with background updates on.
/// - Always: visits + significant-location-change monitoring. These are what
///   relaunch an app iOS has terminated, so recording comes back by itself.
/// - Core Motion labels each point and its raw transitions are stored too.
///   Full detail runs live activity updates; Battery saver asks the motion
///   coprocessor for its history at each wake-up instead (it keeps 7 days).
/// - Energy (2026-10-31): standard updates pause when still, uploads are paced
///   and back off on failure, the timer allows coalescing, and turning the
///   switch off stops every service. Each decision logs under
///   `io.amux.app` / `Location` and shows in `status`.
/// - RAW: every fix delivered is stored with every field, nothing thinned or
///   dropped; `delivered` and `stored` count both sides so a gap is visible.
/// - Every point goes to a durable buffer first; uploads drain it.
@MainActor
final class LocationRecorder: NSObject, ObservableObject {
    static let shared = LocationRecorder()

    static let enabledKey = "locationHistoryEnabled"
    static let lastUploadKey = "locationHistoryLastUpload"
    static let modeKey = "locationHistoryMode"
    static let deliveredKey = "locationHistoryDelivered"
    static let storedKey = "locationHistoryStored"
    static let lastMotionKey = "locationHistoryLastMotion"
    static let statusChanged = Notification.Name("amuxLocationStatusChanged")

    @Published private(set) var enabled = UserDefaults.standard.bool(forKey: LocationRecorder.enabledKey)
    @Published private(set) var authorization: CLAuthorizationStatus = .notDetermined
    @Published private(set) var precise = true
    @Published private(set) var motionState = "unknown"
    @Published private(set) var pending = 0
    @Published private(set) var lastUpload: Double = UserDefaults.standard.double(forKey: LocationRecorder.lastUploadKey)
    @Published private(set) var lastError: String?
    @Published private(set) var mode = LocationMode(rawValue: UserDefaults.standard.string(forKey: LocationRecorder.modeKey) ?? "") ?? .full
    /// Lifetime counts: fixes Core Location handed over, and fixes written to
    /// the buffer. Raw capture means these are equal; a gap is a lost fix.
    @Published private(set) var delivered = UserDefaults.standard.integer(forKey: LocationRecorder.deliveredKey)
    @Published private(set) var stored = UserDefaults.standard.integer(forKey: LocationRecorder.storedKey)
    @Published private(set) var motionPending = 0

    private let manager = CLLocationManager()
    private let motion = CMMotionActivityManager()
    private var activity: (name: String, confidence: String) = ("unknown", "low")
    private var liveTask: Task<Void, Never>?
    private var backgroundSession: AnyObject?      // CLBackgroundActivitySession on iOS 17+
    private var serviceSession: AnyObject?         // CLServiceSession on iOS 18+
    private var uploadTimer: Timer?
    private var uploading = false
    /// Upload pacing (LocationRules.uploadDue): when the last request was tried
    /// and how many failed in a row, so no network means a backoff, not a
    /// request per fix.
    private var lastAttempt: Double = 0
    @Published private(set) var uploadFailures = 0
    /// iOS paused standard updates because the phone stayed still.
    private var managerPaused = false
    /// Newest motion transition stored, so live updates and history queries
    /// never store the same transition twice.
    private var lastMotionTs = UserDefaults.standard.double(forKey: LocationRecorder.lastMotionKey)
    private var lastMotion: MotionSample?
    private var lastMotionQuery: Double = 0
    /// Live-stream health (AMUX-5549). 2026-10-03: live fixes stopped at 9:31
    /// and nothing restarted them; only wake-up fixes and visits arrived until
    /// evening, so every move that day went unrecorded. The stream is now
    /// checked on every wake-up, foreground and timer tick, and restarted.
    private var lastLiveFix: Double = 0
    private var liveStationary = false
    private var lastLiveRestart: Double = 0
    @Published private(set) var liveRestarts = 0
    /// Standard updates run as a backstop after a stall until live fixes resume.
    private var backstop = false
    private static let liveStallS: Double = 180

    let points: DurableBuffer<LocationSample>
    let visits: DurableBuffer<VisitSample>
    let motionLog: DurableBuffer<MotionSample>

    private override init() {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("location-history", isDirectory: true)
        points = DurableBuffer(url: dir.appendingPathComponent("points.jsonl"))
        visits = DurableBuffer(url: dir.appendingPathComponent("visits.jsonl"))
        motionLog = DurableBuffer(url: dir.appendingPathComponent("motion.jsonl"))
        super.init()
        manager.delegate = self
        authorization = manager.authorizationStatus
        precise = manager.accuracyAuthorization == .fullAccuracy
        pending = points.count
        motionPending = motionLog.count
        NotificationCenter.default.addObserver(forName: UIApplication.didEnterBackgroundNotification,
                                               object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in self?.uploadInBackgroundTask() }
        }
        NotificationCenter.default.addObserver(forName: UIApplication.didBecomeActiveNotification,
                                               object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in
                guard let self, self.enabled else { return }
                self.ensureLive(moved: false, why: "foreground")
                self.backfillMotion(why: "foreground")
                // The radio is up for the dashboard anyway: send what waits.
                self.maybeUpload(interval: 0, why: "foreground")
            }
        }
    }

    /// Called at every launch, including a background relaunch for a location
    /// event: picks recording back up if the owner left it on.
    func resumeIfEnabled() {
        if enabled { start() }
    }

    func setEnabled(_ on: Bool) {
        enabled = on
        UserDefaults.standard.set(on, forKey: Self.enabledKey)
        if on { start() } else { stop() }
        publish()
    }

    func setMode(_ m: LocationMode) {
        guard m != mode else { return }
        mode = m
        UserDefaults.standard.set(m.rawValue, forKey: Self.modeKey)
        if enabled {
            stopContinuous()
            startContinuous()
            startMotion()   // live updates in Full detail, history queries in saver
        }
        log.info("location mode=\(m.rawValue, privacy: .public)")
        publish()
    }

    // MARK: - Recording

    private func start() {
        switch manager.authorizationStatus {
        case .notDetermined:
            manager.requestWhenInUseAuthorization()   // Always is asked once this is granted
        case .authorizedWhenInUse:
            manager.requestAlwaysAuthorization()
        default: break
        }
        if manager.accuracyAuthorization == .reducedAccuracy {
            manager.requestTemporaryFullAccuracyAuthorization(withPurposeKey: "history")
        }
        startMotion()
        manager.startMonitoringVisits()
        if CLLocationManager.significantLocationChangeMonitoringAvailable() {
            manager.startMonitoringSignificantLocationChanges()
        }
        startContinuous()
        if uploadTimer == nil {
            // The stall check and upload pacing need a tick, not a precise one:
            // the tolerance lets iOS fold it into other wake-ups.
            let t = Timer(timeInterval: 60, repeats: true) { [weak self] _ in
                Task { @MainActor in
                    guard let self else { return }
                    self.ensureLive(moved: false, why: "timer")
                    self.maybeUpload(interval: self.uploadInterval, why: "timer")
                }
            }
            t.tolerance = 15
            RunLoop.main.add(t, forMode: .common)
            uploadTimer = t
        }
        log.info("location start: mode=\(self.mode.rawValue, privacy: .public) visits=on significant_change=\(CLLocationManager.significantLocationChangeMonitoringAvailable(), privacy: .public) motion=\(self.mode == .full ? "live" : "query", privacy: .public)")
        publish()
    }

    /// Standard updates with the mode's settings (iOS 16 Full detail, and the
    /// iOS 17+ backstop). Background updates are allowed only while they run.
    private func startStandardUpdates(why: String) {
        guard let c = LocationRules.standardUpdates(for: mode) else { return }
        manager.desiredAccuracy = c.desiredAccuracy
        manager.distanceFilter = c.distanceFilter
        manager.activityType = c.activityType
        manager.pausesLocationUpdatesAutomatically = c.pausesAutomatically
        manager.allowsBackgroundLocationUpdates = c.allowsBackground
        manager.showsBackgroundLocationIndicator = c.showsIndicator
        managerPaused = false
        manager.startUpdatingLocation()
        log.info("location standard updates on (\(why, privacy: .public)): pauses_when_still=\(c.pausesAutomatically, privacy: .public)")
    }

    private func stopStandardUpdates() {
        manager.stopUpdatingLocation()
        manager.allowsBackgroundLocationUpdates = false
        manager.showsBackgroundLocationIndicator = false
        managerPaused = false
    }

    /// Full detail only: every fix, navigation-grade, no distance filter.
    /// Battery saver relies on visits and significant changes alone.
    private func startContinuous() {
        guard mode == .full else { return }
        if #available(iOS 17.0, *) {
            startLiveUpdates()
        } else {
            startStandardUpdates(why: "full detail")
        }
    }

    private func stopContinuous() {
        liveTask?.cancel()
        liveTask = nil
        backstop = false
        if #available(iOS 17.0, *) {
            (backgroundSession as? CLBackgroundActivitySession)?.invalidate()
        }
        if #available(iOS 18.0, *) {
            (serviceSession as? CLServiceSession)?.invalidate()
        }
        backgroundSession = nil
        serviceSession = nil
        stopStandardUpdates()
    }

    /// Off means off: no location service, motion stream, session or timer is
    /// left running. One last upload sends what is already buffered.
    private func stop() {
        stopContinuous()
        manager.stopMonitoringVisits()
        manager.stopMonitoringSignificantLocationChanges()
        motion.stopActivityUpdates()
        uploadTimer?.invalidate()
        uploadTimer = nil
        log.info("location stop: live, standard, visits, significant_change, motion and timer all off")
        Task { await upload() }   // send whatever is already buffered
    }

    @available(iOS 17.0, *)
    private func startLiveUpdates() {
        guard liveTask == nil else { return }
        if backgroundSession == nil { backgroundSession = CLBackgroundActivitySession() }
        if #available(iOS 18.0, *), serviceSession == nil {
            serviceSession = CLServiceSession(authorization: .always, fullAccuracyPurposeKey: "history")
        }
        liveTask = Task { [weak self] in
            var why = "live updates ended"
            do {
                for try await update in CLLocationUpdate.liveUpdates(.otherNavigation) {
                    if Task.isCancelled { return }
                    let still: Bool
                    if #available(iOS 18.0, *) {
                        still = update.stationary
                        // iOS 18 says why a stream carries no location; keep it visible.
                        let blocked = [(update.authorizationDenied, "authorization denied"),
                                       (update.authorizationDeniedGlobally, "Location Services off"),
                                       (update.insufficientlyInUse, "needs Always or an active session"),
                                       (update.serviceSessionRequired, "service session required"),
                                       (update.locationUnavailable, "location unavailable")]
                            .filter(\.0).map(\.1)
                        if !blocked.isEmpty { await self?.noteError("live updates: " + blocked.joined(separator: ", ")) }
                    } else {
                        still = update.isStationary
                    }
                    await self?.noteLive(stationary: still, fix: update.location)
                }
            } catch {
                why = "live updates stopped: \(error.localizedDescription)"
            }
            if Task.isCancelled { return }
            // The stream ended by itself: clear it so the next check restarts it.
            await self?.liveEnded(why)
        }
    }

    private func noteLive(stationary: Bool, fix: CLLocation?) {
        liveStationary = stationary
        guard let fix else { return }
        lastLiveFix = Date().timeIntervalSince1970
        if backstop {
            backstop = false
            stopStandardUpdates()
            log.info("location backstop off: live fixes are back")
        }
        record([fix], source: "live")
    }

    private func liveEnded(_ why: String) {
        liveTask = nil
        noteError(why)
        ensureLive(moved: true, why: "stream ended")
    }

    /// Restart the live stream when it is missing, or silent while the phone
    /// moves. `moved` is true for a significant-change or visit wake-up, which
    /// iOS only sends after real movement, so a stale stationary flag is ignored.
    func ensureLive(moved: Bool, why: String) {
        guard enabled, mode == .full else { return }
        guard #available(iOS 17.0, *) else {
            // iOS 16: standard updates paused while still; a wake-up after
            // real movement turns them back on.
            if moved && managerPaused { startStandardUpdates(why: "resume after pause: \(why)") }
            return
        }
        if moved && backstop && managerPaused {
            // The backstop paused while still and the phone has moved again.
            startStandardUpdates(why: "backstop resume: \(why)")
        }
        let now = Date().timeIntervalSince1970
        let silent = now - lastLiveFix > Self.liveStallS
        let stalled = liveTask == nil || (silent && (moved || !liveStationary))
        guard stalled, now - lastLiveRestart > 120 else { return }
        lastLiveRestart = now
        liveRestarts += 1
        liveTask?.cancel()
        liveTask = nil
        // The CLBackgroundActivitySession is kept: it can only be created in the
        // foreground, and holding it is what lets the new stream run here.
        startLiveUpdates()
        if moved && UIApplication.shared.applicationState != .active {
            // Backstop until live fixes come back: standard updates keep a
            // woken app running and recording in the background.
            backstop = true
            startStandardUpdates(why: "backstop: \(why)")
        }
        log.notice("location live stream restarted (\(why, privacy: .public)) restarts=\(self.liveRestarts, privacy: .public)")
        lastError = "live stream restarted (\(why))"
        publish()
    }

    /// Full detail: live activity updates, since the app runs while moving and
    /// labels a fix a second. Battery saver: no live stream. The app is
    /// suspended between wake-ups there, so live updates would deliver nothing
    /// while it sleeps; the coprocessor keeps 7 days of history, and each
    /// wake-up asks it once for everything since the last stored transition
    /// (Apple's documented pattern for a suspended app). Both modes backfill on
    /// start and foreground, which also recovers transitions from while the
    /// app was terminated.
    private func startMotion() {
        guard CMMotionActivityManager.isActivityAvailable() else {
            motionState = "unavailable"
            return
        }
        motion.stopActivityUpdates()
        if mode == .full {
            motion.startActivityUpdates(to: .main) { [weak self] a in
                guard let a else { return }
                Task { @MainActor in self?.storeMotion([a]) }
            }
        }
        backfillMotion(why: "start", force: true)
        switch CMMotionActivityManager.authorizationStatus() {
        case .denied: motionState = "denied in iOS Settings"
        case .restricted: motionState = "restricted"
        case .authorized: motionState = "on"
        default: motionState = "asking"
        }
    }

    private static func motionSample(_ a: CMMotionActivity) -> MotionSample {
        let conf: String
        switch a.confidence {
        case .high: conf = "high"
        case .medium: conf = "medium"
        default: conf = "low"
        }
        return MotionSample(id: UUID().uuidString, ts: a.startDate.timeIntervalSince1970,
                            stationary: a.stationary, walking: a.walking, running: a.running,
                            cycling: a.cycling, automotive: a.automotive, unknown: a.unknown,
                            confidence: conf)
    }

    /// Store transitions not stored yet and take the newest as the label.
    @discardableResult
    private func storeMotion(_ acts: [CMMotionActivity]) -> Int {
        let fresh = LocationRules.newMotion(acts.map(Self.motionSample), afterTs: lastMotionTs,
                                            lastStored: lastMotion)
        guard let newest = fresh.last else { return 0 }
        lastMotion = newest
        activity = (LocationRules.activityName(automotive: newest.automotive, cycling: newest.cycling,
                                               running: newest.running, walking: newest.walking,
                                               stationary: newest.stationary), newest.confidence)
        motionState = "on"
        if motionLog.append(fresh) { motionPending += fresh.count }
        lastMotionTs = newest.ts
        UserDefaults.standard.set(lastMotionTs, forKey: Self.lastMotionKey)
        return fresh.count
    }

    /// Ask the motion coprocessor for transitions since the last stored one.
    /// At most once a minute unless forced; `then` runs after the label is
    /// current, so a wake-up fix is labelled with the activity that woke it.
    private func backfillMotion(why: String, force: Bool = false, then: (() -> Void)? = nil) {
        let now = Date().timeIntervalSince1970
        guard enabled, CMMotionActivityManager.isActivityAvailable(),
              force || now - lastMotionQuery >= 60 else { then?(); return }
        lastMotionQuery = now
        // No stored cursor yet (first enable, or the first run of the build
        // that added it): start from now. A week of history from before would
        // re-send transitions the live stream already uploaded, under new ids.
        let from = lastMotionTs > 0 ? max(lastMotionTs, now - 7 * 86_400) : now - 60
        motion.queryActivityStarting(from: Date(timeIntervalSince1970: from), to: Date(), to: .main) { [weak self] acts, err in
            Task { @MainActor in
                guard let self else { return }
                let n = self.storeMotion(acts ?? [])
                if let err {
                    log.error("location motion query failed (\(why, privacy: .public)): \(err.localizedDescription, privacy: .public)")
                } else if n > 0 {
                    log.info("location motion backfill (\(why, privacy: .public)): stored=\(n, privacy: .public)")
                }
                then?()
            }
        }
    }

    /// RAW: every fix delivered is stored, unchanged. No accuracy, age or
    /// distance filter here; the server's cleaned view does that at query time.
    private func record(_ locs: [CLLocation], source: String) {
        guard !locs.isEmpty else { return }
        let now = Date().timeIntervalSince1970
        delivered += locs.count
        let samples = locs.map {
            LocationSample(raw: $0, receivedAt: now, activity: activity.name,
                           confidence: activity.confidence, source: source)
        }
        if points.append(samples) {
            stored += samples.count
            pending += samples.count
        } else {
            lastError = "could not write \(samples.count) fix(es) to the buffer"
        }
        UserDefaults.standard.set(delivered, forKey: Self.deliveredKey)
        UserDefaults.standard.set(stored, forKey: Self.storedKey)
        maybeUpload(interval: uploadInterval, why: "fix")
    }

    // MARK: - Upload pacing

    private var uploadInterval: Double {
        UIApplication.shared.applicationState == .active
            ? LocationRules.foregroundUploadS : LocationRules.backgroundUploadS
    }

    /// Upload when LocationRules.uploadDue says so. Every automatic trigger
    /// comes through here; "Upload now" and the dashboard call upload() direct.
    private func maybeUpload(interval: Double, why: String) {
        let waiting = pending + motionPending + visits.count
        guard LocationRules.uploadDue(waiting: waiting, now: Date().timeIntervalSince1970,
                                      lastAttempt: lastAttempt, failures: uploadFailures,
                                      interval: interval) else { return }
        log.debug("location upload due (\(why, privacy: .public)): waiting=\(waiting, privacy: .public)")
        Task { await upload() }
    }

    // MARK: - Upload

    private var device: String {
        "iphone-" + (UIDevice.current.identifierForVendor?.uuidString.prefix(8).lowercased() ?? "unknown")
    }

    /// Drain the buffers to the server, 1000 points per request. A point leaves
    /// the buffer only once the server accepted it, already had it, or refused
    /// it for good (a refused point would never be accepted on retry).
    func upload() async {
        guard !uploading, let server = AmuxStore.serverURL else { return }
        uploading = true
        lastAttempt = Date().timeIntervalSince1970
        var sent = 0
        defer { uploading = false; pending = points.count; motionPending = motionLog.count; publish() }
        do {
            while true {
                let batch = points.first(LocationRules.batchSize)
                if batch.isEmpty { break }
                let body: [String: Any] = [
                    "device": device,
                    "points": try batch.map { try JSONSerialization.jsonObject(with: JSONEncoder().encode($0)) },
                ]
                let reply = try await AmuxClient.postJSON(path: "api/map/location/points", body: body, server: server)
                guard reply["ok"] as? Bool == true else {
                    throw AmuxClient.ClientError.malformed("upload not acknowledged")
                }
                points.remove(ids: Set(batch.map(\.id)))
                sent += batch.count
                lastUpload = Date().timeIntervalSince1970
                UserDefaults.standard.set(lastUpload, forKey: Self.lastUploadKey)
                if batch.count < LocationRules.batchSize { break }
            }
            while true {
                let batch = motionLog.first(LocationRules.batchSize)
                if batch.isEmpty { break }
                let body: [String: Any] = [
                    "device": device,
                    "motion": try batch.map { try JSONSerialization.jsonObject(with: JSONEncoder().encode($0)) },
                ]
                let reply = try await AmuxClient.postJSON(path: "api/map/location/motion", body: body, server: server)
                guard reply["ok"] as? Bool == true else {
                    throw AmuxClient.ClientError.malformed("motion upload not acknowledged")
                }
                motionLog.remove(ids: Set(batch.map(\.id)))
                if batch.count < LocationRules.batchSize { break }
            }
            let v = visits.all()
            if !v.isEmpty {
                let body: [String: Any] = [
                    "device": device,
                    "visits": try v.map { try JSONSerialization.jsonObject(with: JSONEncoder().encode($0)) },
                ]
                let reply = try await AmuxClient.postJSON(path: "api/map/location/visits", body: body, server: server)
                if reply["ok"] as? Bool == true { visits.remove(ids: Set(v.map(\.id))) }
            }
            lastError = nil
            if uploadFailures > 0 || sent > 0 {
                log.info("location upload ok: points=\(sent, privacy: .public) after_failures=\(self.uploadFailures, privacy: .public)")
            }
            uploadFailures = 0
        } catch {
            uploadFailures += 1
            lastError = error.localizedDescription
            log.error("location upload failed: failures=\(self.uploadFailures, privacy: .public) next_try_s=\(Int(LocationRules.backoff(failures: self.uploadFailures)), privacy: .public) error=\(error.localizedDescription, privacy: .public)")
        }
    }

    private func uploadInBackgroundTask() {
        guard pending + motionPending + visits.count > 0,
              LocationRules.uploadDue(waiting: pending + motionPending + visits.count,
                                      now: Date().timeIntervalSince1970, lastAttempt: lastAttempt,
                                      failures: uploadFailures, interval: 0) else { return }
        var id: UIBackgroundTaskIdentifier = .invalid
        id = UIApplication.shared.beginBackgroundTask(withName: "location-upload") {
            UIApplication.shared.endBackgroundTask(id)
        }
        Task {
            await upload()
            UIApplication.shared.endBackgroundTask(id)
        }
    }

    private func noteError(_ message: String) {
        lastError = message
        publish()
    }

    // MARK: - Status (Settings sheet and the dashboard's Location history tab)

    var status: [String: Any] {
        let auth: String
        switch authorization {
        case .authorizedAlways: auth = "always"
        case .authorizedWhenInUse: auth = "whenInUse"
        case .denied: auth = "denied"
        case .restricted: auth = "restricted"
        default: auth = "notDetermined"
        }
        var s: [String: Any] = ["enabled": enabled, "authorization": auth, "precise": precise,
                                "motion": motionState, "pending": pending, "mode": mode.rawValue,
                                "delivered": delivered, "stored": stored, "motion_pending": motionPending,
                                "live_restarts": liveRestarts, "live_stationary": liveStationary, "backstop": backstop,
                                "upload_failures": uploadFailures, "paused_when_still": managerPaused,
                                "motion_source": mode == .full ? "live" : "query"]
        if uploadFailures > 0 {
            s["next_upload_s"] = max(0, Int(lastAttempt + LocationRules.backoff(failures: uploadFailures) - Date().timeIntervalSince1970))
        }
        if lastLiveFix > 0 { s["last_live_fix"] = lastLiveFix }
        if lastUpload > 0 { s["last_upload"] = lastUpload }
        if let lastError { s["last_error"] = lastError }
        return s
    }

    private func publish() {
        NotificationCenter.default.post(name: Self.statusChanged, object: nil)
    }
}

extension LocationRecorder: CLLocationManagerDelegate {
    nonisolated func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
        let status = manager.authorizationStatus
        let full = manager.accuracyAuthorization == .fullAccuracy
        Task { @MainActor in
            self.authorization = status
            self.precise = full
            // Ask for Always as the second step, once While Using is granted.
            if self.enabled && status == .authorizedWhenInUse {
                manager.requestAlwaysAuthorization()
            }
            self.publish()
        }
    }

    nonisolated func locationManager(_ manager: CLLocationManager, didUpdateLocations locations: [CLLocation]) {
        Task { @MainActor in
            // iOS 16 standard updates, the stall backstop, and significant-change
            // wake-ups on every iOS. A wake-up means the phone moved: make sure
            // the live stream is running to record the rest of the move.
            if self.mode == .saver {
                // A saver wake-up is short: label the fix with the coprocessor's
                // history first, then store it.
                self.backfillMotion(why: "wake-up") {
                    self.record(locations, source: "manager")
                }
            } else {
                self.record(locations, source: "manager")
                self.ensureLive(moved: true, why: "location wake-up")
            }
        }
    }

    nonisolated func locationManagerDidPauseLocationUpdates(_ manager: CLLocationManager) {
        Task { @MainActor in
            self.managerPaused = true
            log.info("location standard updates paused by iOS: phone still (backstop=\(self.backstop, privacy: .public))")
            self.publish()
        }
    }

    nonisolated func locationManagerDidResumeLocationUpdates(_ manager: CLLocationManager) {
        Task { @MainActor in
            self.managerPaused = false
            log.info("location standard updates resumed by iOS")
            self.publish()
        }
    }

    nonisolated func locationManager(_ manager: CLLocationManager, didVisit visit: CLVisit) {
        let arrival = visit.arrivalDate == .distantPast ? nil : visit.arrivalDate.timeIntervalSince1970
        let departure = visit.departureDate == .distantFuture ? nil : visit.departureDate.timeIntervalSince1970
        guard let arrival else { return }
        // One id per visit: iOS reports it on arrival and again on departure
        // with slightly different coordinates, so an id that included them
        // stored the departure as a second visit and left the arrival open
        // forever (2026-10-04, a 24 h stop). The server's "later report wins"
        // upsert needs both reports to share the id.
        let v = VisitSample(id: "\(Int(arrival))",
                            arrival: arrival, departure: departure,
                            lat: visit.coordinate.latitude, lon: visit.coordinate.longitude,
                            h_acc: visit.horizontalAccuracy)
        Task { @MainActor in
            self.visits.append([v])
            if departure != nil { self.ensureLive(moved: true, why: "visit ended") }
            self.backfillMotion(why: "visit")
            // A visit wake-up is the saver mode's only window: send now unless
            // the server is failing and backing off.
            self.maybeUpload(interval: 0, why: "visit")
        }
    }

    nonisolated func locationManager(_ manager: CLLocationManager, didFailWithError error: Error) {
        Task { @MainActor in self.noteError(error.localizedDescription) }
    }
}
