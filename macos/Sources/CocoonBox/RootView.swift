import CocoonBoxKit
import CocoonBoxTerminal
import SwiftUI

struct RootView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        @Bindable var model = model
        @Bindable var screen = model.screen
        NavigationSplitView {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 200, ideal: 220, max: 300)
        } detail: {
            if model.screen.poppedOut {
                ConversationView()
            } else {
                HSplitView {
                    LiveScreenPane()
                        .frame(minWidth: 480, maxWidth: .infinity, maxHeight: .infinity)
                        .layoutPriority(1)
                    ConversationView()
                        .frame(minWidth: 320, idealWidth: 380, maxWidth: 560, maxHeight: .infinity)
                }
            }
        }
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                if !model.reconnect.isEmpty {
                    Button {
                        model.openConnections()
                    } label: {
                        Label("Reconnect", systemImage: "exclamationmark.arrow.triangle.2.circlepath")
                            .foregroundStyle(.orange)
                    }
                    .help("Reconnect \(model.reconnect.joined(separator: ", ")) in CocoonBox Connect")
                }
                Button {
                    model.showConnectAgent = true
                } label: {
                    Label("Connect a local agent", systemImage: "point.3.connected.trianglepath.dotted")
                }
                .help("Drive the box from Claude Code, Codex or Cursor over MCP")
                .disabled(model.box == nil)
                Button {
                    model.showShare = true
                } label: {
                    Label("Share", systemImage: "person.2.badge.plus")
                }
                .help("Share the box's live view and tools by a link")
                .disabled(model.box == nil)
                Toggle(isOn: $screen.takeOver) {
                    Label("Take over", systemImage: model.screen.takeOver ? "hand.raised.fill" : "hand.raised")
                }
                .help("When on, your mouse and keyboard drive the box screen")
                .disabled(!model.agentsReady)
                Button {
                    model.screen.poppedOut.toggle()
                } label: {
                    Label(model.screen.poppedOut ? "Dock screen" : "Pop out screen", systemImage: "rectangle.on.rectangle")
                }
            }
        }
        .sheet(isPresented: $model.showSignIn) {
            SignInSheet()
        }
        .sheet(isPresented: $model.showConnectAgent) {
            ConnectAgentSheet()
        }
        .sheet(isPresented: $model.showShare) {
            ShareSheet()
        }
        .sheet(isPresented: $model.showTimeCapsule) {
            TimeCapsuleSheet()
        }
        .sheet(isPresented: $model.showPasswordSignIn) {
            PasswordSignInSheet()
        }
        .sheet(item: $model.boxCLISheet) { sheet in
            BoxCLISheetView(sheet: sheet).id(sheet.id)
        }
        .sheet(isPresented: Binding(get: { model.pluginSetup != nil }, set: { if !$0 { model.pluginSetup = nil } })) {
            if let setup = model.pluginSetup { PluginSetupSheet(setup: setup) }
        }
        .alert(model.busyWarning?.title ?? "", isPresented: Binding(get: { model.busyWarning != nil }, set: { if !$0 { model.busyWarning = nil } }), presenting: model.busyWarning) { warning in
            Button("Continue", role: .destructive) { warning.proceed() }
            Button("Cancel", role: .cancel) {}
        } message: { warning in
            Text(warning.detail)
        }
        .onChange(of: model.screen.poppedOut) { _, poppedOut in
            if poppedOut { openWindow(id: "screen") }
        }
    }
}

struct SidebarView: View {
    @Environment(AppModel.self) private var model
    @State private var externalExpanded = true
    @State private var newName = ""
    @State private var editingID: String?
    @FocusState private var nameFocused: Bool

