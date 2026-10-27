// The native recorder's core, shared by the Mac app and the iOS app
// (MSG-98487, docs/design/recorder.md). The Mac app's glue is
// amux-recorder.swift; the iOS app's is ios/AmuxApp/Sources/RecorderBridge.swift.
//
// The Record tab captured through the page's MediaRecorder, so closing or
// reloading a window ended the recording, quitting ignored it, there was no
// system audio and nothing on screen said a recording was running. This
// recorder belongs to the app delegate, not to a window: windows open, close
// and reload around it.
//
// One rule carries it: audio is on disk, in finished pieces, before anything
// else happens to it. Each track (mic, system) is written by its own serial
// queue as 10 s ADTS AAC segments (48 kHz mono) under
//   ~/Library/Application Support/Amux/Recordings/<id>/
// ADTS is a stream of self-delimiting frames, so the open segment of a
// recording interrupted by a crash still decodes up to its last frame. The
// manifest (every segment with its sha256, start and duration, every gap) is
// rewritten atomically after each segment. Uploading is a separate loop that
// sends finished segments to /api/recordings/sessions and deletes the spool
// only after the server filed the recording; offline, it waits.
//
// Never silent: a buffer arriving late (sleep, a device change, an overloaded
// writer, a pause) is a GAP with a reason, in the manifest and in the filed
// recording's sidecar.
//
// Verdicts (desktop-app.log): recorder_started, recorder_stopped,
// recorder_gap, recorder_segment_failed, recorder_recovered,
// recorder_source_failed, recorder_shipped, recorder_ship_failed,
// recorder_filed.

import AVFoundation
import CryptoKit
import Foundation
#if os(macOS)
import AppKit
#endif
#if canImport(ScreenCaptureKit)
import ScreenCaptureKit
#endif

let recorderSampleRate: Double = 48_000
let recorderSegmentSeconds: Double = Double(ProcessInfo.processInfo.environment["AMUX_RECORDER_SEGMENT_S"] ?? "") ?? 10
/// A buffer starting this long after the last one ended is a gap.
let recorderGapSlackMs: Int64 = 250
/// Buffers waiting for a writer past this many seconds of audio are dropped,
/// and the drop is recorded as a gap. 30 s (about 11 MB at 48 kHz stereo
/// float) rides out a stalled disk: 5 s dropped 10 s of a two-hour test run
/// while the disk was 99% full (2026-10-27).
let recorderMaxBacklogSeconds: Double = 30

func nowMs() -> Int64 { Int64(Date().timeIntervalSince1970 * 1000) }

func recorderDeviceName() -> String {
    #if os(macOS)
    return "Mac " + (Host.current().localizedName ?? "")
    #else
    return ProcessInfo.processInfo.hostName
    #endif
}

struct RecGap: Codable { var track: String; var at_ms: Int64; var dur_ms: Int64; var why: String }
struct RecSegment: Codable {
    var track: String; var seq: Int; var start_ms: Int64; var dur_ms: Int64
    var bytes: Int64; var sha256: String; var file: String; var uploaded: Bool
}
struct RecManifest: Codable {
    var id: String; var started_at: Int64; var tz_offset_min: Int; var device: String
    var tracks: [String]; var state: String; var segments: [RecSegment]; var gaps: [RecGap]
    var ended_at: Int64?; var recovered: Bool; var session_open: Bool; var error: String?
}

func recorderSpoolRoot() -> URL {
    if let p = ProcessInfo.processInfo.environment["AMUX_RECORDER_SPOOL"], !p.isEmpty { return URL(fileURLWithPath: p) }
    let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
        ?? URL(fileURLWithPath: NSHomeDirectory() + "/Library/Application Support")
    return base.appendingPathComponent("Amux/Recordings")
}

func sha256Hex(_ url: URL) -> String {
    guard let h = try? FileHandle(forReadingFrom: url) else { return "" }
    defer { try? h.close() }
    var hasher = SHA256()
    while let chunk = try? h.read(upToCount: 1 << 20), !chunk.isEmpty { hasher.update(data: chunk) }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
}

/// What the recorder needs from the app around it: where amux is, the
/// owner token, and how to talk TLS to a self-signed server.
protocol RecorderHost: AnyObject {
    var recorderServer: URL? { get }
    func recorderToken(_ go: @escaping (String?) -> Void)
    func recorderSessionDelegate() -> URLSessionDelegate?
}

// MARK: - One track's writer

