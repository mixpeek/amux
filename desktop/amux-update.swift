import Cocoa

// Updates (Settings > Updates, amux > Check for Updates…).
//
// Every push to main that touches the app publishes a universal build to the
// `desktop-latest` release (.github/workflows/desktop.yml), and build-desktop.sh
// stamps the commit a build came from into Info.plist (AmuxBuildCommit). A
// check asks GitHub where the release tag points and how that commit compares
// with this app's; an update downloads the zip, checks the bundle, swaps it in
// place of this one and relaunches. Windows are saved on quit as always, so the
// new build reopens them where they were.
//
// AMUX_DESKTOP_UPDATE_API and AMUX_DESKTOP_UPDATE_ZIP point both at a test
// server (desktop/test/update-e2e.py).

final class Updater {
    static let shared = Updater()

    enum State { case idle, checking, upToDate, available, installing, failed }
    private(set) var state = State.idle
    private(set) var detail = ""
    private(set) var latest = ""
    private(set) var checkedAt: Date?
    /// Called on the main queue whenever state changes (Settings redraws).
    var changed: (() -> Void)?

    let api: String
    let zip: String

    init() {
        let env = ProcessInfo.processInfo.environment
        api = env["AMUX_DESKTOP_UPDATE_API"] ?? "https://api.github.com/repos/mixpeek/amux"
        zip = env["AMUX_DESKTOP_UPDATE_ZIP"] ?? "https://github.com/mixpeek/amux/releases/download/desktop-latest/Amux-macOS.zip"
    }

    /// The commit this build came from ("" for a build made before the stamp).
    var commit: String { Bundle.main.object(forInfoDictionaryKey: "AmuxBuildCommit") as? String ?? "" }
    var builtAt: String { Bundle.main.object(forInfoDictionaryKey: "AmuxBuildDate") as? String ?? "" }

    private func set(_ s: State, _ d: String) {
        state = s
        detail = d
        changed?()
    }

    private func get(_ url: String, _ done: @escaping (Int, Any?) -> Void) {
        guard let u = URL(string: url) else { done(0, nil); return }
        var req = URLRequest(url: u)
        req.timeoutInterval = 20
        req.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        URLSession.shared.dataTask(with: req) { data, resp, _ in
            let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
            let obj = data.flatMap { try? JSONSerialization.jsonObject(with: $0) }
            DispatchQueue.main.async { done(code, obj) }
        }.resume()
    }

    func check() {
        if state == .checking || state == .installing { return }
        set(.checking, "Checking…")
        get(api + "/git/ref/tags/desktop-latest") { [weak self] code, obj in
            guard let self = self else { return }
            guard code == 200, let sha = ((obj as? [String: Any])?["object"] as? [String: Any])?["sha"] as? String else {
                dlog("verdict=desktop_update_check_failed http=\(code)")
                self.set(.failed, code == 403 ? "GitHub is rate limiting checks from this network. Try again in a while."
                                              : "Could not reach GitHub (HTTP \(code)).")
                return
            }
            self.latest = sha
            self.checkedAt = Date()
            let mine = self.commit
            if mine == sha {
                dlog("verdict=desktop_update_none commit=\(mine.prefix(9))")
                self.set(.upToDate, "You have the latest build.")
                return
            }
            if mine.isEmpty || mine.hasSuffix("-dirty") {
                dlog("verdict=desktop_update_available commit=\(mine.isEmpty ? "unstamped" : mine) latest=\(sha.prefix(9))")
                self.set(.available, "The latest build is from \(sha.prefix(9)). This one "
                    + (mine.isEmpty ? "does not record what it was built from." : "was built from uncommitted changes."))
                return
            }
            // A local build from a later commit already has the release's changes.
            self.get(self.api + "/compare/\(sha)...\(mine)") { code, obj in
                let st = (obj as? [String: Any])?["status"] as? String ?? ""
                let behind = (obj as? [String: Any])?["behind_by"] as? Int ?? 0
                if code == 200, st == "ahead" || st == "identical" {
                    dlog("verdict=desktop_update_none commit=\(mine.prefix(9)) latest=\(sha.prefix(9)) compare=\(st)")
                    self.set(.upToDate, "This build is newer than the latest release (\(sha.prefix(9))).")
                } else {
                    dlog("verdict=desktop_update_available commit=\(mine.prefix(9)) latest=\(sha.prefix(9)) compare=\(code == 200 ? st : "http \(code)")")
                    self.set(.available, "A newer build is out (\(sha.prefix(9))"
                        + (code == 200 && st == "behind" && behind > 0 ? ", \(behind) commit\(behind == 1 ? "" : "s") ahead of this one)." : ")."))
                }
            }
        }
    }