    var body: some View {
        @Bindable var model = model
        VStack(alignment: .leading, spacing: 0) {
            BoxSwitcher()
                .padding([.horizontal, .top], 12)
            BoxHeader()
                .padding(12)
            Divider()
            HStack {
                Text("Agents").font(.headline)
                Spacer()
                if let replacing = model.replacing {
                    Text(replacing.status)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Menu {
                    Button("Agent in the box") { model.createAgent() }
                    Menu("Local agent") {
                        ForEach(LocalAgentKind.allCases) { kind in
                            Button(kind.title) { model.launchLocal(kind) }
                        }
                    }
                } label: {
                    Image(systemName: "plus")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .disabled(!model.agentsReady)
                .help("New agent")
                Button {
                    model.deletingAgent = model.selectedAgent
                } label: {
                    Image(systemName: "minus")
                }
                .buttonStyle(.borderless)
                .disabled(!deletable)
                .help("Delete the selected agent")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            let external = model.externalAgents()
            List(selection: $model.selectedAgentID) {
                Section {
                    ForEach(model.agents.filter { $0.externalClient == nil }, id: \.id) { agent in
                        Group {
                            if editingID == agent.id {
                                nameField { commitRename(agent) }
                            } else {
                                AgentRow(agent: agent)
                            }
                        }
                        .tag(agent.id)
                        .simultaneousGesture(TapGesture(count: 2).onEnded { startRename(agent) })
                    }
                }
                Section(isExpanded: $externalExpanded) {
                    ForEach(external, id: \.id) { agent in
                        ShareSessionRow(agent: agent).tag(agent.id)
                    }
                } header: {
                    if !external.isEmpty {
                        Text("External")
                    }
                }
                Section {
                    ForEach(model.localAgents) { session in
                        Group {
                            if editingID == AppModel.selectionID(session) {
                                nameField { commitRename(session) }
                            } else {
                                LocalAgentRow(session: session)
                            }
                        }
                        .tag(AppModel.selectionID(session))
                        .simultaneousGesture(TapGesture(count: 2).onEnded { startRename(session) })
                    }
                    ForEach(model.failedLocal, id: \.id) { record in
                        FailedLocalAgentRow(record: record)
                    }
                } header: {
                    if !model.localAgents.isEmpty || !model.failedLocal.isEmpty {
                        Text("On this Mac")
                    }
                }
                Section {
                    ForEach(model.offloads, id: \.repo) { record in
                        OffloadRow(record: record).tag(AppModel.selectionID(offload: record.repo))
                    }
                } header: {
                    OffloadedHeader()
                }
            }
            .listStyle(.sidebar)
            .alert("Delete “\(model.deletingAgent?.name ?? "agent")”?", isPresented: Binding(get: { model.deletingAgent != nil }, set: { if !$0 { model.deletingAgent = nil } }), presenting: model.deletingAgent) { agent in
                Button("Delete", role: .destructive) { model.deleteAgent(agent) }
                Button("Cancel", role: .cancel) {}
            } message: { _ in
                Text("The agent and its conversation are removed from the box. This cannot be undone.")
            }
            if let error = model.lastError {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .lineLimit(3)
                    .padding(12)
                    .textSelection(.enabled)
            }
        }
    }

    // deletable is false for no selection, a local session, or an MCP agent whose session is still open.
    private var deletable: Bool {
        guard let agent = model.selectedAgent else { return false }
        return agent.externalClient == nil || (agent.status != .running && agent.status != .waitingForUser)
    }

    private func nameField(commit: @escaping () -> Void) -> some View {
        TextField("Name", text: $newName)
            .textFieldStyle(.roundedBorder)
            .focused($nameFocused)
            .onSubmit(commit)
            .onExitCommand { editingID = nil }
            .onChange(of: nameFocused) { if !nameFocused { commit() } }
    }

    private func startRename(_ agent: Agent) {
        newName = agent.name
        editingID = agent.id
        nameFocused = true
    }

    private func commitRename(_ agent: Agent) {
        guard editingID == agent.id else { return }
        editingID = nil
        let name = newName.trimmingCharacters(in: .whitespaces)
        if !name.isEmpty, name != agent.name { model.renameAgent(agent, to: name) }
    }

    private func startRename(_ session: LocalAgentSession) {
        newName = session.record.displayName
        editingID = AppModel.selectionID(session)
        nameFocused = true
    }

    private func commitRename(_ session: LocalAgentSession) {
        guard editingID == AppModel.selectionID(session) else { return }
        editingID = nil
        if newName != session.record.displayName { model.renameLocal(session, to: newName) }
    }
}

struct AgentRow: View {
    let agent: Agent

    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(color)
                .frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 2) {
                if let client = agent.externalClient {
                    Text(agent.name.dropFirst(4)).lineLimit(1)
                    Text("External · \(client)")
                        .font(.caption2.bold())
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(.teal.opacity(0.2), in: Capsule())
                        .foregroundStyle(.teal)
                        .lineLimit(1)
                } else {
                    Text(agent.name).lineLimit(1)
                    Text(agent.mainModel.split(separator: "/").last.map(String.init) ?? agent.mainModel)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
        }
    }

    private var color: Color {
        switch agent.status {
        case .running: .green
        case .waitingForUser: .orange
        case .failed: .red
        default: .secondary
        }
    }
}

// ShareSessionRow is a guest's MCP session through the share: who, which client, since when, and End.
struct ShareSessionRow: View {
    @Environment(AppModel.self) private var model
    let agent: Agent

    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(open ? Color.green : Color.secondary)
                .frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 2) {
                Text(agent.shareSession.map(model.guest(of:)) ?? agent.name).lineLimit(1)
                Text("\(agent.externalClient ?? "MCP") · \(since.formatted(date: .omitted, time: .shortened))")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .help("Connected since \(since.formatted(date: .abbreviated, time: .shortened))")
            }
            Spacer()
            if open {
                Button("End") { model.endSession(agent) }
                    .controlSize(.small)
                    .help("End this guest session")
            }
        }
    }

    private var open: Bool {
        agent.status == .running || agent.status == .waitingForUser
    }

    private var since: Date {
        Date(timeIntervalSince1970: Double(agent.createdAtMs) / 1000)
    }
}

