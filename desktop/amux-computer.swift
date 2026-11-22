import Cocoa
import ApplicationServices

// Computer use on this Mac's own apps (owner, 2026-11-22: build Athena's
// computer use into amux; research/athena-mac-app-teardown.md).
//
// The app runs trycua's cua-driver from Contents/Helpers, embedded: it acts
// with Amux's own Accessibility and Screen Recording grants and never asks
// for its own. It works in the background, with its own cursor, without
// taking the owner's pointer or keyboard. The app writes
// <AMUX_HOME>/cua/endpoint.json naming the binary and its socket; the server
// (integrations/computer_mac.rs) forwards a worker's tool calls to it after
// checking which apps that worker may use.
//
// Off until the owner turns it on (Settings, or the menu bar's Computer Use
// submenu). Stop Computer Use (⌘⇧Esc anywhere) ends the driver, and with it
// every action in flight; it starts again on the next use of the switch.

final class MacComputer {
    static let shared = MacComputer()
    private var proc: Process?
    private var restarts = 0
    private var stopping = false
    /// Set by Stop Computer Use; cleared by turning the switch on again.
    private(set) var stoppedByOwner = false

    var home: URL {
        if let h = ProcessInfo.processInfo.environment["AMUX_HOME"], !h.isEmpty { return URL(fileURLWithPath: h) }
        return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".amux")
    }
    var dir: URL { home.appendingPathComponent("cua") }
    var endpointFile: URL { dir.appendingPathComponent("endpoint.json") }
    var socket: URL { dir.appendingPathComponent("driver.sock") }

    /// The bundled driver; AMUX_CUA_DRIVER points a test copy at another one.
    var binary: URL? {
        if let p = ProcessInfo.processInfo.environment["AMUX_CUA_DRIVER"], !p.isEmpty { return URL(fileURLWithPath: p) }
        let b = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/cua-driver")
        return FileManager.default.isExecutableFile(atPath: b.path) ? b : nil
    }

    var running: Bool { proc?.isRunning == true }
    var accessibility: Bool { AXIsProcessTrusted() }
    var screenRecording: Bool { CGPreflightScreenCaptureAccess() }

    /// One line for the menu and Settings.
    var stateLine: String {
        if binary == nil { return "Not in this build of amux" }
        if !running { return stoppedByOwner ? "Stopped" : "Off" }
        let missing = [accessibility ? nil : "Accessibility", screenRecording ? nil : "Screen Recording"].compactMap { $0 }
        return missing.isEmpty ? "On: workers can use the apps you allow" : "On, needs " + missing.joined(separator: " and ")
    }

    /// Starts or stops the driver to match the switch.
    func apply(_ on: Bool) {
        if on { stoppedByOwner = false; start() } else { stop(reason: "switch_off") }
    }

    func start() {
        guard !running else { return }
        guard let bin = binary else {
            dlog("verdict=computer_use_unavailable reason=no_driver_in_bundle")
            return
        }
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try? FileManager.default.removeItem(at: socket)
        let p = Process()
        p.executableURL = bin
        p.arguments = ["serve", "--embedded", "--no-permissions-gate", "--no-automation-prompts",
                       "--host-bundle-id", Bundle.main.bundleIdentifier ?? "com.amux.desktop",
                       "--socket", socket.path, "--pid-file", dir.appendingPathComponent("driver.pid").path]
        var env = ProcessInfo.processInfo.environment
        env["CUA_DRIVER_EMBEDDED"] = "1"
        env["CUA_DRIVER_TELEMETRY"] = "0"
        p.environment = env
        let log = dir.appendingPathComponent("driver.log")
        FileManager.default.createFile(atPath: log.path, contents: nil)
        if let h = try? FileHandle(forWritingTo: log) { h.seekToEndOfFile(); p.standardOutput = h; p.standardError = h }
        p.terminationHandler = { [weak self] t in
            DispatchQueue.main.async { self?.exited(t.terminationStatus) }
        }
        do { try p.run() } catch {
            dlog("verdict=computer_use_start_failed error=\(error.localizedDescription)")
            return
        }
        proc = p
        // The driver's own telemetry off, once: it is content-free, but this is the owner's Mac.
        let t = Process(); t.executableURL = bin; t.arguments = ["telemetry", "disable"]
        t.standardOutput = FileHandle.nullDevice; t.standardError = FileHandle.nullDevice
        try? t.run()
        writeEndpoint(pid: p.processIdentifier, bin: bin)
        dlog("verdict=computer_use_started pid=\(p.processIdentifier) accessibility=\(accessibility ? 1 : 0) screen_recording=\(screenRecording ? 1 : 0)")
    }

    func stop(reason: String) {
        try? FileManager.default.removeItem(at: endpointFile)
        guard let p = proc, p.isRunning else { proc = nil; return }
        stopping = true
        p.terminate()
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { if p.isRunning { kill(p.processIdentifier, SIGKILL) } }
        dlog("verdict=computer_use_stopped reason=\(reason) pid=\(p.processIdentifier)")
    }

    /// Stop Computer Use: ends every action now, and stays off until the switch is used again.
    func stopByOwner() {
        stoppedByOwner = true
        stop(reason: "owner_stop")
    }

    private func exited(_ status: Int32) {
        let wasStopping = stopping
        stopping = false
        proc = nil
        if wasStopping { return }
        try? FileManager.default.removeItem(at: endpointFile)
        dlog("verdict=computer_use_driver_exited status=\(status) restarts=\(restarts)")
        // A crash restarts, a few times, then stays down and says so.
        guard restarts < 3, (AmuxDesktop.delegate.config.computerUse ?? false), !stoppedByOwner else { return }
        restarts += 1
        DispatchQueue.main.asyncAfter(deadline: .now() + Double(restarts * 2)) { [weak self] in self?.start() }
    }

    private func writeEndpoint(pid: Int32, bin: URL) {
        let d: [String: Any] = ["binary": bin.path, "socket": socket.path, "pid": Int(pid),
                                "app": Bundle.main.bundleIdentifier ?? "", "started_at": Int(Date().timeIntervalSince1970)]
        if let data = try? JSONSerialization.data(withJSONObject: d, options: [.sortedKeys]) {
            try? data.write(to: endpointFile, options: .atomic)
        }
    }

    /// The System Settings pane for a permission this needs.
    func openPrivacyPane(_ which: String) {
        if which == "screen" { _ = CGRequestScreenCaptureAccess() }
        else {
            let opts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
            _ = AXIsProcessTrustedWithOptions(opts)
        }
        let anchor = which == "screen" ? "Privacy_ScreenCapture" : "Privacy_Accessibility"
        if let u = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(anchor)") { NSWorkspace.shared.open(u) }
    }
}

extension AppDelegate {
    func startComputerUse() {
        MacComputer.shared.apply(config.computerUse ?? false)
    }

    @objc func toggleComputerUse(_ sender: Any?) {
        config = loadConfig()
        config.computerUse = !(config.computerUse ?? false)
        saveConfig(config)
        MacComputer.shared.apply(config.computerUse ?? false)
    }

    @objc func stopComputerUse(_ sender: Any?) { MacComputer.shared.stopByOwner() }
    @objc func showQuickAsk(_ sender: Any?) { QuickAsk.shared.show() }
    @objc func grantAccessibility(_ sender: Any?) { MacComputer.shared.openPrivacyPane("accessibility") }
    @objc func grantScreenRecording(_ sender: Any?) { MacComputer.shared.openPrivacyPane("screen") }
}

/// ⌘⇧Esc from any app (registered with the other Carbon hotkeys) stops computer use, as in Athena.
func stopComputerUseHotKey() {
    guard MacComputer.shared.running else { return }
    MacComputer.shared.stopByOwner()
}
