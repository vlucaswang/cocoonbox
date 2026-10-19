import AppKit
import CocoonBoxKit
import CocoonBoxTerminal
import Foundation
import Observation
import SystemConfiguration

typealias Box = Cocoonbox_Control_V1_Box
typealias BoxState = Cocoonbox_Control_V1_BoxState
typealias OperationEvent = Cocoonbox_Control_V1_OperationEvent
typealias Agent = Cocoonbox_Agent_V1_Agent
typealias Entry = Cocoonbox_Agent_V1_TranscriptEntry
typealias ComputerAction = Cocoonbox_Agent_V1_ComputerAction
typealias Share = Cocoonbox_Control_V1_Share
typealias BoxSnapshot = Cocoonbox_Control_V1_Snapshot
typealias SnapshotEntry = Cocoonbox_Control_V1_SnapshotEntry

// LiveBlock is an assistant text or reasoning block still streaming as deltas.
struct LiveBlock: Equatable {
    var turnID: String
    var block: UInt32
    var reasoning: Bool
    var subagent: String
    var text: String
}

// CapsuleBrowser is the time capsule sheet's view of one box: its snapshots and one directory of the chosen one.
struct CapsuleBrowser: Equatable {
    var snapshots: [BoxSnapshot] = []
    var snapshotID: String?
    var dir = ""
    var entries: [SnapshotEntry] = []
    var truncated = false
}

struct OperationProgress: Equatable {
    var id: String
    var boxID: String
    var title: String
    var events: [OperationEvent] = []
    var finished = false
    var failed = false
    var replacing = false

    var status: String {
        "\(title) · \(events.last?.phase.label ?? "…")"
    }
}

@MainActor @Observable
final class AppModel {
    let endpointURL: String
    var showSignIn = false
    var showConnectAgent = false
    var showShare = false
    var showTimeCapsule = false
    var showPasswordSignIn = false
    var confirmSave = false
    var confirmUpgrade = false
    var deletingAgent: Agent?
    var showNewBox = false
    var imageChoices: [ImageChoice] = []
    var renamingBox: Box?
    var deletingBox: Box?
    // boxes is oldest first, as the api lists them; the screen, agents and lifecycle follow the selected one.
    var boxes: [Box] = []
    var selectedBoxID: String?
    var operation: OperationProgress?
    var agents: [Agent] = []
    var selectedAgentID: String? {
        didSet { if selectedAgentID != oldValue { loadSelectedTranscript() } }
    }
    var transcripts: [String: [Entry]] = [:]
    var live: [String: [LiveBlock]] = [:]
    var hello: Cocoonbox_Agent_V1_HelloResponse?
    // automations keeps each box's automation from its last Hello, so a hibernated phone keeps its wording.
    var automations: [String: Cocoonbox_Agent_V1_Automation] = [:]
    var eventsLive = false
    var lastError: String?
    var busy = false
    var answeredApprovals: Set<String> = []
    var localAgents: [LocalAgentSession] = []
    // failedLocal is the local agents that did not reopen; the session file keeps them until the user removes them.
    var failedLocal: [LocalAgentRecord] = []
    // localCalls is each local agent's last box-tool call per box id.
    var localCalls: [UUID: [String: Date]] = [:]
    var share: Share?
    // browserSignIn is the sign-in through the browser while it waits for the console's answer.
    var browserSignIn: Task<Void, Never>?
    var browserSignInError: String?
    var connectors: [Cocoonbox_Control_V1_Connector] = []
    var capsule = CapsuleBrowser()
    // offloads is the offloads in flight, from the records on this Mac, oldest first.
    var offloads: [OffloadRecord] = []
    // offloadActivity is each repo key's box CLI status and last activity, from `offload status` and offload events.
    var offloadActivity: [String: OffloadActivity] = [:]
    var boxTerminals: [String: RemoteTerminal] = [:]
    var boxCLISheet: BoxCLISheet?
    var busyWarning: BusyWarning?
    var pluginSetup: PluginSetup?
    @ObservationIgnored var attachTasks: [String: Task<Void, Never>] = [:]
    @ObservationIgnored var computerActions = 0
    @ObservationIgnored private var loadedTranscripts: Set<String> = []
    @ObservationIgnored private var restoredLocal = false
    // pendingLocal is the session file's records until the local agents reopen.
    @ObservationIgnored private lazy var pendingLocal: [LocalAgentRecord] = LocalAgentStore.load()
    @ObservationIgnored private var link: AgentLinkServer?

    let screen = ScreenController()

    @ObservationIgnored private(set) var connection: BoxConnection?
    @ObservationIgnored private var boxesTask: Task<Void, Never>?
    @ObservationIgnored private var boxesLive = false
    @ObservationIgnored private var eventsTask: Task<Void, Never>?
    @ObservationIgnored private var operationTask: Task<Void, Never>?
    @ObservationIgnored private var settleTask: Task<Void, Never>?

