import Cocoa
import WebKit
import AVFoundation

// Dictation in the Mac app: the page asks (app.js _dictNativeStart), the app
// listens and recognizes on device (amux-dictation-core.swift), and the page
// gets the text back and puts it where you were typing. Holding 🌐 dictates
// until you let go (amux-workspace.swift sends "Globe Hold" and "Globe
// Release"), the way Wispr Flow's fn key does.
//
// Messages from the page (amuxDictation): {op: "start", hints: [String]},
// {op: "stop"}, {op: "cancel"}. Back to the page (window._dictNativeEvent):
// {type: "started"}, {type: "level", v}, {type: "unavailable", why} (the
// page then records the take itself, as in a browser), {type: "error", why},
// and {type: "done", text, audio (base64 AAC), mime, ms, finish_ms}.

final class NativeDictation {
    static let shared = NativeDictation()
    private weak var page: DashboardWindowController?
    private var take: AnyObject?       // DictationTake, macOS 26
    private var starting = false
    private var stopAsked = false
    private var cancelled = false
    private var feeding = false

    func message(_ d: [String: Any], from c: DashboardWindowController) {
        switch d["op"] as? String {
        case "start": start(hints: (d["hints"] as? [String]) ?? [], page: c)
        case "stop": stop()
        case "cancel": cancel()
        default: break
        }
    }

    private func send(_ ev: [String: Any], to c: DashboardWindowController? = nil) {
        guard let c = c ?? page, let data = try? JSONSerialization.data(withJSONObject: ev),
              let json = String(data: data, encoding: .utf8) else { return }
        c.webView.evaluateJavaScript("typeof window._dictNativeEvent === 'function' && window._dictNativeEvent(\(json))", completionHandler: nil)
    }

    private func start(hints: [String], page c: DashboardWindowController) {
        guard take == nil, !starting else { dlog("verdict=dictation_native_busy view=\(c.viewHash)"); return }
        guard #available(macOS 26.0, *) else {
            send(["type": "unavailable", "why": "on-device dictation needs macOS 26"], to: c)
            return
        }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .denied, .restricted:
            dlog("verdict=dictation_native_mic_denied view=\(c.viewHash)")
            send(["type": "error", "why": "The microphone is off for amux. Turn it on in System Settings → Privacy & Security → Microphone."], to: c)
            return
        case .notDetermined:
            activateApp()
            AVCaptureDevice.requestAccess(for: .audio) { ok in
                DispatchQueue.main.async {
                    dlog("verdict=mic_permission_\(ok ? "granted" : "refused") view=\(c.viewHash) for=dictation")
                    if ok { self.start(hints: hints, page: c) }
                    else { self.send(["type": "error", "why": "amux was not allowed the microphone."], to: c) }
                }
            }
            return
        default: break
        }
        page = c
        starting = true
        stopAsked = false
        cancelled = false
        let t0 = Date()
        Task { @MainActor in
            do {
                let t = try await DictationTake(hints: hints)
                self.starting = false
                if self.cancelled { t.cancel(); return }
                t.onLevel = { [weak self] v in self?.send(["type": "level", "v": (v * 100).rounded() / 100]) }
                if desktopTestMode, let path = ProcessInfo.processInfo.environment["AMUX_DESKTOP_DICTATION_FILE"] {
                    try self.feedFile(path, into: t)
                } else {
                    try t.startMic()
                    self.watchForSilence(t)
                }
                self.take = t
                chime("Tink")
                let setupMs = Int(Date().timeIntervalSince(t0) * 1000)
                dlog("verdict=dictation_native_started setup_ms=\(setupMs) hints=\(hints.count) view=\(c.viewHash)")
                self.send(["type": "started", "setup_ms": setupMs])
                if self.stopAsked { self.stop() }
            } catch {
                self.starting = false
                if case DictationError.unavailable(let why) = error {
                    dlog("verdict=dictation_native_unavailable why=\(why)")
                    self.send(["type": "unavailable", "why": why], to: c)
                } else {
                    dlog("verdict=dictation_native_failed stage=start error=\(error.localizedDescription)")
                    self.send(["type": "error", "why": "Dictation could not start: \(error.localizedDescription)"], to: c)
                }
            }
        }
    }

    /// A microphone that delivers nothing (no permission for this build, a
    /// dead device) is said, rather than a take of silence.
    @available(macOS 26.0, *)
    private func watchForSilence(_ t: DictationTake) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self, weak t] in
            guard let self = self, let t = t, self.take === t, t.buffersSeen == 0 else { return }
            dlog("verdict=dictation_native_no_input")
            self.cancel()
            self.send(["type": "error", "why": "The microphone gives no audio. If amux was just updated, allow it again in System Settings → Privacy & Security → Microphone."])
        }
    }

    /// AMUX_DESKTOP_DICTATION_FILE (test copies only): this file is the
    /// microphone, as fast as it reads (a timed feed fell behind on a loaded
    /// machine), so a test hears a known take without recording the room.
    @available(macOS 26.0, *)
    private func feedFile(_ path: String, into t: DictationTake) throws {
        let f = try AVAudioFile(forReading: URL(fileURLWithPath: path))
        feeding = true
        Task.detached { [weak self] in
            let chunk: AVAudioFrameCount = 1600
            var fed = 0
            while self?.feeding == true, f.framePosition < f.length,
                  let b = AVAudioPCMBuffer(pcmFormat: f.processingFormat, frameCapacity: chunk) {
                guard (try? f.read(into: b, frameCount: chunk)) != nil, b.frameLength > 0 else { break }
                t.ingest(b)
                fed += Int(b.frameLength)
                await Task.yield()
            }
            dlog("verdict=dictation_test_file_fed frames=\(fed) of=\(f.length) rate=\(f.processingFormat.sampleRate)")
        }
    }

    /// A quiet sound as listening starts and stops, as Wispr Flow does. None in a test copy.
    private func chime(_ name: String) {
        guard !desktopTestMode, let s = NSSound(named: name) else { return }
        s.volume = 0.3
        s.play()
    }

    private func stop() {
        if starting { stopAsked = true; return }
        guard #available(macOS 26.0, *), let t = take as? DictationTake else { return }
        take = nil
        feeding = false
        chime("Pop")
        let c = page
        Task { @MainActor in
            do {
                let r = try await t.finish()
                let words = r.text.split(whereSeparator: { $0.isWhitespace }).count
                dlog("verdict=dictation_native_done words=\(words) ms=\(r.ms) finish_ms=\(r.finishMs) audio_bytes=\(r.audio?.count ?? 0)")
                self.send(["type": "done", "text": r.text, "audio": r.audio?.base64EncodedString() ?? "", "mime": "audio/mp4",
                           "ms": r.ms, "finish_ms": r.finishMs], to: c)
            } catch {
                dlog("verdict=dictation_native_failed stage=finish error=\(error.localizedDescription)")
                self.send(["type": "error", "why": "Dictation failed: \(error.localizedDescription)"], to: c)
            }
        }
    }

    private func cancel() {
        cancelled = true
        feeding = false
        guard #available(macOS 26.0, *), let t = take as? DictationTake else { return }
        take = nil
        t.cancel()
        dlog("verdict=dictation_native_cancelled")
    }
}
