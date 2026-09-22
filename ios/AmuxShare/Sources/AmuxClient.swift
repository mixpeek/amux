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

    struct Worker: Identifiable, Hashable {
        let name: String
        let status: String
        var id: String { name }
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

    /// Workers a human would plausibly share to, newest activity first.
    static func workers(server: URL) async throws -> [Worker] {
        var req = URLRequest(url: server.appendingPathComponent("api/sessions"),
                             timeoutInterval: 15)
        req.httpMethod = "GET"
        let (data, response) = try await session.data(for: req)
        try check(data, response)
        guard let rows = try JSONSerialization.jsonObject(with: data) as? [[String: Any]] else {
            throw ClientError.malformed("session list was not an array")
        }
        return rows.compactMap { row in
            guard let name = row["name"] as? String, !name.isEmpty else { return nil }
            if (row["archived"] as? Bool) == true { return nil }
            return Worker(name: name, status: row["status"] as? String ?? "")
        }
    }

    /// Upload one file and return the ABSOLUTE path on the amux host.
    ///
    /// Uses the chunked API rather than the single-shot multipart one on
    /// purpose. `/api/fs/upload` answers with a bare filename and needs the
    /// caller to already know the host's uploads directory; `/api/upload`
    /// answers with `path`, so nothing here has to hardcode a directory that
    /// differs per machine. It also streams, so a shared video is not buffered
    /// whole into one request body.
    static func upload(fileURL: URL, server: URL) async throws -> String {
        let data = try Data(contentsOf: fileURL)
        let name = fileURL.lastPathComponent

        var start = URLRequest(url: server.appendingPathComponent("api/upload/start"),
                               timeoutInterval: 30)
        start.httpMethod = "POST"
        start.setValue("application/json", forHTTPHeaderField: "Content-Type")
        start.httpBody = try JSONSerialization.data(
            withJSONObject: ["filename": name, "size": data.count])
        let (startData, startResp) = try await session.data(for: start)
        try check(startData, startResp)
        guard let started = try JSONSerialization.jsonObject(with: startData) as? [String: Any],
              let id = started["id"] as? String,
              let chunks = started["chunks"] as? Int, chunks > 0 else {
            throw ClientError.malformed("upload/start gave no id and chunk count")
        }

        let chunkSize = Int(ceil(Double(data.count) / Double(chunks)))
        for n in 0..<chunks {
            let lower = n * chunkSize
            let upper = min(lower + chunkSize, data.count)
            guard lower < upper else { break }
            var put = URLRequest(url: server.appendingPathComponent("api/upload/\(id)/chunk/\(n)"),
                                 timeoutInterval: 120)
            put.httpMethod = "PUT"
            put.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
            put.httpBody = data.subdata(in: lower..<upper)
            let (cData, cResp) = try await session.data(for: put)
            try check(cData, cResp)
        }

        var finish = URLRequest(url: server.appendingPathComponent("api/upload/\(id)/finish"),
                                timeoutInterval: 60)
        finish.httpMethod = "POST"
        let (fData, fResp) = try await session.data(for: finish)
        try check(fData, fResp)
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
    static func send(text: String, to worker: String, server: URL) async throws {
        let encoded = worker.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? worker
        guard let url = URL(string: "api/sessions/\(encoded)/send", relativeTo: server) else {
            throw ClientError.malformed("could not build the send URL for \(worker)")
        }
        var req = URLRequest(url: url, timeoutInterval: 30)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONSerialization.data(withJSONObject: ["text": text])
        let (data, response) = try await session.data(for: req)
        try check(data, response)
    }
}