/// Writes one track as numbered segments on its own serial queue. Every
/// input format is converted to 48 kHz mono float first, so every segment of
/// every track has the same shape and the server can join them.
final class TrackWriter {
    let track: String
    let dir: URL
    let queue: DispatchQueue
    let outFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: recorderSampleRate, channels: 1, interleaved: false)!
    var converter: AVAudioConverter?
    var converterIn: AVAudioFormat?
    var file: AVAudioFile?
    var tmpURL: URL?
    var seq = 0
    var segFrames: AVAudioFramePosition = 0
    var segStartMs: Int64 = 0
    var lastEndMs: Int64 = 0
    /// lastEndMs before rounding. Summing whole-ms buffer durations lost
    /// ~0.3 ms a buffer, so start_ms ran 50 ms a segment behind the audio.
    var lastEndExactMs: Double = 0
    /// The end of the last buffer on the SOURCE's own clock (sample time, or
    /// presentation time), when the source has one. Loss is a jump on this
    /// clock; a buffer merely delivered late under load is not (a two-hour
    /// test flagged 0.3 to 0.7 s "gaps" that were late deliveries).
    var lastSourceEndMs: Double?
    var pendingReason: String?
    var backlogFrames = 0
    let lock = NSLock()
    var level: Float = 0
    var onSegment: ((RecSegment) -> Void)?
    var onGap: ((RecGap) -> Void)?
    var onError: ((String) -> Void)?
    let startedAt: Int64

    init(track: String, dir: URL, startedAt: Int64, firstSeq: Int) {
        self.track = track
        self.dir = dir
        self.startedAt = startedAt
        self.seq = firstSeq
        queue = DispatchQueue(label: "amux.recorder.\(track)", qos: .userInitiated)
    }

    /// Called from the capture thread: never blocks it. A backlog past the
    /// limit drops the buffer and records the drop.
    func append(_ buffer: AVAudioPCMBuffer, arrivedMs: Int64, sourceMs: Double? = nil) {
        let frames = Int(buffer.frameLength)
        lock.lock()
        let backlog = backlogFrames
        let over = Double(backlog) / buffer.format.sampleRate > recorderMaxBacklogSeconds
        if !over { backlogFrames += frames }
        lock.unlock()
        if over {
            queue.async { self.pendingReason = "overload" }
            return
        }
        // Level for the meter, computed here because it is cheap.
        if let ch = buffer.floatChannelData?[0], frames > 0 {
            var sum: Float = 0
            for i in stride(from: 0, to: frames, by: 8) { sum += ch[i] * ch[i] }
            level = (sum / Float(max(1, frames / 8))).squareRoot()
        }
        queue.async {
            self.write(buffer, arrivedMs: arrivedMs, sourceMs: sourceMs)
            self.lock.lock(); self.backlogFrames -= frames; self.lock.unlock()
        }
    }

    /// Mark the reason for the next gap (sleep, device_change, paused); the
    /// gap itself is measured from the buffers around it.
    func expectGap(_ why: String) { queue.async { self.pendingReason = why } }

    private func convert(_ buffer: AVAudioPCMBuffer) -> AVAudioPCMBuffer? {
        if buffer.format == outFormat { return buffer }
        if converter == nil || converterIn != buffer.format {
            converter = AVAudioConverter(from: buffer.format, to: outFormat)
            converterIn = buffer.format
        }
        guard let conv = converter else { return nil }
        let cap = AVAudioFrameCount(Double(buffer.frameLength) * outFormat.sampleRate / buffer.format.sampleRate) + 64
        guard let out = AVAudioPCMBuffer(pcmFormat: outFormat, frameCapacity: cap) else { return nil }
        var fed = false
        var err: NSError?
        conv.convert(to: out, error: &err) { _, status in
            if fed { status.pointee = .noDataNow; return nil }
            fed = true
            status.pointee = .haveData
            return buffer
        }
        return err == nil ? out : nil
    }

    private func write(_ buffer: AVAudioPCMBuffer, arrivedMs: Int64, sourceMs: Double?) {
        guard let pcm = convert(buffer), pcm.frameLength > 0 else { return }
        let durExactMs = Double(pcm.frameLength) / recorderSampleRate * 1000
        let durMs = Int64(durExactMs.rounded())
        // On the source's clock when it has one and nothing restarted it;
        // across a restart (sleep, device change, pause) the wall clock.
        var startMs = arrivedMs - durMs
        var startExactMs: Double?
        var gapMs: Int64 = 0
        if let src = sourceMs, let prev = lastSourceEndMs, pendingReason == nil {
            gapMs = Int64(src - prev)
            startMs = lastEndMs + max(0, gapMs)
        } else if lastEndMs > 0 {
            gapMs = startMs - lastEndMs
        }
        lastSourceEndMs = sourceMs.map { $0 + Double(buffer.frameLength) / buffer.format.sampleRate * 1000 }
        if lastEndMs > 0, gapMs > recorderGapSlackMs {
            let g = RecGap(track: track, at_ms: lastEndMs - startedAt, dur_ms: gapMs, why: pendingReason ?? "dropped")
            onGap?(g)
            // A gap ends the segment, so segments stay contiguous inside.
            closeSegment()
        } else if lastEndMs > 0 && gapMs <= recorderGapSlackMs {
            // Contiguous: the segment timeline follows the audio, not delivery jitter.
            startMs = lastEndMs
            startExactMs = lastEndExactMs
        }
        pendingReason = nil
        if file == nil { openSegment(startMs: startMs) }
        guard file != nil else { return }
        // No local reference to the file: closeSegment() must hold the last
        // one, so releasing it there finishes the encoder BEFORE the segment
        // is hashed and renamed (a held `let f` left the tail unflushed, and
        // every upload failed its sha check, 2026-10-27).
        do { try file?.write(from: pcm) } catch {
            onError?("writing \(track) segment \(seq): \(error.localizedDescription)")
            NSLog("amux-recorder: verdict=recorder_segment_failed track=%@ seq=%d error=%@", track, seq, error.localizedDescription)
            closeSegment()
            return
        }
        segFrames += AVAudioFramePosition(pcm.frameLength)
        lastEndExactMs = (startExactMs ?? Double(startMs)) + durExactMs
        lastEndMs = Int64(lastEndExactMs.rounded())
        if Double(segFrames) >= recorderSegmentSeconds * recorderSampleRate { closeSegment() }
    }

    private func openSegment(startMs: Int64) {
        let tmp = dir.appendingPathComponent(String(format: ".tmp-%@-%08d.aac", track, seq))
        let settings: [String: Any] = [AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: recorderSampleRate,
                                       AVNumberOfChannelsKey: 1, AVEncoderBitRateKey: 64_000]
        do {
            file = try AVAudioFile(forWriting: tmp, settings: settings, commonFormat: .pcmFormatFloat32, interleaved: false)
            tmpURL = tmp
            segFrames = 0
            segStartMs = startMs
        } catch {
            onError?("opening \(track) segment: \(error.localizedDescription)")
            NSLog("amux-recorder: verdict=recorder_segment_failed track=%@ seq=%d error=%@", track, seq, error.localizedDescription)
        }
    }

    /// Close the open segment: releasing the AVAudioFile finishes it, then the
    /// rename makes it a finished segment.
    func closeSegment() {
        guard let tmp = tmpURL else { return }
        file = nil
        tmpURL = nil
        let frames = segFrames
        segFrames = 0
        guard frames > 0 else { try? FileManager.default.removeItem(at: tmp); return }
        let final = dir.appendingPathComponent(String(format: "%@-%08d.aac", track, seq))
        do { try FileManager.default.moveItem(at: tmp, to: final) } catch {
            onError?("finishing \(track) segment \(seq): \(error.localizedDescription)")
            return
        }
        let bytes = (try? FileManager.default.attributesOfItem(atPath: final.path)[.size] as? Int64) ?? 0
        let seg = RecSegment(track: track, seq: seq, start_ms: segStartMs, dur_ms: Int64(Double(frames) / recorderSampleRate * 1000),
                             bytes: bytes, sha256: sha256Hex(final), file: final.lastPathComponent, uploaded: false)
        seq += 1
        onSegment?(seg)
    }

    /// Finish everything queued, then the open segment. Blocks the caller.
    func flush() { queue.sync { self.closeSegment() } }
}