    /// Download, check, swap the bundle and relaunch.
    func install() {
        if state == .installing { return }
        let app = Bundle.main.bundleURL
        set(.installing, "Downloading…")
        guard let src = URL(string: zip) else { fail("bad download address"); return }
        URLSession.shared.downloadTask(with: src) { [weak self] tmp, resp, err in
            guard let self = self else { return }
            let code = (resp as? HTTPURLResponse)?.statusCode ?? (src.isFileURL ? 200 : 0)
            guard let tmp = tmp, err == nil, code == 200 else {
                DispatchQueue.main.async { self.fail("download failed (HTTP \(code)\(err.map { ", " + $0.localizedDescription } ?? ""))") }
                return
            }
            do {
                // Unpack beside the app so the swap is a rename on one volume.
                let fm = FileManager.default
                let work = try fm.url(for: .itemReplacementDirectory, in: .userDomainMask, appropriateFor: app, create: true)
                let z = work.appendingPathComponent("Amux-macOS.zip")
                try fm.moveItem(at: tmp, to: z)
                DispatchQueue.main.async { self.set(.installing, "Installing…") }
                guard run("/usr/bin/ditto", ["-x", "-k", z.path, work.path]) == 0 else { throw UpdateError("could not unzip the download") }
                let new = work.appendingPathComponent("Amux.app")
                let info = NSDictionary(contentsOf: new.appendingPathComponent("Contents/Info.plist"))
                guard info?["CFBundleIdentifier"] as? String == Bundle.main.bundleIdentifier else {
                    throw UpdateError("the download is not this app")
                }
                guard run("/usr/bin/codesign", ["--verify", "--deep", new.path]) == 0 else { throw UpdateError("the download's signature does not verify") }
                let got = info?["AmuxBuildCommit"] as? String ?? "unstamped"
                _ = try fm.replaceItemAt(app, withItemAt: new)
                try? fm.removeItem(at: work)
                DispatchQueue.main.async {
                    dlog("verdict=desktop_update_installed from=\(self.commit.isEmpty ? "unstamped" : String(self.commit.prefix(9))) to=\(got.prefix(9)) path=\(app.path)")
                    self.relaunch(app)
                }
            } catch {
                DispatchQueue.main.async { self.fail("\(error)") }
            }
        }.resume()
    }

    private func fail(_ why: String) {
        dlog("verdict=desktop_update_failed why=\(why)")
        set(.failed, "Update failed: \(why). This app was left as it was.")
    }

    /// Start the new build once this process has exited, then quit (which saves
    /// the windows). Through `open` so it launches as an app; AMUX_DESKTOP_*
    /// settings and a file log carry over.
    private func relaunch(_ app: URL) {
        var args = ["-n", app.path]
        for (k, v) in ProcessInfo.processInfo.environment where k.hasPrefix("AMUX_DESKTOP_") { args += ["--env", "\(k)=\(v)"] }
        var buf = [CChar](repeating: 0, count: Int(MAXPATHLEN))
        var st = stat()
        if fstat(2, &st) == 0, (st.st_mode & S_IFMT) == S_IFREG, fcntl(2, F_GETPATH, &buf) == 0 {
            let log = String(cString: buf)
            args += ["--stdout", log, "--stderr", log]
        }
        let quoted = args.map { "'" + $0.replacingOccurrences(of: "'", with: "'\\''") + "'" }.joined(separator: " ")
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        p.arguments = ["-c", "while kill -0 \(getpid()) 2>/dev/null; do sleep 0.2; done; echo \"amux-desktop: verdict=desktop_update_relaunch\" >&2; /usr/bin/open \(quoted) || echo \"amux-desktop: verdict=desktop_update_relaunch_failed rc=$?\" >&2"]
        do { try p.run() } catch { fail("could not relaunch: \(error)"); return }
        NSApp.terminate(nil)
    }
}

struct UpdateError: Error, CustomStringConvertible {
    let description: String
    init(_ d: String) { description = d }
}

@discardableResult
private func run(_ tool: String, _ args: [String]) -> Int32 {
    let p = Process()
    p.executableURL = URL(fileURLWithPath: tool)
    p.arguments = args
    p.standardOutput = FileHandle.nullDevice
    p.standardError = FileHandle.nullDevice
    do { try p.run() } catch { return -1 }
    p.waitUntilExit()
    return p.terminationStatus
}