// LocalAgentRow is a local agent with the boxes it acted on in the last
// minute, on a line kept when empty so the row height stays fixed.
// FailedLocalAgentRow is a local agent the app could not reopen, kept with its reason until the user removes it.
struct FailedLocalAgentRow: View {
    @Environment(AppModel.self) private var model
    let record: LocalAgentRecord

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle")
                .foregroundStyle(.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(record.displayName).lineLimit(1)
                Text("Could not reopen: \(record.failure ?? "unknown")")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .help(record.failure ?? "")
            }
        }
        .contextMenu {
            Button("Remove") { model.removeFailedLocal(record) }
        }
    }
}

struct LocalAgentRow: View {
    @Environment(AppModel.self) private var model
    let session: LocalAgentSession

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "terminal")
                .foregroundStyle(session.closed ? Color.secondary : Color.teal)
            VStack(alignment: .leading, spacing: 2) {
                Text(session.record.displayName).lineLimit(1)
                Text(Self.subtitle(session))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                TimelineView(.periodic(from: .now, by: 5)) { context in
                    let activity = Self.activity(model.recentBoxes(of: session, now: context.date))
                    Text(activity ?? " ")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .help(activity == nil ? "" : "Boxes this agent acted on in the last minute")
                }
            }
        }
    }

    static func subtitle(_ session: LocalAgentSession) -> String {
        session.closed ? "\(session.kind.title) · exited" : session.kind.title
    }

    static func activity(_ boxes: [Box]) -> String? {
        boxes.isEmpty ? nil : "↗ " + boxes.map(\.name).joined(separator: " · ")
    }
}

// DefaultBoxPicker sets the box of a local agent's calls that name none, from its next call on.
struct DefaultBoxPicker: View {
    @Environment(AppModel.self) private var model
    let title: String
    let session: LocalAgentSession

    var body: some View {
        Picker(title, selection: Binding(get: { session.record.boxID }, set: { id in
            if let box = model.boxes.first(where: { $0.id == id }) { model.setDefaultBox(session, to: box) }
        })) {
            ForEach(model.boxes, id: \.id) { box in
                Text(box.name).tag(box.id)
            }
        }
    }
}

// BoxSwitcher picks the box the screen, agents and lifecycle buttons follow, and creates, renames and deletes boxes.
struct BoxSwitcher: View {
    @Environment(AppModel.self) private var model
    @State private var name = ""