// MARK: - Sources

protocol RecSource: AnyObject {
    var track: String { get }
    func start() throws
    func stop()
}

/// The microphone, through AVAudioEngine. A device change or a wake restarts
/// the engine; the writer measures the gap.
final class MicSource: RecSource {
    let track = "mic"
    let writer: TrackWriter
    var engine = AVAudioEngine()
    var observer: NSObjectProtocol?
    init(writer: TrackWriter) { self.writer = writer }

    func start() throws {
        let input = engine.inputNode
        let fmt = input.outputFormat(forBus: 0)
        guard fmt.sampleRate > 0, fmt.channelCount > 0 else {
            throw NSError(domain: "amux.recorder", code: 1, userInfo: [NSLocalizedDescriptionKey: "no microphone input is available"])
        }
        input.removeTap(onBus: 0)
        input.installTap(onBus: 0, bufferSize: 4096, format: fmt) { [weak self] buf, when in
            let src = when.isSampleTimeValid ? Double(when.sampleTime) / fmt.sampleRate * 1000 : nil
            self?.writer.append(buf, arrivedMs: nowMs(), sourceMs: src)
        }
        engine.prepare()
        try engine.start()
        if observer == nil {
            observer = NotificationCenter.default.addObserver(forName: .AVAudioEngineConfigurationChange, object: engine, queue: .main) { [weak self] _ in
                self?.restart(why: "device_change")
            }
        }
    }

