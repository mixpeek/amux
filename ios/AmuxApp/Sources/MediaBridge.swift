import Photos
import WebKit
import os.log

private let logger = Logger(subsystem: "io.amux.app", category: "MediaBridge")

/// Teleprompter takes (AA-39). The dashboard records the front camera with
/// MediaRecorder and hands the file over in base64 slices, because a WKWebView
/// has no download path into Photos. Slices are appended to a temp file and
/// `save` adds it to the library, then deletes the temp file.
///
/// Ops: begin {name} -> id, chunk {id, data} -> bytes written,
/// save {id} -> true, abort {id}. A failure rejects the page's promise with
/// a sentence the dashboard shows as-is.
final class MediaBridge: NSObject, WKScriptMessageHandlerWithReply {
    private var files: [String: (url: URL, handle: FileHandle)] = [:]

    func userContentController(_ controller: WKUserContentController,
                               didReceive message: WKScriptMessage,
                               replyHandler: @escaping (Any?, String?) -> Void) {
        guard let body = message.body as? [String: Any], let op = body["op"] as? String else {
            replyHandler(nil, "Malformed message")
            return
        }
        let id = body["id"] as? String ?? ""
        switch op {
        case "begin":
            let raw = body["name"] as? String ?? "take.mp4"
            let name = raw.components(separatedBy: CharacterSet(charactersIn: "/\\:")).joined(separator: "-")
            let newId = UUID().uuidString
            let url = FileManager.default.temporaryDirectory.appendingPathComponent(newId + "-" + name)
            guard FileManager.default.createFile(atPath: url.path, contents: nil),
                  let handle = try? FileHandle(forWritingTo: url) else {
                logger.error("Take temp file could not be created at \(url.path)")
                replyHandler(nil, "Could not create a temporary file for the take")
                return
            }
            files[newId] = (url, handle)
            logger.info("Take \(newId) started: \(name)")
            replyHandler(newId, nil)
        case "chunk":
            guard let file = files[id] else { replyHandler(nil, "Unknown take"); return }
            guard let b64 = body["data"] as? String, let data = Data(base64Encoded: b64) else {
                discard(id)
                replyHandler(nil, "A slice of the take arrived corrupted")
                return
            }
            do {
                try file.handle.write(contentsOf: data)
                replyHandler(data.count, nil)
            } catch {
                discard(id)
                logger.error("Take \(id) write failed: \(error.localizedDescription)")
                replyHandler(nil, error.localizedDescription)
            }
        case "save":
            guard let file = files.removeValue(forKey: id) else { replyHandler(nil, "Unknown take"); return }
            try? file.handle.close()
            save(file.url, id: id, replyHandler: replyHandler)
        case "abort":
            discard(id)
            replyHandler(true, nil)
        default:
            replyHandler(nil, "Unknown op \(op)")
        }
    }

    private func save(_ url: URL, id: String, replyHandler: @escaping (Any?, String?) -> Void) {
        PHPhotoLibrary.requestAuthorization(for: .addOnly) { status in
            guard status == .authorized || status == .limited else {
                try? FileManager.default.removeItem(at: url)
                logger.error("Take \(id) not saved: Photos access is \(String(describing: status.rawValue))")
                DispatchQueue.main.async {
                    replyHandler(nil, "Photos access is off for amux. Allow it in Settings > amux > Photos")
                }
                return
            }
            PHPhotoLibrary.shared().performChanges({
                PHAssetCreationRequest.creationRequestForAssetFromVideo(atFileURL: url)
            }) { ok, error in
                try? FileManager.default.removeItem(at: url)
                if ok {
                    logger.info("Take \(id) saved to Photos")
                } else {
                    logger.error("Take \(id) refused by Photos: \(error?.localizedDescription ?? "no error")")
                }
                DispatchQueue.main.async {
                    if ok { replyHandler(true, nil) }
                    else { replyHandler(nil, error?.localizedDescription ?? "Photos refused the video") }
                }
            }
        }
    }

    private func discard(_ id: String) {
        guard let file = files.removeValue(forKey: id) else { return }
        try? file.handle.close()
        try? FileManager.default.removeItem(at: file.url)
    }
}
