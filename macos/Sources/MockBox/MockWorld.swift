import CocoonBoxKit
import Foundation
import GRPCCore
import ImageIO
import RFBTestServer
import SwiftProtobuf
import UniformTypeIdentifiers

typealias Box = Cocoonbox_Control_V1_Box
typealias OperationEvent = Cocoonbox_Control_V1_OperationEvent
typealias Event = Cocoonbox_Agent_V1_Event
typealias Entry = Cocoonbox_Agent_V1_TranscriptEntry
typealias Agent = Cocoonbox_Agent_V1_Agent
typealias Share = Cocoonbox_Control_V1_Share

// MockWorld holds the state of a user's fake boxes: their lifecycle within
// the api's running limit and box quota, and the agents, transcript, event
// bus and synthetic desktop that every box shows alike.
actor MockWorld {
    static let ringSize = 512
    static let maxBoxes = 3
    static let users = ["qa2": "u-qa2000000002", "qa3": "u-qa3000000003"]
    static let shareID = "sh-mock000000"
    static let noBox = RPCError(code: .notFound, message: "no such box for this user (it may have been deleted)")
    static let usage = Cocoonbox_Control_V1_UsageResponse.with {
        let sonnet = Cocoonbox_Control_V1_ModelUsage.with {
            $0.model = "anthropic/claude-sonnet-5"
            $0.requests = 2
            $0.promptTokens = 1000
            $0.completionTokens = 100
            $0.totalTokens = 1100
            $0.costMicros = 4_000_000
        }
        let jev = Cocoonbox_Control_V1_ModelUsage.with {
            $0.model = "typesafe/jev-1.13"
            $0.requests = 3
            $0.promptTokens = 900
            $0.completionTokens = 30
            $0.totalTokens = 930
            $0.costMicros = 1500
        }
        $0.today = .with {
            $0.sinceMs = 1_760_313_600_000
            $0.requests = 5
            $0.promptTokens = 1900
            $0.completionTokens = 130
            $0.totalTokens = 2030
            $0.costMicros = 4_001_500
            $0.models = [sonnet, jev]
        }
        $0.month = .with {
            $0.sinceMs = 1_759_276_800_000
            $0.requests = 5
            $0.promptTokens = 1900
            $0.completionTokens = 130
            $0.totalTokens = 2030
            $0.costMicros = 4_001_500
            $0.models = [sonnet, jev]
        }
        $0.dailyCapMicros = 5_000_000
        $0.defaultCap = true
        $0.remainingTodayMicros = 998_500
    }

    let bootDelay: Duration
    // boxes is oldest first, as the api lists them.
    private(set) var boxes: [Box] {
        didSet {
            guard boxes != oldValue else { return }
            for watcher in boxWatchers.values { watcher.yield(boxes) }
        }
    }
    private var boxWatchers: [UUID: AsyncStream<[Box]>.Continuation] = [:]
    // deletions counts deleted boxes; a delete ends every screen stream, as removing a machine ends its own.
    private(set) var deletions = 0
    private var boxesCreated = 1
    private var operations: [String: [OperationEvent]] = [:]
    private var operationBoxes: [String: String] = [:]
    private var operationWaiters: [String: [UUID: AsyncStream<OperationEvent>.Continuation]] = [:]
    private var agents: [String: Agent] = [:]
    private var untitled: Set<String> = []
    private var openSessions: Set<String> = []
    private var share: Share?
    private(set) var wakeSchedules: [Cocoonbox_Control_V1_WakeSchedule] = []
    private var sharesCreated = 0
    private var nonces: [String: String] = [:]
    private var transcripts: [String: [Entry]] = [:]
    private var nextSeq: UInt64 = 1
    private var ring: [Event] = []
    private var nextOffset: UInt64 = 1
    private var subscribers: [UUID: AsyncStream<Event>.Continuation] = [:]
    private var approvals: [String: CheckedContinuation<Cocoonbox_Agent_V1_RespondRequest, Never>] = [:]
    private var desktopText = ""
    private weak var screen: SyntheticRFBServer?
    private var animateFPS = 0
    private var externalDelay: Duration?
    private let restoreOnWake: Bool
    nonisolated let connect: MockConnect

    init(bootDelay: Duration, animateFPS: Int, externalDelay: Duration?, externalHistory: Bool = false, restoreOnWake: Bool = false, connect: MockConnect) {
        self.connect = connect
        self.bootDelay = bootDelay
        self.animateFPS = animateFPS
        self.externalDelay = externalDelay
        self.restoreOnWake = restoreOnWake
        var box = Box()
        box.id = "cb-mock00000000"
        box.name = "main"
        box.state = .hibernated
        box.image = "cbdesk:v22"
        box.latestImage = "cbdesk:v23"
        box.lastSavedAtMs = Self.now() - 12 * 60_000
        box.flavor.vcpus = 4
        box.flavor.memoryMib = 8192
        box.flavor.diskGib = 40
        box.createdAtMs = Self.now()
        boxes = [box]
        if externalHistory {
            agents = Self.externalHistory()
        }
    }

    // externalHistory is idle MCP sessions of several clients, hours old,
    // like a box that other clients have used before.
    private static func externalHistory() -> [String: Agent] {
        var agents: [String: Agent] = [:]
        let now = Self.now()
        let hour: Int64 = 3_600_000
        let sessions: [(String, Int64)] = [
            ("cb-api-curl", 5 * hour), ("cb-api-curl", 3 * hour), ("cb-api-curl", 2 * hour),
            ("inspector-cli", 4 * hour), ("inspector-cli", 90 * 60_000), ("diag", 10 * 60_000),
        ]
        for (client, age) in sessions {
            var agent = Agent()
            agent.id = "ag-\(UUID().uuidString.prefix(8).lowercased())"
            agent.name = "mcp:\(client)@\(Self.shareID)"
            agent.status = .idle
            agent.createdAtMs = now - age
            agent.updatedAtMs = now - age + 60_000
            agents[agent.id] = agent
        }
        return agents
    }

    static func now() -> Int64 {
        Int64(Date().timeIntervalSince1970 * 1000)
    }

    // MARK: boxes

    // watchBoxes returns the boxes and a stream of the boxes after each later change, as Boxes.Watch sends them.
    func watchBoxes() -> (boxes: [Box], changes: AsyncStream<[Box]>) {
        let key = UUID()
        let (stream, continuation) = AsyncStream<[Box]>.makeStream(bufferingPolicy: .bufferingNewest(1))
        boxWatchers[key] = continuation
        continuation.onTermination = { _ in Task { await self.dropBoxWatcher(key) } }
        return (boxes, stream)
    }

    private func dropBoxWatcher(_ key: UUID) {
        boxWatchers[key] = nil
    }

    // box is the first box, which a client without a box id means.
    var box: Box { boxes.first ?? Box() }

    func box(_ ref: String) -> Box? {
        ref.isEmpty ? boxes.first : boxes.first { $0.id == ref || $0.name == ref }
    }

    func ensure(_ boxID: String = "") throws -> Cocoonbox_Control_V1_EnsureResponse {
        guard let target = box(boxID) else { throw MockWorld.noBox }
        var response = Cocoonbox_Control_V1_EnsureResponse()
        if target.state == .running || target.state == .starting {
            response.box = target
            if target.state == .starting, let id = operations.keys.filter({ operationBoxes[$0] == target.id }).sorted().last {
                response.operation.id = id
                response.operation.boxID = target.id
            }
            return response
        }
        try checkRunning(besides: target.id)
        let waking = target.state == .hibernated
        let id = newOperation(target.id)
        setState(target.id, .starting)
        response.box = box(target.id) ?? target
        response.operation = operation(id)
        if waking, restoreOnWake {
            Task { await self.restore(id: id) }
            return response
        }
        boot(id, phase: waking ? .waking : .creating, image: target.image)
        return response
    }

    static let offeredImages = ["cbdesk:v22", "cbcore:v1", "cbdroid:v1"]

    // create adds a box within the quota and starts it, as Boxes.Create does; cbcore:v1 and cbdroid:v1, or their families, are the others offered.
    func create(name: String, image: String = "") throws -> Cocoonbox_Control_V1_EnsureResponse {
        let offered = ["cbcore": "cbcore:v1", "cbdroid": "cbdroid:v1"]
        let target = offered[image] ?? (offered.values.contains(image) ? image : nil)
        guard image.isEmpty || target != nil else { throw RPCError(code: .invalidArgument, message: "image \"\(image)\" is not offered") }
        guard !name.isEmpty, name.allSatisfy({ $0.isLowercase || $0.isNumber || $0 == "-" || $0 == "_" }), !name.hasPrefix("cb-") else {
            throw RPCError(code: .invalidArgument, message: "invalid request: a box name is 1 to 32 lowercase letters, digits, - or _")
        }
        guard !boxes.contains(where: { $0.name == name }) else { throw RPCError(code: .alreadyExists, message: "already exists: a box is already named \"\(name)\"") }
        guard boxes.count < Self.maxBoxes else { throw RPCError(code: .failedPrecondition, message: "quota reached: \(Self.maxBoxes) boxes is the most this user may have") }
        try checkRunning(besides: "")
        var created = boxes[0]
        created.id = "cb-mock" + String(format: "%08d", boxesCreated)
        boxesCreated += 1
        created.name = name
        created.state = .starting
        if let target { created.latestImage = target }
        created.image = created.latestImage
        created.lastSavedAtMs = 0
        created.createdAtMs = Self.now()
        boxes.append(created)
        let id = newOperation(created.id)
        boot(id, phase: .creating, image: created.image)
        return .with {
            $0.box = created
            $0.operation = operation(id)
        }
    }

    func rename(box boxID: String, name: String) throws -> Box {
        guard let index = boxes.firstIndex(where: { $0.id == boxID }) else { throw MockWorld.noBox }
        guard !boxes.contains(where: { $0.name == name }) else { throw RPCError(code: .alreadyExists, message: "already exists: a box is already named \"\(name)\"") }
        boxes[index].name = name
        return boxes[index]
    }

    // delete plays Boxes.Delete: a save when the box runs, then the machine and the box go.
    func delete(box boxID: String) throws -> Cocoonbox_Control_V1_Operation {
        guard let target = box(boxID), target.id == boxID else { throw MockWorld.noBox }
        let id = newOperation(target.id)
        Task {
            if target.state == .running {
                await self.transfer(id: id, phase: .saving, detail: "uploading changed files", state: .running, prefix: [])
            }
            self.emitOperation(id: id, phase: .deleting, detail: "removing machine \(target.id)", state: target.state)
            self.boxes.removeAll { $0.id == target.id }
            self.deletions += 1
            self.emitOperation(id: id, phase: .done, detail: "deleted; the saved state of \(target.id) is removed after the retention of deleted boxes", state: .unspecified)
        }
        return operation(id)
    }

    // checkRunning refuses a start while another box holds the running limit of 1.
    private func checkRunning(besides boxID: String) throws {
        guard let running = boxes.first(where: { $0.id != boxID && [.starting, .running, .hibernating].contains($0.state) }) else { return }
        throw RPCError(code: .failedPrecondition, message: "box \(running.name) (\(running.id)) is running, and 1 may run at a time; hibernate it first")
    }

    private func boot(_ id: String, phase: Cocoonbox_Control_V1_Phase, image: String) {
        let steps: [(Cocoonbox_Control_V1_Phase, String, Cocoonbox_Control_V1_BoxState)] = [
            (phase, "scheduling on cocoon-cb", .starting),
            (phase, phase == .waking ? "restoring memory snapshot" : "cloning \(image)", .starting),
            (phase, "waiting for the agent", .starting),
            (.done, "box is running", .running),
        ]
        let delay = bootDelay
        Task {
            for step in steps {
                try? await Task.sleep(for: delay / steps.count)
                self.emitOperation(id: id, phase: step.0, detail: step.1, state: step.2)
            }
        }
    }

    func hibernate(_ boxID: String = "") -> Cocoonbox_Control_V1_Operation {
        let target = box(boxID)?.id ?? box.id
        let id = newOperation(target)
        setState(target, .hibernating)
        Task {
            try? await Task.sleep(for: .milliseconds(400))
            self.emitOperation(id: id, phase: .snapshotting, detail: "snapshotting memory and disk", state: .hibernating)
            try? await Task.sleep(for: .milliseconds(600))
            self.emitOperation(id: id, phase: .done, detail: "hibernated", state: .hibernated)
        }
        return operation(id)
    }

    // saveState plays a quiesced save: the browser closes, then chunks upload with byte progress.
    func saveState(_ boxID: String = "") -> Cocoonbox_Control_V1_Operation {
        let target = box(boxID) ?? box
        let id = newOperation(target.id)
        let state = target.state
        Task {
            await self.transfer(id: id, phase: .saving, detail: "uploading changed files", state: state, prefix: [(.snapshotting, "closing the browser and snapshotting home, /workspace and agent.db")])
            self.update(target.id) { $0.lastSavedAtMs = Self.now() }
            self.emitOperation(id: id, phase: .done, detail: "saved", state: state)
        }
        return operation(id)
    }

    // rebase plays an image upgrade with the box states the api reports: save
    // on the old machine, new machine from the latest image, restore, switch.
    func rebase(_ boxID: String = "", image: String) -> Cocoonbox_Control_V1_Operation {
        let rebased = box(boxID) ?? box
        let id = newOperation(rebased.id)
        let target = image.isEmpty ? rebased.latestImage : image
        Task {
            await self.transfer(id: id, phase: .saving, detail: "uploading changed files", state: .running, prefix: [(.snapshotting, "closing the browser and snapshotting home, /workspace and agent.db")])
            self.emitOperation(id: id, phase: .creating, detail: "new machine from \(target)", state: .starting)
            try? await Task.sleep(for: self.bootDelay)
            await self.transfer(id: id, phase: .restoring, detail: "restoring files into the new machine", state: .running, prefix: [])
            self.update(rebased.id) {
                $0.image = target
                $0.lastSavedAtMs = Self.now()
            }
            self.emitOperation(id: id, phase: .done, detail: "running on \(target)", state: .running)
        }
        return operation(id)
    }

    // snapshots are three saves an hour apart, newest first; the oldest predates changed-file counts.
    func snapshots(of boxID: String) -> [Cocoonbox_Control_V1_Snapshot] {
        let newest = (box(boxID)?.lastSavedAtMs).flatMap { $0 > 0 ? $0 : nil } ?? Self.now()
        return (0..<3).map { index in
            .with {
                $0.id = "snap-\(index)"
                $0.savedAtMs = newest - Int64(index) * 3_600_000
                $0.files = 1200
                $0.logicalBytes = 96 << 20
                $0.addedBytes = UInt64(3 - index) << 20
                if index < 2 { $0.changedFiles = UInt64(index + 1) }
            }
        }
    }

    func browse(box boxID: String, snapshot: String, dir: String) throws -> Cocoonbox_Control_V1_BrowseSnapshotResponse {
        guard snapshots(of: boxID).contains(where: { $0.id == snapshot }) else {
            throw RPCError(code: .notFound, message: "snapshot not found")
        }
        guard let children = Self.capsuleTree[dir] else {
            throw RPCError(code: .invalidArgument, message: "invalid box path: \(dir)")
        }
        return .with {
            $0.entries = children.map { path in
                .with {
                    $0.path = path
                    $0.kind = Self.capsuleTree[path] == nil ? .file : .dir
                    $0.size = Self.capsuleTree[path] == nil ? 4096 : 0
                    $0.mtimeMs = Self.now() - 86_400_000
                }
            }
        }
    }

    // restorePaths plays a file restore into the running box; every path must be in the mock tree.
    func restorePaths(box boxID: String, snapshot: String, paths: [String]) throws -> Cocoonbox_Control_V1_Operation {
        guard let target = box(boxID), target.state == .running else {
            throw RPCError(code: .failedPrecondition, message: "box is not running")
        }
        guard let saved = snapshots(of: boxID).first(where: { $0.id == snapshot }) else {
            throw RPCError(code: .notFound, message: "snapshot not found")
        }
        let known = Set(Self.capsuleTree.values.joined())
        guard !paths.isEmpty, paths.allSatisfy(known.contains) else {
            throw RPCError(code: .invalidArgument, message: "invalid box path")
        }
        let id = newOperation(target.id)
        let folder = Self.restoredFolder(saved.savedAtMs)
        Task {
            await self.transfer(id: id, phase: .restoringFiles, detail: "restoring", state: .running, prefix: [])
            self.emitOperation(id: id, phase: .done, detail: "restored \(paths.count) paths (\(paths.joined(separator: ", "))) into ~/Restored/\(folder)", state: .running)
        }
        return operation(id)
    }

    // setWakeSchedule keeps the api's checks a client sees: an IANA zone, five cron fields, awake_for in 1 min..1 h
    // (0 is 15 min) and 3 schedules a user; the next time is an hour ahead, since the mock reads no cron.
    func setWakeSchedule(box boxID: String, _ request: Cocoonbox_Control_V1_SetWakeScheduleRequest) throws -> Cocoonbox_Control_V1_WakeSchedule {
        guard TimeZone(identifier: request.tz) != nil else {
            throw RPCError(code: .invalidArgument, message: "tz \"\(request.tz)\" must name an IANA zone, such as Asia/Singapore")
        }
        guard request.cron.split(separator: " ").count == 5 else { throw RPCError(code: .invalidArgument, message: "expected exactly 5 fields") }
        let awakeFor = request.awakeForS == 0 ? 900 : request.awakeForS
        guard (60...3600).contains(awakeFor) else { throw RPCError(code: .invalidArgument, message: "awake_for must be in 1m0s..1h0m0s") }
        let others = wakeSchedules.filter { $0.boxID != boxID }
        guard others.count < 3 else { throw RPCError(code: .failedPrecondition, message: "quota reached: 3 wake schedules is the most this user may have") }
        let schedule = Cocoonbox_Control_V1_WakeSchedule.with {
            $0.boxID = boxID
            $0.cron = request.cron
            $0.tz = request.tz
            $0.awakeForS = awakeFor
            $0.nextAtMs = Self.now() + 3_600_000
        }
        wakeSchedules = others + [schedule]
        return schedule
    }

    func clearWakeSchedule(box boxID: String) throws {
        guard wakeSchedules.contains(where: { $0.boxID == boxID }) else { throw RPCError(code: .notFound, message: "the box has no wake schedule") }
        wakeSchedules.removeAll { $0.boxID == boxID }
    }

    private static let capsuleTree: [String: [String]] = [
        "": ["/home/user", "/workspace"],
        "/home/user": ["/home/user/Documents", "/home/user/notes.txt"],
        "/home/user/Documents": ["/home/user/Documents/drafts", "/home/user/Documents/report.pdf"],
        "/home/user/Documents/drafts": ["/home/user/Documents/drafts/v1.txt"],
        "/workspace": ["/workspace/README.md", "/workspace/app"],
        "/workspace/app": ["/workspace/app/main.go"],
    ]

    private static func restoredFolder(_ ms: Int64) -> String {
        let format = DateFormatter()
        format.timeZone = TimeZone(identifier: "UTC")
        format.dateFormat = "yyyy-MM-dd'T'HHmm'Z'"
        return format.string(from: Date(timeIntervalSince1970: Double(ms) / 1000))
    }

    // restore plays a wake of a box with no machine: a new one boots and the saved state is restored into it.
    private func restore(id: String) async {
        emitOperation(id: id, phase: .creating, detail: "machine for the saved state", state: .starting)
        try? await Task.sleep(for: bootDelay)
        await transfer(id: id, phase: .restoring, detail: "restoring files into the new machine", state: .running, prefix: [])
        emitOperation(id: id, phase: .done, detail: "restored the saved state", state: .running)
    }

    private func newOperation(_ boxID: String) -> String {
        let id = "op-\(UUID().uuidString.prefix(8).lowercased())"
        operations[id] = []
        operationBoxes[id] = boxID
        return id
    }

    private func operation(_ id: String) -> Cocoonbox_Control_V1_Operation {
        var op = Cocoonbox_Control_V1_Operation()
        op.id = id
        op.boxID = operationBoxes[id] ?? box.id
        return op
    }

    private func update(_ boxID: String, _ change: (inout Box) -> Void) {
        if let index = boxes.firstIndex(where: { $0.id == boxID }) { change(&boxes[index]) }
    }

    private func setState(_ boxID: String, _ state: Cocoonbox_Control_V1_BoxState) {
        update(boxID) { $0.state = state }
    }

    private func transfer(id: String, phase: Cocoonbox_Control_V1_Phase, detail: String, state: Cocoonbox_Control_V1_BoxState, prefix: [(Cocoonbox_Control_V1_Phase, String)]) async {
        for (step, text) in prefix {
            emitOperation(id: id, phase: step, detail: text, state: state)
            try? await Task.sleep(for: .milliseconds(700))
        }
        let total: UInt64 = 48 << 20
        for step in 1...6 {
            emitOperation(id: id, phase: phase, detail: detail, state: state, done: total * UInt64(step) / 6, total: total)
            try? await Task.sleep(for: .milliseconds(500))
        }
    }

    private func emitOperation(id: String, phase: Cocoonbox_Control_V1_Phase, detail: String, state: Cocoonbox_Control_V1_BoxState, done: UInt64 = 0, total: UInt64 = 0) {
        var event = OperationEvent()
        event.doneBytes = done
        event.totalBytes = total
        event.operationID = id
        event.phase = phase
        event.detail = detail
        event.offsetKey = String(format: "%04d", (operations[id]?.count ?? 0) + 1)
        event.atMs = Self.now()
        event.boxState = state
        if state != .unspecified { setState(operationBoxes[id] ?? box.id, state) }
        if state == .running, let delay = externalDelay {
            externalDelay = nil
            Task {
                try? await Task.sleep(for: delay)
                await self.runExternal()
            }
        }
        operations[id, default: []].append(event)
        for continuation in operationWaiters[id, default: [:]].values {
            continuation.yield(event)
            if phase == .done || phase == .failed {
                continuation.finish()
            }
        }
    }

    func watch(operation id: String, after offsetKey: String) throws -> (replay: [OperationEvent], live: AsyncStream<OperationEvent>?) {
        guard let events = operations[id] else {
            throw RPCError(code: .notFound, message: "operation \(id) not found")
        }
        let replay = events.filter { offsetKey.isEmpty || $0.offsetKey > offsetKey }
        if let last = events.last, last.phase == .done || last.phase == .failed {
            return (replay, nil)
        }
        let (stream, continuation) = AsyncStream<OperationEvent>.makeStream()
        let key = UUID()
        operationWaiters[id, default: [:]][key] = continuation
        continuation.onTermination = { _ in Task { await self.dropOperationWaiter(id: id, key: key) } }
        return (replay, stream)
    }

    private func dropOperationWaiter(id: String, key: UUID) {
        operationWaiters[id]?[key] = nil
    }

    // MARK: event bus

    func subscribe(from offset: UInt64) throws -> (replay: [Event], live: AsyncStream<Event>) {
        var replay: [Event] = []
        if offset > 0 {
            let first = ring.first?.offset ?? nextOffset
            guard offset + 1 >= first, offset < nextOffset else {
                throw RPCError(code: .outOfRange, message: "offset \(offset) is outside the replay window; resync with Transcript")
            }
            replay = ring.filter { $0.offset > offset }
        }
        let (stream, continuation) = AsyncStream<Event>.makeStream(bufferingPolicy: .bufferingNewest(4096))
        let key = UUID()
        subscribers[key] = continuation
        continuation.onTermination = { _ in Task { await self.dropSubscriber(key) } }
        return (replay, stream)
    }

    private func dropSubscriber(_ key: UUID) {
        subscribers[key] = nil
    }

    private func publish(_ body: Event.OneOf_Body) {
        var event = Event()
        event.offset = nextOffset
        event.body = body
        nextOffset += 1
        ring.append(event)
        if ring.count > Self.ringSize {
            ring.removeFirst(ring.count - Self.ringSize)
        }
        for continuation in subscribers.values {
            continuation.yield(event)
        }
    }

    // MARK: agents

    func createAgent(name: String, nonce: String, model: String) -> Agent {
        if !nonce.isEmpty, let id = nonces["create:\(nonce)"], let agent = agents[id] {
            return agent
        }
        var agent = Agent()
        agent.id = "ag-\(UUID().uuidString.prefix(8).lowercased())"
        agent.name = name.isEmpty ? "agent" : name
        agent.status = .idle
        agent.mainModel = model.isEmpty ? "deepseek/deepseek-v4.1-flash" : model
        agent.createdAtMs = Self.now()
        agent.updatedAtMs = agent.createdAtMs
        agents[agent.id] = agent
        if name.isEmpty {
            untitled.insert(agent.id)
        }
        if !nonce.isEmpty {
            nonces["create:\(nonce)"] = agent.id
        }
        publish(.agent(agent))
        return agent
    }

    func listAgents() -> [Agent] {
        agents.values.sorted { $0.createdAtMs < $1.createdAtMs }
    }

    func transcript(agent id: String, after seq: UInt64, limit: Int) -> (entries: [Entry], hasMore: Bool) {
        let all = (transcripts[id] ?? []).filter { $0.seq > seq }
        let page = Array(all.prefix(limit))
        return (page, all.count > page.count)
    }

    func send(agent id: String, text: String, nonce: String) throws -> String {
        guard agents[id] != nil else {
            throw RPCError(code: .notFound, message: "agent \(id) not found")
        }
        if !nonce.isEmpty, let turn = nonces["send:\(id):\(nonce)"] {
            return turn
        }
        let turn = "turn-\(UUID().uuidString.prefix(8).lowercased())"
        if !nonce.isEmpty {
            nonces["send:\(id):\(nonce)"] = turn
        }
        append(agent: id, turn: turn, role: .user, body: .text(.with { $0.text = text }))
        if untitled.contains(id), let title = Self.title(text), var agent = agents[id] {
            untitled.remove(id)
            agent.name = title
            agents[id] = agent
            publish(.agent(agent))
        }
        setStatus(agent: id, .running)
        Task { await self.runTurn(agent: id, turn: turn, prompt: text.lowercased()) }
        return turn
    }

    // title is the first non-blank line, trimmed and cut to 24 characters, as the box names an untitled agent.
    static func title(_ text: String) -> String? {
        let line = text.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .first { !$0.isEmpty }
        return line.map { String($0.prefix(24)).trimmingCharacters(in: .whitespaces) }
    }

    func respond(_ request: Cocoonbox_Agent_V1_RespondRequest) throws {
        guard let waiter = approvals.removeValue(forKey: request.approvalID) else {
            throw RPCError(code: .notFound, message: "approval \(request.approvalID) not pending")
        }
        waiter.resume(returning: request)
    }

    func cancel(agent id: String) {
        setStatus(agent: id, .idle)
    }

    func rename(agent id: String, name: String) throws -> Agent {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else {
            throw RPCError(code: .invalidArgument, message: "name is empty")
        }
        guard var agent = agents[id] else {
            throw RPCError(code: .notFound, message: "agent \(id) not found")
        }
        untitled.remove(id)
        agent.name = name
        agent.updatedAtMs = Self.now()
        agents[id] = agent
        publish(.agent(agent))
        return agent
    }

    // delete drops the agent and its transcript; a turn still playing writes nothing more.
    func delete(agent id: String) throws {
        guard agents[id] != nil else {
            throw RPCError(code: .notFound, message: "agent \(id) not found")
        }
        guard !openSessions.contains(id) else {
            throw RPCError(code: .failedPrecondition, message: "agent \(id) has an open MCP session; close the client's session first")
        }
        agents[id] = nil
        transcripts[id] = nil
        untitled.remove(id)
        publish(.agentDeleted(.with { $0.agentID = id }))
    }

    private func setStatus(agent id: String, _ status: Cocoonbox_Agent_V1_AgentStatus) {
        guard var agent = agents[id] else { return }
        agent.status = status
        agent.updatedAtMs = Self.now()
        agents[id] = agent
        publish(.agent(agent))
    }

    @discardableResult
    private func append(agent id: String, turn: String, role: Cocoonbox_Agent_V1_Role, body: Entry.OneOf_Body) -> Entry {
        var entry = Entry()
        guard agents[id] != nil else { return entry }
        entry.seq = nextSeq
        nextSeq += 1
        entry.agentID = id
        entry.turnID = turn
        entry.role = role
        entry.atMs = Self.now()
        entry.body = body
        transcripts[id, default: []].append(entry)
        publish(.transcript(entry))
        return entry
    }

    private func computer(agent id: String, action: String, x: Int32, y: Int32, text: String = "") {
        publish(.computerAction(.with {
            $0.agentID = id
            $0.action = action
            $0.x = x
            $0.y = y
            $0.text = text
        }))
    }

    // connectorTask plays a box agent's write through the box door (design §16.5): the
    // broker asks, the user answers in Connectors.Approve and Agents.Respond, the agent retries.
    private func connectorTask(agent id: String, turn: String) async {
        let approval = "apr_\(UUID().uuidString.prefix(8).lowercased())"
        connect.pend(approval)
        append(agent: id, turn: turn, role: .assistant, body: .approval(.with {
            $0.approvalID = approval
            $0.kind = "connector"
            $0.summary = "GitHub: create_issue repo=loomhand/cocoonbox title=Mock issue"
        }))
        setStatus(agent: id, .waitingForUser)
        let answer = await withCheckedContinuation { approvals[approval] = $0 }
        setStatus(agent: id, .running)
        guard answer.approve else {
            _ = connect.take(approval)
            append(agent: id, turn: turn, role: .user, body: .text(.with { $0.text = "Denied." }))
            return
        }
        let call = "call-\(UUID().uuidString.prefix(6).lowercased())"
        append(agent: id, turn: turn, role: .assistant, body: .toolCall(.with {
            $0.callID = call
            $0.name = "github_create_issue"
            $0.argumentsJson = #"{"repo":"loomhand/cocoonbox","title":"Mock issue"}"#
        }))
        let decision = connect.take(approval)
        append(agent: id, turn: turn, role: .tool, body: .toolResult(.with {
            $0.callID = call
            switch decision {
            case .once, .always:
                $0.text = "Created issue #42 (\(decision == .always ? "always" : "once"))."
            case .deny:
                $0.isError = true
                $0.text = "Refused: denied."
            default:
                $0.isError = true
                $0.text = "Refused: not answered yet."
            }
        }))
    }

    private func runTurn(agent id: String, turn: String, prompt: String) async {
        let pause = Duration.milliseconds(350)
        try? await Task.sleep(for: pause)
        append(agent: id, turn: turn, role: .assistant, body: .reasoning(.with {
            $0.text = "The user asked: \(prompt). I will decide which tool tier fits: shell first, the computer subagent only for GUI work."
        }))
        try? await Task.sleep(for: pause)
        if prompt.contains("mousepad") || prompt.contains("type") || prompt.contains("click") {
            await guiTask(agent: id, turn: turn, pause: pause)
        } else if prompt.contains("login") || prompt.contains("take over") {
            let approval = "ap-\(UUID().uuidString.prefix(6).lowercased())"
            append(agent: id, turn: turn, role: .assistant, body: .approval(.with {
                $0.approvalID = approval
                $0.kind = "take_over"
                $0.summary = "The site needs you to sign in. Take over the screen, sign in, then press I'm done."
            }))
            setStatus(agent: id, .waitingForUser)
            let answer = await withCheckedContinuation { approvals[approval] = $0 }
            setStatus(agent: id, .running)
            append(agent: id, turn: turn, role: .user, body: .text(.with {
                $0.text = answer.approve ? (answer.text.isEmpty ? "I'm done, continue." : answer.text) : "Denied."
            }))
        } else if prompt.contains("issue") {
            await connectorTask(agent: id, turn: turn)
        } else {
            let call = "call-\(UUID().uuidString.prefix(6).lowercased())"
            append(agent: id, turn: turn, role: .assistant, body: .toolCall(.with {
                $0.callID = call
                $0.name = "shell"
                $0.argumentsJson = #"{"command":"uname -a && df -h /workspace"}"#
            }))
            try? await Task.sleep(for: pause)
            append(agent: id, turn: turn, role: .tool, body: .toolResult(.with {
                $0.callID = call
                $0.text = "Linux cocoonbox 6.12.0 #1 SMP x86_64 GNU/Linux\nFilesystem  Size  Used Avail Use% Mounted on\n/dev/vda1    40G  6.1G   34G  16% /"
            }))
        }
        let reply = "Done. The box runs Linux 6.12 with 34 GB free in /workspace. Tell me what to do next."
        for word in reply.split(separator: " ", omittingEmptySubsequences: false) {
            try? await Task.sleep(for: .milliseconds(40))
            guard agents[id] != nil else { return }
            publish(.textDelta(.with {
                $0.agentID = id
                $0.turnID = turn
                $0.block = 0
                $0.text = word + " "
            }))
        }
        append(agent: id, turn: turn, role: .assistant, body: .text(.with {
            $0.text = reply
            $0.block = 0
        }))
        append(agent: id, turn: turn, role: .assistant, body: .turnEnd(.with {
            $0.ok = true
            $0.usage.inputTokens = 1834
            $0.usage.outputTokens = 212
            $0.usage.cachedInputTokens = 1024
        }))
        setStatus(agent: id, .idle)
    }

    private func guiTask(agent id: String, turn: String, pause: Duration) async {
        let steps: [(String, Int32, Int32, String)] = [
            ("click", 640, 380, ""),
            ("type_text", 640, 380, "Hello from the cocoonbox agent\n"),
            ("click", 1000, 160, ""),
        ]
        append(agent: id, turn: turn, role: .assistant, body: .toolCall(.with {
            $0.callID = "call-delegate"
            $0.name = "delegate_computer"
            $0.argumentsJson = #"{"task":"type into Mousepad"}"#
        }))
        append(agent: id, turn: turn, role: .assistant, body: .text(.with {
            $0.text = "Mousepad is open; I will click into the editor and type."
            $0.subagent = "computer"
        }))
        for (action, x, y, text) in steps {
            let call = "call-\(UUID().uuidString.prefix(6).lowercased())"
            append(agent: id, turn: turn, role: .assistant, body: .toolCall(.with {
                $0.callID = call
                $0.name = "computer"
                $0.subagent = "computer"
                $0.argumentsJson = text.isEmpty ? #"{"action":"\#(action)","x":\#(x),"y":\#(y)}"# : #"{"action":"\#(action)","text":"\#(text.replacingOccurrences(of: "\n", with: "\\n"))"}"#
            }))
            computer(agent: id, action: action, x: x, y: y, text: text)
            try? await Task.sleep(for: .milliseconds(150))
            if text.isEmpty {
                screen?.click(x: Int(x), y: Int(y))
            } else {
                desktopText += text
                screen?.type(text)
            }
            try? await Task.sleep(for: pause)
            append(agent: id, turn: turn, role: .tool, body: .toolResult(.with {
                $0.callID = call
                $0.text = "\(action) ok"
                $0.image = screen?.snapshotPNG() ?? Data()
            }))
        }
        append(agent: id, turn: turn, role: .tool, body: .toolResult(.with {
            $0.callID = "call-delegate"
            $0.text = "typed the text into Mousepad"
        }))
    }

    // runExternal plays one scripted MCP session of a guest's Claude Code through the share, which stays open for a minute or until ended.
    private func runExternal() async {
        let guest = share.map { $0.id + ($0.users.first.map { ":" + $0.id } ?? "") } ?? Self.shareID
        let id = openExternal(client: "claude-code@\(guest)", version: "2.1.285")
        let calls: [(String, String)] = [
            ("shell", #"{"command":"uname -sr && ls /workspace"}"#),
            ("screenshot", "{}"),
            ("computer_act", #"{"action":"click","x":640,"y":380}"#),
            ("computer_act", #"{"action":"type_text","text":"Typed by Claude Code over MCP\n"}"#),
            ("read_file", #"{"path":"/workspace/notes.md"}"#),
        ]
        for (tool, arguments) in calls where openSessions.contains(id) {
            _ = await externalCall(agent: id, tool: tool, arguments: arguments)
            try? await Task.sleep(for: .milliseconds(900))
        }
        for _ in 0..<600 where openSessions.contains(id) {
            try? await Task.sleep(for: .milliseconds(100))
        }
        if openSessions.contains(id) { closeExternal(agent: id, reason: "deleted by the client") }
    }

    // MARK: shares

    func createShare(_ request: Cocoonbox_Control_V1_CreateShareRequest) throws -> Cocoonbox_Control_V1_ShareSecret {
        guard box(request.boxID)?.id == request.boxID else { throw MockWorld.noBox }
        if let share, share.expiresAtMs > Self.now() { throw RPCError(code: .alreadyExists, message: "the box already has a live share") }
        guard (0...30 * 86400).contains(request.ttlSeconds) else { throw RPCError(code: .invalidArgument, message: "ttl must be at most 30 days") }
        var created = Share()
        created.id = sharesCreated == 0 ? Self.shareID : "sh-\(UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(12).lowercased())"
        created.boxID = request.boxID
        created.write = request.write
        created.createdAtMs = Self.now()
        created.expiresAtMs = created.createdAtMs + (request.ttlSeconds == 0 ? 86400 : request.ttlSeconds) * 1000
        for name in request.userNames {
            guard let id = Self.users[name] else { throw RPCError(code: .invalidArgument, message: "unknown user \(name)") }
            created.users.append(.with {
                $0.id = id
                $0.name = name
            })
        }
        sharesCreated += 1
        return issue(created)
    }

    func listShares() -> [Share] {
        share.map { $0.expiresAtMs > Self.now() ? [$0] : [] } ?? []
    }

    func setShareWrite(id: String, write: Bool) throws -> Share {
        guard var live = share, live.id == id else { throw RPCError(code: .notFound, message: "share not found") }
        live.write = write
        share = live
        return live
    }

    func regenerateShare(id: String) throws -> Cocoonbox_Control_V1_ShareSecret {
        guard let live = share, live.id == id else { throw RPCError(code: .notFound, message: "share not found") }
        endSessions(share: id, reason: "the share link was regenerated")
        return issue(live)
    }

    func revokeShare(id: String) throws {
        guard share?.id == id else { throw RPCError(code: .notFound, message: "share not found") }
        share = nil
        endSessions(share: id, reason: "the share was revoked")
    }

    func end(_ request: Cocoonbox_Agent_V1_EndRequest) throws {
        switch request.target {
        case .agentID(let id):
            guard openSessions.contains(id) else { throw RPCError(code: .notFound, message: "no open MCP session drives agent \(id)") }
            closeExternal(agent: id, reason: "ended by the owner")
        case .shareID(let id):
            endSessions(share: id, reason: "the share ended")
        case nil:
            throw RPCError(code: .invalidArgument, message: "end needs an agent_id or a share_id")
        }
    }

    private func endSessions(share id: String, reason: String) {
        for agent in openSessions where agents[agent]?.shareSession?.shareID == id {
            closeExternal(agent: agent, reason: reason)
        }
    }

    // issue gives the share a new secret and keeps it, so List returns the link as the api does.
    private func issue(_ live: Share) -> Cocoonbox_Control_V1_ShareSecret {
        var issued = live
        issued.secret = "cbs_" + UUID().uuidString.replacingOccurrences(of: "-", with: "").lowercased()
        share = issued
        return .with {
            $0.share = issued
            $0.secret = issued.secret
        }
    }

    // MARK: external agents (MCP)

    // openExternal starts an MCP session's agent, named mcp:<clientInfo.name>
    // as the contract says, with the session notes the box's agent writes:
    // system text entries that share turn and block.
    func openExternal(client: String, version: String) -> String {
        let id = createAgent(name: "mcp:\(client)", nonce: "", model: "").id
        openSessions.insert(id)
        setStatus(agent: id, .running)
        append(agent: id, turn: "mcp-\(id)", role: .system, body: .text(.with { $0.text = "MCP session opened by \(client) \(version)" }))
        return id
    }

    func closeExternal(agent id: String, reason: String) {
        openSessions.remove(id)
        append(agent: id, turn: "mcp-\(id)", role: .system, body: .text(.with { $0.text = "MCP session closed: \(reason)" }))
        append(agent: id, turn: "mcp-\(id)", role: .assistant, body: .turnEnd(.with { $0.ok = true }))
        setStatus(agent: id, .idle)
    }

    // externalCall runs one MCP tool call against the mock desktop and records it in the agent's transcript; an owner session has no agent and records nothing.
    func externalCall(agent id: String, tool: String, arguments: String, on boxName: String = "main") async -> (text: String, image: Data?, isError: Bool) {
        let call = "call-\(UUID().uuidString.prefix(6).lowercased())"
        let turn = "mcp-\(id)"
        append(agent: id, turn: turn, role: .assistant, body: .toolCall(.with {
            $0.callID = call
            $0.name = tool
            $0.argumentsJson = arguments
        }))
        let args = (try? JSONSerialization.jsonObject(with: Data(arguments.utf8))) as? [String: Any] ?? [:]
        var text: String
        var image: Data?
        var isError = false
        switch tool {
        case "shell":
            text = "$ \(args["command"] as? String ?? "")\nLinux 6.12.0 (mockbox) on \(boxName)\nnotes.md"
        case "read_file":
            text = "# Notes\nThe box is reachable over MCP."
        case "write_file", "edit_file":
            text = "wrote \(args["path"] as? String ?? "")"
        case "list_dir":
            text = "notes.md"
        case "screenshot", "computer_state":
            text = "1280x800 PNG"
            image = screen?.snapshotPNG()
        case "computer_act":
            let action = args["action"] as? String ?? "click"
            let x = Int32(args["x"] as? Int ?? 640)
            let y = Int32(args["y"] as? Int ?? 380)
            let typed = args["text"] as? String ?? ""
            computer(agent: id, action: action, x: x, y: y, text: typed)
            try? await Task.sleep(for: .milliseconds(150))
            if typed.isEmpty {
                screen?.click(x: Int(x), y: Int(y))
            } else {
                desktopText += typed
                screen?.type(typed)
            }
            text = "\(action) ok"
            image = screen?.snapshotPNG()
        case let name where name.hasPrefix("browser_"):
            text = "\(name) ok (mock browser)"
        default:
            text = "unknown tool \(tool)"
            isError = true
        }
        append(agent: id, turn: turn, role: .tool, body: .toolResult(.with {
            $0.callID = call
            $0.text = text
            $0.isError = isError
            $0.image = image ?? Data()
        }))
        return (text, image, isError)
    }

    // MARK: screen

    func attach(screen server: SyntheticRFBServer) {
        if !desktopText.isEmpty {
            server.type(desktopText)
        }
        if animateFPS > 0 {
            server.animate(fps: animateFPS)
        }
        screen = server
    }

    func detach(screen server: SyntheticRFBServer, text: String) {
        guard screen === server else { return }
        desktopText = text
        screen = nil
    }
}
