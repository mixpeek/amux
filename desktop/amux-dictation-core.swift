import Foundation
import AVFoundation
import Speech
#if os(macOS)
import CoreAudio
import AudioToolbox
#endif

// Dictation's engine in the Mac app (owner, 2026-11-16: "Improve the dictation
// flow ... until its as clean as wisprflow"). Wispr Flow put text in a median
// 0.6 s after the owner stopped talking (his last 500 takes). amux sent the
// whole clip to the server after the take, where whisper runs as an Intel
// build under Rosetta with no GPU: 5.1 s for two words, 6 to 11 s for ten
// seconds of speech. Apple's on-device recognizer (SpeechAnalyzer, macOS 26)
// hears the take while it is spoken, so at release only the last words are
// left: 10 s of speech came back in 0.55 s even when run after the fact.
//
// One take: the microphone (or, in a test, a file) goes to the recognizer,
// a level for the page's bars, and an AAC copy that the server keeps like any
// other dictation's audio. No UI here; amux-dictation.swift wires it to pages.

enum DictationError: LocalizedError {
    case unavailable(String)
    var errorDescription: String? { if case .unavailable(let w) = self { return w }; return nil }
}

/// The audio a take kept, as the server stores it.
struct DictationResult {
    let text: String
    let audio: Data?
    let ms: Int          // how long the take was
    let finishMs: Int    // stop to text: the wait the owner sees
}

@available(macOS 26.0, *)
final class DictationTake {
    let transcriber: SpeechTranscriber
    let analyzer: SpeechAnalyzer
    let format: AVAudioFormat
    private let feed: AsyncStream<AnalyzerInput>.Continuation
    private let results: Task<String, Error>
    private let queue = DispatchQueue(label: "amux.dictation")
    private var converter: AVAudioConverter?
    private var file: AVAudioFile?
    private let fileURL: URL
    private var engine: AVAudioEngine?
    private var lastLevelAt: TimeInterval = 0
    private(set) var buffersSeen = 0
    let startedAt = Date()
    /// About 15 a second, 0 to 1, on the main queue.
    var onLevel: ((Double) -> Void)?

    /// Whether this Mac can dictate on device, and if not, why. A missing
    /// language model starts downloading, so the next take can.
    static func ready() async -> String? {
        guard let loc = await locale() else { return "no on-device speech model for this language" }
        let t = SpeechTranscriber(locale: loc, transcriptionOptions: [], reportingOptions: [], attributeOptions: [])
        // status() reads "supported" for a model another app installed, so
        // the installed list is asked first.
        if await SpeechTranscriber.installedLocales.contains(where: { $0.identifier == loc.identifier }) { return nil }
        if await AssetInventory.status(forModules: [t]) == .unsupported { return "this Mac has no on-device speech model for \(loc.identifier)" }
        do {
            guard let r = try await AssetInventory.assetInstallationRequest(supporting: [t]) else { return nil }
            Task {
                do {
                    try await r.downloadAndInstall()
                    NSLog("amux-desktop: verdict=dictation_model_installed locale=%@", loc.identifier)
                } catch {
                    NSLog("amux-desktop: verdict=dictation_model_install_failed error=%@", error.localizedDescription)
                }
            }
            return "the on-device speech model is downloading"
        } catch {
            return "the on-device speech model could not be installed: \(error.localizedDescription)"
        }
    }

