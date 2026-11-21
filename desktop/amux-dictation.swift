import Cocoa
import WebKit
import AVFoundation
import CoreAudio

// Dictation in the Mac app: the page asks (app.js _dictNativeStart), the app
// listens and recognizes on device (amux-dictation-core.swift), and the page
// gets the text back and puts it where you were typing. Holding 🌐 dictates
// until you let go (amux-workspace.swift sends "Globe Hold" and "Globe
// Release"), the way Wispr Flow's fn key does.
//
// Messages from the page (amuxDictation): {op: "start", hints: [String],
// input: String (the microphone's name from Dictation settings, "" = default)},
// {op: "stop"}, {op: "cancel"}. Back to the page (window._dictNativeEvent):
// {type: "started"}, {type: "level", v}, {type: "notice", why}, {type: "unavailable", why} (the
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

    /// The page whose take is running, if any. 🌐 goes there rather than to the
    /// focused page (owner, 2026-11-17: a take kept running in one worker's page
    /// while 🌐 in another worker's page started a second one).
    var owner: DashboardWindowController? { (take != nil || starting) ? page : nil }

    func message(_ d: [String: Any], from c: DashboardWindowController) {
        switch d["op"] as? String {
        case "start": start(hints: (d["hints"] as? [String]) ?? [], input: (d["input"] as? String) ?? "", page: c)
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

    private func start(hints: [String], input: String, page c: DashboardWindowController) {
        guard take == nil, !starting else {
            // One take per app. The asking page is told, or its pill would sit
            // on "recording" with nothing behind it.
            dlog("verdict=dictation_native_busy view=\(c.viewHash)")
            if c !== page { send(["type": "busy"], to: c) }
            return
        }
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
                    if ok { self.start(hints: hints, input: input, page: c) }
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
                    if let why = try t.startMic(input: input) { self.send(["type": "notice", "why": why]) }
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

/// Microphones coming and going (owner, 2026-11-21: "a little notification when a
/// microphone is connected or disconnected and the dictation is using it or not").
/// CoreAudio says when the device list or the default input changes; after the
/// change settles, the page you are in is told what came and went
/// (window._dictMicsChanged in app.js), and it says whether dictation uses it, since
/// the microphone chosen in Dictation settings lives in the page.
final class MicWatch {
    static let shared = MicWatch()
    private var known: [String]?
    private var lastDefault = ""
    private var settle: DispatchWorkItem?
    /// amux://test/mics: inputs a test copy pretends are connected, and its default.
    var testExtra: [String] = []
    var testDefault: String?

    func start() {
        known = inputs()
        lastDefault = defaultName()
        dlog("verdict=mic_watch_started inputs=\(known?.count ?? 0)")
        let sys = AudioObjectID(kAudioObjectSystemObject)
        for sel in [kAudioHardwarePropertyDevices, kAudioHardwarePropertyDefaultInputDevice] {
            var a = AudioObjectPropertyAddress(mSelector: sel, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
            let st = AudioObjectAddPropertyListenerBlock(sys, &a, DispatchQueue.main) { [weak self] _, _ in self?.changed() }
            if st != noErr { dlog("verdict=mic_watch_listen_failed selector=\(sel) status=\(st)") }
        }
    }

    /// A plug-in moves the list and then the default; one look once both have.
    func changed() {
        settle?.cancel()
        let w = DispatchWorkItem { [weak self] in self?.report() }
        settle = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6, execute: w)
    }

    private func inputs() -> [String] {
        var names = recorderInputDevices().map { $0.name }
        for n in testExtra where !names.contains(n) { names.append(n) }
        return names
    }

    private func defaultName() -> String {
        if let t = testDefault { return t }
        var a = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDefaultInputDevice, mScope: kAudioObjectPropertyScopeGlobal,
                                           mElement: kAudioObjectPropertyElementMain)
        var id = AudioDeviceID(0)
        var n = UInt32(MemoryLayout<AudioDeviceID>.size)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &n, &id) == noErr else { return "" }
        return recorderInputDevices().first { $0.id == id }?.name ?? ""
    }

    private func report() {
        let now = inputs(), def = defaultName(), wasDefault = lastDefault
        let before = known ?? now
        known = now
        lastDefault = def
        let added = now.filter { !before.contains($0) }, removed = before.filter { !now.contains($0) }
        guard !added.isEmpty || !removed.isEmpty else { return }
        let c = (NSApp.delegate as? AppDelegate)?.keyPage
        dlog("verdict=mic_changed added=\(added.joined(separator: ",")) removed=\(removed.joined(separator: ",")) default=\(def) told=\(c?.viewHash ?? "none")")
        guard let c = c, let data = try? JSONSerialization.data(withJSONObject: [
            "added": added, "removed": removed, "inputs": now, "default": def, "was_default": wasDefault]),
              let json = String(data: data, encoding: .utf8) else { return }
        c.webView.evaluateJavaScript("typeof window._dictMicsChanged === 'function' && window._dictMicsChanged(\(json))", completionHandler: nil)
    }
}
