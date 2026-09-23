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
    @State private var sort: SortOrder = .activity
    /// ACTIVE ONLY, BY DEFAULT (Ethan, 2026-09-23). 165 sessions exist and 17
    /// are lifecycle-active; the other 148 are paused or archived and cannot
    /// take a share. Offering them is offering a mistake.
    @State private var activeOnly = true

    /// ACTIVITY IS THE DEFAULT because the worker you want is almost always the
    /// one you were just looking at. Name is there for the other case: you know
    /// exactly which lane you want out of 160-odd and do not care what it has
    /// been doing.
    enum SortOrder: String, CaseIterable, Identifiable {
        case activity = "Activity"
        case name = "Name"
        var id: String { rawValue }
    }

    /// Search matches more than the name on purpose. Half the workers here are
    /// named for a repo area and the thing you remember is the task text or the
    /// directory, so matching only names makes the field useless exactly when
    /// the list is long enough to need it.
    private var shown: [AmuxClient.Worker] {
        // SEARCHING OVERRIDES THE FILTER. Typing a name you know and being told
        // it does not exist is worse than a longer list: the one case where you
        // are sure which worker you want is the one where hiding it is most
        // annoying. The header says which population is on screen.
        let pool = (activeOnly && filter.isEmpty) ? workers.filter(\.running) : workers
        let matched = filter.isEmpty ? pool : pool.filter {
            $0.name.localizedCaseInsensitiveContains(filter)
                || $0.task.localizedCaseInsensitiveContains(filter)
                || $0.workspace.localizedCaseInsensitiveContains(filter)
        }
        switch sort {
        case .name:
            return matched.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        case .activity:
            // Running first, then most recent. Without the first clause a lane
            // that is live right now but quiet sorts below a stopped one that
            // happened to be touched more recently, which reads as wrong.
            return matched.sorted {
                if $0.running != $1.running { return $0.running }
                if $0.lastActivity != $1.lastActivity { return $0.lastActivity > $1.lastActivity }
                return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
            }
        }
    }

    private static let ago: RelativeDateTimeFormatter = {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .abbreviated
        return f
    }()

    private func lastSeen(_ w: AmuxClient.Worker) -> String {
        // 0 means the server has never recorded activity. Saying "56 years ago"
        // is worse than saying nothing.
        guard w.lastActivity > 0 else { return "" }
        return Self.ago.localizedString(
            for: Date(timeIntervalSince1970: TimeInterval(w.lastActivity)), relativeTo: Date())
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
                                .accessibilityIdentifier("note")
                        }
                        Section {
                            Picker("Sort", selection: $sort) {
                                ForEach(SortOrder.allCases) { Text($0.rawValue).tag($0) }
                            }
                            .pickerStyle(.segmented)
                            .accessibilityIdentifier("sortOrder")
                            Toggle("Active workers only", isOn: $activeOnly)
                                .accessibilityIdentifier("activeOnly")
                        }
                        Section {
                            ForEach(shown) { w in
                                Button {
                                    selected = w.name
                                } label: {
                                    HStack(spacing: 10) {
                                        // A filled dot for a live lane. The word
                                        // beside it still says which kind of live,
                                        // because colour alone is not readable to
                                        // everyone.
                                        Circle()
                                            .fill(w.running ? Color.green : Color.secondary.opacity(0.35))
                                            .frame(width: 8, height: 8)
                                        VStack(alignment: .leading, spacing: 2) {
                                            Text(w.name)
                                                .foregroundStyle(.primary)
                                                .lineLimit(1)
                                            HStack(spacing: 6) {
                                                Text(w.display)
                                                if !lastSeen(w).isEmpty {
                                                    Text("·")
                                                    Text(lastSeen(w))
                                                }
                                                if !w.workspace.isEmpty {
                                                    Text("·")
                                                    Text(w.workspace).lineLimit(1)
                                                }
                                            }
                                            .font(.caption)
                                            .foregroundStyle(.secondary)
                                            if !w.task.isEmpty {
                                                Text(w.task)
                                                    .font(.caption2)
                                                    .foregroundStyle(.tertiary)
                                                    .lineLimit(1)
                                            }
                                        }
                                        Spacer(minLength: 8)
                                        if selected == w.name {
                                            Image(systemName: "checkmark")
                                                .foregroundStyle(Color.accentColor)
                                        }
                                    }
                                }
                                // A Button's accessibility label is everything
                                // inside it concatenated, so the row cannot be
                                // addressed by worker name without this.
                                .accessibilityIdentifier("worker-\(w.name)")
                            }
                        } header: {
                            HStack {
                                Text("Send to")
                                Spacer()
                                Text(countLabel)
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                            }
                        } footer: {
                            Text(summary)
                        }
                    }
                    .searchable(text: $filter, prompt: "Search name, task or folder")
                }
            }
            .navigationTitle("Share to amux")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                // Identifiers, not labels. A toolbar Button's LABEL is matched
                // only after its identifier, and `buttons["Send"]` failed with
                // `No matches found for Elements matching predicate
                // '"Send" IN identifiers'` once the view around it changed.
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: onCancel)
                        .accessibilityIdentifier("cancel")
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Send") { onSend(selected, note) }
                        .disabled(selected.isEmpty || loading)
                        .accessibilityIdentifier("send")
                }
            }
        }
        .task { await load() }
    }

    /// Says which population the list is showing. Without it a filter that
    /// matches nothing looks identical to a fleet with no workers.
    /// Says which population is on screen. A filter that matches nothing must
    /// not look like a fleet with no workers, and "active only" must not look
    /// like the whole fleet went away.
    private var countLabel: String {
        let running = workers.filter(\.running).count
        if !filter.isEmpty {
            return "\(shown.count) of \(workers.count), all workers"
        }
        if activeOnly {
            return "\(running) running · \(workers.count - running) hidden"
        }
        return "\(workers.count) workers · \(running) running"
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
