import SwiftUI
import UIKit

/// Pick one or more workers, add an optional note, send.
///
/// Layout (Ethan, 2026-09-30): search by name at the top, then Filter and Sort
/// on one row under it, then the worker list (tap to select, several allowed),
/// then the note in a bar at the bottom. Cancel and Send live in the navigation
/// bar and never leave it: Send turns into a spinner while sending instead of
/// disappearing. The search header and the note bar are pinned safe-area
/// insets, so the keyboard pushes them up and never covers them; scrolling the
/// list puts the keyboard away. The list paints from the last cached copy and
/// refreshes behind it, from the server's light `view=picker` list.
struct ShareView: View {
    /// What was shared. Filled in the background: the picker opens at once and
    /// the list loads while photos or videos are still being copied (Ethan,
    /// 2026-10-01: "make the workers load faster").
    @ObservedObject var input: ShareInput
    /// workers, note, progress(step text), done(sent / queued / failed)
    let onSend: ([String], String, @escaping (String) -> Void, @escaping (ShareOutcome) -> Void) -> Void
    let onCancel: () -> Void

    /// THE CACHED LIST IS IN THE FIRST FRAME (Ethan, 2026-10-03: "loading
    /// workers should be instant"). It used to be read in `.task`, after the
    /// first render, so every open showed a spinner frame before the rows.
    init(input: ShareInput,
         onSend: @escaping ([String], String, @escaping (String) -> Void, @escaping (ShareOutcome) -> Void) -> Void,
         onCancel: @escaping () -> Void) {
        self.input = input
        self.onSend = onSend
        self.onCancel = onCancel
        let cached = AmuxStore.shareWorkersCache
            .flatMap { try? JSONDecoder().decode([AmuxClient.Worker].self, from: $0) } ?? []
        _workers = State(initialValue: cached)
        _loading = State(initialValue: cached.isEmpty)
        _refreshing = State(initialValue: !cached.isEmpty)
        _frozenRank = State(initialValue: Self.activityRank(cached))
        if let last = AmuxStore.lastWorker, cached.contains(where: { $0.name == last }) {
            _selected = State(initialValue: [last])
        }
        _sort = State(initialValue: AmuxStore.shareSort.flatMap(SortOrder.init(rawValue:)) ?? .recentlyShared)
    }