    var body: some View {
        @Bindable var model = model
        Menu {
            ForEach(model.boxes, id: \.id) { box in
                Button {
                    model.selectBox(box.id)
                } label: {
                    Label("\(box.name) · \(StateBadge.label(box.state).lowercased())", systemImage: box.id == model.selectedBoxID ? "checkmark" : "shippingbox")
                }
            }
            Divider()
            Button("New Box…") { model.showNewBox = true }
            Button("Rename Box…") {
                name = model.box?.name ?? ""
                model.renamingBox = model.box
            }
            .disabled(model.box == nil)
            Button("Delete Box…", role: .destructive) { model.deletingBox = model.box }
                .disabled(model.box == nil)
        } label: {
            HStack {
                Image(systemName: "shippingbox")
                VStack(alignment: .leading, spacing: 0) {
                    Text(model.box?.name ?? "No box").lineLimit(1)
                    if let box = model.box {
                        Text(box.id)
                            .font(.caption2.monospaced())
                            .foregroundStyle(.secondary)
                    }
                }
            }
        }
        .menuStyle(.borderlessButton)
        .disabled(model.connection == nil)
        .help("Switch, create, rename or delete boxes")
        .alert("New box", isPresented: $model.showNewBox) {
            TextField("Name", text: $name)
            if model.imageChoices.isEmpty {
                Button("Create") {
                    model.createBox(name: name)
                    name = ""
                }
            }
            ForEach(model.imageChoices, id: \.image) { choice in
                Button("Create \(choice.label)") {
                    model.createBox(name: name, image: choice.image)
                    name = ""
                }
            }
            Button("Cancel", role: .cancel) { name = "" }
        } message: {
            Text("A name of lowercase letters, digits, - or _. If another box runs, hibernate it first.")
        }
        .alert("Rename “\(model.renamingBox?.name ?? "box")”", isPresented: Binding(get: { model.renamingBox != nil }, set: { if !$0 { model.renamingBox = nil } }), presenting: model.renamingBox) { box in
            TextField("Name", text: $name)
            Button("Rename") { model.renameBox(box, to: name) }
            Button("Cancel", role: .cancel) {}
        }
        .alert("Delete “\(model.deletingBox?.name ?? "box")”?", isPresented: Binding(get: { model.deletingBox != nil }, set: { if !$0 { model.deletingBox = nil } }), presenting: model.deletingBox) { box in
            Button("Delete", role: .destructive) { model.deleteBox(box) }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("A running box is saved first. Its running programs stop, and its saved state is kept for 7 days, then removed.")
        }
    }
}