    init() {
        endpointURL = ProcessInfo.processInfo.environment["CB_ENDPOINT"] ?? Endpoint.configuredURL ?? ""
        listen()
        NotificationCenter.default.addObserver(forName: NSApplication.willTerminateNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.persistLocal()
                self?.closeAllLocal()
                self?.link?.close()
            }
        }
        NotificationCenter.default.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.connection != nil else { return }
                Task {
                    await self.refreshOffloads()
                    await self.refreshConnectors()
                }
            }
        }
    }

    var selectedAgent: Agent? {
        agents.first { $0.id == selectedAgentID }
    }

    var box: Box? {
        boxes.first { $0.id == selectedBoxID }
    }

    // boxOperation is the operation on the selected box, the one the sidebar shows.
    var boxOperation: OperationProgress? {
        operation.flatMap { $0.boxID == selectedBoxID ? $0 : nil }
    }

    var boxRunning: Bool {
        box?.state == .running
    }

    // replacing is the unfinished upgrade or restore that moves the selected box to a new machine.
    var replacing: OperationProgress? {
        boxOperation.flatMap { $0.replacing && !$0.finished ? $0 : nil }
    }

    // agentsReady is whether agents can start or prompt: the box runs and no operation replaces its machine.
    var agentsReady: Bool {
        boxRunning && replacing == nil
    }

    var mcpSetup: MCPSetup? {
        guard let box, let endpoint = Endpoint(url: endpointURL) else { return nil }
        return MCPSetup(endpoint: endpoint, boxID: box.id)
    }

    var selectedLocal: LocalAgentSession? {
        localAgents.first { Self.selectionID($0) == selectedAgentID }
    }

    static func selectionID(_ session: LocalAgentSession) -> String {
        "local:\(session.id)"
    }

    // bridge is the cocoonbox CLI that serves a local agent its box: in the app bundle, or next to a development build.
    static let bridge: String? = {
        let bundled = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/cocoonbox")
        let built = Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent("cocoonbox-cli")
        return [bundled, built].compactMap { $0 }.first { FileManager.default.isExecutableFile(atPath: $0.path) }?.path
    }()

    private func localTarget(_ boxID: String) -> MCPTarget? {
        guard let bridge = Self.bridge, let endpoint = Endpoint(url: endpointURL), let token = Keychain.token(for: endpoint) else { return nil }
        return MCPTarget(bridge: bridge, box: boxID, endpoint: endpoint.url, token: token, socket: link?.path)
    }

    // listen opens the socket on which the local agents' bridges report their
    // calls and take their default box; without it they keep their --box.
    private func listen() {
        let path = LocalAgentStore.defaultURL.deletingLastPathComponent().appendingPathComponent("agents.sock").path
        do {
            try FileManager.default.createDirectory(atPath: (path as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
            link = try AgentLinkServer(
                path: path,
                defaultBox: { [weak self] agent in MainActor.assumeIsolated { self?.localAgents.first { $0.id.uuidString == agent }?.record.boxID } },
                onCall: { [weak self] agent, box, at in MainActor.assumeIsolated { self?.noteCall(agent: agent, box: box, at: at) } }
            )
            Perf.mark("agent_link", path)
        } catch {
            Perf.mark("agent_link", "off: \(error)")
        }
    }

    // recentCall is how long a box badge stays on a local agent's row after its last call there (design.md §19.6).
    static let recentCall: TimeInterval = 60

    func defaultBox(of session: LocalAgentSession) -> Box? {
        boxes.first { $0.id == session.record.boxID }
    }

    // recentBoxes is the boxes session called in the last recentCall seconds.
    func recentBoxes(of session: LocalAgentSession, now: Date = Date()) -> [Box] {
        let calls = localCalls[session.id] ?? [:]
        return boxes.filter { box in calls[box.id].map { now.timeIntervalSince($0) < Self.recentCall } ?? false }
    }

    // setDefaultBox moves session's calls that name no box to box, from its next call on.
    func setDefaultBox(_ session: LocalAgentSession, to box: Box) {
        session.setDefaultBox(box.id)
        persistLocal()
        link?.push(agent: session.id.uuidString, box: box.id)
        Perf.mark("local_agent", "default \(session.kind.rawValue) \(box.name)")
    }

    // noteCall records a local agent's box-tool call.
    private func noteCall(agent: String, box: String, at: Date) {
        guard let session = localAgents.first(where: { $0.id.uuidString == agent }) else { return }
        localCalls[session.id, default: [:]][box] = at
    }

    // launchLocal starts an agent CLI on this Mac whose MCP server is the bridge, with the selected box as its default.
    func launchLocal(_ kind: LocalAgentKind) {
        guard let box, let target = localTarget(box.id) else {
            lastError = "local agent: sign in and start the box first; the cocoonbox CLI must sit next to the app"
            return
        }
        Task {
            do {
                let session = try await LocalAgentSession.start(kind, target: target, boxID: box.id)
                add([session])
                selectedAgentID = Self.selectionID(session)
                Perf.mark("local_agent", "started \(kind.rawValue)")
            } catch {
                report("local agent", error)
            }
        }
    }

    // restoreLocal reopens, once per launch, every local agent the session file
    // keeps; one whose default box is gone takes the selected box. One that
    // fails to reopen stays in the file and in failedLocal, with its reason.
    private func restoreLocal() {
        guard !restoredLocal, let box, let base = localTarget(box.id) else { return }
        restoredLocal = true
        let records = pendingLocal.map { record in
            var record = record
            if !boxes.contains(where: { $0.id == record.boxID }) { record.boxID = box.id }
            return record
        }
        Task {
            let (sessions, failed) = await LocalAgentStore.reopen(records) { record in
                let target = MCPTarget(bridge: base.bridge, box: record.boxID, endpoint: base.endpoint, token: base.token, socket: base.socket)
                let session = try await LocalAgentSession.start(record.kind, target: target, boxID: record.boxID, restoring: record)
                Perf.mark("local_agent", "reopened \(record.kind.rawValue)")
                return session
            }
            for record in failed {
                Perf.mark("local_agent", "reopen failed \(record.kind.rawValue)")
                lastError = "reopen local agent: \(record.failure ?? "")"
            }
            pendingLocal = []
            failedLocal = failed
            add(sessions)
        }
    }

    // removeFailedLocal drops a local agent that did not reopen from the session file.
    func removeFailedLocal(_ record: LocalAgentRecord) {
        failedLocal.removeAll { $0.id == record.id }
        persistLocal()
    }

    private func add(_ sessions: [LocalAgentSession]) {
        for session in sessions {
            if session.record.sessionID == nil {
                localAgents.filter { $0.kind == session.kind }.forEach { $0.until(session.startedAt) }
            }
            session.onTitle = { [weak self] in self?.persistLocal() }
            localAgents.append(session)
        }
        persistLocal()
        settleLocal()
    }

    // settleLocal saves each new Codex or Cursor session's id and name once they are on disk, so a killed app still resumes it.
    private func settleLocal() {
        guard settleTask == nil else { return }
        settleTask = Task { [weak self] in
            while let self, self.localAgents.contains(where: { $0.record.sessionID == nil && !$0.closed }) {
                try? await pause(.seconds(3))
                let before = self.localAgents.map(\.record)
                self.localAgents.forEach { $0.settle() }
                if self.localAgents.map(\.record) != before { self.persistLocal() }
            }
            self?.settleTask = nil
        }
    }

    // persistLocal writes the open local agents to the session file, with the session ids Codex and Cursor have chosen by now.
    func persistLocal() {
        localAgents.forEach { $0.settle() }
        do {
            try LocalAgentStore.save(localAgents.map(\.record) + failedLocal + pendingLocal)
        } catch {
            report("save local agents", error)
        }
    }

    func renameLocal(_ session: LocalAgentSession, to name: String) {
        session.rename(name)
        persistLocal()
    }

    func closeAllLocal() {
        localAgents.forEach { $0.close(immediately: true) }
        localAgents = []
    }

    func closeLocal(_ session: LocalAgentSession) {
        session.close()
        localAgents.removeAll { $0 === session }
        localCalls[session.id] = nil
        persistLocal()
        if selectedAgentID == Self.selectionID(session) {
            selectedAgentID = agents.last { $0.externalClient == nil }?.id
        }
        Perf.mark("local_agent", "closed \(session.kind.rawValue)")
    }

    // externalAgents is the open sessions of guests through a share, plus the selected one.
    func externalAgents() -> [Agent] {
        agents.filter { agent in
            guard agent.shareSession != nil else { return false }
            return agent.id == selectedAgentID || agent.status == .running || agent.status == .waitingForUser
        }
    }

    // guest names who uses a share session: "link" for an open share, else the user's name, or its id once the share is gone.
    func guest(of session: ShareSession) -> String {
        guard let user = session.userID else { return "link" }
        return share?.users.first { $0.id == user }?.name ?? user
    }

    @discardableResult
    func copyMCP(_ snippet: MCPSetup.Snippet) -> Bool {
        guard let setup = mcpSetup else { return false }
        copy(setup.text(snippet))
        Perf.mark("mcp_copy", snippet.rawValue)
        return true
    }

    // shareSecret is nil for a share created before the api kept secrets.
    var shareSecret: String? {
        guard let secret = share?.secret, !secret.isEmpty else { return nil }
        return secret
    }

    var shareSetup: ShareSetup? {
        guard let box, let share, let shareSecret, let endpoint = Endpoint(url: endpointURL) else { return nil }
        return ShareSetup(endpoint: endpoint, boxID: box.id, secret: shareSecret, named: !share.users.isEmpty)
    }

    @discardableResult
    func copyShare(_ snippet: ShareSetup.Snippet) -> Bool {
        guard let setup = shareSetup else { return false }
        copy(setup.text(snippet))
        Perf.mark("share_copy", snippet.rawValue)
        return true
    }

    func copy(_ text: String) {
        screen.pasteboard.clearContents()
        screen.pasteboard.setString(text, forType: .string)
    }

    // MARK: sharing

    func loadShare() async {
        guard let connection, let box else { return }
        do {
            share = try await connection.listShares().first { $0.boxID == box.id }
        } catch {
            report("list shares", error)
        }
    }

    func createShare(ttlSeconds: Int64, userNames: [String], write: Bool) async {
        guard let connection, let box else { return }
        do {
            let created = try await connection.createShare(boxID: box.id, ttlSeconds: ttlSeconds, userNames: userNames, write: write)
            share = created.share
            markShare("created")
        } catch {
            report("share", error)
            await loadShare()
        }
    }

    func setShareWrite(_ write: Bool) async {
        guard let connection, let current = share else { return }
        do {
            share = try await connection.setShareWrite(shareID: current.id, write: write)
            markShare("write")
        } catch {
            report("share write", error)
        }
    }

    func regenerateShare() async {
        guard let connection, let current = share else { return }
        do {
            let fresh = try await connection.regenerateShare(shareID: current.id)
            share = fresh.share
            markShare("regenerated")
        } catch {
            report("regenerate share", error)
        }
    }

    func revokeShare() async {
        guard let connection, let current = share else { return }
        do {
            try await connection.revokeShare(shareID: current.id)
            share = nil
            markShare("revoked")
        } catch {
            report("revoke share", error)
        }
    }

    func endSession(_ agent: Agent) {
        guard let connection else { return }
        Task {
            do {
                try await connection.endSession(agentID: agent.id)
                Perf.mark("session_end", "agent=\(agent.id)")
            } catch {
                report("end session", error)
            }
        }
    }

    // markShare logs the share's state; the secret appears only as its length.
    private func markShare(_ what: String) {
        Perf.mark("share", "\(what) id=\(share?.id ?? "-") write=\(share?.write ?? false) users=\(share?.users.map(\.name).joined(separator: ",") ?? "-") expires_in_s=\(share.map { ($0.expiresAtMs - Int64(Date().timeIntervalSince1970 * 1000)) / 1000 } ?? 0) secret_len=\(shareSecret?.count ?? 0)")
    }

    // MARK: time capsule

    static let restoreTitle = "Restoring files"

    // openCapsule lists the selected box's snapshots and shows the newest one's roots.
    func openCapsule() async {
        guard let connection, let box else { return }
        do {
            let snapshots = try await connection.listSnapshots(boxID: box.id)
            capsule = CapsuleBrowser(snapshots: snapshots)
            Perf.mark("capsule", "snapshots=\(snapshots.count) changed=\(snapshots.map { $0.hasChangedFiles ? String($0.changedFiles) : "-" }.joined(separator: ","))")
            if let newest = snapshots.first { await browse(snapshot: newest.id, dir: "") }
        } catch {
            report("list snapshots", error)
        }
    }

    func browse(snapshot id: String, dir: String) async {
        guard let connection, let box else { return }
        do {
            let listing = try await connection.browseSnapshot(boxID: box.id, snapshotID: id, dir: dir)
            capsule.snapshotID = id
            capsule.dir = dir
            capsule.entries = listing.entries.sorted { ($0.kind == .dir ? 0 : 1, $0.path) < ($1.kind == .dir ? 0 : 1, $1.path) }
            capsule.truncated = listing.truncated
            Perf.mark("capsule_browse", "dir=\(dir.isEmpty ? "/" : dir) entries=\(listing.entries.count) truncated=\(listing.truncated)")
        } catch {
            report("browse snapshot", error)
        }
    }

    func restoreFiles(_ paths: [String]) {
        guard let connection, let box, let id = capsule.snapshotID, !paths.isEmpty else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                let op = try await connection.restorePaths(boxID: box.id, snapshotID: id, paths: paths)
                watch(operation: op.id, box: box.id, title: Self.restoreTitle)
            } catch {
                report("restore files", error)
            }
        }
    }

    // MARK: session

    func start() {
        if ProcessInfo.processInfo.environment["CB_TOKEN_STDIN"] == "1", let endpoint = Endpoint(url: endpointURL),
           let line = readLine(strippingNewline: true), !line.isEmpty {
            do {
                try Keychain.setToken(line, for: endpoint)
                Perf.mark("keychain_import", endpoint.url)
            } catch {
                report("keychain import", error)
            }
        }
        Perf.mark("keychain_read_start")
        let token = Endpoint(url: endpointURL).flatMap { Keychain.token(for: $0) }
        Perf.mark("keychain_read_done")
        guard let endpoint = Endpoint(url: endpointURL), let token else {
            showSignIn = true
            return
        }
        connect(endpoint: endpoint, token: token)
    }

    func signIn(token: String) -> String? {
        guard let endpoint = Endpoint(url: endpointURL) else { return "Endpoint must be an http(s) URL" }
        guard endpoint.usesTLS || endpoint.isLoopback else { return "Use https: over http the token would travel in plaintext" }
        let token = token.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !token.isEmpty else { return "Token is empty" }
        do {
            try Keychain.setToken(token, for: endpoint)
        } catch {
            return "Keychain: \(error.localizedDescription)"
        }
        UserDefaults.standard.removeObject(forKey: Self.browserTokenKey(endpoint))
        showSignIn = false
        connect(endpoint: endpoint, token: token)
        return nil
    }

    // signInWithBrowser opens the console's consent page and, once the user allows this Mac, signs in with a device token of the app's own.
    func signInWithBrowser() -> String? {
        guard let endpoint = Endpoint(url: endpointURL) else { return "Endpoint must be an http(s) URL" }
        guard endpoint.usesTLS || endpoint.isLoopback else { return "Use https: over http the token would travel in plaintext" }
        browserSignInError = nil
        let login = BrowserLogin(endpoint: endpoint, client: BrowserLogin.appClient,
                                 deviceName: SCDynamicStoreCopyComputerName(nil, nil).map { $0 as String } ?? BrowserLogin.hostName())
        browserSignIn = Task {
            defer { browserSignIn = nil }
            do {
                let token = try await login.run { url in
                    Task { @MainActor in NSWorkspace.shared.open(url) }
                }
                do {
                    try Keychain.setToken(token, for: endpoint)
                } catch {
                    try? await BrowserLogin.revoke(token: token, endpoint: endpoint, client: BrowserLogin.appClient)
                    browserSignInError = "Keychain: \(error.localizedDescription)"
                    return
                }
                UserDefaults.standard.set(true, forKey: Self.browserTokenKey(endpoint))
                showSignIn = false
                connect(endpoint: endpoint, token: token)
            } catch is CancellationError {
                return
            } catch {
                browserSignInError = "\(error)"
            }
        }
        return nil
    }

    func cancelBrowserSignIn() {
        browserSignIn?.cancel()
    }

    func signOut() {
        if let endpoint = Endpoint(url: endpointURL) {
            if UserDefaults.standard.bool(forKey: Self.browserTokenKey(endpoint)), let token = Keychain.token(for: endpoint) {
                Task { try? await BrowserLogin.revoke(token: token, endpoint: endpoint, client: BrowserLogin.appClient) }
            }
            UserDefaults.standard.removeObject(forKey: Self.browserTokenKey(endpoint))
            Keychain.deleteToken(for: endpoint)
        }
        disconnect()
        boxes = []
        selectedBoxID = nil
        share = nil
        connectors = []
        showSignIn = true
    }

    // browserTokenKey marks that the app's token for endpoint is a device token it got through the browser, which sign-out revokes.
    private static func browserTokenKey(_ endpoint: Endpoint) -> String {
        "browserToken." + endpoint.url
    }

    private func connect(endpoint: Endpoint, token: String) {
        disconnect()
        do {
            connection = try BoxConnection(endpoint: endpoint, token: token)
        } catch {
            lastError = "connect: \(error)"
            return
        }
        Perf.mark("connect", endpoint.url)
        guard let connection else { return }
        boxesTask = Task {
            await connection.watchBoxes(
                onBoxes: { list, offered in await self.received(boxes: list, offered: offered) },
                onDisconnect: { error in await self.boxesDropped(error) }
            )
        }
    }

    private func disconnect() {
        stopAgentSide()
        operationTask?.cancel()
        boxesTask?.cancel()
        boxesTask = nil
        boxesLive = false
        connection?.close()
        connection = nil
    }

    // received takes a list from Boxes.Watch; the first one of a connection also loads the offloads and connectors.
    private func received(boxes list: [Box], offered: [String]) async {
        imageChoices = ImageChoice.choices(offered)
        apply(boxes: list)
        guard !boxesLive else { return }
        boxesLive = true
        lastError = nil
        await refreshOffloads()
        await refreshConnectors()
    }

    // boxesDropped reports a watch that never delivered a list, such as one refused for a bad token; the watch resubscribes.
    private func boxesDropped(_ error: any Error) {
        Perf.mark("boxes_dropped", "\(error)")
        if !boxesLive { report("watch boxes", error) }
    }

    // reconnect names the connected accounts whose grant the vendor refused (design §16.4).
    var reconnect: [String] {
        connectors.filter { $0.status == "needs_reconnect" }.map(\.name)
    }

    // refreshConnectors keeps the last list when the call fails: Connect is optional, and an older api lacks it.
    func refreshConnectors() async {
        guard let connection, let list = try? await connection.listConnectors() else { return }
        connectors = list
    }

    func openConnections() {
        guard let endpoint = Endpoint(url: endpointURL), let url = URL(string: endpoint.url + "/cb/connect") else { return }
        NSWorkspace.shared.open(url)
    }

    // apply takes the box list; without a selection, or when the selected box is gone, it selects the box created again
    // under its name, else the newest running box, else the first. Local agents follow their default box the same way.
    private func apply(boxes list: [Box]) {
        defer { restoreLocal() }
        let wasRunning = boxRunning
        let renewed = Box.renewed(from: boxes, in: list)
        boxes = list
        for session in localAgents {
            if let again = renewed[session.record.boxID] { setDefaultBox(session, to: again) }
        }
        guard box == nil else { return followSelected(wasRunning: wasRunning) }
        if let pick = selectedBoxID.flatMap({ renewed[$0] }) ?? list.last(where: { $0.state == .running }) ?? list.first {
            selectBox(pick.id)
        } else {
            stopAgentSide()
            selectedBoxID = nil
        }
    }

    private func apply(box newBox: Box) {
        let wasRunning = boxRunning
        if let index = boxes.firstIndex(where: { $0.id == newBox.id }) {
            boxes[index] = newBox
        } else {
            boxes.append(newBox)
        }
        if newBox.id == selectedBoxID { followSelected(wasRunning: wasRunning) }
    }

    private func followSelected(wasRunning: Bool) {
        Perf.mark("box_state", "\(box?.id ?? "-") \(box?.state ?? .unspecified)")
        if boxRunning, !wasRunning || eventsTask == nil {
            startAgentSide()
        } else if !boxRunning, wasRunning {
            stopAgentSide()
        }
    }

    // selectBox moves the screen, the in-box agents and the lifecycle buttons to box id; a selected local agent stays selected.
    func selectBox(_ id: String) {
        guard id != selectedBoxID else { return }
        stopAgentSide()
        agents = []
        transcripts = [:]
        live = [:]
        loadedTranscripts = []
        hello = nil
        share = nil
        selectedBoxID = id
        selectedAgentID = selectedLocal.map(Self.selectionID) ?? selectedOffload.map { Self.selectionID(offload: $0.repo) }
        Perf.mark("box_select", "\(id) \(box?.name ?? "-") \(box?.state ?? .unspecified)")
        if boxRunning { startAgentSide() }
    }

    // MARK: boxes

    func createBox(name: String, image: String = "") {
        guard let connection else { return }
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        busy = true
        Task {
            defer { busy = false }
            do {
                let response = try await connection.createBox(name: name, image: image)
                apply(box: response.box)
                selectBox(response.box.id)
                if response.hasOperation, !response.operation.id.isEmpty {
                    watch(operation: response.operation.id, box: response.box.id, title: "Creating \(name)")
                }
            } catch {
                report("create box", error)
            }
        }
    }

    func renameBox(_ target: Box, to name: String) {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let connection, !name.isEmpty, name != target.name else { return }
        Task {
            do {
                apply(box: try await connection.renameBox(boxID: target.id, name: name))
            } catch {
                report("rename box", error)
            }
        }
    }

    func deleteBox(_ target: Box) {
        guard let connection else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                let op = try await connection.deleteBox(boxID: target.id)
                watch(operation: op.id, box: target.id, title: "Deleting \(target.name)")
            } catch {
                report("delete box", error)
            }
        }
    }

    // MARK: box lifecycle

    func ensure() {
        guard let connection else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                Perf.mark("ensure_start")
                let response = try await connection.ensure(boxID: box?.id ?? "")
                apply(box: response.box)
                if selectedBoxID == nil { selectBox(response.box.id) }
                if response.hasOperation, !response.operation.id.isEmpty {
                    watch(operation: response.operation.id, box: response.box.id, title: response.box.state == .starting ? "Starting box" : "Ensure")
                }
            } catch {
                report("ensure", error)
            }
        }
    }

    var upgradeAvailable: Bool {
        guard let box, !box.latestImage.isEmpty else { return false }
        return box.latestImage != box.image
    }

    func saveNow() {
        guard let connection, let box else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                let op = try await connection.saveState(boxID: box.id)
                watch(operation: op.id, box: box.id, title: "Saving")
            } catch {
                report("save", error)
            }
        }
    }

    func upgrade() {
        guard let box, upgradeAvailable else { return }
        warnIfBusy(box, title: "Upgrade while a box agent works?", consequence: "The upgrade restarts it from its last hand-over.") { self.upgradeNow() }
    }

    private func upgradeNow() {
        guard let connection, let box, upgradeAvailable else { return }
        busy = true
        let target = box.latestImage
        Task {
            defer { busy = false }
            do {
                let op = try await connection.rebase(boxID: box.id, image: target)
                watch(operation: op.id, box: box.id, title: "Upgrading to \(target)", replacing: true)
            } catch {
                report("upgrade", error)
            }
        }
    }

    func hibernate() {
        guard let box else { return }
        warnIfBusy(box, title: "Hibernate while a box agent works?", consequence: "Hibernating pauses it until the box wakes.") { self.hibernateNow() }
    }

    private func hibernateNow() {
        guard let connection, let box else { return }
        busy = true
        stopAgentSide()
        Task {
            defer { busy = false }
            do {
                let op = try await connection.hibernate(boxID: box.id)
                watch(operation: op.id, box: box.id, title: "Hibernating")
            } catch {
                report("hibernate", error)
            }
        }
    }

    private func watch(operation id: String, box boxID: String, title: String, replacing: Bool = false) {
        operationTask?.cancel()
        operation = OperationProgress(id: id, boxID: boxID, title: title, replacing: replacing)
        if replacing { screen.takeOver = false }
        operationTask = Task { [weak self] in
            var attempt = 0
            let backoff = Backoff()
            while !Task.isCancelled {
                guard let self, let connection = self.connection else { return }
                let from = self.operation?.events.last?.offsetKey ?? ""
                do {
                    try await connection.watchOperation(id, after: from) { event in
                        await self.record(operationEvent: event)
                    }
                } catch {
                    if Task.isCancelled { return }
                    Perf.mark("watch_operation_error", "\(error)")
                }
                if self.operation?.finished ?? true { return }
                try? await pause(backoff.delay(attempt: attempt))
                attempt += 1
            }
        }
    }

    private func record(operationEvent event: OperationEvent) async {
        guard var progress = operation, progress.id == event.operationID else { return }
        progress.events.append(event)
        progress.replacing = progress.replacing || event.phase == .restoring
        Perf.mark("operation", "\(event.phase) \(event.detail) box=\(event.boxState) bytes=\(event.doneBytes)/\(event.totalBytes)")
        if event.phase == .done || event.phase == .failed {
            progress.finished = true
            progress.failed = event.phase == .failed
        }
        operation = progress
        if replacing != nil { screen.takeOver = false }
        if var current = boxes.first(where: { $0.id == progress.boxID }), event.boxState != .unspecified {
            current.state = event.boxState
            apply(box: current)
        }
    }

    // MARK: agent side

    private func startAgentSide() {
        guard let connection, let box else { return }
        connection.boxID = box.id
        eventsTask?.cancel()
        eventsTask = Task {
            await loadHello()
            await connection.followEvents(
                onEvent: { event in await self.handle(event: event) },
                onResync: { await self.resync() },
                onDisconnect: { error in await self.eventsDropped(error) }
            )
        }
        screen.start(connection)
        Task { await loadShare() }
    }

    private func stopAgentSide() {
        eventsTask?.cancel()
        eventsTask = nil
        eventsLive = false
        screen.stop()
    }

    var wording: BoxWording { wording(box) }

    func wording(_ box: Box?) -> BoxWording {
        BoxWording(box.flatMap { automations[$0.id] })
    }

    private func loadHello() async {
        guard let connection else { return }
        let boxID = selectedBoxID
        for attempt in 0..<8 {
            do {
                hello = try await connection.hello(clientVersion: "cocoonbox-macos/0.1")
                if let boxID, let hello { automations[boxID] = hello.automation }
                Perf.mark("hello", "agent=\(hello?.agentVersion ?? "") screen=\(hello?.screenWidth ?? 0)x\(hello?.screenHeight ?? 0)")
                return
            } catch {
                report("hello", error)
                try? await pause(Backoff().delay(attempt: attempt))
            }
        }
    }

    // eventsDropped forgets streaming blocks: deltas are not replayed, so a
    // block whose entry never arrives would otherwise stay half-written.
    private func eventsDropped(_ error: any Error) {
        eventsLive = false
        live = [:]
        Perf.mark("events_dropped", "\(error)")
    }

    private func resync() async {
        guard let connection else { return }
        do {
            agents = try await connection.listAgents().sorted { $0.createdAtMs < $1.createdAtMs }
            live = [:]
            selectedAgentID = Agent.resynced(selectedAgentID, local: selectedLocal != nil || selectedOffload != nil, in: agents)
            loadedTranscripts = []
            if let id = selectedTranscriptID {
                loadedTranscripts.insert(id)
                if !(await loadTranscript(agentID: id)) { loadedTranscripts.remove(id) }
            }
            eventsLive = true
            lastError = nil
        } catch {
            report("list agents", error)
        }
    }

    private var selectedTranscriptID: String? {
        agents.contains { $0.id == selectedAgentID } ? selectedAgentID : nil
    }

    private func loadSelectedTranscript() {
        guard eventsLive, let id = selectedTranscriptID, !loadedTranscripts.contains(id) else { return }
        loadedTranscripts.insert(id)
        Task { if !(await loadTranscript(agentID: id)) { loadedTranscripts.remove(id) } }
    }

    @discardableResult
    private func loadTranscript(agentID: String) async -> Bool {
        guard let connection else { return false }
        var after: UInt64 = 0
        while true {
            do {
                let page = try await connection.transcript(agentID: agentID, after: after, limit: 200)
                for entry in page.entries {
                    upsert(entry)
                }
                guard page.hasMore_p, let last = page.entries.last else { return true }
                after = last.seq
            } catch {
                report("transcript", error)
                return false
            }
        }
    }

    private func handle(event: Cocoonbox_Agent_V1_Event) {
        eventsLive = true
        switch event.body {
        case .transcript(let entry):
            upsert(entry)
        case .agent(let agent):
            if let index = agents.firstIndex(where: { $0.id == agent.id }) {
                agents[index] = agent
            } else {
                agents.append(agent)
                if selectedAgentID == nil, agent.externalClient == nil { selectedAgentID = agent.id }
            }
        case .agentDeleted(let deleted):
            forget(agentID: deleted.agentID)
        case .textDelta(let delta):
            appendLive(agentID: delta.agentID, turnID: delta.turnID, block: delta.block, reasoning: false, subagent: delta.subagent, text: delta.text)
        case .reasoningDelta(let delta):
            appendLive(agentID: delta.agentID, turnID: delta.turnID, block: delta.block, reasoning: true, subagent: delta.subagent, text: delta.text)
        case .computerAction(let action):
            Perf.mark("computer_action", "\(action.action) \(action.x),\(action.y)")
            computerActions += 1
            screen.showMarker(action)
        case .offload(let update):
            notifyOffload(update)
        case nil:
            break
        }
    }

    // forget drops a deleted agent; a selection on it moves to the next agent in the box, else the last one.
    private func forget(agentID: String) {
        guard let index = agents.firstIndex(where: { $0.id == agentID }) else { return }
        let removed = agents.remove(at: index)
        transcripts[agentID] = nil
        live[agentID] = nil
        loadedTranscripts.remove(agentID)
        guard selectedAgentID == agentID else { return }
        let inBox = agents.filter { $0.externalClient == nil }
        selectedAgentID = (inBox.first { $0.createdAtMs >= removed.createdAtMs } ?? inBox.last)?.id
    }

    private func appendLive(agentID: String, turnID: String, block: UInt32, reasoning: Bool, subagent: String, text: String) {
        var blocks = live[agentID] ?? []
        if let index = blocks.firstIndex(where: { $0.turnID == turnID && $0.block == block && $0.reasoning == reasoning }) {
            blocks[index].text += text
        } else {
            blocks.append(LiveBlock(turnID: turnID, block: block, reasoning: reasoning, subagent: subagent, text: text))
        }
        live[agentID] = blocks
    }

    // settleLive drops the streamed copy once its complete block arrives.
    private func settleLive(_ entry: Entry) {
        guard let blocks = live[entry.agentID], !blocks.isEmpty else { return }
        let remaining: [LiveBlock]
        switch entry.body {
        case .text(let text):
            remaining = blocks.filter { !($0.turnID == entry.turnID && $0.block == text.block && !$0.reasoning) }
        case .reasoning(let reasoning):
            remaining = blocks.filter { !($0.turnID == entry.turnID && $0.block == reasoning.block && $0.reasoning) }
        case .turnEnd:
            remaining = blocks.filter { $0.turnID != entry.turnID }
        default:
            return
        }
        if remaining.count != blocks.count {
            live[entry.agentID] = remaining
        }
    }

    private func upsert(_ entry: Entry) {
        settleLive(entry)
        var list = transcripts[entry.agentID] ?? []
        if let last = list.last, last.seq < entry.seq {
            list.append(entry)
        } else if let index = list.firstIndex(where: { $0.seq >= entry.seq }) {
            if list[index].seq == entry.seq {
                list[index] = entry
            } else {
                list.insert(entry, at: index)
            }
        } else {
            list.append(entry)
        }
        transcripts[entry.agentID] = list
    }

    // MARK: agent actions

    func createAgent(name: String = "") {
        guard let connection else { return }
        Task {
            do {
                let agent = try await connection.createAgent(name: name)
                if !agents.contains(where: { $0.id == agent.id }) {
                    agents.append(agent)
                }
                selectedAgentID = agent.id
            } catch {
                report("create agent", error)
            }
        }
    }

    func send(_ text: String) {
        guard let connection, let agentID = selectedAgentID else { return }
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        Perf.mark("send", "agent=\(agentID)")
        Task {
            do {
                try await connection.send(agentID: agentID, text: text)
            } catch {
                report("send", error)
            }
        }
    }

    func renameAgent(_ agent: Agent, to name: String) {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let connection, !name.isEmpty, name != agent.name else { return }
        Task {
            do {
                try await connection.renameAgent(agentID: agent.id, name: name)
            } catch {
                report("rename agent", error)
            }
        }
    }

    func deleteAgent(_ agent: Agent) {
        guard let connection else { return }
        Task {
            do {
                try await connection.deleteAgent(agentID: agent.id)
                forget(agentID: agent.id)
            } catch {
                report("delete agent", error)
            }
        }
    }

    func respond(to approval: Cocoonbox_Agent_V1_ApprovalRequest, agentID: String, approve: Bool, text: String = "") {
        guard let connection else { return }
        answeredApprovals.insert(approval.approvalID)
        if approval.kind == "take_over" {
            screen.takeOver = false
        }
        Task {
            do {
                try await connection.respond(agentID: agentID, approvalID: approval.approvalID, approve: approve, text: text)
            } catch {
                answeredApprovals.remove(approval.approvalID)
                report("respond", error)
            }
        }
    }

    func answerConnector(_ approval: Cocoonbox_Agent_V1_ApprovalRequest, agentID: String, decision: Cocoonbox_Control_V1_ApprovalDecision) {
        guard let connection else { return }
        answeredApprovals.insert(approval.approvalID)
        Task {
            do {
                guard try await connection.answerConnector(agentID: agentID, approvalID: approval.approvalID, decision: decision) else {
                    lastError = "That approval had expired or was already answered in the console."
                    return
                }
            } catch {
                answeredApprovals.remove(approval.approvalID)
                report("approve", error)
            }
        }
    }

    func cancelTurn() {
        guard let connection, let agentID = selectedAgentID else { return }
        Task {
            do {
                try await connection.cancel(agentID: agentID)
            } catch {
                report("cancel", error)
            }
        }
    }

    func report(_ what: String, _ error: any Error) {
        let message = "\(what): \(BoxConnection.describe(error))"
        lastError = message
        Perf.mark("error", message)
    }
}