    func restart(why: String) {
        writer.expectGap(why)
        NSLog("amux-recorder: verdict=recorder_gap track=mic why=%@ restarting=1", why)
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        engine = AVAudioEngine()
        if let o = observer { NotificationCenter.default.removeObserver(o); observer = nil }
        do { try start() } catch {
            NSLog("amux-recorder: verdict=recorder_source_failed track=mic error=%@ retry_s=2", error.localizedDescription)
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in self?.restart(why: why) }
        }
    }

    func stop() {
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        if let o = observer { NotificationCenter.default.removeObserver(o); observer = nil }
    }
}

/// A synthetic 440 Hz tone for tests: the same writer, segments, manifest,
/// gaps and upload as the microphone, with no permission prompt. Selected
/// only by name (`sources=tone`).
final class ToneSource: RecSource {
    let track: String
    let writer: TrackWriter
    var timer: DispatchSourceTimer?
    var phase: Double = 0
    var stalledUntil: Date = .distantPast
    init(track: String, writer: TrackWriter) { self.track = track; self.writer = writer }

    func start() throws {
        let t = DispatchSource.makeTimerSource(queue: DispatchQueue(label: "amux.recorder.tone"))
        let sr = 44_100.0
        let fmt = AVAudioFormat(standardFormatWithSampleRate: sr, channels: 2)!
        t.schedule(deadline: .now(), repeating: .milliseconds(100))
        let t0 = Date()
        var produced: Double = 0   // frames, on the source's own clock
        t.setEventHandler { [weak self] in
            guard let self = self else { return }
            let due = Date().timeIntervalSince(t0) * sr
            if Date() < self.stalledUntil { return }
            // Like a device's own buffer: a late timer tick catches up; a
            // stall (a simulated sleep) jumps the clock, which is a gap.
            if due - produced > sr * 2 { produced = due - sr / 10 }
            while due - produced >= sr / 10 {
                let n = AVAudioFrameCount(sr / 10)
                guard let b = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: n) else { return }
                b.frameLength = n
                for i in 0..<Int(n) {
                    let v = Float(sin(self.phase) * 0.2)
                    self.phase += 2 * .pi * 440 / sr
                    b.floatChannelData![0][i] = v
                    b.floatChannelData![1][i] = v
                }
                self.writer.append(b, arrivedMs: nowMs(), sourceMs: produced / sr * 1000)
                produced += Double(n)
            }
        }
        t.resume()
        timer = t
    }

    /// Tests: produce nothing for `seconds`, the way a sleeping Mac does.
    func stall(_ seconds: Double) { stalledUntil = Date().addingTimeInterval(seconds) }
    func stop() { timer?.cancel(); timer = nil }
}

#if canImport(ScreenCaptureKit) && os(macOS)
/// Everything the Mac plays (Zoom, Meet, FaceTime), through ScreenCaptureKit's
/// audio stream. macOS 13 and later, and the Screen Recording permission.
@available(macOS 13.0, *)
final class SystemAudioSource: NSObject, RecSource, SCStreamOutput, SCStreamDelegate {
    let track = "system"
    let writer: TrackWriter
    var stream: SCStream?
    let queue = DispatchQueue(label: "amux.recorder.system")
    var onFailure: ((String) -> Void)?
    init(writer: TrackWriter) { self.writer = writer }

