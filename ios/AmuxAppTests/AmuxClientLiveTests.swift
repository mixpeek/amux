import XCTest

/// AMUX-4475. The AMUX-4976 fix is an AUTHENTICATION fix, and nothing in the
/// bootstrap-parser tests can fail if it regresses: those feed a synthetic page
/// to a pure function. What broke was the network path — every call the Share
/// Extension makes was anonymous, which a server tolerates from loopback
/// (`api/auth.rs` returns early when the peer IP `is_loopback`) and refuses
/// from anywhere else. So it worked on a Mac talking to itself and 401'd from a
/// phone, which is the only way anyone uses a share sheet.
///
/// These drive the SHIPPING client against a real server at a real
/// NON-LOOPBACK address, which is the only place the bug was ever visible.
///
/// OPT-IN, because a hermetic CI runner has no such server:
///
///     AMUX_LIVE_SERVER=https://<host-or-tailscale-ip>:<port> \
///       xcodebuild test -project AmuxApp.xcodeproj -scheme AmuxAppTests ...
///
/// Unset, every test here reports SKIPPED rather than passing — a silent pass
/// on an absent server is the shape this file exists to stop.
final class AmuxClientLiveTests: XCTestCase {

    /// Both spellings are accepted because xcodebuild does not forward its own
    /// environment to the test process uniformly: `TEST_RUNNER_`-prefixed
    /// variables are delivered with the prefix stripped, while a bare variable
    /// arrives only in some hosting configurations. Reading one of them turns
    /// a passing setup into a silent skip depending on how it was invoked.
    private static let envKeys = ["AMUX_LIVE_SERVER", "TEST_RUNNER_AMUX_LIVE_SERVER"]

    private func liveServer() throws -> URL {
        let env = ProcessInfo.processInfo.environment
        guard let raw = Self.envKeys.lazy.compactMap({ env[$0] }).first(where: { !$0.isEmpty }),
              let url = URL(string: raw) else {
            throw XCTSkip("set \(Self.envKeys[0]) to a non-loopback amux URL to run this")
        }
        return url
    }

    /// Accepts the self-signed cert the same way the client does, so a TLS
    /// refusal here is never mistaken for the 401 under test.
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