    static func locale() async -> Locale? {
        if let l = await SpeechTranscriber.supportedLocale(equivalentTo: Locale.current) { return l }
        return await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: "en-US"))
    }

    /// `hints` are words the recognizer should expect: worker names and the
    /// owner's dictionary.
    init(hints: [String]) async throws {
        if let why = await DictationTake.ready() { throw DictationError.unavailable(why) }
        guard let loc = await DictationTake.locale() else { throw DictationError.unavailable("no on-device speech model") }
        let t = SpeechTranscriber(locale: loc, transcriptionOptions: [], reportingOptions: [], attributeOptions: [])
        guard let fmt = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [t]) else {
            throw DictationError.unavailable("the speech model gave no audio format")
        }
        transcriber = t
        format = fmt
        analyzer = SpeechAnalyzer(modules: [t])
        let (stream, cont) = AsyncStream<AnalyzerInput>.makeStream()
        feed = cont
        results = Task { try await t.results.reduce("") { $0 + String($1.text.characters) } }
        fileURL = FileManager.default.temporaryDirectory.appendingPathComponent("amux-dictation-\(UUID().uuidString).m4a")
        if !hints.isEmpty {
            let ctx = AnalysisContext()
            ctx.contextualStrings[.general] = Array(hints.prefix(200))
            try? await analyzer.setContext(ctx)
        }
        try await analyzer.start(inputSequence: stream)
    }

    /// The microphone, from now until finish() or cancel(). `input` names the microphone
    /// chosen in Dictation settings ("" is the system default). One that is not connected
    /// falls back to the default, and the returned note says so.
    @discardableResult
    func startMic(input name: String = "") throws -> String? {
        let e = AVAudioEngine()
        let input = e.inputNode
        var note: String?
        #if os(macOS)
        if !name.isEmpty {
            if let dev = recorderInputDevices().first(where: { $0.name == name }), let unit = input.audioUnit {
                var id = dev.id
                let st = AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &id,
                                              UInt32(MemoryLayout<AudioDeviceID>.size))
                NSLog("amux-dictation: verdict=%@ device=%@ status=%d", st == noErr ? "dictation_input_device" : "dictation_input_switch_failed", name, st)
                if st != noErr { note = "Could not switch to \(name); using the default microphone." }
            } else {
                NSLog("amux-dictation: verdict=dictation_input_missing device=%@", name)
                note = "\(name) is not connected; using the default microphone."
            }
        }
        #endif
        let fmt = input.outputFormat(forBus: 0)
        guard fmt.sampleRate > 0, fmt.channelCount > 0 else {
            throw DictationError.unavailable("no microphone input is available")
        }
        input.installTap(onBus: 0, bufferSize: 1024, format: fmt) { [weak self] buf, _ in self?.ingest(buf) }
        e.prepare()
        try e.start()
        engine = e
        return note
    }

    /// One buffer of the take, in any format: to the recognizer, the level and the kept copy.
    func ingest(_ buf: AVAudioPCMBuffer) {
        buffersSeen += 1
        level(buf)
        queue.async { [self] in
            if converter == nil || converter?.inputFormat != buf.format {
                converter = AVAudioConverter(from: buf.format, to: format)
            }
            guard let conv = converter else { return }
            let cap = AVAudioFrameCount(Double(buf.frameLength) * format.sampleRate / buf.format.sampleRate) + 64
            guard let out = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: cap) else { return }
            var given = false
            var err: NSError?
            conv.convert(to: out, error: &err) { _, status in
                if given { status.pointee = .noDataNow; return nil }
                given = true
                status.pointee = .haveData
                return buf
            }
            guard err == nil, out.frameLength > 0 else { return }
            feed.yield(AnalyzerInput(buffer: out))
            keep(out)
        }
    }

    private func keep(_ b: AVAudioPCMBuffer) {
        if file == nil {
            let settings: [String: Any] = [AVFormatIDKey: kAudioFormatMPEG4AAC, AVSampleRateKey: format.sampleRate,
                                           AVNumberOfChannelsKey: Int(format.channelCount), AVEncoderBitRateKey: 24000]
            file = try? AVAudioFile(forWriting: fileURL, settings: settings, commonFormat: format.commonFormat, interleaved: format.isInterleaved)
        }
        try? file?.write(from: b)
    }

    private func level(_ b: AVAudioPCMBuffer) {
        let now = ProcessInfo.processInfo.systemUptime
        guard now - lastLevelAt > 0.066, let ch = b.floatChannelData?[0], b.frameLength > 0 else { return }
        lastLevelAt = now
        var sum: Float = 0
        for i in 0..<Int(b.frameLength) { sum += ch[i] * ch[i] }
        let rms = sqrt(sum / Float(b.frameLength))
        // Speech sits around -40 to -10 dBFS; that range fills the bars.
        let v = max(0, min(1, (20 * log10(max(rms, 1e-6)) + 50) / 40))
        DispatchQueue.main.async { [weak self] in self?.onLevel?(Double(v)) }
    }

    private func stopSource() {
        engine?.inputNode.removeTap(onBus: 0)
        engine?.stop()
        engine = nil
    }

    /// Stops listening and returns everything said.
    func finish() async throws -> DictationResult {
        let stopAt = Date()
        stopSource()
        queue.sync {}            // the last buffers reach the recognizer first
        feed.finish()
        try await analyzer.finalizeAndFinishThroughEndOfInput()
        let text = try await results.value.trimmingCharacters(in: .whitespacesAndNewlines)
        queue.sync { file = nil }   // closes the AAC file
        let audio = try? Data(contentsOf: fileURL)
        try? FileManager.default.removeItem(at: fileURL)
        return DictationResult(text: text, audio: audio, ms: Int(stopAt.timeIntervalSince(startedAt) * 1000),
                               finishMs: Int(Date().timeIntervalSince(stopAt) * 1000))
    }

    func cancel() {
        stopSource()
        feed.finish()
        Task { await analyzer.cancelAndFinishNow() }
        results.cancel()
        queue.async { [self] in file = nil; try? FileManager.default.removeItem(at: fileURL) }
    }
}
