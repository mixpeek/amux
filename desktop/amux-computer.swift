import Cocoa
import ApplicationServices

// Computer use on this Mac's own apps (owner, 2026-11-22: build Athena's
// computer use into amux; research/athena-mac-app-teardown.md).
//
// The app runs trycua's cua-driver from Contents/Helpers, embedded: it acts
// with Amux's own Accessibility and Screen Recording grants and never asks
// for its own. It works in the background, with its own cursor, without
// taking the owner's pointer or keyboard.
//
// The driver is this app's child over stdin and stdout (`cua-driver mcp
// --direct --embedded`), with no socket, so nothing but this app can reach it
// (amux-security, 2026-11-23: the old `serve --socket --no-permissions-gate`
// daemon answered any same-user process). The app writes
// <AMUX_HOME>/cua/endpoint.json naming the driver's pid, then long-polls the
// server with the owner bearer for calls that passed its per-app gate
// (integrations/computer_mac.rs), runs each on the driver and posts the
// result back.
//
// Off until the owner turns it on (Settings, or the menu bar's Computer Use
// submenu). Stop Computer Use (⌘⇧Esc anywhere) ends the driver, and with it
// every action in flight; it starts again on the next use of the switch.

/// cua-driver as a stdio MCP child: one JSON-RPC message per line.
final class MCPDriver {
    let proc = Process()
    private let toDriver = Pipe(), fromDriver = Pipe()
    private var buf = Data()
    private var nextId = 1
    private var pending: [Int: ([String: Any]?, String?) -> Void] = [:]
    private let q = DispatchQueue(label: "amux.cua.mcp")

    init(bin: URL, args: [String], env: [String: String], log: FileHandle?) {
        proc.executableURL = bin
        proc.arguments = args
        proc.environment = env
        proc.standardInput = toDriver
        proc.standardOutput = fromDriver
        if let log = log { proc.standardError = log }
    }

    func start() throws {
        fromDriver.fileHandleForReading.readabilityHandler = { [weak self] h in
            let d = h.availableData
            self?.q.async { self?.received(d) }
        }
        try proc.run()
    }

    func stop() {
        fromDriver.fileHandleForReading.readabilityHandler = nil
        try? toDriver.fileHandleForWriting.close()
        q.async { [weak self] in
            let waiting = self?.pending ?? [:]
            self?.pending = [:]
            for (_, done) in waiting { done(nil, "the driver stopped") }
        }
    }

    private func received(_ d: Data) {
        if d.isEmpty { return }
        buf.append(d)
        while let nl = buf.firstIndex(of: 0x0A) {
            let line = buf.subdata(in: buf.startIndex..<nl)
            buf.removeSubrange(buf.startIndex...nl)
            guard let obj = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any],
                  let id = obj["id"] as? Int, let done = pending.removeValue(forKey: id) else { continue }
            if let e = obj["error"] as? [String: Any] {
                done(nil, (e["message"] as? String) ?? "\(e)")
            } else {
                done(obj["result"] as? [String: Any] ?? [:], nil)
            }
        }
    }

    private func write(_ obj: [String: Any]) {
        guard var data = try? JSONSerialization.data(withJSONObject: obj) else { return }
        data.append(0x0A)
        toDriver.fileHandleForWriting.write(data)
    }

    func notify(_ method: String) {
        q.async { [weak self] in self?.write(["jsonrpc": "2.0", "method": method]) }
    }

    /// One request; `done` runs on this driver's queue, once.
    func request(_ method: String, _ params: [String: Any], timeout: Double = 45,
                 _ done: @escaping ([String: Any]?, String?) -> Void) {
        q.async { [weak self] in
            guard let self = self else { return }
            let id = self.nextId; self.nextId += 1
            self.pending[id] = done
            self.write(["jsonrpc": "2.0", "id": id, "method": method, "params": params])
            self.q.asyncAfter(deadline: .now() + timeout) { [weak self] in
                self?.pending.removeValue(forKey: id).map { $0(nil, "the driver did not answer \(method) within \(Int(timeout))s") }
            }
        }
    }
}

