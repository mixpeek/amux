import Foundation

/// The two calls a share needs, against an unmodified amux server.
///
/// Verified by hand against a live server on 2026-09-22 before any of this was
/// written, which is where the two non-obvious rules below come from.
enum AmuxClient {

    /// amux serves self-signed TLS on the local network and over Tailscale, the
    /// same reason `ServerManager` carries its own trust delegate.
    private final class TrustAll: NSObject, URLSessionDelegate {
        func urlSession(_ session: URLSession,
                        didReceive challenge: URLAuthenticationChallenge,
                        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
            if let trust = challenge.protectionSpace.serverTrust {
                completionHandler(.useCredential, URLCredential(trust: trust))
            } else {
                completionHandler(.performDefaultHandling, nil)
            }
        }
    }

    private static let session = URLSession(configuration: .ephemeral,
                                            delegate: TrustAll(),
                                            delegateQueue: nil)

    struct Worker: Identifiable, Hashable, Codable {
        let name: String
        let status: String
        /// `active` | `paused` | `archived`. Distinct from `status`: a worker
        /// can be lifecycle-active and momentarily idle.
        let lifecycle: String
        let running: Bool
        /// Unix seconds. 0 when the server has never seen activity, which is
        /// kept as 0 rather than mapped to "now" so activity sort puts it last.
        let lastActivity: Int
        let dir: String
        let task: String
        /// The worker's groups (`tags` on /api/sessions: amux, ops, gtm...).
        var groups: [String] = []
        var id: String { name }

        /// One word for the row. `status` is EMPTY on most rows (measured: 156
        /// of 164 sessions), so a row that showed only `status` would be blank
        /// for almost everyone — which is what the first version did.
        var display: String {
            if running {
                switch status {
                case "active": return "working"
                case "idle": return "idle"
                default: return "running"
                }
            }
            return lifecycle.isEmpty ? "stopped" : lifecycle
        }

        /// The last path component of the workspace, which is what
        /// distinguishes two workers with similar names.
        var workspace: String {
            dir.split(separator: "/").last.map(String.init) ?? ""
        }
    }

    enum ClientError: LocalizedError {
        case noServer
        case http(Int, String)
        case malformed(String)

        var errorDescription: String? {
            switch self {
            case .noServer:
                return "No amux server selected. Open the amux app and pick one first."
            case let .http(code, body):
                return "amux returned \(code): \(body)"
            case let .malformed(what):
                return "Unexpected response: \(what)"
            }
        }
    }

    private static func check(_ data: Data, _ response: URLResponse?) throws {
        guard let http = response as? HTTPURLResponse else {
            throw ClientError.malformed("no HTTP response")
        }
        guard (200..<300).contains(http.statusCode) else {
            let body = String(data: data, encoding: .utf8) ?? ""
            throw ClientError.http(http.statusCode, String(body.prefix(400)))
        }
    }

    // MARK: - Owner credential

    /// AMUX-4976. Every call here used to be ANONYMOUS, and the server only
    /// tolerates that from loopback: `api/auth.rs` returns early when the peer
    /// IP `is_loopback`, and otherwise demands the owner cookie AND a bearer.
    /// So sharing worked on a Mac talking to itself and 401'd from a phone,
    /// which is the only way anyone actually uses it. Measured over Tailscale
    /// before this existed: `api/sessions` 401, `api/upload/start` 401, both
    /// 200 over loopback.
    ///
    /// The fix deliberately stores NOTHING. The bearer is obtained the way the
    /// dashboard obtains it, per process, and lives only in memory:
    ///
    ///   1. GET / establishes the owner session. The server sets
    ///      `__Host-amux_owner` and redirects, and `.ephemeral` keeps that
    ///      cookie in memory for this session object only.
    ///   2. GET / again, now carrying the cookie, returns the SPA with an
    ///      injected `window._AMUX_AUTH_TOKEN`.
    ///
    /// Writing the token into the App Group instead would have put a long
    /// lived owner credential on disk for a second process to read. That is a
    /// security posture change and is not needed: this grants the extension
    /// exactly the access the browser on the same device already has, on the
    /// same basis, and it expires with the process.
    private static var cachedToken: String?

