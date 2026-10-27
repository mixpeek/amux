// The Mac app's side of the native recorder (amux-recorder-core.swift):
// the page bridge, the menu bar dot, the hotkey, quit handling, test hooks.

import Cocoa
import CoreAudio
import UserNotifications

// MARK: - The app's side: the page bridge, the menu bar dot, the hotkey

var recorderStatusItem: NSStatusItem?
var meetingTimer: Timer?
var meetingSince: Date?
var meetingNotified = false
var meetingAutoStarted = false
var meetingEndedAt: Date?
var recorderPushTimer: Timer?
var recordHotKeyHandler: (() -> Void)?

extension AppDelegate {
    func setUpRecorder() {
        let r = NativeRecorder.shared
        r.recoverInterrupted()
        r.onChange = { [weak self] in self?.recorderChanged() }
        r.startShipping(host: self)
        recordHotKeyHandler = { [weak self] in self?.toggleRecording(nil) }
        meetingTimer = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] _ in self?.checkMeeting() }
    }

    // ── Meeting detection (opt-in) ──
    // Off by default. "ask": a notification when a call starts, recording only
    // if it is clicked. "auto": recording starts, and stops once the call has
    // been over for a minute. A call is another app using the microphone while
    // a conferencing app or a browser runs. Never records without the setting.
    static let meetingApps: Set<String> = ["us.zoom.xos", "com.microsoft.teams", "com.microsoft.teams2", "com.apple.FaceTime",
        "com.tinyspeck.slackmacgap", "com.cisco.webexmeetingsapp", "com.cisco.webex.meetings", "com.google.Chrome",
        "com.apple.Safari", "company.thebrowser.Browser", "org.mozilla.firefox", "com.microsoft.edgemac", "com.brave.Browser"]

    var meetingMode: String {
        get { UserDefaults.standard.string(forKey: "amuxRecorderMeeting") ?? "off" }
        set { UserDefaults.standard.set(newValue, forKey: "amuxRecorderMeeting") }
    }
    var meetingSystemAudio: Bool {
        get { UserDefaults.standard.bool(forKey: "amuxRecorderSystem") }
        set { UserDefaults.standard.set(newValue, forKey: "amuxRecorderSystem") }
    }

    func micInUseElsewhere() -> Bool {
        var dev = AudioDeviceID(0)
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        var addr = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDefaultInputDevice,
                                              mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &dev) == noErr, dev != 0 else { return false }
        var running: UInt32 = 0
        size = UInt32(MemoryLayout<UInt32>.size)
        addr.mSelector = kAudioDevicePropertyDeviceIsRunningSomewhere
        guard AudioObjectGetPropertyData(dev, &addr, 0, nil, &size, &running) == noErr else { return false }
        return running != 0
    }

    func checkMeeting() {
        let r = NativeRecorder.shared
        let mode = meetingMode
        guard mode != "off" else { meetingSince = nil; return }
        if r.isActive {
            // Auto-started and the call ended a minute ago: stop.
            if meetingAutoStarted, !micInUseElsewhereIgnoringSelf() {
                meetingEndedAt = meetingEndedAt ?? Date()
                if Date().timeIntervalSince(meetingEndedAt!) > 60 {
                    NSLog("amux-desktop: verdict=recorder_meeting_ended_stop")
                    meetingAutoStarted = false; meetingEndedAt = nil
                    r.stop(); recorderChanged()
                }
            } else { meetingEndedAt = nil }
            return
        }
        let apps = NSWorkspace.shared.runningApplications.compactMap { $0.bundleIdentifier }
        let inCall = micInUseElsewhere() && apps.contains(where: { AppDelegate.meetingApps.contains($0) })
        if !inCall { meetingSince = nil; meetingNotified = false; return }
        meetingSince = meetingSince ?? Date()
        // Ten seconds of a live microphone, so a short voice memo is not a meeting.
        guard Date().timeIntervalSince(meetingSince!) >= 10, !meetingNotified else { return }
        meetingNotified = true
        NSLog("amux-desktop: verdict=recorder_meeting_detected mode=%@", mode)
        if mode == "auto" {
            if r.start(sources: meetingSystemAudio ? ["mic", "system"] : ["mic"]) == nil { meetingAutoStarted = true }
            recorderChanged()
            postMeetingNotice("Recording this call", "amux started recording because a call began. Stop it from the menu bar.")
        } else {
            postMeetingNotice("A call started", "Click to record it with amux.")
        }
    }

    /// While our own recorder holds the microphone the device reads busy, so
    /// the end of a call is the end of every OTHER use: approximated by the
    /// conferencing apps no longer running, or the device going idle.
    func micInUseElsewhereIgnoringSelf() -> Bool {
        let apps = NSWorkspace.shared.runningApplications.compactMap { $0.bundleIdentifier }
        return apps.contains(where: { AppDelegate.meetingApps.contains($0) && !["com.google.Chrome", "com.apple.Safari",
            "company.thebrowser.Browser", "org.mozilla.firefox", "com.microsoft.edgemac", "com.brave.Browser"].contains($0) })
    }

    func postMeetingNotice(_ title: String, _ body: String) {
        let c = UNMutableNotificationContent()
        c.title = title
        c.body = body
        c.userInfo = ["recorder_meeting": 1]
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: "recorder-meeting", content: c, trigger: nil))
    }

    func meetingNotificationClicked() {
        let r = NativeRecorder.shared
        guard !r.isActive else { return }
        if let e = r.start(sources: meetingSystemAudio ? ["mic", "system"] : ["mic"]) { r.lastError = e }
        NSLog("amux-desktop: verdict=recorder_meeting_record_clicked")
        recorderChanged()
    }

    /// `window.webkit.messageHandlers.amuxRecorder.postMessage({op})` from the
    /// Record tab: start (with `sources`), stop, pause, resume, status.
    func recorderMessage(_ d: [String: Any], from c: DashboardWindowController) {
        let r = NativeRecorder.shared
        switch d["op"] as? String ?? "" {
        case "start":
            var wanted = (d["sources"] as? [String]) ?? ["mic"]
            // Tests only: the page's own button, without a permission prompt.
            if desktopTestMode, ProcessInfo.processInfo.environment["AMUX_RECORDER_FAKE_MIC"] == "1" {
                wanted = wanted.map { $0 == "mic" ? "tone" : $0 == "system" ? "tone2" : $0 }
            }
            if let e = r.start(sources: wanted) { r.lastError = e; NSLog("amux-desktop: verdict=recorder_start_refused error=%@", e) }
        case "stop": r.stop(); meetingAutoStarted = false
        case "pause": r.pause()
        case "resume": r.resume()
        case "settings":
            if let m = d["meeting"] as? String, ["off", "ask", "auto"].contains(m) { meetingMode = m }
            if let sys = d["system"] as? Bool { meetingSystemAudio = sys }
        default: break
        }
        pushRecorderStatus(to: [c])
        recorderChanged()
    }

    @objc func toggleRecording(_ sender: Any?) {
        let r = NativeRecorder.shared
        if r.isActive { r.stop() } else if let e = r.start(sources: ["mic"]) { r.lastError = e }
        recorderChanged()
    }
    @objc func pauseOrResumeRecording(_ sender: Any?) {
        let r = NativeRecorder.shared
        if r.state == "paused" { r.resume() } else { r.pause() }
    }
    @objc func openRecordTab(_ sender: Any?) {
        guard let s = serverURL else { return }
        openWindow(url: URL(string: normalized(s.absoluteString) + "/#view=record"), frame: nil)
    }

    func pushRecorderStatus(to cs: [DashboardWindowController]) {
        var st = NativeRecorder.shared.status()
        st["meeting"] = meetingMode
        guard let data = try? JSONSerialization.data(withJSONObject: st),
              let json = String(data: data, encoding: .utf8) else { return }
        for c in cs where c.webView.url.map(isServerURL) ?? false {
            c.webView.evaluateJavaScript("window._nativeRecorderStatus && window._nativeRecorderStatus(\(json)); 0", completionHandler: nil)
        }
    }

    func recorderChanged() {
        let r = NativeRecorder.shared
        pushRecorderStatus(to: controllers)
        if r.isActive {
            if recorderStatusItem == nil {
                let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
                let menu = NSMenu()
                menu.addItem(NSMenuItem(title: "Pause", action: #selector(pauseOrResumeRecording(_:)), keyEquivalent: ""))
                menu.addItem(NSMenuItem(title: "Stop recording", action: #selector(toggleRecording(_:)), keyEquivalent: ""))
                menu.addItem(.separator())
                menu.addItem(NSMenuItem(title: "Open recordings", action: #selector(openRecordTab(_:)), keyEquivalent: ""))
                menu.items.forEach { $0.target = self }
                item.menu = menu
                recorderStatusItem = item
            }
            recorderPushTimer?.invalidate()
            recorderPushTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
                self?.updateRecorderItem()
                self?.pushRecorderStatus(to: self?.controllers.filter { $0.window?.isVisible == true } ?? [])
            }
            updateRecorderItem()
        } else {
            recorderPushTimer?.invalidate(); recorderPushTimer = nil
            if let item = recorderStatusItem { NSStatusBar.system.removeStatusItem(item); recorderStatusItem = nil }
        }
    }

    func updateRecorderItem() {
        let r = NativeRecorder.shared
        guard let item = recorderStatusItem else { return }
        let ms = (r.status()["elapsed_ms"] as? Int64) ?? 0
        let t = ms / 1000
        let clock = t >= 3600 ? String(format: "%d:%02d:%02d", t / 3600, (t / 60) % 60, t % 60) : String(format: "%d:%02d", t / 60, t % 60)
        let paused = r.state == "paused"
        let s = NSMutableAttributedString(string: paused ? "❙❙ " : "● ", attributes: [.foregroundColor: paused ? NSColor.secondaryLabelColor : NSColor.systemRed])
        s.append(NSAttributedString(string: clock, attributes: [.font: NSFont.monospacedDigitSystemFont(ofSize: 12, weight: .regular)]))
        item.button?.attributedTitle = s
        item.button?.toolTip = "amux is recording" + (r.lastError.map { " (" + $0 + ")" } ?? "")
        item.menu?.items.first?.title = paused ? "Resume" : "Pause"
    }

    /// Quitting while recording: ask, unless macOS is shutting the app down,
    /// in which case the recording is stopped and saved first.
    func recorderAllowsQuit(fromSignal: Bool) -> Bool {
        let r = NativeRecorder.shared
        guard r.isActive else { return true }
        if !fromSignal {
            let a = NSAlert()
            a.messageText = "A recording is in progress"
            a.informativeText = "Quitting stops it and saves what was recorded. Keep recording instead?"
            a.addButton(withTitle: "Keep recording")
            a.addButton(withTitle: "Stop and quit")
            if a.runModal() == .alertFirstButtonReturn {
                NSLog("amux-desktop: verdict=recorder_quit_cancelled")
                return false
            }
        }
        r.stop()
        NSLog("amux-desktop: verdict=recorder_stopped_for_quit signal=%d", fromSignal ? 1 : 0)
        return true
    }

    /// amux://test/recorder?op=start&sources=tone|stop|pause|resume|sleep&s=N|state
    /// writes the status to <state file>.recorder.json.
    func recorderTestHook(_ q: [URLQueryItem]) {
        let r = NativeRecorder.shared
        let v = { (k: String) in q.first { $0.name == k }?.value ?? "" }
        switch v("op") {
        case "start": if let e = r.start(sources: v("sources").split(separator: ",").map(String.init)) { r.lastError = e }
        case "stop": r.stop()
        case "pause": r.pause()
        case "resume": r.resume()
        case "sleep": r.simulateSleep(Double(v("s")) ?? 3)
        case "ship": r.ship()
        default: break
        }
        recorderChanged()
        if let path = ProcessInfo.processInfo.environment["AMUX_DESKTOP_TEST_STATE"] {
            var st = r.status()
            st["status_item"] = recorderStatusItem != nil
            st["spool"] = recorderSpoolRoot().path
            if let data = try? JSONSerialization.data(withJSONObject: st, options: [.sortedKeys]) {
                try? data.write(to: URL(fileURLWithPath: path + ".recorder.json"), options: .atomic)
            }
        }
    }
}

extension AppDelegate: RecorderHost {
    var recorderServer: URL? { serverURL }
    func recorderToken(_ go: @escaping (String?) -> Void) { withAPIToken(go) }
    func recorderSessionDelegate() -> URLSessionDelegate? { InsecureDelegate() }
}
