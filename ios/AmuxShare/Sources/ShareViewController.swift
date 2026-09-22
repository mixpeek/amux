import UIKit
import SwiftUI
import UniformTypeIdentifiers

/// Share-sheet entry point: extract what was shared, show the picker, deliver.
final class ShareViewController: UIViewController {

    private var fileURLs: [URL] = []
    private var sharedText: String = ""

    override func viewDidLoad() {
        super.viewDidLoad()
        AmuxStore.migrateFromStandardIfNeeded()
        Task {
            await extractSharedItems()
            presentPicker()
        }
    }

    // MARK: - Extraction

    /// iOS hands attachments over as `NSItemProvider`s that may be files, URLs
    /// or text, often several representations of one thing. Files are asked for
    /// first because a photo offered as both an image and a URL is more useful
    /// to a worker as the actual bytes on disk.
    private func extractSharedItems() async {
        guard let items = extensionContext?.inputItems as? [NSExtensionItem] else { return }
        for item in items {
            if let text = item.attributedContentText?.string, !text.isEmpty {
                append(text: text)
            }
            for provider in item.attachments ?? [] {
                if let url = await loadFile(from: provider) {
                    fileURLs.append(url)
                } else if let text = await loadText(from: provider) {
                    append(text: text)
                }
            }
        }
    }

    private func append(text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !sharedText.contains(trimmed) else { return }
        sharedText += (sharedText.isEmpty ? "" : "\n") + trimmed
    }

    /// Copies into the extension's own temp directory. The URL the system
    /// hands back is only valid inside the completion handler, so uploading
    /// from it later would race the system reclaiming it.
    private func loadFile(from provider: NSItemProvider) async -> URL? {
        guard provider.hasItemConformingToTypeIdentifier(UTType.item.identifier) else { return nil }
        return await withCheckedContinuation { continuation in
            _ = provider.loadFileRepresentation(forTypeIdentifier: UTType.item.identifier) { url, _ in
                guard let url else { return continuation.resume(returning: nil) }
                let copy = FileManager.default.temporaryDirectory
                    .appendingPathComponent(UUID().uuidString + "-" + url.lastPathComponent)
                do {
                    try FileManager.default.copyItem(at: url, to: copy)
                    continuation.resume(returning: copy)
                } catch {
                    continuation.resume(returning: nil)
                }
            }
        }
    }

    private func loadText(from provider: NSItemProvider) async -> String? {
        for type in [UTType.url, UTType.plainText] {
            guard provider.hasItemConformingToTypeIdentifier(type.identifier) else { continue }
            let loaded: String? = await withCheckedContinuation { continuation in
                provider.loadItem(forTypeIdentifier: type.identifier) { value, _ in
                    if let url = value as? URL { continuation.resume(returning: url.absoluteString) }
                    else if let s = value as? String { continuation.resume(returning: s) }
                    else { continuation.resume(returning: nil) }
                }
            }
            if let loaded, !loaded.isEmpty { return loaded }
        }
        return nil
    }

    // MARK: - UI

    private func presentPicker() {
        let view = ShareView(
            attachmentCount: fileURLs.count,
            sharedText: sharedText,
            onSend: { [weak self] worker, note in self?.deliver(to: worker, note: note) },
            onCancel: { [weak self] in self?.extensionContext?.completeRequest(returningItems: nil) }
        )
        let host = UIHostingController(rootView: view)
        addChild(host)
        host.view.frame = self.view.bounds
        host.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        self.view.addSubview(host.view)
        host.didMove(toParent: self)
    }

    private func showFailure(_ message: String) {
        let alert = UIAlertController(title: "Could not send", message: message, preferredStyle: .alert)
        // Do NOT complete the request here. Dismissing on failure would look
        // exactly like a successful send, and the share would be silently gone.
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }

    // MARK: - Delivery

    private func deliver(to worker: String, note: String) {
        guard let server = AmuxStore.serverURL else {
            showFailure(AmuxClient.ClientError.noServer.localizedDescription)
            return
        }
        Task {
            do {
                var paths: [String] = []
                for url in fileURLs {
                    paths.append(try await AmuxClient.upload(fileURL: url, server: server))
                }
                // `@<abs path>` is how amux already inlines an attachment into a
                // prompt; the dashboard composer produces the same shape.
                var parts: [String] = []
                if !note.isEmpty { parts.append(note) }
                if !sharedText.isEmpty { parts.append(sharedText) }
                parts.append(contentsOf: paths.map { "@\($0)" })
                let text = parts.joined(separator: "\n")
                guard !text.isEmpty else {
                    await MainActor.run { showFailure("Nothing to send.") }
                    return
                }
                try await AmuxClient.send(text: text, to: worker, server: server)
                AmuxStore.lastWorker = worker
                await MainActor.run {
                    extensionContext?.completeRequest(returningItems: nil)
                }
            } catch {
                await MainActor.run { showFailure(error.localizedDescription) }
            }
        }
    }
}