    /// Read a BARE (unquoted) bootstrap value, e.g. `_AMUX_AUTH_WITHHELD=false`.
    ///
    /// Separate from `bootstrapValue` because the server emits these two
    /// differently and they are one character apart to the eye: strings go
    /// through `jstr(...)` and are quoted, `auth_withheld` is a Rust bool and
    /// is not. The first version of this file read the flag with the STRING
    /// reader, which looks for `="`, so it never matched and the withheld
    /// branch below was unreachable. Caught by testing the parser against the
    /// real served page rather than against an assumed shape.
    static func bootstrapFlag(_ html: String, key: String) -> String? {
        guard let marker = html.range(of: "window.\(key)=") else { return nil }
        let rest = html[marker.upperBound...]
        guard let end = rest.firstIndex(where: { $0 == ";" || $0 == "<" }) else { return nil }
        return String(rest[..<end])
    }

    /// Extract a `window._AMUX_*` JSON string value from the injected
    /// bootstrap. Scans for the closing quote rather than splitting on `"`,
    /// because the server writes these with serde_json and an escaped quote
    /// inside the value would truncate a naive split.
    static func bootstrapValue(_ html: String, key: String) -> String? {
        guard let marker = html.range(of: "window.\(key)=\"") else { return nil }
        var out = String()
        var i = marker.upperBound
        while i < html.endIndex {
            let c = html[i]
            if c == "\\" {
                let n = html.index(after: i)
                guard n < html.endIndex else { return nil }
                out.append(html[n])
                i = html.index(after: n)
                continue
            }
            if c == "\"" { return out }
            out.append(c)
            i = html.index(after: i)
        }
        return nil
    }

    private static func fetchToken(server: URL) async throws -> String {
        var html = ""
        // Twice, not once: the first GET is answered with a redirect to
        // `/api/_clear_sw`, so the bootstrap only appears on a request that
        // already carries the cookie the first one set.
        for _ in 0..<2 {
            let (data, response) = try await session.data(
                for: URLRequest(url: server, timeoutInterval: 20))
            try check(data, response)
            html = String(data: data, encoding: .utf8) ?? ""
            if let t = bootstrapValue(html, key: "_AMUX_AUTH_TOKEN"), !t.isEmpty {
                return t
            }
        }
        // AF-639's distinction, kept rather than collapsed: an empty token has
        // two causes and only one is worth a human's attention.
        if bootstrapFlag(html, key: "_AMUX_AUTH_WITHHELD") == "true" {
            throw ClientError.malformed(
                "this device is not signed in as the owner, so amux withheld the "
                + "credential. Open the amux app and connect to this server first.")
        }
        if html.contains("_AMUX_AUTH_TOKEN") {
            // Present and empty: the server has no token configured, so nothing
            // will 401 and an anonymous request is the correct one to send.
            return ""
        }
        throw ClientError.malformed(
            "no amux bootstrap at \(server.absoluteString) — is this an amux server?")
    }

    /// Authorize a request, bootstrapping once per process.
    private static func authorized(_ req: inout URLRequest, server: URL) async throws {
        if cachedToken == nil { cachedToken = try await fetchToken(server: server) }
        if let t = cachedToken, !t.isEmpty {
            req.setValue("Bearer \(t)", forHTTPHeaderField: "Authorization")
        }
    }