    func start() throws {
        let done = DispatchSemaphore(value: 0)
        var failure: Error?
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { content, err in
            defer { done.signal() }
            guard let content = content, let display = content.displays.first else {
                failure = err ?? NSError(domain: "amux.recorder", code: 2, userInfo: [NSLocalizedDescriptionKey: "no display to capture system audio from"])
                return
            }
            let filter = SCContentFilter(display: display, excludingApplications: [], exceptingWindows: [])
            let cfg = SCStreamConfiguration()
            cfg.capturesAudio = true
            cfg.excludesCurrentProcessAudio = true
            cfg.sampleRate = Int(recorderSampleRate)
            cfg.channelCount = 1
            // The video half is required by the API; keep it as small and slow as allowed.
            cfg.width = 2
            cfg.height = 2
            cfg.minimumFrameInterval = CMTime(value: 1, timescale: 1)
            let s = SCStream(filter: filter, configuration: cfg, delegate: self)
            do {
                try s.addStreamOutput(self, type: .audio, sampleHandlerQueue: self.queue)
                self.stream = s
            } catch { failure = error }
        }
        done.wait()
        if let f = failure { throw f }
        stream?.startCapture { [weak self] err in
            if let err = err {
                NSLog("amux-recorder: verdict=recorder_source_failed track=system error=%@", err.localizedDescription)
                self?.onFailure?(err.localizedDescription)
            }
        }
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .audio, let desc = CMSampleBufferGetFormatDescription(sb),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(desc) else { return }
        var a = asbd.pointee
        guard let fmt = AVAudioFormat(streamDescription: &a) else { return }
        let n = AVAudioFrameCount(CMSampleBufferGetNumSamples(sb))
        guard n > 0, let buf = AVAudioPCMBuffer(pcmFormat: fmt, frameCapacity: n) else { return }
        buf.frameLength = n
        let st = CMSampleBufferCopyPCMDataIntoAudioBufferList(sb, at: 0, frameCount: Int32(n), into: buf.mutableAudioBufferList)
        let pts = CMSampleBufferGetPresentationTimeStamp(sb)
        let src = pts.isValid ? CMTimeGetSeconds(pts) * 1000 : nil
        if st == noErr { writer.append(buf, arrivedMs: nowMs(), sourceMs: src) }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        NSLog("amux-recorder: verdict=recorder_source_failed track=system error=%@", error.localizedDescription)
        writer.expectGap("system_audio_stopped")
        onFailure?(error.localizedDescription)
    }

    func stop() { stream?.stopCapture { _ in }; stream = nil }
}
#endif

// MARK: - The recorder

final class NativeRecorder {
    static let shared = NativeRecorder()
    var state = "idle"           // idle, recording, paused, recovering, failed
    var manifest: RecManifest?
    var dir: URL?
    var writers: [String: TrackWriter] = [:]
    var sources: [RecSource] = []
    var pausedAt: Int64 = 0
    var lastError: String?
    var onChange: (() -> Void)?
    let manifestQueue = DispatchQueue(label: "amux.recorder.manifest")
    var shipping = false
    var shipTimer: Timer?
    var sleepObservers: [NSObjectProtocol] = []

    var isActive: Bool { state == "recording" || state == "paused" }

    static var systemAudioAvailable: Bool {
        #if os(macOS)
        if #available(macOS 13.0, *) { return true }
        #endif
        return false
    }

    // ── manifest ──
    func saveManifest() {
        manifestQueue.sync {
            guard let m = manifest, let d = dir else { return }
            if let data = try? JSONEncoder().encode(m) {
                try? data.write(to: d.appendingPathComponent("manifest.json"), options: .atomic)
            }
        }
    }

    func status() -> [String: Any] {
        var s: [String: Any] = ["available": true, "state": state, "system_audio_available": NativeRecorder.systemAudioAvailable]
        if let m = manifest {
            let pending = m.segments.filter { !$0.uploaded }.count
            s["id"] = m.id
            s["started_at"] = m.started_at
            s["elapsed_ms"] = (state == "paused" ? pausedAt : nowMs()) - m.started_at - pausedTotal
            s["tracks"] = m.tracks
            s["segments"] = m.segments.count
            s["pending_upload"] = pending
            s["gaps"] = m.gaps.count
            s["levels"] = writers.mapValues { Double($0.level) }
        }
        s["spooled"] = spooledIDs().count
        if let e = lastError { s["error"] = e }
        return s
    }
    var pausedTotal: Int64 = 0

    func changed() { DispatchQueue.main.async { self.onChange?() } }