    private func status(_ url: URL, method: String = "GET", body: Data? = nil) async throws -> Int {
        let session = URLSession(configuration: .ephemeral, delegate: TrustAll(), delegateQueue: nil)
        var req = URLRequest(url: url, timeoutInterval: 20)
        req.httpMethod = method
        if let body {
            req.httpBody = body
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        let (_, response) = try await session.data(for: req)
        return (response as? HTTPURLResponse)?.statusCode ?? -1
    }

    /// THE PREMISE, ASSERTED RATHER THAN ASSUMED. Every test below claims the
    /// client obtained a credential it needed. That claim is empty if the
    /// server would have answered anonymously anyway, which is exactly what
    /// happens against `localhost` — so pointing `AMUX_LIVE_SERVER` at a
    /// loopback URL would make the whole file pass while testing nothing.
    /// This fails, loudly and specifically, in that case.
    func testTheConfiguredServerActuallyRefusesAnonymousCallers() async throws {
        let server = try liveServer()
        let code = try await status(server.appendingPathComponent("api/sessions"))
        XCTAssertEqual(
            code, 401,
            "\(server) answered an ANONYMOUS /api/sessions with \(code). "
            + "This suite can only prove anything against a server that refuses "
            + "anonymous callers; a loopback URL is not a valid target.")
    }

    /// The share sheet's first screen is the worker list. Before AMUX-4976 this
    /// was the 401 the user saw as "amux returned 401".
    func testWorkersSucceedsWhereAnAnonymousCallerIsRefused() async throws {
        let server = try liveServer()
        let workers = try await AmuxClient.workers(server: server)
        XCTAssertFalse(workers.isEmpty,
                       "authorized /api/sessions returned an empty list")
        XCTAssertFalse(workers.contains { $0.name.isEmpty })
    }

    /// The attachment path. Chunked upload, three authorized round trips
    /// (start, chunk, finish), and the host-absolute path the share message
    /// then references.
    func testUploadReturnsAnAbsolutePathOnTheHost() async throws {
        let server = try liveServer()
        let payload = "amux ios share verification \(UUID().uuidString)\n"
        let tmp = FileManager.default.temporaryDirectory
            .appendingPathComponent("amux-share-verify-\(UUID().uuidString).txt")
        try payload.write(to: tmp, atomically: true, encoding: .utf8)
        defer { try? FileManager.default.removeItem(at: tmp) }

        let path = try await AmuxClient.upload(fileURL: tmp, server: server)
        XCTAssertTrue(path.hasPrefix("/"),
                      "upload/finish gave \(path), which is not host-absolute")
    }

    /// THE WHOLE SHARE, END TO END, and the only test that proves a share
    /// ARRIVES rather than merely being accepted.
    ///
    /// Ethan asked for exactly this: "confirm the share + note gets delivered
    /// to a worker". So it uploads a file, sends the message the Share
    /// Extension would compose (note + shared text + the host-absolute
    /// attachment path), and then reads the RECIPIENT'S OWN history back until
    /// the message shows up. A 200 from /send is not delivery; the recipient
    /// having it is.
    ///
    /// Target is `amux`, this repo's own lane, so the verification prompt lands
    /// where it is expected rather than interrupting somebody else's work.
    func testAShareWithANoteIsDeliveredToTheWorker() async throws {
        let server = try liveServer()
        let token = "amux-share-e2e-\(UUID().uuidString.prefix(8))"

        let payload = "attachment body for \(token)\n"
        let tmp = FileManager.default.temporaryDirectory
            .appendingPathComponent("amux-share-\(token).txt")
        try payload.write(to: tmp, atomically: true, encoding: .utf8)
        defer { try? FileManager.default.removeItem(at: tmp) }
        let uploaded = try await AmuxClient.upload(fileURL: tmp, server: server)

        let note = "verification note \(token)"
        let body = "\(note)\n\nShared from iOS:\n\(uploaded)"
        try await AmuxClient.send(text: body, to: Self.target, server: server)

        let delivered = try await waitForDelivery(of: token, to: Self.target, timeout: 90)
        XCTAssertTrue(delivered.contains(note),
                      "the message reached \(Self.target) without the note text")
        XCTAssertTrue(delivered.contains(uploaded),
                      "the message reached \(Self.target) without the attachment path")
    }

    private static let target = "amux"

    /// Reads the recipient's own history until the token appears. Loopback,
    /// where the server answers anonymously, so this needs no credential of its
    /// own and cannot accidentally prove the auth path twice.
    private func waitForDelivery(of token: String, to worker: String,
                                 timeout: TimeInterval) async throws -> String {
        let url = URL(string: "https://localhost:8823/api/history?session=\(worker)&limit=25")!
        let session = URLSession(configuration: .ephemeral, delegate: TrustAll(), delegateQueue: nil)
        let deadline = Date().addingTimeInterval(timeout)
        var lastBytes = 0
        while Date() < deadline {
            let (data, _) = try await session.data(for: URLRequest(url: url, timeoutInterval: 20))
            lastBytes = data.count
            let body = String(data: data, encoding: .utf8) ?? ""
            if body.contains(token) { return body }
            try await Task.sleep(nanoseconds: 2_000_000_000)
        }
        XCTFail(
            "no message carrying \(token) reached \(worker) within \(Int(timeout))s. "
            + "The last history read was \(lastBytes) bytes; 0 would mean the loopback "
            + "server was unreachable, which is a rig gap rather than a delivery failure.")
        return ""
    }

    /// The delivery path, WITHOUT delivering anything to a real worker.
    ///
    /// A share that reached a live lane would inject a prompt into somebody's
    /// session, so this sends to a name that cannot exist. The assertion is
    /// about WHICH failure comes back: anything other than 401 means the
    /// request was authorized and got as far as resolving the worker, which is
    /// the only property this test is for.
    func testSendIsAuthorizedEvenWhenTheWorkerDoesNotExist() async throws {
        let server = try liveServer()
        let ghost = "amux-share-verify-\(UUID().uuidString)"
        do {
            try await AmuxClient.send(text: "verification", to: ghost, server: server)
            XCTFail("sending to \(ghost) succeeded; the name was supposed to be absent")
        } catch let AmuxClient.ClientError.http(code, body) {
            XCTAssertNotEqual(
                code, 401,
                "send was refused as unauthenticated, which is the AMUX-4976 "
                + "regression: \(body)")
        }
    }
}