    /// Send, and re-bootstrap ONCE on a 401. The owner token rotates and the
    /// cookie expires, so a cached bearer can be stale through no fault of the
    /// caller; retrying blind would loop, so this retries exactly once.
    private static func authedData(_ build: () -> URLRequest,
                                   server: URL) async throws -> Data {
        var req = build()
        try await authorized(&req, server: server)
        let (data, response) = try await session.data(for: req)
        if let http = response as? HTTPURLResponse, http.statusCode == 401 {
            cachedToken = nil
            var retry = build()
            try await authorized(&retry, server: server)
            let (d2, r2) = try await session.data(for: retry)
            try check(d2, r2)
            return d2
        }
        try check(data, response)
        return data
    }

    /// POST a JSON body to an amux API path as the owner, returning the decoded
    /// object. The app's location history upload uses this (AMUX-5458): the same
    /// per-process bootstrap and one retry on a 401 as every other call here.
    static func postJSON(path: String, body: [String: Any], server: URL) async throws -> [String: Any] {
        let payload = try JSONSerialization.data(withJSONObject: body)
        let data = try await authedData({
            var req = URLRequest(url: server.appendingPathComponent(path), timeoutInterval: 30)
            req.httpMethod = "POST"
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = payload
            return req
        }, server: server)
        return (try JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
    }

    /// Retired worker names and the worker each became (GET /api/session-renames).
    /// An older server has no such route; that reads as no renames.
    static func renames(server: URL) async -> [String: String] {
        guard let data = try? await authedData({
            var req = URLRequest(url: server.appendingPathComponent("api/session-renames"), timeoutInterval: 10)
            req.httpMethod = "GET"
            return req
        }, server: server),
        let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
        let map = obj["renames"] as? [String: String] else { return [:] }
        return map
    }

    /// Workers a human would plausibly share to, newest activity first.
    static func workers(server: URL) async throws -> [Worker] {
        let data = try await authedData({
            // The light picker view (AMUX-5386): env, meta and one tmux call
            // instead of the full ~3.8s list. An older server ignores the
            // parameter and answers with the full list, which decodes the same.
            var comps = URLComponents(url: server.appendingPathComponent("api/sessions"),
                                      resolvingAgainstBaseURL: false)!
            comps.queryItems = [URLQueryItem(name: "view", value: "picker")]
            var req = URLRequest(url: comps.url!, timeoutInterval: 15)
            req.httpMethod = "GET"
            return req
        }, server: server)
        guard let rows = try JSONSerialization.jsonObject(with: data) as? [[String: Any]] else {
            throw ClientError.malformed("session list was not an array")
        }
        return rows.compactMap { row in
            guard let name = row["name"] as? String, !name.isEmpty else { return nil }
            if (row["archived"] as? Bool) == true { return nil }
            return Worker(
                name: name,
                status: row["status"] as? String ?? "",
                lifecycle: row["lifecycle"] as? String ?? "",
                running: (row["running"] as? Bool) ?? false,
                lastActivity: (row["last_activity"] as? Int) ?? 0,
                dir: row["dir"] as? String ?? "",
                task: row["task_name"] as? String ?? "",
                groups: (row["tags"] as? [String]) ?? [])
        }
    }

    /// 1MB, the size the PWA's queued upload uses. The server caps a chunk PUT
    /// at 8MB, and this client used to send no `chunks`, so the server
    /// answered one and the whole file went in one request: anything over 8MB
    /// failed outright. Small chunks matter on their own too: the phone's link
    /// to the host can run at ~40KB/s with stalls (measured 2026-10-03, a 4.5MB
    /// Timeline.json), and one dropped connection then costs one chunk and a
    /// retry instead of the whole share.
    static let uploadChunkBytes = 1024 * 1024

    /// Wait before each retry of a chunk; its length is the retry budget.
    private static let chunkBackoff: [UInt64] = [1, 2, 4, 8, 15]

    /// Failures worth retrying: the link dropped or stalled. Anything else
    /// (a 4xx, a refused credential) will fail the same way again.
    private static func transient(_ error: Error) -> Bool {
        if let u = error as? URLError {
            return [.networkConnectionLost, .timedOut, .notConnectedToInternet,
                    .cannotConnectToHost, .cannotFindHost, .dnsLookupFailed,
                    .secureConnectionFailed, .dataNotAllowed, .internationalRoamingOff]
                .contains(u.code)
        }
        if case let ClientError.http(code, _) = error {
            return [408, 425, 429, 500, 502, 503, 504].contains(code)
        }
        return false
    }

    /// A dropped connection never reaches the server's request log (hyper
    /// drops the request with the socket), so the extension reports its own
    /// chunk retries and failures. They land in server-rs.log as
    /// `client-debug beacon kind=share-upload-...`. Best effort: a beacon that
    /// fails must never fail the share.
    private static func beacon(_ kind: String, server: URL, _ fields: [String: Any]) async {
        var body = fields
        body["kind"] = kind
        guard let data = try? JSONSerialization.data(withJSONObject: body) else { return }
        var req = URLRequest(url: server.appendingPathComponent("api/client-debug"), timeoutInterval: 10)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = data
        if let t = cachedToken, !t.isEmpty {
            req.setValue("Bearer \(t)", forHTTPHeaderField: "Authorization")
        }
        _ = try? await session.data(for: req)
    }

    /// Upload one file and return the ABSOLUTE path on the amux host.
    ///
    /// Uses the chunked API rather than the single-shot multipart one on
    /// purpose. `/api/fs/upload` answers with a bare filename and needs the
    /// caller to already know the host's uploads directory; `/api/upload`
    /// answers with `path`, so nothing here has to hardcode a directory that
    /// differs per machine. It also streams, so a shared video is not buffered
    /// whole into one request body.
    /// Streams the file: a share extension is killed by iOS at roughly 120 MB
    /// of memory, and the old code held the whole file plus a copy of each
    /// chunk, so a video or large photo died after upload/start with nothing
    /// on screen (2026-10-02 phone share: start logged, no chunk, no send).
    /// `progress` gets the fraction uploaded after each chunk.
    static func upload(fileURL: URL, server: URL,
                       progress: ((Double) -> Void)? = nil) async throws -> String {
        let name = fileURL.lastPathComponent
        let attrs = try FileManager.default.attributesOfItem(atPath: fileURL.path)
        let size = (attrs[.size] as? NSNumber)?.intValue ?? 0

        let startBody = try JSONSerialization.data(
            withJSONObject: ["name": name, "size": size,
                             "chunks": max(1, (size + uploadChunkBytes - 1) / uploadChunkBytes)])
        let startData = try await authedData({
            var start = URLRequest(url: server.appendingPathComponent("api/upload/start"),
                                   timeoutInterval: 30)
            start.httpMethod = "POST"
            start.setValue("application/json", forHTTPHeaderField: "Content-Type")
            start.httpBody = startBody
            return start
        }, server: server)
        guard let started = try JSONSerialization.jsonObject(with: startData) as? [String: Any],
              let id = started["id"] as? String,
              let chunks = started["chunks"] as? Int, chunks > 0 else {
            throw ClientError.malformed("upload/start gave no id and chunk count")
        }

        let handle = try FileHandle(forReadingFrom: fileURL)
        defer { try? handle.close() }
        let chunkSize = max(1, Int(ceil(Double(max(size, 1)) / Double(chunks))))
        for n in 0..<chunks {
            let slice: Data = try autoreleasepool {
                try handle.seek(toOffset: UInt64(n * chunkSize))
                return try handle.read(upToCount: chunkSize) ?? Data()
            }
            if slice.isEmpty && n > 0 { break }
            var attempt = 0
            while true {
                let began = Date()
                do {
                    _ = try await authedData({
                        var put = URLRequest(
                            url: server.appendingPathComponent("api/upload/\(id)/chunk/\(n)"),
                            timeoutInterval: 120)
                        put.httpMethod = "PUT"
                        put.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
                        put.httpBody = slice
                        return put
                    }, server: server)
                    break
                } catch {
                    let fields: [String: Any] = [
                        "upload": id, "chunk": n, "chunks": chunks, "bytes": slice.count,
                        "size": size, "attempt": attempt + 1,
                        "elapsed_s": Date().timeIntervalSince(began),
                        "error": error.localizedDescription,
                        "code": (error as? URLError)?.code.rawValue ?? 0,
                    ]
                    guard transient(error), attempt < chunkBackoff.count else {
                        await beacon("share-upload-failed", server: server, fields)
                        throw error
                    }
                    await beacon("share-upload-retry", server: server, fields)
                    try await Task.sleep(nanoseconds: chunkBackoff[attempt] * 1_000_000_000)
                    attempt += 1
                }
            }
            progress?(Double(n + 1) / Double(chunks))
        }

        let fData = try await authedData({
            var finish = URLRequest(url: server.appendingPathComponent("api/upload/\(id)/finish"),
                                    timeoutInterval: 60)
            finish.httpMethod = "POST"
            return finish
        }, server: server)
        guard let done = try JSONSerialization.jsonObject(with: fData) as? [String: Any],
              let path = done["path"] as? String, !path.isEmpty else {
            throw ClientError.malformed("upload/finish gave no path")
        }
        return path
    }

    /// Deliver the shared text and attachments to a worker.
    ///
    /// DELIBERATELY SENDS NO `X-Amux-Session` HEADER. With one, the server
    /// reads this as a message from a PEER WORKER, and a paused lane refuses it
    /// outright: `409 lifecycle_not_active`, "Active workers interact only with
    /// active workers". Measured on a live server: identical request, 409 with
    /// the header and 200 without. A share from a phone is the OWNER acting,
    /// and most workers a human picks from a list are idle or paused.
    ///
    /// A STOPPED WORKER IS STARTED BY THE SEND, which can take 40s or more, so
    /// one 30s request used to time out while the server went on to deliver it:
    /// the sheet then reported a failure for a message that arrived (found on
    /// the simulator, 2026-10-01). Every attempt now carries the same `msg_id`,
    /// which the server dedupes: a retry after a timeout or a 503 "pending"
    /// answers with the real outcome of the one send, never a second copy.
    /// `waiting` is called once when the first attempt runs long.
    static func send(text: String, to worker: String, server: URL,
                     deadline: TimeInterval = 180,
                     msgID givenID: String? = nil,
                     waiting: (() -> Void)? = nil) async throws {
        let encoded = worker.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? worker
        guard let url = URL(string: "api/sessions/\(encoded)/send", relativeTo: server) else {
            throw ClientError.malformed("could not build the send URL for \(worker)")
        }
        // The outbox passes a stable id per (share, worker), so a send replayed
        // from the queue after a lost response is deduped by the server.
        let msgID = givenID ?? ("share-" + UUID().uuidString.lowercased())
        // record_history: the owner's message, recorded in the worker's Messages
        // tab exactly like the dashboard composer's. Without it a share reached
        // the worker but never appeared in Messages (found on the simulator,
        // 2026-10-01: zero history rows for two delivered shares).
        let body = try JSONSerialization.data(withJSONObject: ["text": text, "msg_id": msgID, "record_history": true])
        let until = Date().addingTimeInterval(deadline)
        var told = false
        while true {
            do {
                _ = try await authedData({
                    var req = URLRequest(url: url, timeoutInterval: 30)
                    req.httpMethod = "POST"
                    req.setValue("application/json", forHTTPHeaderField: "Content-Type")
                    req.httpBody = body
                    return req
                }, server: server)
                return
            } catch let error as URLError where error.code == .timedOut && Date() < until {
                // The server keeps going without us; ask again with the same id.
            } catch ClientError.http(503, _) where Date() < until {
                // Same id still in flight on the server.
            }
            if !told { told = true; waiting?() }
            try await Task.sleep(nanoseconds: 3_000_000_000)
        }
    }
}