final class MacComputer {
    static let shared = MacComputer()
    private var proc: Process?
    private var driver: MCPDriver?
    /// The poll loop's generation: a stop or restart ends the old loop.
    private var pollGen = 0
    private var pollFailures = 0
    /// When the server last answered a poll.
    private(set) var lastPollOK: Date?
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
    /// Left by builds before 2026-11-23, whose daemon listened here.
    var oldSocket: URL { dir.appendingPathComponent("driver.sock") }
    var oldPidFile: URL { dir.appendingPathComponent("driver.pid") }

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
        if !missing.isEmpty { return "On, needs " + missing.joined(separator: " and ") }
        guard let ok = lastPollOK, Date().timeIntervalSince(ok) < 40 else { return "On, not connected to the server yet" }
        return "On: workers can use the apps you allow"
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
        endOldSocketDaemon()
        var env = ProcessInfo.processInfo.environment
        env["CUA_DRIVER_EMBEDDED"] = "1"
        env["CUA_DRIVER_TELEMETRY"] = "0"
        // Scripts are refused by the server's gate too; the driver refuses them as well.
        env["CUA_DRIVER_DISABLE_RUN_SCRIPT"] = "1"
        let log = dir.appendingPathComponent("driver.log")
        FileManager.default.createFile(atPath: log.path, contents: nil)
        let h = try? FileHandle(forWritingTo: log)
        h?.seekToEndOfFile()
        let d = MCPDriver(bin: bin, args: ["mcp", "--direct", "--embedded",
                                           "--host-bundle-id", Bundle.main.bundleIdentifier ?? "com.amux.desktop"],
                          env: env, log: h)
        let p = d.proc
        p.terminationHandler = { [weak self] t in
            DispatchQueue.main.async { self?.exited(t.terminationStatus) }
        }
        do { try d.start() } catch {
            dlog("verdict=computer_use_start_failed error=\(error.localizedDescription)")
            return
        }
        proc = p
        driver = d
        d.request("initialize", ["protocolVersion": "2025-06-18", "capabilities": [:] as [String: Any],
                                 "clientInfo": ["name": "amux-desktop", "version": "1"]]) { [weak self] _, err in
            if let err = err { dlog("verdict=computer_use_initialize_failed error=\(err)"); return }
            d.notify("notifications/initialized")
            DispatchQueue.main.async { self?.startPolling() }
        }
        // The driver's own telemetry off, once: it is content-free, but this is the owner's Mac.
        let t = Process(); t.executableURL = bin; t.arguments = ["telemetry", "disable"]
        t.standardOutput = FileHandle.nullDevice; t.standardError = FileHandle.nullDevice
        try? t.run()
        writeEndpoint(pid: p.processIdentifier, bin: bin)
        dlog("verdict=computer_use_started pid=\(p.processIdentifier) transport=stdio accessibility=\(accessibility ? 1 : 0) screen_recording=\(screenRecording ? 1 : 0)")
    }

    /// A build before 2026-11-23 ran `cua-driver serve` on driver.sock, which
    /// answered any process of this user. End that daemon (only when its pid
    /// is a cua-driver) and remove its socket, so no copy of it outlives the update.
    private func endOldSocketDaemon() {
        if let s = try? String(contentsOf: oldPidFile, encoding: .utf8), let pid = Int32(s.trimmingCharacters(in: .whitespacesAndNewlines)), pid > 0 {
            var path = [CChar](repeating: 0, count: 4096)
            if proc_pidpath(pid, &path, UInt32(path.count)) > 0, String(cString: path).hasSuffix("/cua-driver") {
                kill(pid, SIGTERM)
                dlog("verdict=computer_use_old_daemon_ended pid=\(pid)")
            }
        }
        for f in [oldSocket, oldPidFile] where FileManager.default.fileExists(atPath: f.path) {
            do { try FileManager.default.removeItem(at: f); dlog("verdict=computer_use_old_socket_removed path=\(f.lastPathComponent)") }
            catch { dlog("verdict=computer_use_old_socket_remove_failed path=\(f.path) error=\(error.localizedDescription)") }
        }
    }

    // ── The poll: the only way a call reaches the driver ──

    private func startPolling() {
        pollGen += 1
        pollFailures = 0
        poll(pollGen)
    }

    private func poll(_ gen: Int) {
        guard gen == pollGen, running, let d = driver else { return }
        guard let base = AmuxDesktop.delegate.serverURL else { retryPoll(gen, why: "no server connection"); return }
        AmuxDesktop.delegate.withAPIToken { [weak self] token in
            guard let self = self, gen == self.pollGen else { return }
            guard let token = token else { self.retryPoll(gen, why: "no owner token yet (open an amux window)"); return }
            self.send(base: base, path: "/api/computer/mac/broker/next?wait=25", token: token, body: nil, timeout: 40) { status, obj in
                guard gen == self.pollGen else { return }
                guard status == 200, let obj = obj else { self.retryPoll(gen, why: "poll answered \(status)"); return }
                self.pollFailures = 0
                self.lastPollOK = Date()
                self.restoreEndpointIfMissing()
                if let job = obj["job"] as? [String: Any], let id = job["id"] as? String, let tool = job["tool"] as? String {
                    let args = job["args"] as? [String: Any] ?? [:]
                    d.request("tools/call", ["name": tool, "arguments": args]) { result, err in
                        var answer: [String: Any] = ["id": id]
                        if let err = err { answer["error"] = err } else { answer["result"] = result ?? [:] }
                        DispatchQueue.main.async {
                            self.send(base: base, path: "/api/computer/mac/broker/result", token: token, body: answer, timeout: 20) { st, _ in
                                if st != 200 { dlog("verdict=computer_use_result_not_taken status=\(st) tool=\(tool)") }
                            }
                        }
                    }
                }
                self.poll(gen)
            }
        }
    }

    private func retryPoll(_ gen: Int, why: String) {
        pollFailures += 1
        // Logged on the first failure and every 20th after, so a long gap is visible but quiet.
        if pollFailures == 1 || pollFailures % 20 == 0 { dlog("verdict=computer_use_poll_failed failures=\(pollFailures) why=\(why)") }
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self] in self?.poll(gen) }
    }

    private func send(base: URL, path: String, token: String, body: [String: Any]?, timeout: Double,
                      _ done: @escaping (Int, [String: Any]?) -> Void) {
        guard let url = URL(string: normalized(base.absoluteString) + path) else { done(0, nil); return }
        var req = URLRequest(url: url)
        req.httpMethod = body == nil ? "GET" : "POST"
        req.timeoutInterval = timeout
        req.setValue("Bearer " + token, forHTTPHeaderField: "Authorization")
        if let b = body, let data = try? JSONSerialization.data(withJSONObject: b) {
            req.httpBody = data
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let session = URLSession(configuration: .ephemeral, delegate: InsecureDelegate(), delegateQueue: nil)
        session.dataTask(with: req) { data, resp, _ in
            let status = (resp as? HTTPURLResponse)?.statusCode ?? 0
            let obj = data.flatMap { try? JSONSerialization.jsonObject(with: $0) } as? [String: Any]
            DispatchQueue.main.async {
                // A refused token is stale: drop it so the next poll reads the dashboard's current one.
                if status == 401 { AmuxDesktop.delegate.apiToken = nil }
                done(status, obj)
            }
            session.finishTasksAndInvalidate()
        }.resume()
    }

    /// Removes endpoint.json only when it names this app's own driver. Any
    /// other copy of the app (a quiet test copy shares ~/.amux) quitting used
    /// to delete the owner's file, so the server read computer use as off while
    /// the owner's driver kept running (2026-11-25, twice in one morning).
    private func removeOwnEndpoint(_ pid: Int32?, why: String) {
        guard let data = try? Data(contentsOf: endpointFile),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        let named = (obj["pid"] as? NSNumber)?.int32Value
        guard let pid = pid, named == pid else {
            dlog("verdict=computer_use_endpoint_kept why=\(why) names_pid=\(named.map(String.init) ?? "-") own_pid=\(pid.map(String.init) ?? "none")")
            return
        }
        try? FileManager.default.removeItem(at: endpointFile)
    }

    func stop(reason: String) {
        removeOwnEndpoint(proc?.processIdentifier, why: reason)
        pollGen += 1
        lastPollOK = nil
        driver?.stop()
        driver = nil
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
        let pid = proc?.processIdentifier
        proc = nil
        pollGen += 1
        driver?.stop()
        driver = nil
        if wasStopping { return }
        removeOwnEndpoint(pid, why: "driver_exited")
        dlog("verdict=computer_use_driver_exited status=\(status) restarts=\(restarts)")
        // A crash restarts, a few times, then stays down and says so.
        guard restarts < 3, (AmuxDesktop.delegate.config.computerUse ?? false), !stoppedByOwner else { return }
        restarts += 1
        DispatchQueue.main.asyncAfter(deadline: .now() + Double(restarts * 2)) { [weak self] in self?.start() }
    }

    /// A running driver whose endpoint.json is gone (deleted by another copy,
    /// or by hand) writes it again, so the server sees it within one poll.
    private func restoreEndpointIfMissing() {
        guard let p = proc, p.isRunning, let bin = binary, !FileManager.default.fileExists(atPath: endpointFile.path) else { return }
        writeEndpoint(pid: p.processIdentifier, bin: bin)
        dlog("verdict=computer_use_endpoint_restored pid=\(p.processIdentifier)")
    }

    private func writeEndpoint(pid: Int32, bin: URL) {
        let d: [String: Any] = ["binary": bin.path, "transport": "broker", "pid": Int(pid),
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