struct BoxHeader: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                StateBadge(state: model.box?.state)
                if model.eventsLive {
                    Text("live").font(.caption2).foregroundStyle(.green)
                }
                Spacer()
            }
            HStack {
                Button("Ensure") { model.ensure() }
                    .disabled(model.busy || model.boxRunning || model.box?.state == .starting)
                Button("Hibernate") { model.hibernate() }
                    .disabled(model.busy || !model.boxRunning)
            }
            .controlSize(.small)
            if let box = model.box {
                StateLayerControls(box: box)
            }
            if let operation = model.boxOperation {
                OperationView(progress: operation)
            }
            if let hello = model.hello, model.boxRunning {
                Text("agent \(hello.agentVersion) · \(hello.screenWidth)×\(hello.screenHeight)")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

// StateLayerControls shows when the user's data was last saved to object
// storage and offers a manual save and the desktop upgrade.
struct StateLayerControls: View {
    @Environment(AppModel.self) private var model
    let box: Box

    var body: some View {
        @Bindable var model = model
        let idle = !model.busy && model.boxOperation.map(\.finished) ?? true
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                TimelineView(.periodic(from: .now, by: 30)) { context in
                    Text(Self.saved(box.lastSavedAtMs, now: context.date))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Save now") { model.confirmSave = true }
                    .controlSize(.small)
                    .disabled(!idle || !model.boxRunning)
                    .help(model.wording.saveHelp)
            }
            if model.upgradeAvailable {
                Label("Update available: \(box.latestImage)", systemImage: "arrow.up.circle")
                    .font(.caption)
                    .foregroundStyle(.blue)
            }
            HStack {
                Button("Upgrade \(model.wording.noun)…") { model.confirmUpgrade = true }
                    .disabled(!idle || !model.upgradeAvailable)
                    .help(model.upgradeAvailable ? "Move this box to \(box.latestImage)" : "Already on the latest \(model.wording.noun)")
                Button("Restore files…") { model.showTimeCapsule = true }
                    .disabled(box.lastSavedAtMs == 0)
                    .help("Copy files and folders from an earlier save into ~/Restored")
            }
            .controlSize(.small)
        }
        .alert("Save your box now?", isPresented: $model.confirmSave) {
            Button("Save") { model.saveNow() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(model.wording.saveMessage)
        }
        .alert("Upgrade the \(model.wording.noun) to \(box.latestImage)?", isPresented: $model.confirmUpgrade) {
            Button("Upgrade") { model.upgrade() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Running programs restart. Your files, logins and agent history are kept.")
        }
    }

    static func saved(_ ms: Int64, now: Date) -> String {
        guard ms > 0 else { return "Not saved yet" }
        let minutes = Int(now.timeIntervalSince1970 - Double(ms) / 1000) / 60
        switch minutes {
        case ..<1: return "Saved just now"
        case ..<60: return "Saved \(minutes) min ago"
        case ..<(24 * 60): return "Saved \(minutes / 60) h ago"
        default: return "Saved \(Date(timeIntervalSince1970: Double(ms) / 1000).formatted(date: .abbreviated, time: .shortened))"
        }
    }
}

struct StateBadge: View {
    let state: BoxState?

    var body: some View {
        Text(label)
            .font(.caption.bold())
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(color.opacity(0.18), in: Capsule())
            .foregroundStyle(color)
    }

    private var label: String {
        Self.label(state)
    }

    static func label(_ state: BoxState?) -> String {
        switch state {
        case .starting: "STARTING"
        case .running: "RUNNING"
        case .hibernating: "HIBERNATING"
        case .hibernated: "HIBERNATED"
        case .failed: "FAILED"
        default: "ABSENT"
        }
    }

    private var color: Color {
        switch state {
        case .running: .green
        case .starting, .hibernating: .orange
        case .failed: .red
        default: .secondary
        }
    }
}

struct OperationView: View {
    let progress: OperationProgress
    @State private var expanded = false

    var body: some View {
        let folded = progress.finished && !progress.failed && !expanded
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                if !progress.finished {
                    ProgressView().controlSize(.mini)
                } else {
                    Image(systemName: progress.failed ? "xmark.octagon" : "checkmark.circle")
                        .foregroundStyle(progress.failed ? .red : .green)
                }
                Text(progress.title).font(.caption.bold())
                if progress.finished && !progress.failed {
                    Spacer()
                    Image(systemName: expanded ? "chevron.down" : "chevron.right")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            .contentShape(Rectangle())
            .onTapGesture { if progress.finished { expanded.toggle() } }
            ForEach(Array((folded ? [] : steps).enumerated()), id: \.offset) { _, event in
                HStack(alignment: .firstTextBaseline, spacing: 4) {
                    Text(event.phase.label)
                        .font(.caption2.monospaced())
                        .foregroundStyle(.secondary)
                    Text(event.detail).font(.caption2)
                }
            }
            if !progress.finished, let transfer = progress.events.last, transfer.totalBytes > 0 {
                ProgressView(value: Double(transfer.doneBytes), total: Double(transfer.totalBytes))
                    .controlSize(.small)
                Text("\(transfer.doneBytes.formatted(.byteCount(style: .file))) of \(transfer.totalBytes.formatted(.byteCount(style: .file)))")
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
        .padding(8)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
    }

    // steps keeps the latest event of each run of one phase.
    private var steps: [OperationEvent] {
        progress.events.enumerated().filter { index, event in
            index == progress.events.count - 1 || progress.events[index + 1].phase != event.phase
        }.map(\.element)
    }
}

extension Cocoonbox_Control_V1_Phase {
    var label: String {
        switch self {
        case .creating: "create"
        case .waking: "wake"
        case .snapshotting: "snapshot"
        case .saving: "save"
        case .restoring: "restore"
        case .restoringFiles: "restore files"
        case .deleting: "delete"
        case .done: "done"
        case .failed: "failed"
        default: "…"
        }
    }
}

struct SignInSheet: View {
    @Environment(AppModel.self) private var model
    @State private var token = ""
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Sign in to CocoonBox").font(.title2.bold())
            if model.browserSignIn != nil {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Waiting for the browser: allow this Mac on the console page it opened.")
                    Spacer()
                    Button("Cancel") { model.cancelBrowserSignIn() }
                }
                .font(.callout)
            } else {
                Button("Sign in with browser") {
                    error = model.signInWithBrowser()
                }
                .buttonStyle(.borderedProminent)
            }
            if let failed = model.browserSignInError {
                Text(failed).foregroundStyle(.red).font(.callout)
            }
            Divider()
            Text("Or paste a token. It is stored in your login Keychain.")
                .font(.callout)
                .foregroundStyle(.secondary)
            SecureField("Token", text: $token)
                .textFieldStyle(.roundedBorder)
            if let endpoint = Endpoint(url: model.endpointURL), let console = URL(string: endpoint.url + "/cb/") {
                HStack(spacing: 4) {
                    Text("Get your token at")
                    Link(console.absoluteString, destination: console)
                }
                .font(.callout)
            }
            if let error {
                Text(error).foregroundStyle(.red).font(.callout)
            }
            HStack {
                Spacer()
                Button("Sign In") {
                    error = model.signIn(token: token)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(token.isEmpty)
            }
        }
        .padding(24)
        .frame(width: 520)
    }
}
