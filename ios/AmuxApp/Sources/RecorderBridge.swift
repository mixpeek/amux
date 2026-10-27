// The iOS app's side of the native recorder (desktop/amux-recorder-core.swift,
// docs/design/recorder.md, MSG-98487).
//
// The Record tab used the page's MediaRecorder, and iOS suspends a hidden
// app's web content: the microphone track ended and so did the recording.
// Here the app records natively under the `audio` background mode, so hiding
// the app or locking the phone keeps recording. Segments go to the app's own
// storage first and upload when the server is reachable, offline included.
// A phone call or Siri interrupts the audio session: the gap is recorded and
// recording resumes when the interruption ends.

import AVFoundation
import UIKit
import WebKit

final class RecorderBridge: NSObject, RecorderHost {
    static let shared = RecorderBridge()
    weak var webView: WKWebView?
    private var token: String?
    private var started = false

    private final class TrustAll: NSObject, URLSessionDelegate {
        func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge,
                        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
            if let trust = challenge.protectionSpace.serverTrust {
                completionHandler(.useCredential, URLCredential(trust: trust))
            } else {
                completionHandler(.performDefaultHandling, nil)
            }
        }
    }

    var recorderServer: URL? { AmuxStore.serverURL }

    /// The owner token the dashboard shell hands the page, borrowed the same
    /// way the Mac app borrows it. Cached for the process.
    func recorderToken(_ go: @escaping (String?) -> Void) {
        if let t = token { go(t); return }
        DispatchQueue.main.async {
            guard let wv = self.webView else { go(nil); return }
            wv.evaluateJavaScript("window._AMUX_AUTH_TOKEN || ''") { v, _ in
                let t = (v as? String).flatMap { $0.isEmpty ? nil : $0 }
                self.token = t
                go(t)
            }
        }
    }

    func recorderSessionDelegate() -> URLSessionDelegate? { TrustAll() }

    func setUp(webView: WKWebView) {
        self.webView = webView
        guard !started else { return }
        started = true
        let r = NativeRecorder.shared
        r.recoverInterrupted()
        r.onChange = { [weak self] in self?.push() }
        r.startShipping(host: self)
        let nc = NotificationCenter.default
        nc.addObserver(forName: AVAudioSession.interruptionNotification, object: nil, queue: .main) { n in
            let type = (n.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt).flatMap(AVAudioSession.InterruptionType.init)
            NativeRecorder.shared.interrupted(type == .began)
        }
    }

    func handle(_ body: [String: Any]) {
        let r = NativeRecorder.shared
        switch body["op"] as? String ?? "" {
        case "start":
            do {
                let s = AVAudioSession.sharedInstance()
                try s.setCategory(.playAndRecord, mode: .default, options: [.mixWithOthers, .allowBluetooth, .defaultToSpeaker])
                try s.setActive(true)
            } catch {
                r.lastError = "the audio session: " + error.localizedDescription
                break
            }
            // System audio is a Mac feature; the phone records its microphone.
            let wanted = ((body["sources"] as? [String]) ?? ["mic"]).filter { $0 != "system" }
            if let e = r.start(sources: wanted.isEmpty ? ["mic"] : wanted) { r.lastError = e }
        case "stop":
            r.stop()
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        case "pause": r.pause()
        case "resume": r.resume()
        default: break
        }
        push()
    }

    func push() {
        DispatchQueue.main.async {
            guard let wv = self.webView,
                  let data = try? JSONSerialization.data(withJSONObject: NativeRecorder.shared.status()),
                  let json = String(data: data, encoding: .utf8) else { return }
            wv.evaluateJavaScript("window._nativeRecorderStatus && window._nativeRecorderStatus(\(json)); 0", completionHandler: nil)
        }
    }
}