    /// Each worker's place by activity when the list first painted. Used to
    /// break ties for the whole life of the sheet, so a refresh that changes
    /// activity cannot move a row out from under a finger about to tap it.
    @State private var frozenRank: [String: Int] = [:]
    static func activityRank(_ list: [AmuxClient.Worker]) -> [String: Int] {
        let ordered = list.sorted {
            if $0.running != $1.running { return $0.running }
            if $0.lastActivity != $1.lastActivity { return $0.lastActivity > $1.lastActivity }
            return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
        return Dictionary(ordered.enumerated().map { ($1.name, $0) }, uniquingKeysWith: { a, _ in a })
    }

    @State private var workers: [AmuxClient.Worker] = []
    /// In tap order, so a multi-send goes out in the order you chose.
    @State private var selected: [String] = []
    @State private var note: String = ""
    @State private var loadError: String?
    @State private var loading = true
    /// A fresh list is being fetched behind a cached one.
    @State private var refreshing = false
    /// The refresh failed while a cached list is on screen: say so, keep the list.
    @State private var staleNote: String?
    @State private var filter = ""
    @State private var group: String? = nil
    @State private var sort: SortOrder = .recentlyShared
    @State private var sending = false
    /// Send was tapped with no worker picked: say so instead of ignoring it.
    @State private var pickHint = false
    /// Sent: the card shows a check for a moment before the sheet closes, so a
    /// fast send is still visibly a send (Ethan, 2026-10-01).
    @State private var sent = false
    /// The network failed: the share is in the outbox and will go when online.
    @State private var queuedNote: String?
    @State private var sendStep = ""
    @FocusState private var focused: Field?
    enum Field { case search, note }
    /// ACTIVE ONLY, BY DEFAULT (Ethan, 2026-09-23; AMUX-5015).
    ///
    /// LIFECYCLE, NOT `running`. A send to a stopped-but-active lane queues and
    /// delivers when it starts; a send to a PAUSED lane parks indefinitely
    /// (AMUX-5006). `@State`, so it resets to active for every share.
    @State private var activeOnly = true

    /// RECENTLY SHARED IS THE DEFAULT (Ethan, 2026-09-30: "sort by last shared
    /// should be default"), then volume. Both are counted on this device from
    /// successful sends (AmuxStore.recordShare). Activity and Name stay.
    enum SortOrder: String, CaseIterable, Identifiable {
        case recentlyShared = "Recently shared"
        case mostShared = "Most shared"
        case activity = "Activity"
        case name = "Name"
        var id: String { rawValue }
    }

    private let counts = AmuxStore.shareCounts
    private let lastShared = AmuxStore.shareLastAt

    /// Every group any worker belongs to, for the Filter menu.
    private var groups: [String] {
        Array(Set(workers.flatMap(\.groups))).sorted {
            $0.localizedCaseInsensitiveCompare($1) == .orderedAscending
        }
    }

    /// Search matches the name first; task and folder too, because the thing
    /// you remember is sometimes the task text or the directory.
    private var shown: [AmuxClient.Worker] {
        // SEARCHING OVERRIDES THE ACTIVE FILTER: typing a name you know and
        // being told it does not exist is worse than a longer list. The group
        // filter still applies, because you chose it on purpose.
        var pool = (activeOnly && filter.isEmpty) ? workers.filter { $0.lifecycle == "active" } : workers
        if let group { pool = pool.filter { $0.groups.contains(group) } }
        let matched = filter.isEmpty ? pool : pool.filter {
            $0.name.localizedCaseInsensitiveContains(filter)
                || $0.task.localizedCaseInsensitiveContains(filter)
                || $0.workspace.localizedCaseInsensitiveContains(filter)
        }
        let rank = frozenRank
        let byActivity: (AmuxClient.Worker, AmuxClient.Worker) -> Bool = {
            // The order at first paint wins while the sheet is open (see
            // frozenRank); only a worker new since then falls through.
            if let a = rank[$0.name], let b = rank[$1.name], a != b { return a < b }
            // Running first, then most recent: a live but quiet lane must not
            // sort below a stopped one touched more recently.
            if $0.running != $1.running { return $0.running }
            if $0.lastActivity != $1.lastActivity { return $0.lastActivity > $1.lastActivity }
            return $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
        let (count, last) = ({ (w: AmuxClient.Worker) in counts[w.name] ?? 0 },
                             { (w: AmuxClient.Worker) in lastShared[w.name] ?? 0 })
        switch sort {
        case .name:
            return matched.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        case .activity:
            return matched.sorted(by: byActivity)
        case .recentlyShared:
            return matched.sorted {
                if last($0) != last($1) { return last($0) > last($1) }
                if count($0) != count($1) { return count($0) > count($1) }
                return byActivity($0, $1)
            }
        case .mostShared:
            return matched.sorted {
                if count($0) != count($1) { return count($0) > count($1) }
                if last($0) != last($1) { return last($0) > last($1) }
                return byActivity($0, $1)
            }
        }
    }

    private static let ago: RelativeDateTimeFormatter = {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .abbreviated
        return f
    }()

    private func lastSeen(_ w: AmuxClient.Worker) -> String {
        // 0 means the server never recorded activity; "56 years ago" is worse
        // than saying nothing.
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
                    // Almost always "no server picked yet" or "the Mac is not
                    // reachable". Say which rather than an empty list.
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
                    list
                        .safeAreaInset(edge: .top, spacing: 0) { header }
                        .safeAreaInset(edge: .bottom, spacing: 0) { noteBar }
                }
            }
            .navigationTitle("Share to amux")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                // Identifiers, not labels: a toolbar Button is matched by
                // identifier first (see ShareSheetUITests). Both stay in the bar
                // for the whole life of the sheet, including while sending.
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: onCancel)
                        .accessibilityIdentifier("cancel")
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(action: send) {
                        if sending {
                            HStack(spacing: 6) {
                                ProgressView()
                                Text("Sending")
                            }
                        } else {
                            Text(selected.count > 1 ? "Send (\(selected.count))" : "Send").bold()
                        }
                    }
                    .disabled(loading || sending)
                    .accessibilityIdentifier(sending ? "sending" : "send")
                }
            }
            .overlay { if sending { sendingCard } }
            .onChange(of: sort) { AmuxStore.shareSort = $0.rawValue }
            .onChange(of: selected) { _ in if !selected.isEmpty { pickHint = false } }
        }
        .navigationViewStyle(.stack)
        .task { await load() }
    }

    /// Search by name, and Filter + Sort on one row under it.
    private var header: some View {
        VStack(spacing: 8) {
            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("Search by name", text: $filter)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .submitLabel(.search)
                    .focused($focused, equals: .search)
                    .accessibilityIdentifier("search")
                if !filter.isEmpty {
                    Button { filter = "" } label: {
                        Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                    }
                    .accessibilityLabel("Clear search")
                }
            }
            .padding(.horizontal, 10)
            .frame(minHeight: 40)
            .background(Color.secondary.opacity(0.12), in: RoundedRectangle(cornerRadius: 10))
            HStack(spacing: 8) {
                Menu {
                    Picker("Group", selection: $group) {
                        Text("All groups").tag(String?.none)
                        ForEach(groups, id: \.self) { Text($0).tag(String?.some($0)) }
                    }
                    Toggle("Active workers only", isOn: $activeOnly)
                } label: {
                    menuLabel("line.3.horizontal.decrease.circle",
                              group.map { "Group: \($0)" } ?? (activeOnly ? "All groups · active" : "All groups"))
                }
                .accessibilityIdentifier("groupFilter")
                Menu {
                    Picker("Sort by", selection: $sort) {
                        ForEach(SortOrder.allCases) { Text($0.rawValue).tag($0) }
                    }
                } label: {
                    menuLabel("arrow.up.arrow.down", sort.rawValue)
                }
                .accessibilityIdentifier("sortOrder")
            }
        }
        .padding(.horizontal)
        .padding(.vertical, 8)
        .background(.bar)
        .disabled(sending)
    }

    private func menuLabel(_ icon: String, _ text: String) -> some View {
        HStack(spacing: 4) {
            Image(systemName: icon)
            Text(text).lineLimit(1)
            Image(systemName: "chevron.down").font(.caption2)
        }
        .font(.subheadline)
        .padding(.horizontal, 10)
        .frame(maxWidth: .infinity, minHeight: 36)
        .background(Color.secondary.opacity(0.12), in: Capsule())
    }

    private var list: some View {
        Form {
            Section {
                ForEach(shown) { w in
                    Button {
                        toggle(w.name)
                    } label: {
                        row(w).contentShape(Rectangle())
                    }
                    // Plain, so the row reads as text (name primary, details
                    // secondary) instead of every line tinted like a link.
                    .buttonStyle(.plain)
                    // A Button's accessibility label is everything inside it
                    // concatenated, so rows need an identifier.
                    .accessibilityIdentifier("worker-\(w.name)")
                }
            } header: {
                HStack {
                    Text("Send to")
                    if pickHint {
                        Text("Pick a worker first")
                            .font(.caption.bold())
                            .foregroundStyle(.red)
                            .accessibilityIdentifier("pickHint")
                    }
                    Spacer()
                    if refreshing { ProgressView().controlSize(.mini) }
                    Text(countLabel)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .accessibilityIdentifier("population")
                }
            } footer: {
                VStack(alignment: .leading, spacing: 4) {
                    if let staleNote { Text(staleNote).foregroundStyle(.orange) }
                    Text(summary)
                }
            }
        }
        // Scrolling the list puts the keyboard away instead of leaving the
        // rows you are scrolling to behind it.
        .scrollDismissesKeyboard(.immediately)
        .disabled(sending)
    }

    private func toggle(_ name: String) {
        if let i = selected.firstIndex(of: name) { selected.remove(at: i) } else { selected.append(name) }
    }

    private func row(_ w: AmuxClient.Worker) -> some View {
        HStack(spacing: 10) {
            // A filled dot for a live lane; the word beside it says which kind
            // of live, because colour alone is not readable to everyone.
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
                    if let n = counts[w.name], n > 0 {
                        Text("·")
                        Text("shared \(n)×")
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
            Image(systemName: selected.contains(w.name) ? "checkmark.circle.fill" : "circle")
                .foregroundStyle(selected.contains(w.name) ? Color.accentColor : Color.secondary.opacity(0.5))
                .font(.title3)
                .accessibilityHidden(!selected.contains(w.name))
        }
    }

    /// The note is a bar pinned to the bottom safe area, which rides above the
    /// keyboard: typing a note never pushes the list out of sight, and the
    /// chosen workers stay named right above it.
    private var noteBar: some View {
        VStack(spacing: 6) {
            if !selected.isEmpty {
                Text("To " + selected.joined(separator: ", "))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .accessibilityIdentifier("recipients")
            }
            TextField("Note (optional)", text: $note, axis: .vertical)
                .lineLimit(1...4)
                .textFieldStyle(.roundedBorder)
                .focused($focused, equals: .note)
                .accessibilityIdentifier("note")
        }
        .padding(.horizontal)
        .padding(.vertical, 8)
        .background(.bar)
        .disabled(sending)
    }

    /// What happens after Send, step by step: uploads can take a while and the
    /// sheet used to sit still until it closed (Ethan, 2026-09-29: "when i hit
    /// send indicate its sending").
    private var sendingCard: some View {
        VStack(spacing: 10) {
            if sent {
                Image(systemName: "checkmark.circle.fill")
                    .font(.system(size: 44))
                    .foregroundStyle(.green)
                Text(selected.count == 1 ? "Sent to \(selected[0])" : "Sent to \(selected.count) workers")
                    .font(.headline)
            } else if let queuedNote {
                Image(systemName: "clock.arrow.circlepath")
                    .font(.system(size: 44))
                    .foregroundStyle(.orange)
                Text("Queued, will send when online")
                    .font(.headline)
                Text(queuedNote)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            } else {
                ProgressView()
                    .controlSize(.large)
                Text(selected.count == 1 ? "Sending to \(selected[0])…" : "Sending to \(selected.count) workers…")
                    .font(.headline)
            }
            if !sent && queuedNote == nil && !sendStep.isEmpty {
                Text(sendStep)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
        }
        .padding(24)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 16))
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier(sent ? "sentCard" : (queuedNote != nil ? "queuedCard" : "sendingCard"))
    }

    private func send() {
        guard !sending else { return }
        // EVERY TAP ANSWERS (Ethan, 2026-10-02: "i click send nothing
        // happens"). With no worker picked the button used to be disabled, and
        // a disabled button gives nothing back to a tap.
        if selected.isEmpty {
            UINotificationFeedbackGenerator().notificationOccurred(.warning)
            pickHint = true
            return
        }
        UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        pickHint = false
        focused = nil
        sending = true
        sent = false
        queuedNote = nil
        sendStep = ""
        onSend(selected, note, { step in
            DispatchQueue.main.async { sendStep = step }
        }, { outcome in
            DispatchQueue.main.async {
                switch outcome {
                case .sent:
                    // The controller closes the sheet a moment later.
                    UINotificationFeedbackGenerator().notificationOccurred(.success)
                    sent = true
                case .queued(let why):
                    // Kept in the outbox: it goes when the network is back.
                    UINotificationFeedbackGenerator().notificationOccurred(.warning)
                    queuedNote = "No connection to amux (\(why)). It is saved on this iPhone."
                case .failed:
                    UINotificationFeedbackGenerator().notificationOccurred(.error)
                    sending = false
                    sendStep = ""
                }
            }
        })
    }

    /// Says which population the list is showing, so a filter that matches
    /// nothing never looks like a fleet with no workers.
    private var countLabel: String {
        let running = workers.filter(\.running).count
        if !filter.isEmpty {
            return "\(shown.count) of \(workers.count), all workers"
        }
        if let group {
            return "\(shown.count) in \(group)"
        }
        if activeOnly {
            // COUNTED FROM `shown`, the rows actually on screen. The UI test
            // reads this label (active, running, hidden), so it must describe
            // the list, not a second filter.
            // Running is counted over the same rows: counting the whole fleet
            // read "13 active, 33 running", more running than shown, because
            // paused lanes keep their tmux session alive.
            let onScreen = shown.count
            let runningShown = shown.filter(\.running).count
            return "\(onScreen) active, \(runningShown) running · \(workers.count - onScreen) paused hidden"
        }
        return "\(workers.count) workers · \(running) running"
    }

    private var summary: String {
        var parts: [String] = []
        if !input.ready { return "Preparing what you shared…" }
        if input.attachmentCount > 0 {
            parts.append("\(input.attachmentCount) attachment\(input.attachmentCount == 1 ? "" : "s")")
        }
        if !input.sharedText.isEmpty { parts.append("shared text") }
        return parts.isEmpty ? "Nothing attached" : parts.joined(separator: " + ")
    }

    private func load() async {
        guard let server = AmuxStore.serverURL else {
            loadError = AmuxClient.ClientError.noServer.localizedDescription
            loading = false
            return
        }
        // The cached list is already on screen (see init). Deliver anything
        // the outbox still holds from an earlier offline share, in the
        // background: the picker never waits on it.
        Task.detached { await ShareOutbox.drainShared() }
        do {
            // A worker renamed since the last share keeps its place: the
            // remembered selection and share counts move to its new name.
            let renames = await AmuxClient.renames(server: server)
            if !renames.isEmpty {
                AmuxStore.applyRenames(renames)
                selected = selected.map { renames[$0] ?? $0 }
            }
            let found = try await AmuxClient.workers(server: server)
            workers = found
            if frozenRank.isEmpty { frozenRank = Self.activityRank(found) }
            AmuxStore.shareWorkersCache = try? JSONEncoder().encode(found)
            // A remembered selection that has since been deleted would look
            // selected and then fail at send time.
            selected.removeAll { name in !found.contains(where: { $0.name == name }) }
            if selected.isEmpty { preselect(from: found) }
            staleNote = nil
        } catch {
            if workers.isEmpty {
                loadError = error.localizedDescription
            } else {
                staleNote = "Showing the saved list; could not refresh: \(error.localizedDescription)"
            }
        }
        refreshing = false
        loading = false
    }

    private func preselect(from list: [AmuxClient.Worker]) {
        if let last = AmuxStore.lastWorker, list.contains(where: { $0.name == last }) {
            selected = [last]
        }
    }
}
