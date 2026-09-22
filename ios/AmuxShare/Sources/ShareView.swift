import SwiftUI

/// Pick a worker, add an optional note, send.
struct ShareView: View {
    let attachmentCount: Int
    let sharedText: String
    let onSend: (String, String) -> Void          // worker, note
    let onCancel: () -> Void

    @State private var workers: [AmuxClient.Worker] = []
    @State private var selected: String = ""
    @State private var note: String = ""
    @State private var loadError: String?
    @State private var loading = true
    @State private var filter = ""

    private var shown: [AmuxClient.Worker] {
        filter.isEmpty ? workers
            : workers.filter { $0.name.localizedCaseInsensitiveContains(filter) }
    }

    var body: some View {
        NavigationView {
            Group {
                if loading {
                    ProgressView("Loading workers…")
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else if let loadError {
                    // A failure here is almost always "no server picked yet" or
                    // "the Mac is not reachable". Say which, rather than an
                    // empty list that reads as "you have no workers".
                    VStack(spacing: 12) {
                        Image(systemName: "exclamationmark.triangle")
                            .font(.largeTitle)
                        Text(loadError)
                            .multilineTextAlignment(.center)
                            .foregroundStyle(.secondary)
                    }
                    .padding()
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else {
                    Form {
                        Section("Note") {
                            TextField("Optional", text: $note, axis: .vertical)
                                .lineLimit(1...4)
                        }
                        Section {
                            ForEach(shown) { w in
                                Button {
                                    selected = w.name
                                } label: {
                                    HStack {
                                        Text(w.name).foregroundStyle(.primary)
                                        Spacer()
                                        if !w.status.isEmpty {
                                            Text(w.status)
                                                .font(.caption)
                                                .foregroundStyle(.secondary)
                                        }
                                        if selected == w.name {
                                            Image(systemName: "checkmark")
                                        }
                                    }
                                }
                            }
                        } header: {
                            Text("Send to")
                        } footer: {
                            Text(summary)
                        }
                    }
                    .searchable(text: $filter, prompt: "Filter workers")
                }
            }
            .navigationTitle("Share to amux")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: onCancel)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Send") { onSend(selected, note) }
                        .disabled(selected.isEmpty || loading)
                }
            }
        }
        .task { await load() }
    }

    private var summary: String {
        var parts: [String] = []
        if attachmentCount > 0 {
            parts.append("\(attachmentCount) attachment\(attachmentCount == 1 ? "" : "s")")
        }
        if !sharedText.isEmpty { parts.append("shared text") }
        return parts.isEmpty ? "Nothing attached" : parts.joined(separator: " + ")
    }

    private func load() async {
        guard let server = AmuxStore.serverURL else {
            loadError = AmuxClient.ClientError.noServer.localizedDescription
            loading = false
            return
        }
        do {
            let found = try await AmuxClient.workers(server: server)
            workers = found
            // Re-offer the last target, but only if it is still there. A
            // remembered name that has since been deleted would otherwise look
            // selected and then fail at send time.
            if let last = AmuxStore.lastWorker, found.contains(where: { $0.name == last }) {
                selected = last
            }
        } catch {
            loadError = error.localizedDescription
        }
        loading = false
    }
}