    // ── lifecycle ──
    func start(sources wanted: [String]) -> String? {
        guard !isActive else { return "already recording" }
        let id = "n" + String(Int64(Date().timeIntervalSince1970 * 1000), radix: 36) + String(Int.random(in: 0..<1_679_616), radix: 36)
        let d = recorderSpoolRoot().appendingPathComponent(id)
        do { try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true) } catch {
            return "could not create the recording folder: \(error.localizedDescription)"
        }
        let started = nowMs()
        var tracks: [String] = []
        var made: [RecSource] = []
        writers = [:]
        for name in wanted {
            switch name {
            case "mic":
                let w = makeWriter("mic", d, started); made.append(MicSource(writer: w)); tracks.append("mic")
            case "tone", "tone2":
                let t = name == "tone" ? "mic" : "system"
                let w = makeWriter(t, d, started); made.append(ToneSource(track: t, writer: w)); tracks.append(t)
            case "system":
                #if canImport(ScreenCaptureKit) && os(macOS)
                if #available(macOS 13.0, *) {
                    let w = makeWriter("system", d, started)
                    let s = SystemAudioSource(writer: w)
                    s.onFailure = { [weak self] e in self?.lastError = "system audio: " + e; self?.changed() }
                    made.append(s); tracks.append("system")
                } else { lastError = "system audio needs macOS 13" }
                #else
                lastError = "system audio needs macOS 13"
                #endif
            default: break
            }
        }
        guard !made.isEmpty else { return "no source to record from" }
        dir = d
        manifest = RecManifest(id: id, started_at: started, tz_offset_min: TimeZone.current.secondsFromGMT() / 60,
                               device: recorderDeviceName(), tracks: tracks, state: "recording",
                               segments: [], gaps: [], ended_at: nil, recovered: false, session_open: false, error: nil)
        saveManifest()
        var started_ok: [RecSource] = []
        for s in made {
            do { try s.start(); started_ok.append(s) } catch {
                lastError = "\(s.track): \(error.localizedDescription)"
                NSLog("amux-recorder: verdict=recorder_source_failed track=%@ error=%@", s.track, error.localizedDescription)
            }
        }
        guard !started_ok.isEmpty else {
            try? FileManager.default.removeItem(at: d)
            manifest = nil; dir = nil; writers = [:]
            return lastError ?? "no source started"
        }
        sources = started_ok
        state = "recording"
        pausedTotal = 0
        watchSleep()
        NSLog("amux-recorder: verdict=recorder_started id=%@ tracks=%@", id, tracks.joined(separator: ","))
        changed()
        return nil
    }

    private func makeWriter(_ track: String, _ d: URL, _ started: Int64) -> TrackWriter {
        let w = TrackWriter(track: track, dir: d, startedAt: started, firstSeq: 0)
        w.onSegment = { [weak self] seg in
            guard let self = self else { return }
            self.manifestQueue.sync { self.manifest?.segments.append(seg) }
            self.saveManifest()
            self.changed()
        }
        w.onGap = { [weak self] g in
            guard let self = self else { return }
            NSLog("amux-recorder: verdict=recorder_gap track=%@ at_ms=%lld dur_ms=%lld why=%@", g.track, g.at_ms, g.dur_ms, g.why)
            self.manifestQueue.sync { self.manifest?.gaps.append(g) }
            self.saveManifest()
            self.changed()
        }
        w.onError = { [weak self] e in self?.lastError = e; self?.changed() }
        writers[track] = w
        return w
    }

    func pause() {
        guard state == "recording" else { return }
        sources.forEach { $0.stop() }   // releases the microphone while paused
        writers.values.forEach { $0.expectGap("paused"); $0.flush() }
        pausedAt = nowMs()
        state = "paused"
        manifest?.state = "paused"
        saveManifest()
        changed()
    }

    func resume() {
        guard state == "paused" else { return }
        pausedTotal += nowMs() - pausedAt
        for s in sources { do { try s.start() } catch { lastError = "\(s.track): \(error.localizedDescription)" } }
        state = "recording"
        manifest?.state = "recording"
        saveManifest()
        changed()
    }

    /// Stop capture and finish every segment. Synchronous, so quit and SIGTERM
    /// can call it and know the audio is on disk when it returns.
    func stop() {
        guard isActive, var m = manifest else { return }
        sources.forEach { $0.stop() }
        sources = []
        writers.values.forEach { $0.flush() }
        m = manifest ?? m
        m.state = "stopped"
        m.ended_at = nowMs()
        manifest = m
        saveManifest()
        NSLog("amux-recorder: verdict=recorder_stopped id=%@ segments=%d gaps=%d", m.id, m.segments.count, m.gaps.count)
        state = "idle"
        manifest = nil; dir = nil; writers = [:]
        unwatchSleep()
        changed()
        ship()
    }

    // ── sleep and wake (macOS; iOS reports interruptions instead) ──
    func watchSleep() {
        #if os(macOS)
        let nc = NSWorkspace.shared.notificationCenter
        sleepObservers = [
            nc.addObserver(forName: NSWorkspace.willSleepNotification, object: nil, queue: .main) { [weak self] _ in
                self?.writers.values.forEach { $0.expectGap("sleep"); $0.flush() }
            },
            nc.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) { [weak self] _ in
                guard let self = self, self.state == "recording" else { return }
                for s in self.sources { (s as? MicSource)?.restart(why: "sleep") }
            },
        ]
        #endif
    }

    func unwatchSleep() {
        #if os(macOS)
        sleepObservers.forEach { NSWorkspace.shared.notificationCenter.removeObserver($0) }
        #endif
        sleepObservers = []
    }

    /// iOS: a phone call or Siri took the audio session. The gap is measured
    /// from the buffers around it; the engine restarts when it ends.
    func interrupted(_ began: Bool) {
        guard state == "recording" else { return }
        if began { writers.values.forEach { $0.expectGap("interruption"); $0.flush() } }
        else { for s in sources { (s as? MicSource)?.restart(why: "interruption") } }
    }

    /// Tests: the same handlers a real sleep runs, around a source that goes
    /// quiet for `seconds`.
    func simulateSleep(_ seconds: Double) {
        writers.values.forEach { $0.expectGap("sleep") }
        sources.compactMap { $0 as? ToneSource }.forEach { $0.stall(seconds) }
    }

    // ── recovery ──
    func spooledIDs() -> [String] {
        let root = recorderSpoolRoot()
        let ids = (try? FileManager.default.contentsOfDirectory(atPath: root.path)) ?? []
        return ids.filter { id in id != manifest?.id && FileManager.default.fileExists(atPath: root.appendingPathComponent(id).appendingPathComponent("manifest.json").path) }
    }

    func loadManifest(_ d: URL) -> RecManifest? {
        guard let data = try? Data(contentsOf: d.appendingPathComponent("manifest.json")) else { return nil }
        return try? JSONDecoder().decode(RecManifest.self, from: data)
    }

    func writeManifest(_ m: RecManifest, _ d: URL) {
        if let data = try? JSONEncoder().encode(m) { try? data.write(to: d.appendingPathComponent("manifest.json"), options: .atomic) }
    }

    /// At launch: a recording whose process died is closed with what reached
    /// disk, including the open segment (ADTS decodes up to its last frame).
    func recoverInterrupted() {
        let root = recorderSpoolRoot()
        for id in spooledIDs() {
            let d = root.appendingPathComponent(id)
            guard var m = loadManifest(d), m.state == "recording" || m.state == "paused" else { continue }
            var rescued = 0
            let files = (try? FileManager.default.contentsOfDirectory(atPath: d.path)) ?? []
            for name in files where name.hasPrefix(".tmp-") && name.hasSuffix(".aac") {
                let parts = name.dropFirst(5).dropLast(4).split(separator: "-")
                guard parts.count == 2, let seq = Int(parts[1]) else { continue }
                let track = String(parts[0])
                let tmp = d.appendingPathComponent(name)
                let final = d.appendingPathComponent(String(format: "%@-%08d.aac", track, seq))
                guard let f = try? AVAudioFile(forReading: tmp), f.length > 0 else {
                    m.gaps.append(RecGap(track: track, at_ms: 0, dur_ms: 0, why: "crash_unreadable_segment"))
                    continue
                }
                let dur = Int64(Double(f.length) / f.processingFormat.sampleRate * 1000)
                let prevEnd = m.segments.filter { $0.track == track }.map { $0.start_ms + $0.dur_ms }.max() ?? m.started_at
                try? FileManager.default.moveItem(at: tmp, to: final)
                let bytes = (try? FileManager.default.attributesOfItem(atPath: final.path)[.size] as? Int64) ?? 0
                m.segments.append(RecSegment(track: track, seq: seq, start_ms: prevEnd, dur_ms: dur, bytes: bytes,
                                             sha256: sha256Hex(final), file: final.lastPathComponent, uploaded: false))
                rescued += 1
            }
            m.state = "stopped"
            m.recovered = true
            m.ended_at = m.segments.map { $0.start_ms + $0.dur_ms }.max() ?? m.started_at
            writeManifest(m, d)
            NSLog("amux-recorder: verdict=recorder_recovered id=%@ segments=%d rescued_open=%d", id, m.segments.count, rescued)
        }
    }

    // ── upload ──
    func startShipping(host: RecorderHost) {
        shipTimer?.invalidate()
        shipTimer = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] _ in self?.ship() }
        self.host = host
        ship()
    }
    weak var host: RecorderHost?

    func ship() {
        guard !shipping, let host = host, let server = host.recorderServer else { return }
        let root = recorderSpoolRoot()
        var work: [URL] = spooledIDs().map { root.appendingPathComponent($0) }
        if let d = dir { work.insert(d, at: 0) }   // the live recording's finished segments too
        guard !work.isEmpty else { return }
        shipping = true
        host.recorderToken { [weak self] token in
            DispatchQueue.global(qos: .utility).async {
                for d in work { self?.shipOne(d, server: server, token: token) }
                DispatchQueue.main.async { self?.shipping = false; self?.changed() }
            }
        }
    }

    private func request(_ server: URL, _ path: String, method: String, token: String?, json: Any? = nil, body: Data? = nil,
                         contentType: String = "application/json") -> (Int, [String: Any]?) {
        let base = server.absoluteString.hasSuffix("/") ? String(server.absoluteString.dropLast()) : server.absoluteString
        guard let url = URL(string: base + path) else { return (0, nil) }
        var req = URLRequest(url: url)
        req.httpMethod = method
        req.timeoutInterval = 60
        if let t = token { req.setValue("Bearer " + t, forHTTPHeaderField: "Authorization") }
        req.setValue(contentType, forHTTPHeaderField: "Content-Type")
        if let j = json { req.httpBody = try? JSONSerialization.data(withJSONObject: j) } else { req.httpBody = body }
        let done = DispatchSemaphore(value: 0)
        var status = 0
        var obj: [String: Any]?
        let session = URLSession(configuration: .ephemeral, delegate: host?.recorderSessionDelegate(), delegateQueue: nil)
        session.dataTask(with: req) { data, resp, _ in
            status = (resp as? HTTPURLResponse)?.statusCode ?? 0
            obj = data.flatMap { try? JSONSerialization.jsonObject(with: $0) as? [String: Any] }
            done.signal()
        }.resume()
        _ = done.wait(timeout: .now() + 90)
        session.finishTasksAndInvalidate()
        return (status, obj)
    }

    /// Send what the server lacks, and finish a stopped recording. Runs off
    /// the main thread; every step is idempotent on the server.
    private func shipOne(_ d: URL, server: URL, token: String?) {
        let live = d == dir
        guard var m = live ? manifestQueue.sync(execute: { manifest }) : loadManifest(d) else { return }
        let save: (RecManifest) -> Void = { [weak self] mm in
            guard let self = self else { return }
            if live { self.manifestQueue.sync { if self.manifest?.id == mm.id { self.manifest?.session_open = mm.session_open
                for (i, s) in (self.manifest?.segments ?? []).enumerated() where mm.segments.contains(where: { $0.track == s.track && $0.seq == s.seq && $0.uploaded }) {
                    self.manifest?.segments[i].uploaded = true } } }; self.saveManifest() }
            else { self.writeManifest(mm, d) }
        }
        if !m.session_open {
            let (st, _) = request(server, "/api/recordings/sessions", method: "POST", token: token, json: [
                "id": m.id, "started_at": m.started_at, "tz_offset_min": m.tz_offset_min, "mime": "audio/aac",
                "device": m.device, "chunk_kind": "segment", "tracks": m.tracks, "recovered": m.recovered])
            guard (200..<300).contains(st) else {
                NSLog("amux-recorder: verdict=recorder_ship_failed id=%@ step=open status=%d", m.id, st)
                return
            }
            m.session_open = true
            save(m)
        }
        for i in m.segments.indices where !m.segments[i].uploaded {
            let s = m.segments[i]
            guard let data = try? Data(contentsOf: d.appendingPathComponent(s.file)) else { continue }
            let path = "/api/recordings/sessions/\(m.id)/chunks/\(s.track)/\(s.seq)?sha256=\(s.sha256)&start_ms=\(s.start_ms)&dur_ms=\(s.dur_ms)"
            let (st, obj) = request(server, path, method: "PUT", token: token, body: data, contentType: "audio/aac")
            if (200..<300).contains(st) {
                m.segments[i].uploaded = true
                save(m)
            } else {
                NSLog("amux-recorder: verdict=recorder_ship_failed id=%@ step=chunk seq=%d status=%d error=%@", m.id, s.seq, st,
                      (obj?["error"] as? String) ?? "")
                return
            }
        }
        guard !live, m.state == "stopped" else { return }
        var last: [String: Int] = [:]
        for t in m.tracks { if let mx = m.segments.filter({ $0.track == t }).map({ $0.seq }).max() { last[t] = mx } }
        let gaps = m.gaps.map { ["track": $0.track, "at_ms": $0.at_ms, "dur_ms": $0.dur_ms, "why": $0.why] as [String: Any] }
        let (st, obj) = request(server, "/api/recordings/sessions/\(m.id)/finish", method: "POST", token: token, json: [
            "ended_at": m.ended_at ?? nowMs(), "last_seq": last, "gaps": gaps, "recovered": m.recovered])
        if (200..<300).contains(st), (obj?["state"] as? String) == "finished" {
            NSLog("amux-recorder: verdict=recorder_filed id=%@ file=%@", m.id, ((obj?["recording"] as? [String: Any])?["file"] as? String) ?? "")
            try? FileManager.default.removeItem(at: d)
        } else if st == 409, let missing = obj?["missing"] as? [String: [Int]] {
            for (t, seqs) in missing { for i in m.segments.indices where m.segments[i].track == t && seqs.contains(m.segments[i].seq) { m.segments[i].uploaded = false } }
            save(m)
        } else {
            NSLog("amux-recorder: verdict=recorder_ship_failed id=%@ step=finish status=%d state=%@", m.id, st, (obj?["state"] as? String) ?? "")
        }
    }
}

