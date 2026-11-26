import Cocoa
import WebKit

// Saved page data (Settings > This Mac > Storage).
//
// WebKit keeps each site's localStorage in SQLite with a write-ahead log. On
// 2026-11-26 the owner's app held 23.2 GB of log for the server's address
// beside a 10 MB database: the log was never folded back in, every read waded
// through it, and the page's outbox reported its storage lock stalled about 180
// times an hour ("my amux server isnt working"; owner: "we need to have ways
// to clean this in the app"). Compacting copies the server's localStorage out
// of a page, has WebKit delete that site's storage files, and writes the copy
// back, so queued messages, drafts and preferences stay and the log is gone.
// A key a page wrote in between is kept, never overwritten by the copy.
//
// It runs from the Storage row, by itself when the app starts with more than
// webStorageAutoBytes saved, and hourly while the app is in the background.
// Every run logs verdict=web_storage_compacted (or _compact_failed) with the
// bytes before and after. The copy is saved to disk before anything is deleted
// and removed only once a page has the items back (verdict=web_storage_copy_*).


struct WebStorageUse {
    var bytes: Int64 = 0      // every localStorage file of this app
    var log: Int64 = 0        // of which write-ahead logs
    var measured = false
}

/// This app's WebKit data folder (a test or dev copy has its own bundle id).
func webStorageRoot() -> URL {
    let id = Bundle.main.bundleIdentifier ?? "com.amux.desktop"
    return FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/WebKit/\(id)/WebsiteData", isDirectory: true)
}

func webStorageUse() -> WebStorageUse {
    var u = WebStorageUse()
    let root = webStorageRoot()
    guard let e = FileManager.default.enumerator(at: root, includingPropertiesForKeys: [.fileSizeKey, .isRegularFileKey]) else { return u }
    u.measured = true
    for case let f as URL in e where f.deletingLastPathComponent().lastPathComponent == "LocalStorage" {
        let size = Int64((try? f.resourceValues(forKeys: [.fileSizeKey]))?.fileSize ?? 0)
        u.bytes += size
        if f.lastPathComponent.hasSuffix("-wal") { u.log += size }
    }
    return u
}

/// The copy taken before a compaction deletes anything. It stays on disk until
/// a page has the items back, so a write-back that fails (the page navigated
/// away, the app quit) loses nothing: the next server page that loads gets it.
func webStorageCopyFile() -> URL {
    let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent(Bundle.main.bundleIdentifier ?? "com.amux.desktop", isDirectory: true)
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    return dir.appendingPathComponent("localstorage-copy.json")
}

func pendingWebStorageCopy() -> [String: String]? {
    guard let d = try? Data(contentsOf: webStorageCopyFile()) else { return nil }
    return try? JSONSerialization.jsonObject(with: d) as? [String: String]
}

func storageSize(_ b: Int64) -> String {
    ByteCountFormatter.string(fromByteCount: b, countStyle: .file)
}

extension AppDelegate {
    /// Compact by itself past this much (Settings > This Mac, or the env).
    var webStorageAutoBytes: Int64 { Int64(appLimit("AMUX_DESKTOP_STORAGE_AUTO_MB", 1024) * 1_048_576) }

    /// What the Storage row draws.
    func webStorageState() -> [String: Any] {
        let u = webStorageUse()
        return ["bytes": u.bytes, "log": u.log, "measured": u.measured, "size": storageSize(u.bytes),
                "large": u.bytes >= webStorageAutoBytes, "busy": storageCompacting, "last": storageLastResult]
    }

    /// Copy out, delete, copy back. `why` is "settings", "launch" or "background".
    func compactWebStorage(why: String, done: (() -> Void)? = nil) {
        guard !storageCompacting else { dlog("verdict=web_storage_compact_skipped reason=already_running why=\(why)"); return }
        guard let server = serverURL, let page = allPages.first(where: { $0.webView.url.map { isServerURL($0) } ?? false }) else {
            dlog("verdict=web_storage_compact_skipped reason=no_server_page why=\(why)")
            return
        }
        storageCompacting = true
        settingsChanged()
        let before = webStorageUse()
        let started = Date()
        let finish: (String, Int) -> Void = { [weak self] outcome, keys in
            guard let self = self else { return }
            let after = webStorageUse()
            let ms = Int(Date().timeIntervalSince(started) * 1000)
            self.storageCompacting = false
            if outcome == "ok" {
                self.storageLastResult = "Compacted from \(storageSize(before.bytes)) to \(storageSize(after.bytes)); \(keys) saved items kept."
                dlog("verdict=web_storage_compacted why=\(why) before_bytes=\(before.bytes) before_log=\(before.log) after_bytes=\(after.bytes) keys=\(keys) ms=\(ms)")
            } else {
                self.storageLastResult = "Could not compact: \(outcome)."
                dlog("verdict=web_storage_compact_failed why=\(why) reason=\(outcome) before_bytes=\(before.bytes) ms=\(ms)")
            }
            self.settingsChanged()
            done?()
        }
        // 1. Copy the server's localStorage out of a page that shows it.
        let read = "return JSON.stringify(Object.fromEntries(Object.keys(localStorage).map(k => [k, localStorage.getItem(k)])))"
        page.webView.callAsyncJavaScript(read, arguments: [:], in: nil, in: .page) { [weak self] r in
            guard let self = self else { return }
            guard case .success(let v) = r, let json = v as? String,
                  let copy = try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: String] else {
                finish("the page did not hand over its saved data", 0)
                return
            }
            // Kept on disk before anything is deleted.
            guard let data = try? JSONSerialization.data(withJSONObject: copy), (try? data.write(to: webStorageCopyFile(), options: .atomic)) != nil else {
                finish("the copy could not be saved first", 0)
                return
            }
            // 2. Delete only that site's localStorage.
            let store = page.webView.configuration.websiteDataStore
            let types: Set<String> = [WKWebsiteDataTypeLocalStorage]
            store.fetchDataRecords(ofTypes: types) { records in
                let host = server.host?.lowercased() ?? ""
                let mine = records.filter { $0.displayName.lowercased() == host }
                guard !mine.isEmpty else { finish("WebKit lists no saved data for \(host)", 0); return }
                store.removeData(ofTypes: types, for: mine) {
                    // 3. Write the copy back, through any page on the server.
                    self.restoreWebStorageCopy(preferring: page) { n in
                        guard let n = n else {
                            finish("the saved items could not be written back yet; a copy is kept at \(webStorageCopyFile().path) and is written back within the hour or when amux next starts", 0)
                            return
                        }
                        // Every page on the server reloads to read the new store.
                        for p in self.allPages where p.webView.url.map({ self.isServerURL($0) }) ?? false { p.webView.reload() }
                        finish("ok", n)
                    }
                }
            }
        }
    }

    /// Puts the kept copy back (a key some page set since wins) and deletes the
    /// file once a page has it. `done` gets the item count, or nil when no
    /// page on the server took it.
    func restoreWebStorageCopy(preferring first: DashboardWindowController?, done: @escaping (Int?) -> Void) {
        guard let copy = pendingWebStorageCopy() else { done(nil); return }
        var pages = allPages.filter { $0.webView.url.map { isServerURL($0) } ?? false }
        if let f = first, let i = pages.firstIndex(where: { $0 === f }) { pages.insert(pages.remove(at: i), at: 0) }
        let write = "for (const [k, v] of Object.entries(copy)) { if (localStorage.getItem(k) === null) localStorage.setItem(k, v); } return Object.keys(copy).length"
        func attempt(_ rest: ArraySlice<DashboardWindowController>) {
            guard let p = rest.first else { done(nil); return }
            p.webView.callAsyncJavaScript(write, arguments: ["copy": copy], in: nil, in: .page) { r in
                if case .success(let n) = r {
                    try? FileManager.default.removeItem(at: webStorageCopyFile())
                    done((n as? NSNumber)?.intValue ?? copy.count)
                } else {
                    attempt(rest.dropFirst())
                }
            }
        }
        attempt(pages[...])
    }

    /// A kept copy goes back, and the server's pages reload so they read it
    /// (one path for launch and the hourly check, review F2).
    func restorePendingCopy(why: String) {
        restoreWebStorageCopy(preferring: nil) { [weak self] n in
            guard let self = self else { return }
            guard let n = n else {
                dlog("verdict=web_storage_copy_restore_pending why=\(why) path=\(webStorageCopyFile().path)")
                return
            }
            dlog("verdict=web_storage_copy_restored keys=\(n) why=\(why)")
            for p in self.allPages where p.webView.url.map({ self.isServerURL($0) }) ?? false { p.webView.reload() }
        }
    }

    /// At launch, and hourly while amux is in the background: compact when the
    /// saved data has passed webStorageAutoBytes. Never while you are using it,
    /// since every page reloads.
    func startWebStorageWatch() {
        let u = webStorageUse()
        dlog("verdict=web_storage_size bytes=\(u.bytes) log=\(u.log) measured=\(u.measured ? 1 : 0) auto_bytes=\(webStorageAutoBytes)")
        // A copy left by an interrupted compaction goes back before anything else.
        if pendingWebStorageCopy() != nil {
            DispatchQueue.main.asyncAfter(deadline: .now() + envSeconds("AMUX_DESKTOP_STORAGE_FIRST_S", 20) / 2) { [weak self] in
                self?.restorePendingCopy(why: "launch")
            }
        }
        let check: (String) -> Void = { [weak self] why in
            guard let self = self else { return }
            let now = webStorageUse()
            if pendingWebStorageCopy() != nil {
                self.restorePendingCopy(why: why)
                return
            }
            guard now.bytes >= webStorageAutoBytes else { return }
            if why == "background" && NSApp.isActive { return }
            self.compactWebStorage(why: why)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + envSeconds("AMUX_DESKTOP_STORAGE_FIRST_S", 20)) { check("launch") }
        storageTimer = Timer.scheduledTimer(withTimeInterval: envSeconds("AMUX_DESKTOP_STORAGE_TICK_S", 3600), repeats: true) { _ in check("background") }
    }
}
